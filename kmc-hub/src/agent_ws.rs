use axum::extract::ws::{Message, WebSocket};
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use kmc_proto::{
    AgentToHub, AgentView, AlertLevel, HubToAdmin, HubToAgent, LOW_BATTERY_THRESHOLD,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db;
use crate::state::{AgentConn, AppState};

/// 연결마다 발급하는 단조 증가 식별자. 같은 agent_id 의 옛/새 연결을 구분한다.
static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub async fn handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, peer.ip().to_string()))
}

async fn handle_socket(socket: WebSocket, state: AppState, peer_ip: String) {
    let (mut sink, mut stream) = socket.split();

    // 1. 첫 텍스트 프레임 = Hello.
    let agent_id;
    let name;
    let mut reported_addr: Option<String> = None;
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(txt))) => {
                match serde_json::from_str::<AgentToHub>(&txt) {
                    Ok(AgentToHub::Hello { agent_id: id, name: n, provision_token, stream_addr }) => {
                        let verified = {
                            let conn = state.0.db.lock();
                            db::verify_agent(&conn, id, &provision_token)
                        };
                        match verified {
                            Ok(Some(db_name)) => {
                                agent_id = id;
                                // db의 정식 이름 사용(신뢰 원천).
                                name = db_name;
                                reported_addr = stream_addr;
                                break;
                            }
                            _ => {
                                tracing::warn!(%id, "agent hello auth failed");
                                let _ = sink.send(Message::Close(None)).await;
                                return;
                            }
                        }
                    }
                    _ => {
                        tracing::warn!("agent first frame not a valid hello");
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return,
            Some(Ok(_)) => continue, // ping/binary 무시, hello 계속 대기
            Some(Err(e)) => {
                tracing::warn!(error=%e, "agent ws error during hello");
                return;
            }
        }
    }

    // 2. 등록.
    // 같은 agent_id 로 이미 연결이 있으면(중복 실행·재접속 레이스) 이 새 연결이 대체한다.
    // 대체된 옛 연결의 tx 는 여기서 drop 되어 그쪽 송신 태스크가 스스로 종료된다.
    let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<HubToAgent>();
    {
        let mut online = state.0.online.lock();
        // 재접속 시 상태 보고가 한 번 들어올 때까지 화면이 비지 않도록 직전 상태를 물려준다.
        let prev = online.insert(
            agent_id,
            AgentConn { name: name.clone(), tx, last_status: None, conn_id },
        );
        if let Some(prev) = prev {
            if let Some(entry) = online.get_mut(&agent_id) {
                entry.last_status = prev.last_status;
            }
            tracing::warn!(
                %agent_id, %name, old_conn = prev.conn_id, new_conn = conn_id,
                "duplicate agent connection - replacing previous (agent 중복 실행 의심)"
            );
        }
    }
    // 스트리밍 타겟 주소: agent 가 보고한 tailnet IP 우선, 없으면 WS peer_ip 폴백.
    // (공개 hub 뒤에서는 peer_ip 가 프록시 내부 IP 라 쓸모없으므로 보고값이 필수.)
    let stream_addr = reported_addr.clone().unwrap_or_else(|| peer_ip.clone());
    state.0.agent_addr.lock().insert(agent_id, stream_addr.clone());
    tracing::info!(%agent_id, %name, %stream_addr, %peer_ip, "agent online");

    // HelloOk 전송.
    if send_json(&mut sink, &HubToAgent::HelloOk).await.is_err() {
        cleanup(&state, agent_id, conn_id).await;
        return;
    }
    state.broadcast_agent(agent_id);

    // 3a. mpsc rx -> WS sink 송신 태스크 + 주기적 Ping keepalive.
    // CF/traefik 등 프록시가 하향 무응답 WS 를 idle 로 보고 끊는 것을 막는다(15s Ping).
    let send_task = tokio::spawn(async move {
        let mut ka = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(m) => { if send_json(&mut sink, &m).await.is_err() { break; } }
                        None => break,
                    }
                }
                _ = ka.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                }
            }
        }
    });

    // 3b. WS stream 수신 루프.
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(txt)) => match serde_json::from_str::<AgentToHub>(&txt) {
                Ok(AgentToHub::Status(report)) => {
                    let low_batt = matches!(report.battery_percent, Some(p) if p < LOW_BATTERY_THRESHOLD)
                        && report.battery_charging == Some(false);
                    let batt_pct = report.battery_percent;
                    {
                        let mut online = state.0.online.lock();
                        if let Some(conn) = online.get_mut(&agent_id) {
                            conn.last_status = Some(report);
                        }
                    }
                    state.broadcast_agent(agent_id);
                    if low_batt {
                        let pct = batt_pct.map(|p| format!("{:.0}", p)).unwrap_or_else(|| "?".into());
                        let msg = format!("{} 배터리 부족: {}%", agent_name(&state, agent_id), pct);
                        state.broadcast(HubToAdmin::Alert {
                            agent_id,
                            level: AlertLevel::Critical,
                            message: msg,
                        });
                    }
                }
                Ok(AgentToHub::CommandResult(result)) => {
                    let sender = state.0.pending_cmds.lock().remove(&result.command_id);
                    if let Some(tx) = sender {
                        let _ = tx.send(result);
                    } else {
                        tracing::warn!(cmd=%result.command_id, "command result with no pending waiter");
                    }
                }
                Ok(AgentToHub::Hello { .. }) => {
                    tracing::warn!(%agent_id, "unexpected hello after handshake");
                }
                Err(e) => tracing::warn!(error=%e, "bad agent frame"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error=%e, "agent ws recv error");
                break;
            }
        }
    }

    // 4. 종료 정리.
    send_task.abort();
    cleanup(&state, agent_id, conn_id).await;
}

