use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use kmc_proto::{AgentToHub, HubToAgent};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::config::{self, AgentState};
use crate::{exec, sysstat};

/// 무한 재접속 루프.
pub async fn run(state: AgentState) -> Result<()> {
    if state.provision_token.is_empty() {
        tracing::error!("agent has no provision token (fallback state); cannot connect. Re-provision required.");
        // 그래도 재프로비저닝을 노려 재시도할 수 있으나, 슬라이스에서는 종료.
        return Err(anyhow!("no provision token"));
    }

    let ws_url = ws_url(&config::hub_url());
    let mut state = state;
    loop {
        match connect_once(&ws_url, &state).await {
            Ok(()) => tracing::warn!("ws connection closed; reconnecting in 5s"),
            Err(e) if is_auth_failure(&e) => {
                // hub DB 초기화(볼륨 교체/재배포 등)로 이 agent_id/token 이 hub 에 없어진 경우.
                // stale state 를 버리고 재-provision 해서 새 정체성으로 재등록한다(자가치유).
                tracing::warn!("hello auth rejected — dropping stale state and re-provisioning");
                let _ = std::fs::remove_file(config::state_path());
                match crate::provision::provision().await {
                    Ok(fresh) if !fresh.provision_token.is_empty() => {
                        tracing::info!(agent_id=%fresh.agent_id, name=%fresh.name, "re-provisioned after auth failure");
                        state = fresh;
                    }
                    _ => tracing::warn!("re-provision failed; will retry"),
                }
            }
            Err(e) => tracing::warn!(error=%e, "ws connection error; reconnecting in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Hello 인증 거부(hub 가 hello 중 연결 종료)를 식별한다.
fn is_auth_failure(e: &anyhow::Error) -> bool {
    e.to_string().contains("during hello")
}

fn ws_url(hub_url: &str) -> String {
    let base = hub_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{}/agent/ws", base.trim_end_matches('/'))
}

async fn connect_once(ws_url: &str, state: &AgentState) -> Result<()> {
    tracing::info!(%ws_url, "connecting to hub");
    let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("ws connect")?;
    let (mut sink, mut stream) = ws.split();

    // Hello 전송.
    let hello = AgentToHub::Hello {
        agent_id: state.agent_id,
        name: state.name.clone(),
        provision_token: state.provision_token.clone(),
        stream_addr: crate::tailscale::self_ip(),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await
        .context("send hello")?;

    // HelloOk 대기.
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<HubToAgent>(&txt) {
                Ok(HubToAgent::HelloOk) => {
                    tracing::info!("hub accepted hello");
                    break;
                }
                Ok(other) => tracing::warn!(?other, "unexpected pre-hello message"),
                Err(e) => tracing::warn!(error=%e, "bad hub frame during hello"),
            },
            Some(Ok(Message::Close(_))) | None => {
                return Err(anyhow!("hub closed connection during hello (auth failed?)"));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(anyhow!("ws error during hello: {e}")),
        }
    }

    // 송신 채널: 상태 루프와 명령 결과가 공유.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AgentToHub>();

    // 상태 루프.
    let status_tx = out_tx.clone();
    let status_task = tokio::spawn(async move {
        let mut collector = sysstat::Collector::new();
        let interval_secs = config::status_interval_secs();
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            let report = collector.collect();
            if status_tx.send(AgentToHub::Status(report)).is_err() {
                break;
            }
        }
    });

    // 송신 태스크: out_rx -> ws sink.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let txt = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error=%e, "serialize outbound");
                    continue;
                }
            };
            if sink.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    // 수신 루프: RunCommand -> exec -> CommandResult.
    let recv_result: Result<()> = loop {
        match stream.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<HubToAgent>(&txt) {
                Ok(HubToAgent::RunCommand(req)) => {
                    let cmd_tx = out_tx.clone();
                    tokio::spawn(async move {
                        let result = exec::run(req).await;
                        let _ = cmd_tx.send(AgentToHub::CommandResult(result));
                    });
                }
                Ok(HubToAgent::HelloOk) => {} // 재전송 무시
                Err(e) => tracing::warn!(error=%e, "bad hub frame"),
            },
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) | None => break Ok(()),
            Some(Ok(_)) => {}
            Some(Err(e)) => break Err(anyhow!("ws recv: {e}")),
        }
    };

    status_task.abort();
    send_task.abort();
    recv_result
}
