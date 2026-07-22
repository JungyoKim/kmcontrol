//! hub REST 클라이언트: MCP 도구가 이걸 통해 hub와 통신한다.
//!
//! MCP 서버 시작 시 env로 로그인해 토큰을 보관하고, 401을 만나면 자동 재로그인한다.
//! hub는 영상/스트리밍은 프록시하지 않지만(사양 제약), 명령/상태 조회는 이 REST가 담당한다.

use anyhow::{anyhow, Context, Result};
use kmc_proto::{AgentView, CommandApiReq, CommandKind, CommandResult};
use parking_lot::Mutex;
use uuid::Uuid;

/// hub 접속 설정 + 세션 토큰(자동 재로그인).
pub struct HubClient {
    base_url: String,
    username: String,
    password: String,
    http: reqwest::Client,
    token: Mutex<Option<String>>,
}

impl HubClient {
    /// env에서 구성: KMC_HUB_URL(기본 http://127.0.0.1:8080), KMC_MCP_USER, KMC_MCP_PASSWORD.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("KMC_HUB_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
            .trim_end_matches('/')
            .to_string();
        let username = std::env::var("KMC_MCP_USER")
            .map_err(|_| anyhow!("KMC_MCP_USER not set (hub admin username)"))?;
        let password = std::env::var("KMC_MCP_PASSWORD")
            .map_err(|_| anyhow!("KMC_MCP_PASSWORD not set (hub admin password)"))?;
        Ok(Self {
            base_url,
            username,
            password,
            http: reqwest::Client::new(),
            token: Mutex::new(None),
        })
    }

    /// hub 로그인 → 토큰 저장. 최초 호출 및 401 복구 시 사용.
    async fn login(&self) -> Result<String> {
        let resp = self
            .http
            .post(format!("{}/auth/login", self.base_url))
            .json(&serde_json::json!({ "username": self.username, "password": self.password }))
            .send()
            .await
            .context("login request")?;
        if !resp.status().is_success() {
            return Err(anyhow!("hub login failed: HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp.json().await.context("decode login")?;
        let token = body
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("login response missing token"))?
            .to_string();
        *self.token.lock() = Some(token.clone());
        Ok(token)
    }

    /// 현재 토큰(없으면 로그인).
    async fn token(&self) -> Result<String> {
        if let Some(t) = self.token.lock().clone() {
            return Ok(t);
        }
        self.login().await
    }

    /// 노트북 목록 + 상태 조회. 401이면 1회 재로그인 후 재시도.
    pub async fn list_agents(&self) -> Result<Vec<AgentView>> {
        let mut token = self.token().await?;
        for attempt in 0..2 {
            let resp = self
                .http
                .get(format!("{}/agents", self.base_url))
                .bearer_auth(&token)
                .send()
                .await
                .context("list agents request")?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                token = self.login().await?; // 토큰 만료 → 재로그인.
                continue;
            }
            if !resp.status().is_success() {
                return Err(anyhow!("GET /agents failed: HTTP {}", resp.status()));
            }
            return resp.json::<Vec<AgentView>>().await.context("decode agents");
        }
        unreachable!()
    }

    /// 한 노트북에 PowerShell 명령 실행(30s 타임아웃은 hub가 관리). 401이면 1회 재로그인.
    pub async fn run_command(
        &self,
        agent_id: Uuid,
        script: String,
        destructive: bool,
    ) -> Result<CommandResult> {
        self.run_req(
            agent_id,
            CommandApiReq { script, destructive, kind: None },
        )
        .await
    }

    /// 임의 CommandKind(GUI 등) 실행.
    pub async fn run_kind(
        &self,
        agent_id: Uuid,
        kind: CommandKind,
        destructive: bool,
    ) -> Result<CommandResult> {
        self.run_req(
            agent_id,
            CommandApiReq { script: String::new(), destructive, kind: Some(kind) },
        )
        .await
    }

    /// 공통 명령 POST. 401이면 1회 재로그인 후 재시도.
    async fn run_req(&self, agent_id: Uuid, req: CommandApiReq) -> Result<CommandResult> {
        let mut token = self.token().await?;
        let body = serde_json::to_value(&req).context("serialize command req")?;
        for attempt in 0..2 {
            let resp = self
                .http
                .post(format!("{}/agents/{}/command", self.base_url, agent_id))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
                .context("run command request")?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                token = self.login().await?;
                continue;
            }
            if !resp.status().is_success() {
                let code = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("command failed: HTTP {code}: {text}"));
            }
            return resp.json::<CommandResult>().await.context("decode command result");
        }
        unreachable!()
    }
}
