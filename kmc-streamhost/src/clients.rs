//! 클라이언트 페어링 상태 머신 + 영속 상태.
//!
//! GameStream 페어링 5단계 (moonlight/Sunshine):
//!   1. getservercert: 클라 인증서+salt 수신, PIN 대기, 서버 인증서 반환
//!   2. clientchallenge: PIN으로 유도한 키로 챌린지 복호 → 서버 secret/challenge 생성 → 응답
//!   3. serverchallengeresp: 클라 해시 저장, server_secret + RSA서명 → pairingsecret 반환
//!   4. pairchallenge: (no-op ack)
//!   5. clientpairingsecret: 클라 해시/서명 검증 → 페어링 확정, 인증서 지문 영속화
//!
//! 참조: hgaiser/moonshine (BSD-2).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use parking_lot::RwLock;
use rand::RngCore;
use tokio::sync::Notify;

use crate::crypto;
use crate::state::PersistentState;

/// 페어링 진행 중인 클라이언트.
pub struct PendingClient {
    pub id: String,
    pub pem: String,        // 클라이언트 인증서 PEM
    pub salt: [u8; 16],
    pub pin_notify: Arc<Notify>,
    pub key: Option<[u8; 16]>,       // PIN 유도 키
    pub server_secret: Option<[u8; 16]>,
    pub server_challenge: Option<[u8; 16]>,
    pub client_hash: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct ClientManager {
    pending: Arc<RwLock<BTreeMap<String, PendingClient>>>,
    state: PersistentState,
    server_cert_pem: String,
    server_key_pem: String,
}

impl ClientManager {
    pub fn new(state_path: PathBuf, server_cert_pem: String, server_key_pem: String) -> Result<Self> {
        Ok(Self {
            pending: Arc::new(RwLock::new(BTreeMap::new())),
            state: PersistentState::load_or_default(state_path)?,
            server_cert_pem,
            server_key_pem,
        })
    }

    pub fn is_paired(&self, id: &str) -> bool {
        self.state.has_client(id)
    }

    pub fn is_cert_paired(&self, fingerprint: &str) -> bool {
        self.state.has_paired_cert(fingerprint)
    }

    /// 단계 1: getservercert에서 pending 등록.
    pub fn start_pairing(&self, client: PendingClient) -> Arc<Notify> {
        let notify = client.pin_notify.clone();
        self.pending.write().insert(client.id.clone(), client);
        notify
    }

    /// PIN 입력 → 키 유도 → 대기 중인 getservercert 깨움.
    pub fn register_pin(&self, id: &str, pin: &str) -> Result<()> {
        let mut pending = self.pending.write();
        let client = pending.get_mut(id).ok_or_else(|| anyhow!("no pending client {id}"))?;
        client.key = Some(crypto::derive_key(&client.salt, pin));
        client.pin_notify.notify_waiters();
        Ok(())
    }

    /// 단계 2: 클라이언트 챌린지 처리 → challengeresponse 반환.
    pub fn client_challenge(&self, id: &str, challenge: Vec<u8>) -> Result<Vec<u8>> {
        let mut pending = self.pending.write();
        let client = pending.get_mut(id).ok_or_else(|| anyhow!("no pending client {id}"))?;
        let key = client.key.ok_or_else(|| anyhow!("client {id} has no pin yet"))?;

        let mut server_secret = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut server_secret);
        client.server_secret = Some(server_secret);

        // challenge 복호(ECB) + 서버 인증서 서명 + server_secret → SHA256 → server_challenge 부착 → ECB 암호.
        let mut decrypted = crypto::aes_ecb_decrypt(&challenge, &key)?;
        let cert_sig = crypto::cert_signature(&self.server_cert_pem)?;
        decrypted.extend_from_slice(&cert_sig);
        decrypted.extend_from_slice(&server_secret);

        let mut server_challenge = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut server_challenge);
        client.server_challenge = Some(server_challenge);

        let hash = crypto::sha256(&decrypted);
        let mut resp_plain = hash;
        resp_plain.extend_from_slice(&server_challenge);
        crypto::aes_ecb_encrypt(&resp_plain, &key)
    }

    /// 단계 3: 서버 챌린지 응답 처리 → pairingsecret(server_secret || RSA서명) 반환.
    pub fn server_challenge_response(&self, id: &str, challenge_response: Vec<u8>) -> Result<Vec<u8>> {
        let mut pending = self.pending.write();
        let client = pending.get_mut(id).ok_or_else(|| anyhow!("no pending client {id}"))?;
        let key = client.key.ok_or_else(|| anyhow!("client {id} has no pin yet"))?;

        let decrypted = crypto::aes_ecb_decrypt(&challenge_response, &key)?;
        client.client_hash = Some(decrypted);

        let server_secret = client.server_secret.ok_or_else(|| anyhow!("no server secret"))?;
        let mut pairing_secret = server_secret.to_vec();
        let signed = crypto::rsa_sign_sha256(&server_secret, &self.server_key_pem)?;
        pairing_secret.extend_from_slice(&signed);
        Ok(pairing_secret)
    }

    /// 단계 5: 클라이언트 pairing secret 검증 → 페어링 확정.
    pub fn check_client_pairing_secret(&self, id: &str, client_secret: Vec<u8>) -> Result<()> {
        let mut pending = self.pending.write();
        let client = pending.get_mut(id).ok_or_else(|| anyhow!("no pending client {id}"))?;

        verify_pairing_secret(client, &client_secret)?;

        let fingerprint = crypto::cert_fingerprint(&client.pem)?;
        let pem = client.pem.clone();
        drop(pending);

        self.state.add_paired(id, &fingerprint, &pem)?;
        Ok(())
    }

    /// 페어링된 클라이언트 인증서 PEM (mTLS 지문 매칭용).
    pub fn server_cert_pem(&self) -> &str {
        &self.server_cert_pem
    }
}

/// 단계 5 검증: server_challenge || client_cert_sig || client_secret 의 SHA256 == client_hash,
/// 그리고 client_secret에 대한 클라이언트 인증서 서명 검증.
fn verify_pairing_secret(client: &PendingClient, client_secret: &[u8]) -> Result<()> {
    let client_hash = client
        .client_hash
        .as_ref()
        .ok_or_else(|| anyhow!("no client hash yet"))?;
    let server_challenge = client
        .server_challenge
        .ok_or_else(|| anyhow!("no server challenge"))?;

    if client_secret.len() < 16 {
        bail!("client pairing secret too short: {}", client_secret.len());
    }
    let payload = &client_secret[..16];
    let signature = &client_secret[16..];

    let mut data = server_challenge.to_vec();
    let cert_sig = crypto::cert_signature(&client.pem)?;
    data.extend_from_slice(&cert_sig);
    data.extend_from_slice(payload);

    let hash = crypto::sha256(&data);
    if &hash != client_hash {
        bail!("client hash mismatch (possible MITM)");
    }

    crypto::rsa_verify_cert_sha256(&client.pem, payload, signature)?;
    Ok(())
}
