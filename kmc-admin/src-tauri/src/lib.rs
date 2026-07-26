use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use futures_util::StreamExt;
use kmc_proto::{AgentView, CommandApiReq, CommandResult, HubToAdmin, SessionReq, SessionResp};
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub mod stream;
#[cfg(windows)]
pub mod keyhook;
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

/// 100.64.0.0/10 (CGNAT) = Tailscale 이 노드에 배정하는 대역.
fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(_) => false,
    }
}

/// tailscaled 가 tailnet 에 붙어 있으면 항상 존재하는 MagicDNS 주소.
/// Windows Tailscale 은 100.64/10 전체 경로를 깔지 않고 **피어별 /32** 만 깐다
/// (실측 `Get-NetRoute 100.*`: 자기 /32, 피어 /32, 100.100.100.100/32). 그래서
/// "tailnet 가입 여부"는 임의의 100.x 가 아니라 이 주소로 물어야 한다 —
/// 실측: 미지 피어 100.99.1.2 는 가입 상태에서도 기본 게이트웨이로 떨어진다.
const TAILNET_PROBE: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 100, 100, 100));

/// 이 PC 가 `target` 으로 나갈 때 tailnet 인터페이스를 경유하는지.
/// UDP `connect` 는 패킷을 보내지 않고 커널 라우팅만 질의하므로 부작용 없이 소스 IP 를
/// 알 수 있다. tailscaled 가 없으면 기본 게이트웨이로 떨어져 소스가 192.168.x 등이 된다.
fn routes_via_tailnet(target: IpAddr) -> bool {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect(SocketAddr::new(target, 47989))?;
            s.local_addr()
        })
        .is_ok_and(|a| is_tailnet_ip(a.ip()))
}

/// 스트림 연결 실패가 tailnet 문제로 설명되면 그 이유를 덧붙인다.
/// 노트북(agent)쪽 Tailscale 은 `install.ps1` 이 MSI 설치 + `up --unattended` 까지
/// 책임지지만, **admin PC 는 수동 설치 전제**라(deploy/README.md:119) 여기서만 빠질 수
/// 있다. 힌트가 없으면 화면에는 원시 연결 타임아웃만 떠서 원인 규명이 불가능하다.
fn tailnet_hint(target: Option<IpAddr>) -> Option<String> {
    let ip = target.filter(|ip| is_tailnet_ip(*ip))?;
    // 프로브는 라우팅 조회뿐이라 값싸지만, 가입 여부가 먼저다(미가입이면 피어 경로도
    // 당연히 없어 두 원인이 겹친다).
    let on_tailnet = routes_via_tailnet(TAILNET_PROBE);
    tailnet_hint_for(ip, on_tailnet, on_tailnet && routes_via_tailnet(ip))
}

/// `tailnet_hint` 의 순수 판정부(프로브 결과를 주입받아 테스트 가능).
fn tailnet_hint_for(ip: IpAddr, on_tailnet: bool, peer_routed: bool) -> Option<String> {
    if !on_tailnet {
        return Some(
            "이 PC 가 Tailscale tailnet 에 연결돼 있지 않습니다. Tailscale 설치 후 \
             `tailscale up --advertise-tags=tag:admin` 으로 로그인하세요."
                .into(),
        );
    }
    if !peer_routed {
        // 가입은 돼 있는데 그 피어만 netmap 에 없다. 단순 오프라인은 여기 걸리지 않는다
        // (실측: 오프라인 피어도 /32 경로는 남는다) — 노드 만료/삭제나 ACL 가림이다.
        return Some(format!(
            "tailnet 에는 연결돼 있으나 {ip} 노드가 보이지 않습니다 \
             (노드 만료·삭제 또는 ACL 로 가려짐). Tailscale 콘솔에서 해당 노트북 노드와 \
             tag:admin -> tag:camp-laptop 규칙을 확인하세요."
        ));
    }
    None
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
    allow_hevc: Option<bool>,
) -> Result<(), String> {
    let allow_hevc = allow_hevc.unwrap_or(false);
    let target = address
        .parse::<IpAddr>()
        .ok()
        .or_else(|| address.parse::<SocketAddr>().ok().map(|s| s.ip()));
    let st = stream.inner().clone();
    let r = tokio::task::spawn_blocking(move || st.start(&address, width, height, fps, pin, allow_hevc))
        .await
        .map_err(|e| format!("join: {e}"))?
        .map_err(|e| format!("start_stream: {e}"))
        .map_err(|e| match tailnet_hint(target) {
            Some(hint) => format!("{e}\n\n{hint}"),
            None => e,
        });
    #[cfg(windows)]
    if r.is_ok() {
        // 연결 버튼을 방금 눌렀으니 admin 이 포커스 상태 — 시드로 설정(이후 Focused 이벤트가 갱신).
        keyhook::set_focused(true);
        keyhook::set_streaming(true);
    }
    r
}

