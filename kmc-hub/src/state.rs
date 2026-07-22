use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::Mutex;

use anyhow::Result;
use kmc_proto::{AgentView, HubToAdmin, HubToAgent, CommandResult, StatusReport};
use rusqlite::Connection;
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::db;

/// 온라인 agent 연결.
pub struct AgentConn {
    pub name: String,
    pub tx: mpsc::UnboundedSender<HubToAgent>,
    pub last_status: Option<StatusReport>,
}

/// 제어 세션(락).
#[derive(Clone)]
pub struct ControlSession {
    pub admin_username: String,
    pub session_token: String,
}

pub struct Inner {
    pub db: Mutex<Connection>,
    pub admin_tokens: Mutex<HashMap<String, i64>>, // token -> admin id
    pub admin_names: Mutex<HashMap<String, String>>, // token -> username
    pub online: Mutex<HashMap<Uuid, AgentConn>>,
    /// agent_id -> 도달 가능 주소(WS 연결의 peer IP). 세션 승인 시 admin에게 반환.
    pub agent_addr: Mutex<HashMap<Uuid, String>>,
    pub sessions: Mutex<HashMap<Uuid, ControlSession>>,
    pub pending_cmds: Mutex<HashMap<Uuid, oneshot::Sender<CommandResult>>>,
    pub admin_bcast: broadcast::Sender<HubToAdmin>,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl AppState {
    pub fn new(conn: Connection) -> Self {
        let (admin_bcast, _rx) = broadcast::channel(256);
        AppState(Arc::new(Inner {
            db: Mutex::new(conn),
            admin_tokens: Mutex::new(HashMap::new()),
            admin_names: Mutex::new(HashMap::new()),
            online: Mutex::new(HashMap::new()),
            agent_addr: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_cmds: Mutex::new(HashMap::new()),
            admin_bcast,
        }))
    }

    /// bearer token -> username (검증).
    pub fn admin_from_token(&self, token: &str) -> Option<String> {
        self.0.admin_names.lock().get(token).cloned()
    }

    /// 단일 agent의 뷰 조합 (laptops 행 + online + last_status + session).
    pub fn build_agent_view(&self, agent_id: Uuid) -> Result<Option<AgentView>> {
        let name = {
            let conn = self.0.db.lock();
            db::laptop_name(&conn, agent_id)?
        };
        let Some(name) = name else { return Ok(None) };

        let (online, status) = {
            let map = self.0.online.lock();
            match map.get(&agent_id) {
                Some(c) => (true, c.last_status.clone()),
                None => (false, None),
            }
        };
        let controlled_by = self
            .0
            .sessions
            .lock()
            .get(&agent_id)
            .map(|s| s.admin_username.clone());

        Ok(Some(AgentView {
            agent_id,
            name,
            online,
            status,
            controlled_by,
            tailscale_addr: None,
        }))
    }

    /// 전체 등록 laptops 뷰.
    pub fn snapshot(&self) -> Result<Vec<AgentView>> {
        let laptops = {
            let conn = self.0.db.lock();
            db::all_laptops(&conn)?
        };
        let mut out = Vec::with_capacity(laptops.len());
        for (id, _name) in laptops {
            if let Some(v) = self.build_agent_view(id)? {
                out.push(v);
            }
        }
        Ok(out)
    }

    pub fn broadcast(&self, msg: HubToAdmin) {
        // 구독자 없으면 Err 반환하지만 무해.
        let _ = self.0.admin_bcast.send(msg);
    }

    /// agent_id의 AgentUpdated 브로드캐스트 헬퍼.
    pub fn broadcast_agent(&self, agent_id: Uuid) {
        if let Ok(Some(view)) = self.build_agent_view(agent_id) {
            self.broadcast(HubToAdmin::AgentUpdated { agent: view });
        }
    }
}
