use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use kmc_proto::HubToAdmin;
use serde::Deserialize;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: String,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, q.token))
}

async fn handle_socket(socket: WebSocket, state: AppState, token: String) {
    // 토큰 검증.
    let username = match state.admin_from_token(&token) {
        Some(u) => u,
        None => {
            let (mut sink, _) = socket.split();
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    };
    tracing::info!(%username, "admin ws connected");

    let (mut sink, mut stream) = socket.split();

    // 즉시 스냅샷.
    let snapshot = match state.snapshot() {
        Ok(agents) => HubToAdmin::Snapshot { agents },
        Err(e) => {
            tracing::error!(error=%e, "snapshot failed");
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    };
    if send_admin(&mut sink, &snapshot).await.is_err() {
        return;
    }

    // 브로드캐스트 구독.
    let mut rx = state.0.admin_bcast.subscribe();

    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        if send_admin(&mut sink, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped=n, "admin ws lagged");
                        // 재동기화: 최신 스냅샷 전송.
                        if let Ok(agents) = state.snapshot() {
                            if send_admin(&mut sink, &HubToAdmin::Snapshot { agents }).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // 클라이언트 close 감지.
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    tracing::info!(%username, "admin ws disconnected");
}

async fn send_admin<S>(sink: &mut S, msg: &HubToAdmin) -> anyhow::Result<()>
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
