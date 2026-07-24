//! 제어 채널 (ENet, UDP 47999).
//!
//! Moonlight은 RTSP PLAY 뒤 control 채널을 먼저 세우고, 암호화된 `StartB`(0x0307)를 보낸다.
//! 서버가 StartB를 받으면 비디오/오디오 스트림을 시작해야 클라이언트가 video PING을 보낸다.
//! 입력 주입(InputData)은 R4. 여기서는 핸드셰이크 + StartB/Ping 처리 + 비디오 트리거만.
//!
//! 암호화: AES-128-GCM. 키 = launch의 remote_input_key.
//!   복호 IV = seq(4 LE) ++ [0;6] ++ b"CC"
//!   암호 IV = seq(4 LE) ++ [0;6] ++ b"HC"
//! 참조: hgaiser/moonshine (BSD-2), moonlight-common-c.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Notify;
use tokio_enet::{Event, Host, HostConfig, Packet, PacketMode, PeerId, PeerState};

use crate::crypto;
use crate::session::SessionState;

// control 메시지 타입 (little-endian u16).
const MSG_ENCRYPTED: u16 = 0x0001;
const MSG_TERMINATION_EXT: u16 = 0x0109;
const MSG_PING: u16 = 0x0200;
const MSG_START_B: u16 = 0x0307;
const MSG_REQUEST_IDR: u16 = 0x0302;
const MSG_INVALIDATE_REF: u16 = 0x0301;
const MSG_INPUT_DATA: u16 = 0x0206;
const MSG_FRAME_FEC_STATUS: u16 = 0x5502; // Sunshine 확장: 손실 프레임 FEC 상태(빅엔디언).

const TAG_LEN: usize = 16;

/// 비디오 시작 신호 핸들.
#[derive(Clone)]
pub struct VideoTrigger {
    notify: Arc<Notify>,
}

impl VideoTrigger {
    pub fn new() -> Self {
        Self { notify: Arc::new(Notify::new()) }
    }
    /// StartB 수신 시 호출.
    pub fn trigger(&self) {
        self.notify.notify_waiters();
    }
    /// 비디오 스타터가 대기.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
    pub fn clone_notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }
}

impl Default for VideoTrigger {
    fn default() -> Self {
        Self::new()
    }
}

/// 제어 채널을 spawn. StartB 수신 시 `trigger`를 발동한다.
pub async fn start(
    bind_ip: &str,
    port: u16,
    session: SessionState,
    trigger: VideoTrigger,
    idr_req: Arc<AtomicBool>,
    bitrate: Arc<crate::bitrate::BitrateController>,
) -> Result<()> {
    let addr = format!("{bind_ip}:{port}");
    let socket_addr = addr.parse().context("parse control addr")?;
    let host_config = HostConfig {
        address: Some(socket_addr),
        peer_count: 4,
        channel_limit: 1,
        ..Default::default()
    };
    let host = Host::new(host_config).map_err(|e| anyhow::anyhow!("enet host: {e}"))?;
    tracing::info!(%addr, "control (ENet) listening");

    tokio::spawn(async move {
        run_loop(host, session, trigger, idr_req, bitrate).await;
    });
    Ok(())
}

async fn run_loop(mut host: Host, session: SessionState, trigger: VideoTrigger, idr_req: Arc<AtomicBool>, bitrate: Arc<crate::bitrate::BitrateController>) {
    let mut connected_peer: Option<PeerId> = None;

    loop {
        match host.service(Duration::from_millis(10)).await {
            Ok(Some(Event::Connect { peer_id, .. })) => {
                tracing::info!(?peer_id, "control peer connected");
                // 새 admin 이 붙으면 이전 peer 들을 즉시 정리한다. control 은 단일 제어자만 유효하고,
                // ENet host 는 peer 슬롯이 4개뿐 — 재연결 때 이전(좀비/미정리) peer 가 슬롯을 물고
                // 있으면 반복할수록 슬롯이 소진돼 접속이 느려지거나 실패한다(재연결 점진 지연의 원인).
                let stale: Vec<PeerId> = host
                    .peers()
                    .filter(|p| p.id() != peer_id && p.state() == PeerState::Connected)
                    .map(|p| p.id())
                    .collect();
                for id in stale {
                    tracing::info!(?id, "reaping stale control peer");
                    host.disconnect_now(id, 0);
                }
                connected_peer = Some(peer_id);
            }
            Ok(Some(Event::Disconnect { peer_id, .. })) => {
                tracing::info!(?peer_id, "control peer disconnected");
                if connected_peer == Some(peer_id) {
                    connected_peer = None;
                }
            }
            Ok(Some(Event::Receive { packet, .. })) => {
                // 패킷 처리 panic 격리 — 이상 패킷이 control 채널을 죽이지 않도록.
                let data = packet.data().to_vec();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_packet(&data, &session, &trigger, &idr_req, &bitrate);
                }));
                if r.is_err() {
                    tracing::error!("control packet handler panicked (isolated)");
                }
            }
            Ok(None) => {}
            Err(e) => {
                // ENet 서비스 에러가 나도 control 채널을 죽이지 않고 계속 서비스.
                tracing::warn!(error=%e, "control enet service error (continuing)");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        let _ = connected_peer;
    }
    drop(host);
    tracing::debug!("control stream stopped");
}

