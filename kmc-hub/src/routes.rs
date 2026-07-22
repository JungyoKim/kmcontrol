use std::time::Duration;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use kmc_proto::{
    CommandApiReq, CommandKind, CommandRequest, HubToAgent, LoginReq, LoginResp, ProvisionReq,
    ProvisionResp, SessionReq, SessionResp,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::db;
use crate::state::{AppState, ControlSession};
use crate::util;

/// admin bearer extractor. Authorization: Bearer <token>.
pub struct AdminAuth(pub String /* username */);

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let token = header.strip_prefix("Bearer ").unwrap_or("").trim();
        match state.admin_from_token(token) {
            Some(username) => Ok(AdminAuth(username)),
            None => Err((StatusCode::UNAUTHORIZED, "invalid or missing admin token").into_response()),
        }
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, msg.into()).into_response()
}

// POST /provision — 인증 없음.
pub async fn provision(
    State(state): State<AppState>,
    Json(req): Json<ProvisionReq>,
) -> Response {
    let result = {
        let conn = state.0.db.lock();
        db::get_or_create_laptop(&conn, &req.mac)
    };
    match result {
        Ok(l) => Json(ProvisionResp {
            agent_id: l.agent_id,
            name: l.name,
            provision_token: l.provision_token,
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error=%e, "provision failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "provision failed")
        }
    }
}

// GET /enroll/{secret} — 인증 대신 고정 시크릿 게이트(무입력 부트스트랩용).
// 시크릿이 맞으면 authkey + hub URL 이 주입된 install 원라이너(PowerShell)를 text 로 반환한다.
// 노트북(관리자 PS)에서: irm https://<hub-funnel>/enroll/<secret> | iex
// env 로 구성: KMC_ENROLL_SECRET(필수), KMC_ENROLL_AUTHKEY(주입할 재사용 authkey),
//             KMC_ENROLL_HUB_URL(agent 가 붙을 hub 주소, 보통 tailnet 100.x), KMC_RELEASE_URL, KMC_INSTALL_URL.
pub async fn enroll(Path(secret): Path<String>) -> Response {
    let expected = std::env::var("KMC_ENROLL_SECRET").unwrap_or_default();
    if expected.is_empty() || secret != expected {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let authkey = std::env::var("KMC_ENROLL_AUTHKEY").unwrap_or_default();
    let hub_url = std::env::var("KMC_ENROLL_HUB_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let release_url = std::env::var("KMC_RELEASE_URL").unwrap_or_else(|_| {
        "https://github.com/JungyoKim/kmcontrol/releases/latest/download/kmc-agent-bundle.zip".to_string()
    });
    let install_url = std::env::var("KMC_INSTALL_URL").unwrap_or_else(|_| {
        "https://raw.githubusercontent.com/JungyoKim/kmcontrol/main/deploy/install.ps1".to_string()
    });
    // install.ps1 을 그대로 실행하되, hub/authkey/release 를 env 로 주입해 무입력 설치.
    let script = format!(
        "$env:KMC_HUB_URL='{hub_url}'\n\
         $env:KMC_TS_AUTHKEY='{authkey}'\n\
         $env:KMC_RELEASE_URL='{release_url}'\n\
         irm '{install_url}' | iex\n"
    );
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], script).into_response()
}

// POST /auth/login
pub async fn login(State(state): State<AppState>, Json(req): Json<LoginReq>) -> Response {
    let verified = {
        let conn = state.0.db.lock();
        db::verify_admin(&conn, &req.username, &req.password)
    };
    match verified {
        Ok(Some(admin_id)) => {
            let token = util::random_hex(32);
            state.0.admin_tokens.lock().insert(token.clone(), admin_id);
            state.0.admin_names.lock().insert(token.clone(), req.username.clone());
            Json(LoginResp { token }).into_response()
        }
        Ok(None) => err(StatusCode::UNAUTHORIZED, "invalid credentials"),
        Err(e) => {
            tracing::error!(error=%e, "login failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "login failed")
        }
    }
}

// GET /agents
pub async fn list_agents(_auth: AdminAuth, State(state): State<AppState>) -> Response {
    match state.snapshot() {
        Ok(agents) => Json(agents).into_response(),
        Err(e) => {
            tracing::error!(error=%e, "list agents failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed")
        }
    }
}

// POST /session/request
pub async fn session_request(
    auth: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<SessionReq>,
) -> Response {
    let agent_id = req.agent_id;
    // agent 등록 여부 확인.
    let known = {
        let conn = state.0.db.lock();
        matches!(db::laptop_name(&conn, agent_id), Ok(Some(_)))
    };
    if !known {
        return err(StatusCode::NOT_FOUND, "unknown agent");
    }

    let session_token = {
        let mut sessions = state.0.sessions.lock();
        match sessions.get(&agent_id) {
            Some(s) if s.admin_username != auth.0 => {
                return err(StatusCode::CONFLICT, "agent already controlled by another admin");
            }
            _ => {
                let token = util::random_hex(32);
                sessions.insert(
                    agent_id,
                    ControlSession {
                        admin_username: auth.0.clone(),
                        session_token: token.clone(),
                    },
                );
                token
            }
        }
    };
    let addr = state.0.agent_addr.lock().get(&agent_id).cloned();
    state.broadcast_agent(agent_id);
    Json(SessionResp { session_token, tailscale_addr: addr }).into_response()
}

// POST /session/release
pub async fn session_release(
    auth: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<SessionReq>,
) -> Response {
    let agent_id = req.agent_id;
    {
        let mut sessions = state.0.sessions.lock();
        match sessions.get(&agent_id) {
            Some(s) if s.admin_username == auth.0 => {
                sessions.remove(&agent_id);
            }
            Some(_) => return err(StatusCode::FORBIDDEN, "not the session owner"),
            None => return StatusCode::OK.into_response(),
        }
    }
    state.broadcast_agent(agent_id);
    StatusCode::OK.into_response()
}

// POST /agents/{id}/command
pub async fn run_command(
    _auth: AdminAuth,
    State(state): State<AppState>,
    Path(agent_id): Path<Uuid>,
    Json(req): Json<CommandApiReq>,
) -> Response {
    // agent online 확인 + tx 확보.
    let tx = {
        let online = state.0.online.lock();
        match online.get(&agent_id) {
            Some(conn) => conn.tx.clone(),
            None => return err(StatusCode::CONFLICT, "agent not online"),
        }
    };

    let command_id = Uuid::new_v4();
    let (result_tx, result_rx) = oneshot::channel();
    state.0.pending_cmds.lock().insert(command_id, result_tx);

    let kind = req
        .kind
        .unwrap_or_else(|| CommandKind::PowerShell { script: req.script });
    let cmd = CommandRequest { command_id, kind, destructive: req.destructive };
    if tx.send(HubToAgent::RunCommand(cmd)).is_err() {
        state.0.pending_cmds.lock().remove(&command_id);
        return err(StatusCode::CONFLICT, "agent connection closed");
    }

    match tokio::time::timeout(Duration::from_secs(30), result_rx).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(_)) => {
            state.0.pending_cmds.lock().remove(&command_id);
            err(StatusCode::INTERNAL_SERVER_ERROR, "command channel dropped")
        }
        Err(_) => {
            state.0.pending_cmds.lock().remove(&command_id);
            err(StatusCode::GATEWAY_TIMEOUT, "command timed out")
        }
    }
}
