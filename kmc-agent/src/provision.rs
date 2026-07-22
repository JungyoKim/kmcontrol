use std::time::Duration;

use anyhow::{Context, Result};
use kmc_proto::{ProvisionReq, ProvisionResp};
use uuid::Uuid;

use crate::config::{self, AgentState};

/// 상태 파일 로드 → 없으면 hub /provision (지수 백오프) → 실패 시 fallback.
pub async fn provision() -> Result<AgentState> {
    let path = config::state_path();

    // 1. 기존 상태 파일.
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(state) = serde_json::from_slice::<AgentState>(&bytes) {
            tracing::info!(agent_id=%state.agent_id, name=%state.name, "loaded existing agent state");
            return Ok(state);
        }
        tracing::warn!(%path, "state file present but unparsable; re-provisioning");
    }

    // 2. MAC 수집.
    let mac = match mac_address::get_mac_address() {
        Ok(Some(m)) => m.to_string(),
        _ => {
            tracing::warn!("no MAC address found; using random placeholder mac");
            format!("00:00:00:{:02x}:{:02x}:{:02x}", rand::random::<u8>(), rand::random::<u8>(), rand::random::<u8>())
        }
    };

    // 3. hub /provision 지수 백오프.
    let hub = config::hub_url();
    let url = format!("{hub}/provision");
    let client = reqwest::Client::new();
    let delays = [1u64, 2, 4, 8, 16, 30];
    for (attempt, delay) in delays.iter().enumerate() {
        match try_provision(&client, &url, &mac).await {
            Ok(resp) => {
                let state = AgentState {
                    agent_id: resp.agent_id,
                    name: resp.name,
                    provision_token: resp.provision_token,
                };
                save_state(&path, &state)?;
                tracing::info!(agent_id=%state.agent_id, name=%state.name, "provisioned via hub");
                return Ok(state);
            }
            Err(e) => {
                tracing::warn!(attempt=attempt + 1, error=%e, "provision attempt failed");
                if attempt + 1 < delays.len() {
                    tokio::time::sleep(Duration::from_secs(*delay)).await;
                }
            }
        }
    }

    // 4. Fallback (hub 미등록 → WS Hello는 재프로비저닝 후에나 성공).
    let state = AgentState {
        agent_id: Uuid::new_v4(),
        name: format!("student-temp-{:06x}", rand::random::<u32>() & 0xff_ffff),
        provision_token: String::new(),
    };
    tracing::warn!(
        name = %state.name,
        "provision failed after retries; using fallback name (NOT registered with hub — WS Hello will fail until re-provisioned)"
    );
    // Fallback 상태는 저장하지 않는다: hub 복구 후 정상 프로비저닝을 재시도해야 하므로.
    Ok(state)
}

async fn try_provision(client: &reqwest::Client, url: &str, mac: &str) -> Result<ProvisionResp> {
    let resp = client
        .post(url)
        .json(&ProvisionReq { mac: mac.to_string() })
        .send()
        .await
        .context("send provision request")?
        .error_for_status()
        .context("provision http status")?;
    let body = resp.json::<ProvisionResp>().await.context("decode provision resp")?;
    Ok(body)
}

fn save_state(path: &str, state: &AgentState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    std::fs::write(path, json).with_context(|| format!("write state file {path}"))?;
    Ok(())
}