#[tauri::command]
fn stop_stream(stream: State<'_, SharedStream>) {
    #[cfg(windows)]
    keyhook::set_streaming(false);
    stream.stop();
}

/// 프론트가 canvas 화면 절대 사각형(물리 px)을 보고. sidecar 가 마우스 hover 판정 + 좌표 변환에 사용.
#[tauri::command]
fn set_canvas_rect(l: i32, t: i32, r: i32, b: i32) {
    #[cfg(windows)]
    keyhook::set_canvas_rect(l, t, r, b);
    #[cfg(not(windows))]
    let _ = (l, t, r, b);
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
        // admin 창 포커스 변화를 키보드 캡처 게이트에 반영: 포커스일 때만 그랩(아웃포커스면 로컬 통과).
        .on_window_event(|_window, _event| {
            #[cfg(windows)]
            if let tauri::WindowEvent::Focused(focused) = _event {
                keyhook::set_focused(*focused);
            }
        })
        // 스트림 프레임은 로컬 WebSocket으로 전달한다(stream.rs). 커스텀 프로토콜/RGBA 경로 제거됨.
        .setup(|app| {
            app.manage::<SharedBackend>(Arc::new(Backend::default()));
            app.manage::<SharedStream>(Arc::new(StreamState::default()));
            #[cfg(windows)]
            keyhook::install(app.handle().clone());
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
            stream_codec,
            set_canvas_rect
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tailnet 판정은 100.64.0.0/10 경계가 전부다. `o[0]==100` 만 보거나 상한을 128 로
    /// 포함하면 일반 사설망(100.128.x 등)을 tailnet 으로 오인해 엉뚱한 안내가 뜬다.
    #[test]
    fn tailnet_ip_matches_cgnat_range_only() {
        for s in ["100.64.0.0", "100.64.0.1", "100.100.100.100", "100.127.255.255"] {
            assert!(is_tailnet_ip(s.parse().unwrap()), "{s} 는 100.64/10 안");
        }
        for s in ["100.63.255.255", "100.128.0.0", "99.64.0.1", "101.64.0.1", "192.168.1.10", "127.0.0.1"] {
            assert!(!is_tailnet_ip(s.parse().unwrap()), "{s} 는 100.64/10 밖");
        }
        assert!(!is_tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()), "IPv6 는 대상 아님");
    }

    /// 두 원인은 조치가 다르다(내 PC 에 Tailscale 설치 vs 콘솔에서 노드/ACL 확인).
    /// 분기를 뒤집으면 tailnet 에 멀쩡히 붙은 관리자에게 "설치하세요"가 떠서 오히려
    /// 헛짚게 된다.
    #[test]
    fn tailnet_hint_distinguishes_missing_tailnet_from_missing_peer() {
        let ip: IpAddr = "100.101.102.103".parse().unwrap();

        let not_joined = tailnet_hint_for(ip, false, false).expect("미가입이면 안내");
        assert!(not_joined.contains("tailscale up"), "설치/로그인 조치를 제시해야 함");
        assert!(!not_joined.contains("ACL"), "미가입인데 ACL 을 의심시키면 안 됨");

        let peer_gone = tailnet_hint_for(ip, true, false).expect("피어 미인지면 안내");
        assert!(peer_gone.contains("ACL"), "노드/ACL 확인을 제시해야 함");
        assert!(!peer_gone.contains("tailscale up"), "이미 가입 상태를 오진하면 안 됨");
        assert!(peer_gone.contains("100.101.102.103"), "어느 노드인지 밝혀야 함");

        assert!(tailnet_hint_for(ip, true, true).is_none(), "정상 경로면 원시 오류만");
    }

    /// LAN(비-tailnet) 대상 실패까지 tailnet 탓으로 돌리면 오히려 오진이다.
    /// 이 경로는 프로브를 아예 타지 않아 환경과 무관하게 결정적이다.
    #[test]
    fn tailnet_hint_ignores_non_tailnet_targets() {
        assert!(tailnet_hint(Some("192.168.10.20".parse().unwrap())).is_none());
        assert!(tailnet_hint(None).is_none());
    }

    /// 라이브 프로브 검증(이 PC 가 tailnet 노드일 때만 유효) — 기본 실행 제외.
    /// `cargo test --lib -- --ignored --nocapture`
    #[test]
    #[ignore = "로컬 tailnet 가입 상태에 의존"]
    fn live_tailnet_probe_has_no_false_positive() {
        assert!(
            routes_via_tailnet(TAILNET_PROBE),
            "이 PC 가 tailnet 에 붙어 있는데 프로브가 실패하면 관리자에게 헛안내가 나간다"
        );
        assert!(!routes_via_tailnet("8.8.8.8".parse().unwrap()), "공인 IP 는 tailnet 경로가 아님");
    }
}
