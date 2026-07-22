//! MCP 도구 서버: hub를 Claude 같은 AI에 노출한다.
//!
//! 핵심 가치 = "다수 노트북을 한 번에" — run_powershell_all이 온라인 노트북 전체에
//! 명령을 병렬 팬아웃하고 결과를 취합한다. GUI 조작(cua-driver)은 후속 도구로 추가 예정.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::hub::HubClient;

#[derive(Clone)]
pub struct KmcServer {
    hub: Arc<HubClient>,
    tool_router: ToolRouter<KmcServer>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunOne {
    /// 대상 노트북의 agent_id (UUID). list_agents로 확인.
    pub agent_id: String,
    /// 실행할 PowerShell 스크립트. 파일 조작·즉석 스크립트 생성/실행 가능.
    pub script: String,
    /// 파괴적 명령(재부팅/삭제 등) 여부. 위험 작업이면 true로 명시.
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunAll {
    /// 온라인 노트북 전체에 실행할 PowerShell 스크립트.
    pub script: String,
    /// 파괴적 명령 여부.
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuiAction {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
    /// cua-driver 도구명 (예: list_apps, list_windows, get_window_state, click, type_text, hotkey, launch_app, get_desktop_state).
    pub tool: String,
    /// 도구 인자 JSON 객체 (예: {"pid":1234,"element_index":3}). 없으면 {}.
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuiStep {
    /// cua-driver 도구명.
    pub tool: String,
    /// 도구 인자 JSON 객체. 없으면 {}.
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuiSequence {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
    /// 순서대로 실행할 GUI 스텝들. 한 번의 호출로 back-to-back 실행되어 LLM 왕복을 없앤다.
    /// 예: 브라우저 URL 이동 = [bring_to_front, {hotkey ctrl+l}, {type_text}, {press_key Enter}].
    pub steps: Vec<GuiStep>,
    /// true면 한 스텝이 에러(isError)를 내도 나머지를 계속 실행한다. 기본 false(에러 시 중단).
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebOpen {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
    /// 열 URL (http/https).
    pub url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebRead {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebClick {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
    /// 클릭할 요소의 보이는 텍스트(부분 일치, 대소문자 무시). web_open/web_read가 반환한 elements의 name.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebType {
    /// 대상 노트북 agent_id (UUID).
    pub agent_id: String,
    /// 입력할 텍스트.
    pub text: String,
    /// (선택) 대상 입력란의 이름/레이블 부분 일치. 없으면 첫 편집 가능 요소.
    #[serde(default)]
    pub field: Option<String>,
}

/// 전용 브라우저 트랙 cua 세션 id (모든 web_* 호출 공유).
const WEB_SESSION: &str = "kmc-web";

/// 스냅샷을 토큰 최소화용 compact JSON으로: page + 클릭 가능한 요소(ref/role/name) + 개요(잘라냄).
fn compact_snapshot(snap: &serde_json::Value) -> serde_json::Value {
    let page = snap.get("page").cloned().unwrap_or_else(|| serde_json::json!({}));
    let elements: Vec<serde_json::Value> = snap
        .get("refs")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    serde_json::json!({
                        "ref": e.get("ref"),
                        "role": e.get("role"),
                        "name": e.get("name"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let outline: String = snap
        .get("outline")
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .chars()
        .take(1500)
        .collect();
    serde_json::json!({ "page": page, "elements": elements, "outline": outline })
}

/// 스냅샷 refs에서 name이 query를 부분 포함(대소문자 무시)하는 첫 ref.
/// editable=true면 actions에 "type"이 있는(입력 가능한) 요소만 대상 — role 이름에 의존하지 않음.
fn find_ref<'a>(snap: &'a serde_json::Value, query: &str, editable: bool) -> Option<(&'a str, &'a str)> {
    let q = query.to_lowercase();
    let refs = snap.get("refs").and_then(|r| r.as_array())?;
    for e in refs {
        if editable {
            let typeable = e
                .get("actions")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|x| x.as_str() == Some("type")))
                .unwrap_or(false);
            if !typeable {
                continue;
            }
        }
        let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if q.is_empty() || name.to_lowercase().contains(&q) {
            if let Some(r) = e.get("ref").and_then(|r| r.as_str()) {
                return Some((r, name));
            }
        }
    }
    None
}

#[tool_router]
impl KmcServer {
    pub fn new(hub: Arc<HubClient>) -> Self {
        Self { hub, tool_router: Self::tool_router() }
    }

    #[tool(
        description = "관리 중인 노트북 전체의 목록과 실시간 상태(온라인 여부, 배터리 %, 디스크 여유, 현재 제어 중인 관리자)를 조회한다. 명령을 보내기 전에 대상 agent_id를 여기서 얻는다."
    )]
    async fn list_agents(&self) -> Result<CallToolResult, McpError> {
        match self.hub.list_agents().await {
            Ok(agents) => {
                let json = serde_json::to_string_pretty(&agents)
                    .unwrap_or_else(|e| format!("serialize error: {e}"));
                Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "list_agents failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "한 노트북에서 PowerShell 스크립트를 실행하고 stdout/stderr/종료코드를 받는다. 진단·수리·파일 조작·즉석 Python/스크립트 실행에 사용. destructive=true는 재부팅/삭제 같은 위험 작업에만."
    )]
    async fn run_powershell(
        &self,
        Parameters(RunOne { agent_id, script, destructive }): Parameters<RunOne>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "invalid agent_id '{agent_id}': {e}"
                ))]))
            }
        };
        match self.hub.run_command(id, script, destructive).await {
            Ok(result) => {
                let json = serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|e| format!("serialize error: {e}"));
                Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "run_powershell failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "온라인인 모든 노트북에서 같은 PowerShell 스크립트를 병렬 실행하고 노트북별 결과를 취합해 반환한다. 여러 대를 한 번에 진단/수리할 때 사용. 오프라인 노트북은 건너뛴다."
    )]
    async fn run_powershell_all(
        &self,
        Parameters(RunAll { script, destructive }): Parameters<RunAll>,
    ) -> Result<CallToolResult, McpError> {
        let agents = match self.hub.list_agents().await {
            Ok(a) => a,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "run_powershell_all: list_agents failed: {e}"
                ))]))
            }
        };
        let online: Vec<_> = agents.into_iter().filter(|a| a.online).collect();
        if online.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "온라인 노트북이 없습니다.",
            )]));
        }

        // 온라인 노트북 전체에 병렬 팬아웃.
        let futures = online.into_iter().map(|a| {
            let hub = self.hub.clone();
            let script = script.clone();
            async move {
                let outcome = hub.run_command(a.agent_id, script, destructive).await;
                serde_json::json!({
                    "agent_id": a.agent_id,
                    "name": a.name,
                    "result": match outcome {
                        Ok(r) => serde_json::json!({
                            "ok": true,
                            "exit_code": r.exit_code,
                            "stdout": r.stdout,
                            "stderr": r.stderr,
                            "error": r.error,
                        }),
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    },
                })
            }
        });
        let results: Vec<_> = futures_util::future::join_all(futures).await;
        let json = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|e| format!("serialize error: {e}"));
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "한 노트북에서 GUI 자동화 도구(cua-driver)를 호출한다. 백그라운드로 앱을 조작하므로 사용자 포커스를 뺏지 않는다. 주요 tool: list_apps(실행/설치 앱), list_windows(창), get_window_state(UIA 트리+스크린샷), click, type_text, hotkey, press_key, scroll, launch_app, get_desktop_state(전체 스크린샷). CLI 없는 프로그램(설치 마법사 등) 조작에 사용."
    )]
    async fn gui_action(
        &self,
        Parameters(GuiAction { agent_id, tool, args }): Parameters<GuiAction>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "invalid agent_id '{agent_id}': {e}"
                ))]))
            }
        };
        let args = if args.is_null() { serde_json::json!({}) } else { args };
        let kind = kmc_proto::CommandKind::Gui { tool, args };
        match self.hub.run_kind(id, kind, false).await {
            Ok(result) => {
                // cua-driver 결과 JSON은 stdout에 실려 온다.
                let out = if result.stdout.is_empty() { result.stderr } else { result.stdout };
                Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "gui_action failed: {e}"
            ))])),
        }
    }

    #[tool(
        description = "여러 GUI 스텝을 한 번의 호출로 순서대로 실행한다(back-to-back). GUI 자동화는 액션마다 왕복이 비싸므로, 예측 가능한 다단계 흐름(브라우저 URL 이동, 폼 입력 등)은 개별 gui_action 대신 이걸로 묶어 호출하면 훨씬 빠르다. 각 스텝은 {tool, args}. 예 URL 이동: steps=[{tool:bring_to_front,args:{pid,window_id}},{tool:hotkey,args:{pid,keys:[ctrl,l],delivery_mode:foreground}},{tool:type_text,args:{pid,text:URL,delivery_mode:foreground}},{tool:press_key,args:{pid,key:Enter,delivery_mode:foreground}}]. 기본은 에러 시 중단."
    )]
    async fn gui_sequence(
        &self,
        Parameters(GuiSequence { agent_id, steps, continue_on_error }): Parameters<GuiSequence>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "invalid agent_id '{agent_id}': {e}"
                ))]))
            }
        };
        let mut results = Vec::new();
        for (i, step) in steps.into_iter().enumerate() {
            let args = if step.args.is_null() { serde_json::json!({}) } else { step.args };
            let kind = kmc_proto::CommandKind::Gui { tool: step.tool.clone(), args };
            let (ok, body) = match self.hub.run_kind(id, kind, false).await {
                Ok(r) => {
                    let out = if r.stdout.is_empty() { r.stderr } else { r.stdout };
                    // cua-driver가 구조화된 실패(isError/refusal/code)를 stdout에 실어 보낼 수 있음.
                    let failed = out.contains("\"isError\"") || out.contains("\"refused\"") || out.contains("background_unavailable");
                    (!failed, out)
                }
                Err(e) => (false, format!("transport error: {e}")),
            };
            results.push(serde_json::json!({ "step": i, "tool": step.tool, "ok": ok, "result": body }));
            if !ok && !continue_on_error {
                break;
            }
        }
        let json = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|e| format!("serialize error: {e}"));
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    // ── 전용 브라우저 트랙 내부 헬퍼 (LLM에 노출 안 됨) ──

    /// cua-driver 도구를 호출하고 stdout JSON을 파싱한다.
    async fn gui_json(
        &self,
        id: uuid::Uuid,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let kind = kmc_proto::CommandKind::Gui { tool: tool.to_string(), args };
        let r = self.hub.run_kind(id, kind, false).await.map_err(|e| e.to_string())?;
        let out = if r.stdout.trim().is_empty() { r.stderr } else { r.stdout };
        serde_json::from_str(out.trim())
            .map_err(|e| format!("cua json parse: {e}: {}", out.chars().take(300).collect::<String>()))
    }

    /// 전용 브라우저를 보장하고 exact-bind하여 (target_id, tab_id)를 얻는다.
    /// 세션 생성 → kmc_ensure_browser(pid,window_id) → get_browser_state bind.
    async fn web_ctx(&self, id: uuid::Uuid) -> Result<(String, String), String> {
        let _ = self
            .gui_json(
                id,
                "start_session",
                serde_json::json!({ "session": WEB_SESSION, "capture_scope": "auto" }),
            )
            .await;
        let ens = self.gui_json(id, "kmc_ensure_browser", serde_json::json!({})).await?;
        if let Some(e) = ens.get("error").and_then(|v| v.as_str()) {
            return Err(format!("ensure browser: {e}"));
        }
        let pid = ens.get("pid").and_then(|v| v.as_u64()).ok_or("no pid from ensure")?;
        let window_id = ens
            .get("window_id")
            .and_then(|v| v.as_u64())
            .ok_or("browser window not visible yet; retry web_open")?;
        let st = self
            .gui_json(
                id,
                "get_browser_state",
                serde_json::json!({ "pid": pid, "window_id": window_id, "session": WEB_SESSION }),
            )
            .await?;
        let target = st.get("target_id").and_then(|v| v.as_str()).ok_or("no target_id")?.to_string();
        let tabs = st.get("tabs").and_then(|v| v.as_array()).ok_or("no tabs")?;
        let tab = tabs
            .iter()
            .find(|t| t.get("active").and_then(|a| a.as_bool()).unwrap_or(false))
            .or_else(|| tabs.first())
            .and_then(|t| t.get("tab_id"))
            .and_then(|v| v.as_str())
            .ok_or("no tab_id")?
            .to_string();
        Ok((target, tab))
    }

    /// bound 탭의 semantic_v2 스냅샷.
    async fn web_snapshot(&self, id: uuid::Uuid, target: &str, tab: &str) -> Result<serde_json::Value, String> {
        self.gui_json(
            id,
            "get_browser_state",
            serde_json::json!({
                "target_id": target,
                "tab_id": tab,
                "snapshot_format": "semantic_v2",
                "session": WEB_SESSION,
            }),
        )
        .await
    }

    #[tool(
        description = "전용 CDP Chrome(격리 프로필, agent가 자동 spawn·관리)으로 URL을 연다. 사용자 브라우저·세션과 분리된 AI 전용 브라우저 트랙 — 스크린샷·좌표 없이 DOM(text ref) 기반으로 동작해 토큰이 거의 안 든다. 반환: 페이지 제목/URL + 클릭 가능한 요소 목록(ref·role·name) + 텍스트 개요. 이후 web_click/web_type/web_read로 조작. 사용자의 로그인 세션이나 이미 열린 탭이 필요한 작업이면 이 도구 대신 gui_action(UIA)으로 사용자 Chrome을 조작하라."
    )]
    async fn web_open(
        &self,
        Parameters(WebOpen { agent_id, url }): Parameters<WebOpen>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("invalid agent_id: {e}"))])),
        };
        let (target, tab) = match self.web_ctx(id).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };
        if let Err(e) = self
            .gui_json(
                id,
                "browser_navigate",
                serde_json::json!({ "target_id": target, "tab_id": tab, "url": url, "session": WEB_SESSION }),
            )
            .await
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!("navigate: {e}"))]));
        }
        // 내비게이션은 target/tab/ref를 무효화하므로 재바인딩 후 스냅샷.
        let (t2, tb2) = match self.web_ctx(id).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };
        match self.web_snapshot(id, &t2, &tb2).await {
            Ok(snap) => Ok(CallToolResult::success(vec![ContentBlock::text(
                compact_snapshot(&snap).to_string(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!("snapshot: {e}"))])),
        }
    }

    #[tool(
        description = "전용 브라우저 현재 탭의 상태를 읽는다(스크린샷 없이). 반환: 페이지 제목/URL + 클릭 가능한 요소(ref·role·name) + 텍스트 개요. 화면에서 무엇을 할 수 있는지 파악할 때 사용."
    )]
    async fn web_read(
        &self,
        Parameters(WebRead { agent_id }): Parameters<WebRead>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("invalid agent_id: {e}"))])),
        };
        let (target, tab) = match self.web_ctx(id).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };
        match self.web_snapshot(id, &target, &tab).await {
            Ok(snap) => Ok(CallToolResult::success(vec![ContentBlock::text(
                compact_snapshot(&snap).to_string(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!("snapshot: {e}"))])),
        }
    }

    #[tool(
        description = "전용 브라우저에서 보이는 텍스트로 요소를 클릭한다. text는 web_open/web_read가 반환한 요소 name의 부분 문자열(대소문자 무시). 신뢰된 CDP 클릭이라 좌표·스크린샷이 필요 없다. 클릭 후 갱신된 페이지 상태를 반환한다."
    )]
    async fn web_click(
        &self,
        Parameters(WebClick { agent_id, text }): Parameters<WebClick>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("invalid agent_id: {e}"))])),
        };
        let (target, tab) = match self.web_ctx(id).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };
        let snap = match self.web_snapshot(id, &target, &tab).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("snapshot: {e}"))])),
        };
        let Some((rf, name)) = find_ref(&snap, &text, false) else {
            let names: Vec<&str> = snap
                .get("refs")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|e| e.get("name").and_then(|n| n.as_str())).take(30).collect())
                .unwrap_or_default();
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no element matching '{text}'. available: {names:?}"
            ))]));
        };
        let rf = rf.to_string();
        let name = name.to_string();
        if let Err(e) = self
            .gui_json(
                id,
                "browser_click",
                serde_json::json!({ "target_id": target, "tab_id": tab, "ref": rf, "session": WEB_SESSION }),
            )
            .await
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!("click: {e}"))]));
        }
        // 클릭이 내비게이션을 유발할 수 있으니 재바인딩 후 상태 반환.
        let after = match self.web_ctx(id).await {
            Ok((t, tb)) => self.web_snapshot(id, &t, &tb).await.ok().map(|s| compact_snapshot(&s)),
            Err(_) => None,
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({ "clicked": name, "page": after }).to_string(),
        )]))
    }

    #[tool(
        description = "전용 브라우저의 편집 가능한 요소(입력란)에 텍스트를 입력한다. field가 있으면 그 이름/레이블을 부분 포함하는 입력란, 없으면 첫 편집 가능 요소를 대상으로 한다. CDP Input 주입이라 좌표·포커스 조작이 필요 없다."
    )]
    async fn web_type(
        &self,
        Parameters(WebType { agent_id, text, field }): Parameters<WebType>,
    ) -> Result<CallToolResult, McpError> {
        let id = match uuid::Uuid::parse_str(&agent_id) {
            Ok(id) => id,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("invalid agent_id: {e}"))])),
        };
        let (target, tab) = match self.web_ctx(id).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
        };
        let snap = match self.web_snapshot(id, &target, &tab).await {
            Ok(s) => s,
            Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(format!("snapshot: {e}"))])),
        };
        // field 힌트로 먼저 찾고, 없으면 첫 입력 가능 요소로 폴백(백엔드 자가교정).
        let hit = find_ref(&snap, field.as_deref().unwrap_or(""), true)
            .or_else(|| find_ref(&snap, "", true));
        let Some((rf, name)) = hit else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "no typeable element on page".to_string(),
            )]));
        };
        let rf = rf.to_string();
        let name = name.to_string();
        match self
            .gui_json(
                id,
                "browser_type",
                serde_json::json!({ "target_id": target, "tab_id": tab, "ref": rf, "text": text, "session": WEB_SESSION }),
            )
            .await
        {
            Ok(_) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::json!({ "typed_into": name, "text": text }).to_string(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!("type: {e}"))])),
        }
    }
}

#[tool_handler]
impl ServerHandler for KmcServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "kmc 노트북 관리 hub를 노출한다. list_agents로 노트북과 상태를 보고, \
                 run_powershell로 한 대를, run_powershell_all로 온라인 전체를 한 번에 제어한다. \
                 PowerShell로 진단·수리·파일 조작·즉석 스크립트 실행이 가능하다.",
            )
    }
}
