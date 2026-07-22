use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use kmc_proto::{AgentView, CommandApiReq, CommandResult, HubToAdmin, SessionReq, SessionResp};
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub mod stream;
use stream::{SharedStream, StreamState};

/// 로그인 세션(hub 접속 정보 + admin 토큰).
#[derive(Clone)]
struct Session {
    hub_url: String,
    token: String,
    #[allow(dead_code)]
    username: String,
}

#[derive(Default)]
struct Backend {
    session: Mutex<Option<Session>>,
    /// 최신 전체 스냅샷을 유지(프론트 단순화: 항상 전체 Vec emit).
    agents: Mutex<HashMap<Uuid, AgentView>>,
    /// WS 태스크 핸들(중복 방지).
    ws_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

type SharedBackend = Arc<Backend>;

fn ws_url_from(hub_url: &str, token: &str) -> String {
    let base = hub_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{}/admin/ws?token={}", base.trim_end_matches('/'), token)
}

fn require_session(backend: &Backend) -> Result<Session, String> {
    backend
        .session
        .lock()
        .clone()
        .ok_or_else(|| "not logged in".to_string())
}

#[tauri::command]
async fn login(
    app: AppHandle,
    backend: State<'_, SharedBackend>,
    hub_url: String,
    username: String,
    password: String,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/auth/login", hub_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .map_err(|e| format!("login request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("login failed: HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("decode login: {e}"))?;
    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or("login response missing token")?
        .to_string();

    let session = Session { hub_url: hub_url.clone(), token: token.clone(), username };
    *backend.session.lock() = Some(session);
    backend.agents.lock().clear();

    // 기존 WS 태스크 정리.
    if let Some(handle) = backend.ws_task.lock().take() {
        handle.abort();
    }

    // WS 구독 태스크 스폰.
    let ws_url = ws_url_from(&hub_url, &token);
    let backend_arc = backend.inner().clone();
    let app_handle = app.clone();
    let handle = tokio::spawn(async move {
        run_ws(ws_url, backend_arc, app_handle).await;
    });
    *backend.ws_task.lock() = Some(handle);

    Ok(())
}

async fn run_ws(ws_url: String, backend: SharedBackend, app: AppHandle) {
    loop {
        // 세션이 사라졌으면 종료.
        if backend.session.lock().is_none() {
            return;
        }
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((ws, _)) => {
                let (_sink, mut stream) = ws.split();
                while let Some(frame) = stream.next().await {
                    match frame {
                        Ok(Message::Text(txt)) => {
                            if let Ok(msg) = serde_json::from_str::<HubToAdmin>(&txt) {
                                handle_hub_msg(msg, &backend, &app);
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            }
            Err(e) => {
                eprintln!("admin ws connect failed: {e}");
            }
        }
        // 세션 유지 중이면 재접속.
        if backend.session.lock().is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

fn handle_hub_msg(msg: HubToAdmin, backend: &SharedBackend, app: &AppHandle) {
    match msg {
        HubToAdmin::Snapshot { agents } => {
            let mut map = backend.agents.lock();
            map.clear();
            for a in agents {
                map.insert(a.agent_id, a);
            }
            emit_agents(&map, app);
        }
        HubToAdmin::AgentUpdated { agent } => {
            let mut map = backend.agents.lock();
            map.insert(agent.agent_id, agent);
            emit_agents(&map, app);
        }
        HubToAdmin::Alert { agent_id, level, message } => {
            let payload = serde_json::json!({
                "agent_id": agent_id,
                "level": level,
                "message": message,
            });
            let _ = app.emit("alert", payload);
        }
    }
}

fn emit_agents(map: &HashMap<Uuid, AgentView>, app: &AppHandle) {
    let mut list: Vec<AgentView> = map.values().cloned().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    let _ = app.emit("agents", list);
}

#[tauri::command]
async fn request_session(
    backend: State<'_, SharedBackend>,
    agent_id: Uuid,
) -> Result<Option<String>, String> {
    let session = require_session(&backend)?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/session/request", session.hub_url.trim_end_matches('/')))
        .bearer_auth(&session.token)
        .json(&SessionReq { agent_id })
        .send()
        .await
        .map_err(|e| format!("session request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("session op failed: HTTP {}", resp.status()));
    }
    let body: SessionResp = resp.json().await.map_err(|e| format!("decode session: {e}"))?;
    // tailscale_addr = agent 도달 주소(LAN/Tailscale). 이 주소로 스트림을 직접 연결한다.
    Ok(body.tailscale_addr)
}

#[tauri::command]
async fn release_session(
    backend: State<'_, SharedBackend>,
    agent_id: Uuid,
) -> Result<(), String> {
    let session = require_session(&backend)?;
    post_session(&session, "/session/release", agent_id).await
}

async fn post_session(session: &Session, path: &str, agent_id: Uuid) -> Result<(), String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}{}", session.hub_url.trim_end_matches('/'), path))
        .bearer_auth(&session.token)
        .json(&SessionReq { agent_id })
        .send()
        .await
        .map_err(|e| format!("session request: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("session op failed: HTTP {}", resp.status()))
    }
}

#[tauri::command]
async fn run_command(
    backend: State<'_, SharedBackend>,
    agent_id: Uuid,
    script: String,
    destructive: bool,
) -> Result<CommandResult, String> {
    let session = require_session(&backend)?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/agents/{}/command",
            session.hub_url.trim_end_matches('/'),
            agent_id
        ))
        .bearer_auth(&session.token)
        .json(&CommandApiReq { script, destructive, kind: None })
        .send()
        .await
        .map_err(|e| format!("command request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("command failed: HTTP {status} {body}"));
    }
    resp.json::<CommandResult>()
        .await
        .map_err(|e| format!("decode command result: {e}"))
}

/// 스트림 시작. `address`=호스트 IP, `pin`=페어링 필요 시 4자리(이미 페어링됐으면 무시).
/// 블로킹 FFI(LiStartConnection)를 spawn_blocking으로 감싸 UI 스레드를 막지 않는다.
#[tauri::command]
async fn start_stream(
    stream: State<'_, SharedStream>,
    address: String,
    width: u32,
    height: u32,
    fps: u32,
    pin: Option<String>,
) -> Result<(), String> {
    let st = stream.inner().clone();
    tokio::task::spawn_blocking(move || st.start(&address, width, height, fps, pin))
        .await
        .map_err(|e| format!("join: {e}"))?
        .map_err(|e| format!("start_stream: {e}"))
}

#[tauri::command]
fn stop_stream(stream: State<'_, SharedStream>) {
    stream.stop();
}

/// 로컬 스트림 WS 서버 포트. 프론트는 이 포트로 ws://127.0.0.1:PORT 에 붙어
/// 인코딩된 H.264 AU를 받아 WebCodecs로 디코드한다. start_stream 이후 유효.
#[tauri::command]
fn stream_port(stream: State<'_, SharedStream>) -> Option<u16> {
    stream.port()
}

/// 오디오 WS 서버 포트. 프론트는 ws://127.0.0.1:PORT 에 붙어 Opus를 받아 WebCodecs로 디코드한다.
#[tauri::command]
fn stream_audio_port(stream: State<'_, SharedStream>) -> Option<u16> {
    stream.audio_port()
}

/// 협상된 비디오 코덱("h264" 또는 "hevc"). 프론트가 WebCodecs 설정 전에 조회한다.
#[tauri::command]
fn stream_codec() -> String {
    kmc_moonclient::negotiated_codec().to_string()
}

/// 원격 입력 — 절대 마우스 위치(참조 해상도 w×h 기준).
#[tauri::command]
fn stream_mouse_move(x: i32, y: i32, w: i32, h: i32) {
    kmc_moonclient::send_mouse_position(x as i16, y as i16, w as i16, h as i16);
}

/// 원격 입력 — 마우스 버튼(1=L 2=M 3=R 4=X1 5=X2).
#[tauri::command]
fn stream_mouse_button(button: u8, down: bool) {
    kmc_moonclient::send_mouse_button(button, down);
}

/// 원격 입력 — 키보드(code=Windows VK, modifiers=MODIFIER_* 비트).
#[tauri::command]
fn stream_key(code: i32, down: bool, modifiers: u8) {
    kmc_moonclient::send_key(code as i16, down, modifiers);
}

/// 원격 입력 — 세로 스크롤(WHEEL_DELTA=120 단위).
#[tauri::command]
fn stream_scroll(amount: i32) {
    kmc_moonclient::send_scroll(amount as i16);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        // 스트림 프레임은 로컬 WebSocket으로 전달한다(stream.rs). 커스텀 프로토콜/RGBA 경로 제거됨.
        .setup(|app| {
            app.manage::<SharedBackend>(Arc::new(Backend::default()));
            app.manage::<SharedStream>(Arc::new(StreamState::default()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            login,
            request_session,
            release_session,
            run_command,
            start_stream,
            stop_stream,
            stream_port,
            stream_mouse_move,
            stream_mouse_button,
            stream_key,
            stream_scroll,
            stream_audio_port,
            stream_codec
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