/// 하나의 control 패킷 처리 (필요시 복호화 후 타입 분기).
fn handle_packet(buf: &[u8], session: &SessionState, trigger: &VideoTrigger, idr_req: &AtomicBool, bitrate: &crate::bitrate::BitrateController) {
    let Some((msg_type, body)) = parse_header(buf) else {
        return;
    };

    let (effective_type, payload): (u16, Vec<u8>) = if msg_type == MSG_ENCRYPTED {
        match decrypt_message(buf, session) {
            Some(plain) => match parse_header(&plain) {
                Some((t, p)) => (t, p.to_vec()),
                None => return,
            },
            None => return,
        }
    } else {
        (msg_type, body.to_vec())
    };

    match effective_type {
        MSG_START_B => {
            tracing::info!("control StartB received — triggering video/audio start");
            trigger.trigger();
        }
        MSG_PING => {
            tracing::trace!("control ping");
        }
        MSG_REQUEST_IDR | MSG_INVALIDATE_REF => {
            idr_req.store(true, Ordering::Release);
            tracing::debug!("control IDR frame requested");
        }
        MSG_INPUT_DATA => {
            crate::input::inject(&payload);
        }
        MSG_FRAME_FEC_STATUS => {
            if let Some(frac) = parse_fec_loss(&payload) {
                bitrate.on_loss(frac);
            }
        }
        MSG_TERMINATION_EXT => {
            tracing::info!("control termination received");
        }
        other => {
            tracing::trace!(msg_type = format_args!("0x{other:04x}"), "unhandled control message");
        }
    }
}

/// SS_FRAME_FEC_STATUS(빅엔디언, 21바이트) → 이 프레임의 패킷 손실 비율(0.0~1.0).
/// 레이아웃: frameIndex(4) highestRecvSeq(2) nextContigSeq(2) missingBeforeHighest(2)
///   totalData(2) totalParity(2) recvData(2) recvParity(2) fecPct(1) blockIdx(1) blockCount(1).
/// 손실 비율 = (전송된 총 패킷 - 수신된 총 패킷) / 전송된 총 패킷. FEC 로 복구됐어도 손실
/// 신호로 사용(네트워크 열화의 조기 지표). 클라는 손실이 있을 때만 이 메시지를 보낸다.
fn parse_fec_loss(payload: &[u8]) -> Option<f32> {
    if payload.len() < 21 {
        return None;
    }
    let be16 = |o: usize| u16::from_be_bytes([payload[o], payload[o + 1]]) as u32;
    let total_data = be16(10);
    let total_parity = be16(12);
    let recv_data = be16(14);
    let recv_parity = be16(16);
    let total = total_data + total_parity;
    if total == 0 {
        return None;
    }
    let recv = (recv_data + recv_parity).min(total);
    let lost = total - recv;
    Some(lost as f32 / total as f32)
}

/// control 메시지 헤더 파싱: type(u16 LE) + length(u16 LE) + body.
/// length는 body 길이와 일치해야 함. 반환: (type, body).
fn parse_header(buf: &[u8]) -> Option<(u16, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    let msg_type = u16::from_le_bytes([buf[0], buf[1]]);
    let length = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    let body = &buf[4..];
    if length != body.len() {
        tracing::trace!(length, actual = body.len(), "control length mismatch");
        // 일부 클라이언트/메시지는 길이 불일치 허용 — body 그대로 반환.
    }
    Some((msg_type, body))
}

