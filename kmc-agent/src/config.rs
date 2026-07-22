use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 상태 파일에 저장되는 프로비저닝 결과.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: Uuid,
    pub name: String,
    pub provision_token: String,
}

pub fn hub_url() -> String {
    std::env::var("KMC_HUB_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

pub fn state_path() -> String {
    std::env::var("KMC_AGENT_STATE").unwrap_or_else(|_| "agent-state.json".to_string())
}

pub fn status_interval_secs() -> u64 {
    std::env::var("KMC_STATUS_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

/// 테스트용 가짜 배터리 값 (설정 시 실제 배터리 대신 보고).
pub fn fake_battery() -> Option<f32> {
    std::env::var("KMC_FAKE_BATTERY").ok().and_then(|v| v.parse().ok())
}