fn agent_name(state: &AppState, agent_id: Uuid) -> String {
    state
        .0
        .online
        .lock()
        .get(&agent_id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| agent_id.to_string())
}

/// 이 연결(`conn_id`)이 여전히 등록된 현행 연결일 때만 정리한다.
/// 이미 새 연결로 교체된 뒤라면 아무것도 건드리지 않는다 - 그러지 않으면 뒤늦게
/// 끊긴 옛 연결이 살아있는 새 연결의 주소·제어세션까지 지워 agent 가 오프라인으로
/// 잘못 표시된다(agent 중복 실행·재접속 레이스에서 발생).
///
/// 실측(임시 hub + 같은 agent_id WS 2개): ws1 접속 -> ws2 접속으로 교체
/// (`old_conn=1 new_conn=2`) -> **ws1 종료 시 `stale agent conn closed` 만 찍히고
/// `agent offline` 은 없음** -> ws2 종료에서야 `agent offline`.
async fn cleanup(state: &AppState, agent_id: Uuid, conn_id: u64) {
    {
        let mut online = state.0.online.lock();
        match online.get(&agent_id) {
            Some(c) if c.conn_id == conn_id => {
                online.remove(&agent_id);
            }
            _ => {
                tracing::debug!(%agent_id, conn_id, "stale agent conn closed - 현행 등록 유지");
                return;
            }
        }
    }
    state.0.agent_addr.lock().remove(&agent_id);
    // 해당 agent의 세션 락 해제.
    {
        let mut sessions = state.0.sessions.lock();
        sessions.remove(&agent_id);
    }
    tracing::info!(%agent_id, "agent offline");
    // AgentUpdated(online=false)
    if let Ok(Some(view)) = state.build_agent_view(agent_id) {
        state.broadcast(HubToAdmin::AgentUpdated { agent: view });
    } else {
        // laptops 행이 사라진 극단 케이스: 최소 뷰.
        let view = AgentView {
            agent_id,
            name: agent_id.to_string(),
            online: false,
            status: None,
            controlled_by: None,
            tailscale_addr: None,
        };
        state.broadcast(HubToAdmin::AgentUpdated { agent: view });
    }
}

async fn send_json<S>(sink: &mut S, msg: &HubToAgent) -> anyhow::Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let txt = serde_json::to_string(msg)?;
    sink.send(Message::Text(txt.into()))
        .await
        .map_err(|e| anyhow::anyhow!("ws send: {e}"))?;
    Ok(())
}