/// 암호화 control 메시지 복호화.
/// 레이아웃(body, 헤더 이후): seq(4 LE) + tag(16) + ciphertext.
fn decrypt_message(buf: &[u8], session: &SessionState) -> Option<Vec<u8>> {
    // buf = 전체 암호화 메시지: type(2) len(2) seq(4) tag(16) ciphertext.
    if buf.len() < 4 + 4 + TAG_LEN {
        return None;
    }
    let length = u16::from_le_bytes([buf[2], buf[3]]) as usize; // seq + tag + plaintext.
    let seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let tag: [u8; 16] = buf[8..8 + TAG_LEN].try_into().ok()?;
    // ciphertext는 length로 바운드 (seq 4 + tag 16 제외).
    let ct_len = length.saturating_sub(4 + TAG_LEN);
    let ct_start = 8 + TAG_LEN;
    let ct_end = (ct_start + ct_len).min(buf.len());
    let ciphertext = &buf[ct_start..ct_end];

    let key = session.get().map(|s| s.remote_input_key)?;
    if key.len() != 16 {
        tracing::warn!(len = key.len(), "remote_input_key not 16 bytes");
        return None;
    }
    let key16: [u8; 16] = key.as_slice().try_into().ok()?;

    // 복호 IV = seq(4 LE) ++ [0;6] ++ b"CC" (Client originated, Control stream).
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'C';
    iv[11] = b'C';

    match crypto::aes_gcm_decrypt(ciphertext, &key16, &iv, &tag) {
        Ok(plain) => Some(plain),
        Err(e) => {
            tracing::warn!(error=%e, seq, length, ct_len = ciphertext.len(), buf_len = buf.len(), "control decrypt failed");
            None
        }
    }
}

// 미사용 경고 억제용 (R4에서 사용 예정).
#[allow(dead_code)]
fn encode_control_message(key: &[u8; 16], seq: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'H';
    iv[11] = b'C';
    let mut tag = [0u8; 16];
    let ct = crypto::aes_gcm_encrypt(payload, key, &iv, &mut tag).ok()?;
    let mut out = Vec::with_capacity(4 + 4 + TAG_LEN + ct.len());
    out.extend((MSG_ENCRYPTED).to_le_bytes());
    out.extend(((4 + TAG_LEN + ct.len()) as u16).to_le_bytes());
    out.extend(seq.to_le_bytes());
    out.extend(tag);
    out.extend(ct);
    Some(out)
}

// 참조 억제 (R4에서 send 경로에 사용).
#[allow(dead_code)]
fn send_to_peer(host: &mut Host, peer_id: PeerId, data: &[u8]) {
    if let Some(peer) = host.peer_mut(peer_id) {
        if peer.state() == PeerState::Connected {
            let _ = peer.send(0, Packet::new(data, PacketMode::ReliableSequenced));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SS_FRAME_FEC_STATUS 빌더(빅엔디언, 21바이트).
    fn fec_status(
        total_data: u16,
        total_parity: u16,
        recv_data: u16,
        recv_parity: u16,
    ) -> Vec<u8> {
        let mut b = Vec::with_capacity(21);
        b.extend(0u32.to_be_bytes()); // frameIndex
        b.extend(0u16.to_be_bytes()); // highestReceivedSequenceNumber
        b.extend(0u16.to_be_bytes()); // nextContiguousSequenceNumber
        b.extend(0u16.to_be_bytes()); // missingPacketsBeforeHighestReceived
        b.extend(total_data.to_be_bytes());
        b.extend(total_parity.to_be_bytes());
        b.extend(recv_data.to_be_bytes());
        b.extend(recv_parity.to_be_bytes());
        b.push(20); // fecPercentage
        b.push(0); // multiFecBlockIndex
        b.push(1); // multiFecBlockCount
        b
    }

    #[test]
    fn fec_loss_none_when_all_received() {
        let p = fec_status(100, 20, 100, 20);
        assert_eq!(parse_fec_loss(&p), Some(0.0));
    }

    #[test]
    fn fec_loss_half() {
        // 총 120, 수신 60 → 0.5.
        let p = fec_status(100, 20, 50, 10);
        assert_eq!(parse_fec_loss(&p), Some(0.5));
    }

    #[test]
    fn fec_loss_data_only() {
        // 총 100(parity 0), 수신 90 → 0.1.
        let p = fec_status(100, 0, 90, 0);
        assert_eq!(parse_fec_loss(&p), Some(0.1));
    }

    #[test]
    fn fec_loss_rejects_short() {
        assert_eq!(parse_fec_loss(&[0u8; 20]), None);
    }

    #[test]
    fn fec_loss_rejects_zero_total() {
        let p = fec_status(0, 0, 0, 0);
        assert_eq!(parse_fec_loss(&p), None);
    }

    #[test]
    fn fec_loss_clamps_overreport() {
        // 수신 > 전송(비정상) 이어도 음수/overflow 없이 0.0.
        let p = fec_status(100, 20, 200, 50);
        assert_eq!(parse_fec_loss(&p), Some(0.0));
    }
}
