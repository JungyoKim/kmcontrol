//! 페어링된 클라이언트 영속 상태 (JSON 파일).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct StateData {
    /// unique_id -> 클라이언트 인증서 PEM.
    clients: BTreeMap<String, String>,
    /// 인증서 SHA-256 지문 집합 (mTLS 매칭).
    paired_fingerprints: Vec<String>,
}

#[derive(Clone)]
pub struct PersistentState {
    path: PathBuf,
    data: Arc<RwLock<StateData>>,
}

impl PersistentState {
    pub fn load_or_default(path: PathBuf) -> Result<Self> {
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse state file {}", path.display()))?,
            Err(_) => StateData::default(),
        };
        Ok(Self { path, data: Arc::new(RwLock::new(data)) })
    }

    pub fn has_client(&self, id: &str) -> bool {
        self.data.read().clients.contains_key(id)
    }

    pub fn has_paired_cert(&self, fingerprint: &str) -> bool {
        self.data.read().paired_fingerprints.iter().any(|f| f == fingerprint)
    }

    pub fn add_paired(&self, id: &str, fingerprint: &str, pem: &str) -> Result<()> {
        {
            let mut data = self.data.write();
            data.clients.insert(id.to_string(), pem.to_string());
            if !data.paired_fingerprints.iter().any(|f| f == fingerprint) {
                data.paired_fingerprints.push(fingerprint.to_string());
            }
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let json = {
            let data = self.data.read();
            serde_json::to_vec_pretty(&*data)?
        };
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        std::fs::write(&self.path, json)
            .with_context(|| format!("write state file {}", self.path.display()))?;
        Ok(())
    }
}
