//! 비디오 스트림 UDP 송출 (포트 47998).
//!
//! Moonlight이 먼저 "PING"을 보내면 그 주소를 클라이언트로 등록하고, 이후 인코딩된 프레임을
//! 패킷화해 송출한다. R3a: 채널로 들어온 인코딩 프레임(더미 또는 실제 NAL)을 패킷화·전송.

pub mod packetizer;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// 인코딩된 프레임 (NAL Annex-B 바이트스트림 + 키프레임 여부).
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_key_frame: bool,
    /// 90kHz RTP 타임스탬프.
    pub rtp_timestamp: u32,
}

/// 비디오 스트림 핸들: 인코더가 이 sender로 프레임을 넣는다.
pub type FrameSender = mpsc::Sender<EncodedFrame>;

/// 비디오 UDP 스트림을 spawn하고 프레임 sender를 반환.
pub async fn start(bind_ip: &str, port: u16, packet_size: usize, session_reset: std::sync::Arc<std::sync::atomic::AtomicBool>, client_active: std::sync::Arc<std::sync::atomic::AtomicBool>, bitrate: std::sync::Arc<crate::bitrate::BitrateController>) -> Result<FrameSender> {
    let addr: SocketAddr = format!("{bind_ip}:{port}").parse().context("parse video addr")?;
    let socket = UdpSocket::bind(addr).await.context("bind video udp")?;
    // 송신 버퍼 확대: 4K 키프레임(수백 shard) 버스트 유실 방지 (Sunshine도 큰 SO_SNDBUF 사용).
    {
        use socket2::SockRef;
        let sref = SockRef::from(&socket);
        // 8MB 시도 (OS가 상한 clamp).
        if let Err(e) = sref.set_send_buffer_size(8 * 1024 * 1024) {
            tracing::warn!(error=%e, "failed to enlarge UDP send buffer");
        } else {
            tracing::info!(send_buf = sref.send_buffer_size().unwrap_or(0), "UDP send buffer set");
        }
    }
    tracing::info!(%addr, "video UDP listening (waiting for client PING)");

    let (tx, mut rx) = mpsc::channel::<EncodedFrame>(128);

    tokio::spawn(async move {
        let mut client_addr: Option<SocketAddr> = None;
        let mut recv_buf = [0u8; 1500];
        let mut sequence_number: u32 = 0;
        let mut stream_packet_index: u32 = 0;
        let mut frame_number: u32 = 0;
        // 소유권: 활성 클라이언트가 최근 PING했으면 다른 주소의 PING을 무시(동시 세션 진동 방지).
        let mut last_ping = std::time::Instant::now();
        const OWNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        loop {
            tokio::select! {
                // 클라이언트 PING 수신 → 주소 등록.
                msg = socket.recv_from(&mut recv_buf) => {
                    match msg {
                        Ok((len, addr)) => {
                            if &recv_buf[..len] == b"PING" {
                                let owner_active = client_addr.is_some()
                                    && client_addr != Some(addr)
                                    && last_ping.elapsed() < OWNER_TIMEOUT;
                                if owner_active {
                                    // 다른 클라이언트가 이미 활성 — 경쟁 PING 무시(동시 세션 거부).
                                    tracing::debug!(%addr, "ignoring competing client PING (session busy)");
                                } else {
                                    // 새 세션(새 클라이언트 또는 재연결) 감지 시 프레임 카운터 리셋.
                                    // Moonlight은 매 세션 frameIndex=1부터 기대한다.
                                    if client_addr != Some(addr) {
                                        tracing::info!(%addr, "video client registered via PING (session reset)");
                                        frame_number = 0;
                                        sequence_number = 0;
                                        stream_packet_index = 0;
                                    }
                                    client_addr = Some(addr);
                                    last_ping = std::time::Instant::now();
                                    client_active.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            } else {
                                tracing::trace!(len, %addr, "non-PING on video socket");
                            }
                        }
                        Err(e) => {
                            // Windows: WSAECONNRESET(10054)은 이전 send에 대한 ICMP "port unreachable" 반송.
                            // 소켓은 정상이지만 클라이언트가 수신을 닫은 것 → 송출 중단(client_addr 클리어)해
                            // ICMP 반송 폭주와 tight busy-loop를 막는다. 재연결 시 새 PING으로 다시 등록됨.
                            if e.raw_os_error() == Some(10054) {
                                if client_addr.is_some() {
                                    tracing::info!("client stopped receiving (WSAECONNRESET); pausing send until re-PING");
                                    client_addr = None;
                                    client_active.store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                            } else {
                                tracing::warn!(error=%e, "video recv error");
                            }
                        }
                    }
                }

                // 인코딩된 프레임 → 패킷화 → 송출.
                frame = rx.recv() => {
                    let Some(frame) = frame else {
                        tracing::debug!("video frame channel closed");
                        break;
                    };
                    let Some(dst) = client_addr else {
                        // 아직 PING 안 받음 — 드롭.
                        continue;
                    };
                    // 세션 전환 시 프레임 카운터 리셋 (host가 새 PLAY에서 set).
                    if session_reset.swap(false, std::sync::atomic::Ordering::AcqRel) {
                        tracing::info!("session reset — frame counters back to 0");
                        frame_number = 0;
                        sequence_number = 0;
                        stream_packet_index = 0;
                    }
                    frame_number += 1;
                    // 동적 FEC: 최근 손실률 기반 목표 parity%(20~50) 를 프레임마다 적용.
                    let fec_pct = bitrate.poll_fec_percentage() as usize;
                    let shards = packetizer::packetize_frame(
                        &frame.data,
                        frame.is_key_frame,
                        packet_size,
                        frame_number,
                        &mut sequence_number,
                        &mut stream_packet_index,
                        frame.rtp_timestamp,
                        fec_pct,
                    );
                    // 버스트 유실 방지: shard를 배치로 나눠 송신하고 배치 사이에 짧게 양보.
                    // (Sunshine의 send_batch_size ~64KB 페이싱을 모사.)
                    const SEND_BATCH: usize = 40; // ~40 × 1.4KB ≈ 56KB per batch
                    for (i, shard) in shards.iter().enumerate() {
                        if let Err(e) = socket.send_to(shard, dst).await {
                            tracing::warn!(error=%e, "video send failed");
                            break;
                        }
                        // 배치 경계마다 커널 송신 큐가 빠질 시간을 준다.
                        if (i + 1) % SEND_BATCH == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    if frame_number % 60 == 1 {
                        tracing::info!(frame_number, shards = shards.len(), fec_pct, "video frames flowing");
                    } else {
                        tracing::trace!(frame_number, shards = shards.len(), fec_pct, "video frame sent");
                    }
                }

                // 주기 점검: 클라이언트가 조용히 사라져(PING 끊김, CONNRESET 없이) 있으면 인코딩 정지.
                // Moonlight 은 PING 을 자주 보내므로 3초 무소식이면 시청 종료로 간주.
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    if client_addr.is_some() && last_ping.elapsed() > std::time::Duration::from_secs(3) {
                        tracing::info!("no client PING for 3s; pausing encode until re-PING");
                        client_addr = None;
                        client_active.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
        tracing::debug!("video stream stopped");
    });

    Ok(tx)
}

/// R3a 검증용 더미 NAL 생성기. 실제 디코드는 안 되지만 RTP 패킷화·전달 경로를 검증한다.
/// 매 프레임 Annex-B 시작코드 + 임의 페이로드를 fps 주기로 송출.
pub fn spawn_dummy_generator(sender: FrameSender, fps: u32) {
    tokio::spawn(async move {
        let period = std::time::Duration::from_micros(1_000_000 / fps as u64);
        let mut ticker = tokio::time::interval(period);
        let mut ts: u32 = 0;
        let mut n: u64 = 0;
        loop {
            ticker.tick().await;
            n += 1;
            let is_key = n % 60 == 1; // 주기적 키프레임 플래그.
            // Annex-B: 00 00 00 01 + NAL 헤더 + 더미 바이트.
            let mut data = vec![0x00, 0x00, 0x00, 0x01, 0x65];
            data.extend(std::iter::repeat(0xAB).take(2000));
            if sender
                .send(EncodedFrame { data, is_key_frame: is_key, rtp_timestamp: ts })
                .await
                .is_err()
            {
                break;
            }
            ts = ts.wrapping_add(90_000 / fps.max(1));
        }
    });
}
