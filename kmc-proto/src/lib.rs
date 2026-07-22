use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

pub const CAMP_LAPTOP_TAG: &str = "tag:camp-laptop"; // agent가 tailscale up --advertise-tags에 사용
pub const LOW_BATTERY_THRESHOLD: f32 = 20.0;

// ---- 상태 보고 ----
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusReport {
    pub battery_percent: Option<f32>,   // 배터리 없으면 None
    pub battery_charging: Option<bool>,
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub processes: Vec<ProcessInfo>,    // 메모리 상위 15개
    pub reported_at: DateTime<Utc>,
    /// 하드웨어 인코더(Intel QSV) 사용 가능 여부. None=미확인(구버전 에이전트).
    /// false 면 admin 이 "Intel 드라이버 업데이트 필요" 진단을 띄운다.
    #[serde(default)]
    pub encoder_ok: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem_bytes: u64,
}

// ---- 명령 ----
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandRequest {
    pub command_id: Uuid,
    pub kind: CommandKind,
    pub destructive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandKind {
    /// PowerShell 셸아웃(진단·수리·파일·스크립트).
    PowerShell { script: String },
    /// GUI 자동화: cua-driver 도구 호출. `tool`=cua-driver 도구명, `args`=JSON 인자.
    Gui { tool: String, args: serde_json::Value },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub command_id: Uuid,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>, // 실행 자체 실패(스폰 오류 등)
}

// ---- agent <-> hub (WS, JSON 텍스트) ----
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToHub {
    Hello {
        agent_id: Uuid,
        name: String,
        provision_token: String,
        /// agent 가 보고하는 자기 도달 주소(tailnet 100.x). hub 가 세션 주소로 반환한다.
        /// 없으면 hub 는 WS peer_ip 로 폴백(하위호환).
        #[serde(default)]
        stream_addr: Option<String>,
    },
    Status(StatusReport),
    CommandResult(CommandResult),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToAgent {
    HelloOk,
    RunCommand(CommandRequest),
}

// ---- hub <-> admin ----
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentView {
    pub agent_id: Uuid,
    pub name: String,
    pub online: bool,
    pub status: Option<StatusReport>,
    pub controlled_by: Option<String>,     // 세션 점유 admin username
    pub tailscale_addr: Option<String>,    // 슬라이스에서는 항상 None
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToAdmin {
    Snapshot { agents: Vec<AgentView> },
    AgentUpdated { agent: AgentView },
    Alert { agent_id: Uuid, level: AlertLevel, message: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel { Info, Warning, Critical }

// ---- REST 바디 ----
#[derive(Serialize, Deserialize)] pub struct ProvisionReq { pub mac: String }
#[derive(Serialize, Deserialize)] pub struct ProvisionResp { pub agent_id: Uuid, pub name: String, pub provision_token: String }
#[derive(Serialize, Deserialize)] pub struct LoginReq { pub username: String, pub password: String }
#[derive(Serialize, Deserialize)] pub struct LoginResp { pub token: String }
#[derive(Serialize, Deserialize)] pub struct SessionReq { pub agent_id: Uuid }
#[derive(Serialize, Deserialize)] pub struct SessionResp { pub session_token: String, pub tailscale_addr: Option<String> }
/// 명령 요청 바디. `kind`가 있으면 그걸 쓰고, 없으면 하위호환으로 `script`를 PowerShell로 실행.
#[derive(Serialize, Deserialize)] pub struct CommandApiReq {
    #[serde(default)] pub script: String,
    #[serde(default)] pub destructive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub kind: Option<CommandKind>,
}
