//! 제어 채널 클라이언트 (ENet over UDP).
//!
//! 호스트 `kmc-streamhost/src/control.rs` 의 거울상 구현이다. RTSP PLAY 이후 이 채널을
//! 먼저 세우고 암호화된 `StartB`(0x0307)를 보내야 호스트가 비디오/오디오 송출을 시작한다
//! (호스트 control.rs:158-162 에서 StartB 수신 시 `VideoTrigger` 발동).
//!
//! 접속 포트는 **RTSP SETUP 이 협상한 값**(`rtsp::RtspNegotiated::control_port`)을
//! `connect()` 인자로 받는다. 호스트 기본값은 47999 지만(`StreamPorts::default`) 이 모듈은
//! 그 값을 어디에도 박아 두지 않는다.
//!
//! # 메시지 프레이밍 (호스트 `parse_header`, control.rs:192-204)
//! ```text
//!   type(u16 LE) | length(u16 LE) | body
//! ```
//! 암호화 메시지는 `type = 0x0001`, body 는 아래 레이아웃이다
//! (호스트 `decrypt_message` control.rs:206-242 / `encode_control_message` 244-260):
//! ```text
//!   seq(u32 LE) | tag(16) | ciphertext
//!   length     = 4(seq) + 16(tag) + ciphertext 길이
//!   평문       = inner type(u16 LE) | inner length(u16 LE) | payload
//! ```
//!
//! # 암호화: AES-128-GCM, 키 = launch 의 rikey (호스트 쪽 이름은 `remote_input_key`)
//! IV 는 12바이트이며 방향에 따라 마지막 2바이트가 다르다.
//!   - 클라이언트 송신 IV = `seq(4 LE) ++ [0;6] ++ b"CC"` — 호스트가 이 IV 로 복호한다
//!     (호스트 control.rs:229-233).
//!   - 호스트 송신 IV = `seq(4 LE) ++ [0;6] ++ b"HC"` — 우리가 이 IV 로 복호한다
//!     (호스트 control.rs:247-250).
//! seq 는 0 부터 시작해 메시지마다 1 증가한다 (moonlight-common-c
//! `currentEnetSequenceNumber`, ControlStream.c:353/722).
//!
//! # ENet 구성
//! 호스트는 `peer_count = 4`, `channel_limit = 1` 로 리슨한다 (호스트 control.rs:75-81).
//! 따라서 클라이언트도 채널 1개만 열고 모든 메시지를 채널 0 으로 보낸다 — 호스트의
//! `send_to_peer` 도 채널 0 을 쓴다 (control.rs:264-270). 실제 moonlight 는
//! `CTRL_CHANNEL_COUNT = 0x30` 을 요청하지만(Limelight-internal.h:66) tokio-enet 이
//! `channel_limit` 으로 클램프하므로 어차피 채널 0 만 살아 있다.
//!
//! 참조: hgaiser/moonshine (BSD-2), moonlight-common-c. 후자의 `ControlStream.c` /
//! `Input.h` 줄번호 인용은 **업스트림 소스** 기준이다 — `kmc-mooncommon` FFI 는 잘려나갔고
//! 이 모듈은 그 크레이트에 의존하지 않는다.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::Aead;
use aes_gcm::{AeadInPlace, Aes128Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_enet::{Event, Host, HostConfig, Packet, PacketMode, PeerId, PeerState};

/// 한 프레임의 FEC 수신 통계 (호스트 적응 비트레이트의 손실 신호원).
///
/// 정의는 호스트/클라이언트 공용 크레이트에 있다 (`kmc-gsproto/src/lib.rs:245-320`).
/// 호스트가 바로 그 타입으로 파싱해 소비하므로(control.rs:173-180) 여기서 재정의하지 않고
/// 재수출한다 — 한쪽만 고쳐 생기는 스펙 드리프트를 원천 차단한다.
pub use kmc_gsproto::FrameFecStatus;

// control 메시지 타입 (little-endian u16). 호스트 control.rs:23-31 와 동일.
const MSG_ENCRYPTED: u16 = 0x0001;
const MSG_TERMINATION_EXT: u16 = 0x0109;
const MSG_PING: u16 = 0x0200;
const MSG_INPUT_DATA: u16 = 0x0206;
const MSG_REQUEST_IDR: u16 = 0x0302;
const MSG_START_B: u16 = 0x0307;
/// Sunshine 확장. 호스트도 같은 상수를 공용 크레이트에서 가져온다 (control.rs:31).
const MSG_FRAME_FEC_STATUS: u16 = kmc_gsproto::MSG_FRAME_FEC_STATUS;

const TAG_LEN: usize = 16;

// NV_INPUT_HEADER.magic (little-endian u32). 호스트 input.rs:17-24 와 동일.
const IN_MAGIC_KEY_DOWN: u32 = 0x0000_0003;
const IN_MAGIC_KEY_UP: u32 = 0x0000_0004;
const IN_MAGIC_MOUSE_MOVE_ABS: u32 = 0x0000_0005;
const IN_MAGIC_MOUSE_BTN_DOWN: u32 = 0x0000_0008;
const IN_MAGIC_MOUSE_BTN_UP: u32 = 0x0000_0009;
const IN_MAGIC_SCROLL: u32 = 0x0000_000A;

/// 호스트가 리슨하는 ENet 채널 수 (호스트 control.rs:78 `channel_limit: 1`).
const CTRL_CHANNEL_COUNT: usize = 1;
/// 모든 control 메시지가 나가는 채널 (호스트 `send_to_peer` 도 채널 0, control.rs:267).
const CTRL_CHANNEL: u8 = 0;

/// 주기 ping 간격. moonlight-common-c `PERIODIC_PING_INTERVAL_MS`(ControlStream.c:298).
/// 호스트는 ping 부재를 허용하지만(control.rs:163-165 에서 trace 만 찍는다) 레퍼런스
/// 클라이언트와 같은 케이던스를 유지해 ENet 타임아웃 여유도 함께 확보한다.
const PING_INTERVAL: Duration = Duration::from_millis(100);
/// ENet 서비스 1회당 대기 시간. 이 값이 곧 송신 큐 → 소켓 사이의 최대 지연이다.
const SERVICE_TICK: Duration = Duration::from_millis(2);
/// ENet 핸드셰이크(CONNECT → VerifyConnect) 대기 상한.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 종료 시 DISCONNECT ack 를 기다리는 상한.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// 한 control 메시지 페이로드의 최대 길이. 현재 최대는 `SS_FRAME_FEC_STATUS` 21바이트,
/// 그다음이 `NV_ABS_MOUSE_MOVE_PACKET` 18바이트다. 고정 버퍼라 입력 경로가 무할당이 된다.
const MAX_PAYLOAD: usize = 24;

/// 원격 입력 이벤트 (admin 의 keyhook 사이드카가 만들어 보낸다).
#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    MousePosition { x: i16, y: i16, ref_w: i16, ref_h: i16 },
    MouseButton { button: u8, down: bool },
    Key { key_code: i16, down: bool, modifiers: u8 },
    Scroll { amount: i16 },
}

/// 제어 채널 핸들. ENet 은 전용 태스크가 단독 소유하고, 외부에서는 무잠금 큐로만 접근한다.
pub struct ControlChannel {
    tx: UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

/// 태스크 ↔ 핸들 공유 상태. 모두 원자적이라 아무 스레드에서나 읽어도 안전하다.
struct Shared {
    /// 아직 메시지를 큐에 넣을 가치가 있는가 (종료/끊김 시 false).
    alive: AtomicBool,
    /// `term_code` 가 유효한가.
    term_set: AtomicBool,
    /// 호스트가 통보한 종료 코드.
    term_code: AtomicI32,
}

/// ENet 태스크로 보내는 명령.
#[derive(Clone, Copy)]
enum Cmd {
    Msg(OutMsg),
    Shutdown,
}

/// 송신 대기 중인 control 메시지 하나 (힙 할당 없음).
#[derive(Clone, Copy)]
struct OutMsg {
    /// 암호문 안쪽 헤더의 타입.
    ty: u16,
    mode: PacketMode,
    payload: PayloadBuf,
}

/// 고정 크기 페이로드 조립기. 입력 이벤트마다 Vec 을 만들지 않기 위한 것.
#[derive(Clone, Copy)]
struct PayloadBuf {
    buf: [u8; MAX_PAYLOAD],
    len: u8,
}

impl PayloadBuf {
    const fn new() -> Self {
        Self { buf: [0u8; MAX_PAYLOAD], len: 0 }
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// 바이트열 추가. 버퍼를 넘기면(있을 수 없음) 패닉 대신 잘라낸다.
    fn put(&mut self, s: &[u8]) {
        let n = self.len as usize;
        let end = (n + s.len()).min(MAX_PAYLOAD);
        debug_assert_eq!(end - n, s.len(), "control payload overflow");
        self.buf[n..end].copy_from_slice(&s[..end - n]);
        self.len = end as u8;
    }

    fn byte(&mut self, v: u8) {
        self.put(&[v]);
    }
    fn be16(&mut self, v: u16) {
        self.put(&v.to_be_bytes());
    }
    fn be32(&mut self, v: u32) {
        self.put(&v.to_be_bytes());
    }
    fn le16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }
    fn le32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }
}

impl ControlChannel {
    /// ENet 연결 후 StartB 까지 전송. 반환 시 호스트가 미디어 송출을 시작한다.
    ///
    /// `port` 는 RTSP SETUP 이 협상한 `RtspNegotiated::control_port` 를 그대로 넘긴다
    /// (기본값 47999 는 호스트 `StreamPorts::default` 소관이지 이 모듈의 상수가 아니다).
    ///
    /// `rikey_iv` 는 control 채널에서 쓰이지 않는다 — 호스트는 IV 를 seq 로만 만들기 때문이다
    /// (control.rs:229-233). launch 가 돌려주는 `rikey_iv` 는 rikeyid(오디오/입력 CBC) 용도라
    /// 호출부 대칭성만 위해 받는다.
    pub async fn connect(
        host_ip: &str,
        port: u16,
        rikey: &[u8; 16],
        rikey_iv: &[u8; 16],
    ) -> Result<Self> {
        let _ = rikey_iv;

        let addr = resolve(host_ip, port).await?;
        let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(rikey));

        // 클라이언트 호스트: 임시 포트에 바인드(address: None)하고 peer 1개, 채널 1개.
        // Host::new 는 동기 함수지만 내부에서 UdpSocket::from_std 를 쓰므로 tokio 런타임
        // 컨텍스트가 필요하다 — async fn 안이라 충족된다.
        let mut host = Host::new(HostConfig {
            address: None,
            peer_count: 1,
            channel_limit: CTRL_CHANNEL_COUNT,
            ..Default::default()
        })
        .map_err(|e| anyhow!("control enet host: {e}"))?;

        // 세 번째 인자는 ENet CONNECT 의 `data` (moonlight 의 ControlConnectData). 호스트는
        // RTSP 에서 X-SS-Connect-Data 를 주지 않으므로 0 이다 (RtspConnection.c:1311-1316).
        let peer_id = host
            .connect(addr, CTRL_CHANNEL_COUNT, 0)
            .map_err(|e| anyhow!("control enet connect {addr}: {e}"))?;

        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            term_set: AtomicBool::new(false),
            term_code: AtomicI32::new(0),
        });

        // ENet 핸드셰이크 완료(VerifyConnect ack)까지 서비스.
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!("control enet connect timeout ({addr})"));
            }
            match host.service(SERVICE_TICK).await {
                Ok(Some(Event::Connect { peer_id: id, .. })) if id == peer_id => break,
                Ok(Some(Event::Disconnect { .. })) => {
                    return Err(anyhow!("control enet connect refused ({addr})"))
                }
                Ok(Some(Event::Receive { packet, .. })) => {
                    handle_inbound(packet.data(), &cipher, &shared);
                }
                Ok(_) => {}
                Err(e) => return Err(anyhow!("control enet service: {e}")),
            }
        }

        // StartB 를 실제로 ENet 에 넘기고 소켓까지 밀어낸 뒤에야 성공을 반환한다.
        // 호스트는 이걸 받아야 비디오/오디오를 켠다 (control.rs:158-162).
        let mut seq: u32 = 0;
        let start_b = OutMsg {
            ty: MSG_START_B,
            mode: PacketMode::ReliableSequenced,
            payload: start_b_payload(),
        };
        if !queue_msg(&mut host, peer_id, &cipher, &mut seq, &start_b) {
            return Err(anyhow!("control StartB send failed ({addr})"));
        }
        host.flush()
            .await
            .map_err(|e| anyhow!("control enet flush: {e}"))?;
        tracing::info!(%addr, "control channel connected (StartB sent)");

        let (tx, rx) = unbounded_channel();
        let task_shared = shared.clone();
        tokio::spawn(async move {
            run(host, peer_id, cipher, seq, rx, task_shared).await;
        });

        Ok(Self { tx, shared })
    }

    /// IDR(키프레임) 재전송 요청. 호스트는 0x0302/0x0301 둘 다 IDR 요청으로 처리한다
    /// (control.rs:166-169).
    pub fn request_idr(&self) {
        // requestIdrFrameGen7Enc = { 0, 0 } (ControlStream.c:228).
        let mut p = PayloadBuf::new();
        p.put(&[0, 0]);
        self.enqueue(OutMsg {
            ty: MSG_REQUEST_IDR,
            mode: PacketMode::ReliableSequenced,
            payload: p,
        });
    }

    /// 입력 이벤트 전송. 입력 스레드에서 초당 수백 번 불릴 수 있어 큐잉만 하고 즉시 반환한다.
    pub fn send_input(&self, ev: InputEvent) {
        self.enqueue(OutMsg {
            ty: MSG_INPUT_DATA,
            mode: PacketMode::ReliableSequenced,
            payload: encode_input(ev),
        });
    }

    /// FEC 손실 통계 전송. 호스트 주석대로 **손실이 있을 때만** 부르는 메시지다
    /// (호스트는 이걸 `BitrateController::on_loss` 로 넘긴다, control.rs:173-180).
    /// moonlight 와 동일하게 unsequenced 로 보낸다 (ControlStream.c:1412
    /// `ENET_PACKET_FLAG_UNSEQUENCED`) — 늦게 도착한 손실 보고는 가치가 없기 때문이다.
    pub fn send_fec_status(&self, st: FrameFecStatus) {
        self.enqueue(OutMsg {
            ty: MSG_FRAME_FEC_STATUS,
            mode: PacketMode::Unsequenced,
            payload: fec_status_payload(&st),
        });
    }

    /// 호스트가 보낸 종료 코드 (있으면).
    pub fn termination(&self) -> Option<i32> {
        if self.shared.term_set.load(Ordering::Acquire) {
            Some(self.shared.term_code.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    /// 정상 종료 요청. 태스크가 ENet DISCONNECT 를 보내고 스스로 끝난다.
    pub fn shutdown(&self) {
        self.shared.alive.store(false, Ordering::Release);
        let _ = self.tx.send(Cmd::Shutdown);
    }

    /// 큐잉. 채널이 죽었으면 조용히 버린다 — 닫힌 채널에 대해 절대 패닉하지 않는다.
    fn enqueue(&self, msg: OutMsg) {
        if !self.shared.alive.load(Ordering::Acquire) {
            return;
        }
        let _ = self.tx.send(Cmd::Msg(msg));
    }
}

/// ENet 을 단독 소유하는 태스크. 송신 큐 배수 → 주기 ping → ENet 서비스를 반복한다.
///
/// `Host::service` 를 `select!` 로 취소하지 않는 게 중요하다 — 소켓 송신 도중 취소되면
/// 커맨드 큐가 어중간해진다. 대신 호스트 `run_loop`(control.rs:90-138)와 같은 짧은 틱
/// 폴링 구조를 쓴다.
async fn run(
    mut host: Host,
    peer_id: PeerId,
    cipher: Aes128Gcm,
    mut seq: u32,
    mut rx: UnboundedReceiver<Cmd>,
    shared: Arc<Shared>,
) {
    let ping = OutMsg {
        ty: MSG_PING,
        mode: PacketMode::ReliableSequenced,
        payload: ping_payload(),
    };
    let mut next_ping = Instant::now() + PING_INTERVAL;
    let mut stopping = false;

    while !stopping {
        // (1) 송신 큐 배수. ENet 서비스보다 먼저 해야 입력 지연이 한 틱 이내로 유지된다.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Msg(msg)) => {
                    queue_msg(&mut host, peer_id, &cipher, &mut seq, &msg);
                }
                Ok(Cmd::Shutdown) => {
                    stopping = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                // 핸들이 drop 됐다 — 종료 신호로 취급.
                Err(TryRecvError::Disconnected) => {
                    stopping = true;
                    break;
                }
            }
        }
        if stopping {
            break;
        }

        // (2) 주기 ping.
        let now = Instant::now();
        if now >= next_ping {
            queue_msg(&mut host, peer_id, &cipher, &mut seq, &ping);
            next_ping = now + PING_INTERVAL;
        }

        // (3) ENet 서비스: 큐잉된 커맨드의 실제 송신 + 수신/ack/타임아웃 처리.
        match host.service(SERVICE_TICK).await {
            Ok(Some(Event::Receive { packet, .. })) => {
                handle_inbound(packet.data(), &cipher, &shared);
            }
            Ok(Some(Event::Disconnect { .. })) => {
                tracing::info!("control channel disconnected by host");
                stopping = true;
            }
            Ok(Some(Event::Connect { .. })) | Ok(None) => {}
            Err(e) => {
                // ENet 서비스 에러로 채널을 죽이지 않는다 (호스트 control.rs:128-132 와 동일).
                tracing::warn!(error = %e, "control enet service error (continuing)");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        // 호스트가 종료를 통보했으면 더 보낼 것이 없다.
        if shared.term_set.load(Ordering::Acquire) {
            stopping = true;
        }
    }

    shared.alive.store(false, Ordering::Release);

    // 정상 ENet DISCONNECT. ack 를 짧게 기다렸다가 포기한다.
    if let Some(peer) = host.peer_mut(peer_id) {
        if peer.state() == PeerState::Connected {
            peer.disconnect(0);
        }
    }
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while Instant::now() < deadline {
        match host.service(SERVICE_TICK).await {
            Ok(Some(Event::Disconnect { .. })) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(host);
    tracing::debug!("control channel stopped");
}

/// 메시지 하나를 암호화해 peer 의 채널 0 에 큐잉. 성공 여부를 반환한다.
///
/// seq 는 송신 성공 여부와 무관하게 증가시킨다 — 같은 IV 가 두 번 쓰이는 일을 원천 차단한다
/// (moonlight 도 `currentEnetSequenceNumber++` 를 무조건 수행, ControlStream.c:722).
fn queue_msg(
    host: &mut Host,
    peer_id: PeerId,
    cipher: &Aes128Gcm,
    seq: &mut u32,
    msg: &OutMsg,
) -> bool {
    let this_seq = *seq;
    *seq = seq.wrapping_add(1);

    let Some(bytes) = encode_encrypted(cipher, this_seq, msg.ty, msg.payload.as_slice()) else {
        tracing::warn!(ty = format_args!("0x{:04x}", msg.ty), "control encrypt failed");
        return false;
    };
    let Some(peer) = host.peer_mut(peer_id) else {
        return false;
    };
    if peer.state() != PeerState::Connected {
        tracing::trace!(
            ty = format_args!("0x{:04x}", msg.ty),
            "control peer not connected, dropping"
        );
        return false;
    }
    match peer.send(CTRL_CHANNEL, Packet::new(&bytes, msg.mode)) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(
                error = %e,
                ty = format_args!("0x{:04x}", msg.ty),
                "control send failed"
            );
            false
        }
    }
}

/// 평문 control 메시지를 AES-128-GCM 으로 감싼 최종 ENet 페이로드.
///
/// 출력(호스트 `encode_control_message`, control.rs:244-260 의 거울):
/// ```text
///   0x0001(LE16) | length(LE16) | seq(LE32) | tag(16) | ciphertext
///   length     = 4 + 16 + ciphertext 길이
///   평문       = inner_type(LE16) | payload 길이(LE16) | payload
/// ```
/// 태그 자리를 미리 잡아 두고 제자리 암호화하므로 할당은 결과 버퍼 하나뿐이다.
fn encode_encrypted(
    cipher: &Aes128Gcm,
    seq: u32,
    inner_type: u16,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let pt_len = 4 + payload.len();
    let mut out = Vec::with_capacity(8 + TAG_LEN + pt_len);
    out.extend_from_slice(&MSG_ENCRYPTED.to_le_bytes());
    out.extend_from_slice(&((4 + TAG_LEN + pt_len) as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0u8; TAG_LEN]); // 태그 자리 (8..24)
    out.extend_from_slice(&inner_type.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);

    let iv = client_iv(seq);
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&iv), &[], &mut out[8 + TAG_LEN..])
        .ok()?;
    out[8..8 + TAG_LEN].copy_from_slice(tag.as_slice());
    Some(out)
}

/// 호스트가 보낸 암호화 메시지 복호화. 전체 버퍼(type/length 헤더 포함)를 받는다.
/// 호스트 `decrypt_message`(control.rs:208-242)와 같은 바운드 계산을 쓰되 IV 접미사만
/// `b"HC"` 로 뒤집는다.
fn decrypt_message(buf: &[u8], cipher: &Aes128Gcm) -> Option<Vec<u8>> {
    if buf.len() < 4 + 4 + TAG_LEN {
        return None;
    }
    let length = u16::from_le_bytes([buf[2], buf[3]]) as usize; // seq + tag + plaintext.
    let seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let ct_len = length.saturating_sub(4 + TAG_LEN);
    let ct_start = 8 + TAG_LEN;
    let ct_end = (ct_start + ct_len).min(buf.len());

    // aes-gcm 의 결합형 API 는 ciphertext || tag 를 요구한다 (호스트 crypto.rs:82-87 과 동일).
    let mut sealed = Vec::with_capacity((ct_end - ct_start) + TAG_LEN);
    sealed.extend_from_slice(&buf[ct_start..ct_end]);
    sealed.extend_from_slice(&buf[8..8 + TAG_LEN]);

    let iv = host_iv(seq);
    match cipher.decrypt(Nonce::from_slice(&iv), sealed.as_ref()) {
        Ok(plain) => Some(plain),
        Err(e) => {
            tracing::warn!(error = %e, seq, length, buf_len = buf.len(), "control decrypt failed");
            None
        }
    }
}

/// 클라이언트 → 호스트 방향 IV: `seq(4 LE) ++ [0;6] ++ b"CC"` (호스트 control.rs:229-233).
fn client_iv(seq: u32) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'C'; // Client originated
    iv[11] = b'C'; // Control stream
    iv
}

/// 호스트 → 클라이언트 방향 IV: `seq(4 LE) ++ [0;6] ++ b"HC"` (호스트 control.rs:247-250).
fn host_iv(seq: u32) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'H'; // Host originated
    iv[11] = b'C'; // Control stream
    iv
}

/// control 메시지 헤더 파싱: type(u16 LE) + length(u16 LE) + body.
/// 호스트 `parse_header`(control.rs:192-204)와 같이 길이 불일치는 허용하고 body 를 그대로 준다.
fn parse_header(buf: &[u8]) -> Option<(u16, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    let msg_type = u16::from_le_bytes([buf[0], buf[1]]);
    let length = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    let body = &buf[4..];
    if length != body.len() {
        tracing::trace!(length, actual = body.len(), "control length mismatch");
    }
    Some((msg_type, body))
}

/// 호스트가 보낸 control 패킷 하나 처리 (필요시 복호화 후 타입 분기).
/// 구조는 호스트 `handle_packet`(control.rs:141-188)과 대칭이다.
fn handle_inbound(buf: &[u8], cipher: &Aes128Gcm, shared: &Shared) {
    let Some((msg_type, body)) = parse_header(buf) else {
        return;
    };

    let (effective_type, payload): (u16, Vec<u8>) = if msg_type == MSG_ENCRYPTED {
        match decrypt_message(buf, cipher) {
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
        MSG_TERMINATION_EXT => {
            // 확장형(payload >= 4B)은 HRESULT 를 빅엔디언으로, 단축형(>= 2B)은 reason 을
            // 리틀엔디언 u16 으로 싣는다 (moonlight ControlStream.c:1305-1359).
            let code = if payload.len() >= 4 {
                i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else if payload.len() >= 2 {
                i32::from(u16::from_le_bytes([payload[0], payload[1]]))
            } else {
                0
            };
            tracing::info!(code = format_args!("0x{code:08x}"), "control termination from host");
            shared.term_code.store(code, Ordering::Relaxed);
            shared.term_set.store(true, Ordering::Release);
            shared.alive.store(false, Ordering::Release);
        }
        MSG_PING => {
            tracing::trace!("control ping from host");
        }
        other => {
            tracing::debug!(
                msg_type = format_args!("0x{other:04x}"),
                len = payload.len(),
                "unhandled control message"
            );
        }
    }
}

/// StartB 페이로드: gen5 이상은 0 한 바이트다
/// (moonlight `startBGen5[] = { 0 }`, ControlStream.c:226). 호스트는 페이로드 내용을 보지
/// 않고 타입만 보고 트리거한다 (control.rs:158-162).
fn start_b_payload() -> PayloadBuf {
    let mut p = PayloadBuf::new();
    p.byte(0);
    p
}

/// 주기 ping 페이로드(8B): length(LE16 = 4) + timestamp(LE32 = 0) + 패딩(2B).
/// moonlight 는 8바이트 버퍼에 6바이트만 쓰고 그대로 보낸다(ControlStream.c:1392-1396,
/// 뒤 2바이트는 초기화되지 않은 스택). 우리는 0 으로 채워 결정적으로 보낸다 — 호스트는
/// ping 페이로드를 읽지 않는다(control.rs:163-165).
fn ping_payload() -> PayloadBuf {
    let mut p = PayloadBuf::new();
    p.le16(4);
    p.le32(0);
    p.le16(0);
    p
}

/// `SS_FRAME_FEC_STATUS` 본문(빅엔디언 21바이트)을 고정 버퍼에 싣는다.
/// 직렬화 자체는 공용 크레이트가 한다 (`kmc-gsproto` `FrameFecStatus::to_bytes`,
/// lib.rs:264-287) — 호스트가 `parse` 로 되읽는 바로 그 코드다.
fn fec_status_payload(st: &FrameFecStatus) -> PayloadBuf {
    let mut p = PayloadBuf::new();
    p.put(&st.to_bytes());
    p
}

/// InputEvent → `NV_INPUT_HEADER`(size BE32, magic LE32) + 본문.
///
/// size 는 자기 자신(4바이트)을 뺀 패킷 길이다(Input.h:11-14). 구조체는 `#pragma pack(1)`
/// 이라 패딩이 없다(Input.h:5). 호스트 `input::inject`(input.rs:32-87)가 정확히 이 바이트열을
/// 해석한다 — 안쪽에 별도 AES-CBC 계층은 없다(input.rs:36-38 이 곧바로 magic 을 읽는다).
fn encode_input(ev: InputEvent) -> PayloadBuf {
    let mut p = PayloadBuf::new();
    match ev {
        InputEvent::MousePosition { x, y, ref_w, ref_h } => {
            // NV_ABS_MOUSE_MOVE_PACKET(18B) = header(8) + x/y/unused/width/height (모두 BE16).
            // 호스트: body[0..2]=x, [2..4]=y, [6..8]=w, [8..10]=h (input.rs:47-55).
            p.be32(14);
            p.le32(IN_MAGIC_MOUSE_MOVE_ABS);
            p.be16(x as u16);
            p.be16(y as u16);
            p.be16(0); // unused
            p.be16(ref_w as u16);
            p.be16(ref_h as u16);
        }
        InputEvent::MouseButton { button, down } => {
            // NV_MOUSE_BUTTON_PACKET(9B) = header(8) + button(1) — 호스트는 body.first()
            // 하나만 읽는다 (input.rs:57-66).
            // button: 1=Left 2=Middle 3=Right 4=X1 5=X2 (호스트 input.rs:134-145).
            p.be32(5);
            p.le32(if down { IN_MAGIC_MOUSE_BTN_DOWN } else { IN_MAGIC_MOUSE_BTN_UP });
            p.byte(button);
        }
        InputEvent::Key { key_code, down, modifiers } => {
            // NV_KEYBOARD_PACKET(14B) = header(8) + flags(1) + keyCode(LE16) + modifiers(1)
            // + zero2(2). 호스트는 body[1..3] 을 LE u16 키코드로 읽는다 (input.rs:89-95).
            p.be32(10);
            p.le32(if down { IN_MAGIC_KEY_DOWN } else { IN_MAGIC_KEY_UP });
            p.byte(0); // flags — Sunshine 확장, GFE 는 항상 0
            p.le16(key_code as u16);
            p.byte(modifiers);
            p.le16(0); // zero2
        }
        InputEvent::Scroll { amount } => {
            // NV_SCROLL_PACKET(14B) = header(8) + scrollAmt1/scrollAmt2/zero3 (BE16).
            // 호스트는 body[0..2] 만 읽어 WHEEL_DELTA(120) 단위로 쓴다 (input.rs:69-75).
            p.be32(10);
            p.le32(IN_MAGIC_SCROLL);
            p.be16(amount as u16);
            p.be16(amount as u16);
            p.be16(0);
        }
    }
    p
}

/// `host_ip` 가 IP 리터럴이면 DNS 없이 바로, 아니면 이름 해석 후 첫 주소를 쓴다.
async fn resolve(host_ip: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(ip) = host_ip.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((host_ip, port))
        .await
        .with_context(|| format!("resolve control host {host_ip}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for control host {host_ip}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];

    fn cipher() -> Aes128Gcm {
        Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&KEY))
    }

    /// 호스트 `decrypt_message`(kmc-streamhost/src/control.rs:208-242)를 그대로 재현.
    /// IV 접미사가 `b"CC"` 라는 점이 핵심 — 우리가 보낸 것을 호스트가 이 코드로 연다.
    fn host_decrypt(buf: &[u8]) -> Option<Vec<u8>> {
        if buf.len() < 4 + 4 + TAG_LEN {
            return None;
        }
        let length = u16::from_le_bytes([buf[2], buf[3]]) as usize;
        let seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ct_len = length.saturating_sub(4 + TAG_LEN);
        let ct_start = 8 + TAG_LEN;
        let ct_end = (ct_start + ct_len).min(buf.len());

        let mut iv = [0u8; 12];
        iv[0..4].copy_from_slice(&seq.to_le_bytes());
        iv[10] = b'C';
        iv[11] = b'C';

        let mut sealed = Vec::new();
        sealed.extend_from_slice(&buf[ct_start..ct_end]);
        sealed.extend_from_slice(&buf[8..8 + TAG_LEN]);
        cipher()
            .decrypt(Nonce::from_slice(&iv), sealed.as_ref())
            .ok()
    }

    /// 호스트 `encode_control_message`(control.rs:244-260)를 그대로 재현 (IV 접미사 `b"HC"`).
    fn host_encode(seq: u32, inner_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut iv = [0u8; 12];
        iv[0..4].copy_from_slice(&seq.to_le_bytes());
        iv[10] = b'H';
        iv[11] = b'C';

        let mut plain = Vec::new();
        plain.extend_from_slice(&inner_type.to_le_bytes());
        plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        plain.extend_from_slice(payload);

        let mut sealed = cipher()
            .encrypt(Nonce::from_slice(&iv), plain.as_ref())
            .expect("gcm encrypt");
        let tag = sealed.split_off(sealed.len() - TAG_LEN);

        let mut out = Vec::new();
        out.extend_from_slice(&MSG_ENCRYPTED.to_le_bytes());
        out.extend_from_slice(&((4 + TAG_LEN + sealed.len()) as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&sealed);
        out
    }

    /// 호스트가 암호화 메시지를 열고 안쪽 헤더까지 벗겨낸 결과 (control.rs:146-156).
    fn host_unwrap(buf: &[u8]) -> (u16, Vec<u8>) {
        let plain = host_decrypt(buf).expect("host decrypts CC-IV message");
        let (ty, body) = parse_header(&plain).expect("inner header");
        (ty, body.to_vec())
    }

    #[test]
    fn client_iv_is_cc_suffixed() {
        let iv = client_iv(0x0403_0201);
        assert_eq!(iv, [0x01, 0x02, 0x03, 0x04, 0, 0, 0, 0, 0, 0, b'C', b'C']);
    }

    #[test]
    fn host_iv_is_hc_suffixed() {
        let iv = host_iv(0x0403_0201);
        assert_eq!(iv, [0x01, 0x02, 0x03, 0x04, 0, 0, 0, 0, 0, 0, b'H', b'C']);
    }

    #[test]
    fn encrypted_wire_layout_matches_host_expectation() {
        let payload = [0xAAu8, 0xBB, 0xCC];
        let buf = encode_encrypted(&cipher(), 7, MSG_START_B, &payload).expect("encode");

        // 헤더: type=0x0001, length = seq(4) + tag(16) + inner header(4) + payload(3).
        assert_eq!(u16::from_le_bytes([buf[0], buf[1]]), MSG_ENCRYPTED);
        assert_eq!(
            u16::from_le_bytes([buf[2], buf[3]]) as usize,
            4 + TAG_LEN + 4 + payload.len()
        );
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 7);
        // 전체 길이 = 바깥 헤더(4) + length.
        assert_eq!(buf.len(), 4 + 4 + TAG_LEN + 4 + payload.len());
    }

    #[test]
    fn host_can_decrypt_what_client_encrypts() {
        let payload = [0x01u8, 0x02, 0x03, 0x04, 0x05];
        let buf = encode_encrypted(&cipher(), 42, MSG_INPUT_DATA, &payload).expect("encode");

        let (ty, body) = host_unwrap(&buf);
        assert_eq!(ty, MSG_INPUT_DATA);
        assert_eq!(body, payload);
    }

    #[test]
    fn client_can_decrypt_what_host_encrypts() {
        let payload = 0x8003_0023u32.to_be_bytes();
        let buf = host_encode(3, MSG_TERMINATION_EXT, &payload);

        let plain = decrypt_message(&buf, &cipher()).expect("client decrypts HC-IV message");
        let (ty, body) = parse_header(&plain).expect("inner header");
        assert_eq!(ty, MSG_TERMINATION_EXT);
        assert_eq!(body, &payload);
    }

    #[test]
    fn wrong_direction_iv_fails_to_decrypt() {
        // 클라이언트가 보낸(CC) 메시지를 클라이언트 복호기(HC)로 열면 반드시 실패해야 한다.
        // 이게 깨지면 두 방향이 같은 IV 를 쓰고 있다는 뜻이다.
        let buf = encode_encrypted(&cipher(), 9, MSG_PING, &[0u8; 4]).expect("encode");
        assert!(decrypt_message(&buf, &cipher()).is_none());
    }

    #[test]
    fn sequence_number_lands_in_the_iv() {
        // 같은 평문이라도 seq 가 다르면 ciphertext 가 달라야 한다 (IV 재사용 방지 확인).
        let a = encode_encrypted(&cipher(), 0, MSG_PING, &[1, 2, 3]).expect("encode");
        let b = encode_encrypted(&cipher(), 1, MSG_PING, &[1, 2, 3]).expect("encode");
        assert_ne!(a[8..], b[8..]);
        assert_eq!(host_unwrap(&a).1, host_unwrap(&b).1);
    }

    #[test]
    fn termination_ext_sets_code() {
        let shared = Shared {
            alive: AtomicBool::new(true),
            term_set: AtomicBool::new(false),
            term_code: AtomicI32::new(0),
        };
        let buf = host_encode(0, MSG_TERMINATION_EXT, &0x8003_0023u32.to_be_bytes());
        handle_inbound(&buf, &cipher(), &shared);

        assert!(shared.term_set.load(Ordering::Acquire));
        assert_eq!(shared.term_code.load(Ordering::Relaxed), 0x8003_0023u32 as i32);
        assert!(!shared.alive.load(Ordering::Acquire));
    }

    #[test]
    fn termination_short_form_is_little_endian_u16() {
        let shared = Shared {
            alive: AtomicBool::new(true),
            term_set: AtomicBool::new(false),
            term_code: AtomicI32::new(0),
        };
        // SERVER_TERMINATED_INTENDED = 0x0100, LE16 (ControlStream.c:1332-1349).
        let buf = host_encode(0, MSG_TERMINATION_EXT, &[0x00, 0x01]);
        handle_inbound(&buf, &cipher(), &shared);
        assert_eq!(shared.term_code.load(Ordering::Relaxed), 0x0100);
    }

    #[test]
    fn unknown_inbound_type_is_ignored_not_fatal() {
        let shared = Shared {
            alive: AtomicBool::new(true),
            term_set: AtomicBool::new(false),
            term_code: AtomicI32::new(0),
        };
        handle_inbound(&host_encode(0, 0x010e, &[1, 2, 3]), &cipher(), &shared);
        handle_inbound(&[0xff, 0xff], &cipher(), &shared); // 런트
        assert!(!shared.term_set.load(Ordering::Acquire));
        assert!(shared.alive.load(Ordering::Acquire));
    }

    #[test]
    fn fec_status_is_21_big_endian_bytes() {
        let st = FrameFecStatus {
            frame_index: 0x0102_0304,
            highest_received_seq: 0x0506,
            next_contiguous_seq: 0x0708,
            missing_before_highest: 0x090a,
            total_data_shards: 0x0b0c,
            total_parity_shards: 0x0d0e,
            received_data_shards: 0x0f10,
            received_parity_shards: 0x1112,
            fec_percentage: 0x13,
            block_index: 0x14,
            block_count: 0x15,
        };
        assert_eq!(
            fec_status_payload(&st).as_slice(),
            &[
                0x01, 0x02, 0x03, 0x04, // frameIndex
                0x05, 0x06, // highestReceivedSequenceNumber
                0x07, 0x08, // nextContiguousSequenceNumber
                0x09, 0x0a, // missingPacketsBeforeHighestReceived
                0x0b, 0x0c, // totalDataPackets
                0x0d, 0x0e, // totalParityPackets
                0x0f, 0x10, // receivedDataPackets
                0x11, 0x12, // receivedParityPackets
                0x13, // fecPercentage
                0x14, // multiFecBlockIndex
                0x15, // multiFecBlockCount
            ]
        );
    }

    /// 호스트 control.rs:173-180 이 실제로 하는 일 전체를 재현한다:
    /// 복호 → 안쪽 헤더 벗기기 → `FrameFecStatus::parse` → `loss_fraction`.
    fn host_loss_signal(buf: &[u8]) -> Option<f32> {
        let (ty, payload) = host_unwrap(buf);
        assert_eq!(ty, MSG_FRAME_FEC_STATUS);
        FrameFecStatus::parse(&payload).and_then(|s| s.loss_fraction())
    }

    #[test]
    fn fec_status_reaches_host_bitrate_controller_as_expected_loss() {
        // 총 120(데이터 100 + 패리티 20), 수신 60 → 0.5.
        let st = FrameFecStatus {
            total_data_shards: 100,
            total_parity_shards: 20,
            received_data_shards: 50,
            received_parity_shards: 10,
            fec_percentage: 20,
            block_count: 1,
            ..Default::default()
        };
        let buf = encode_encrypted(
            &cipher(),
            1,
            MSG_FRAME_FEC_STATUS,
            fec_status_payload(&st).as_slice(),
        )
        .expect("encode");

        // 호스트가 되읽은 구조체가 우리가 보낸 것과 완전히 동일해야 한다.
        let (_, payload) = host_unwrap(&buf);
        assert_eq!(payload.len(), FrameFecStatus::SIZE);
        assert_eq!(FrameFecStatus::parse(&payload), Some(st));
        assert_eq!(host_loss_signal(&buf), Some(0.5));
    }

    #[test]
    fn fec_status_loss_edge_cases() {
        let none_lost = FrameFecStatus {
            total_data_shards: 100,
            total_parity_shards: 20,
            received_data_shards: 100,
            received_parity_shards: 20,
            ..Default::default()
        };
        let buf = encode_encrypted(
            &cipher(),
            0,
            MSG_FRAME_FEC_STATUS,
            fec_status_payload(&none_lost).as_slice(),
        )
        .expect("encode");
        assert_eq!(host_loss_signal(&buf), Some(0.0));

        // 패리티 없는 프레임: 100 중 90 수신 → 0.1.
        let data_only = FrameFecStatus {
            total_data_shards: 100,
            received_data_shards: 90,
            ..Default::default()
        };
        let buf = encode_encrypted(
            &cipher(),
            0,
            MSG_FRAME_FEC_STATUS,
            fec_status_payload(&data_only).as_slice(),
        )
        .expect("encode");
        assert_eq!(host_loss_signal(&buf), Some(0.1));

        // 전송 0 → 신호 없음 (호스트는 on_loss 를 아예 호출하지 않는다).
        let empty = FrameFecStatus::default();
        let buf = encode_encrypted(
            &cipher(),
            0,
            MSG_FRAME_FEC_STATUS,
            fec_status_payload(&empty).as_slice(),
        )
        .expect("encode");
        assert_eq!(host_loss_signal(&buf), None);
    }

    #[test]
    fn start_b_payload_is_single_zero_byte() {
        assert_eq!(start_b_payload().as_slice(), &[0u8]);
    }

    #[test]
    fn ping_payload_is_eight_bytes() {
        // length(LE16=4) + timestamp(LE32=0) + 패딩(2).
        assert_eq!(ping_payload().as_slice(), &[0x04, 0x00, 0, 0, 0, 0, 0, 0]);
    }

    /// 호스트 `input::inject`(kmc-streamhost/src/input.rs:32-38)의 진입 파싱을 재현:
    /// magic 은 payload[4..8] LE32, 본문은 payload[8..].
    fn split_input(p: &PayloadBuf) -> (u32, u32, Vec<u8>) {
        let b = p.as_slice();
        // 호스트는 8바이트 미만이면 조용히 버린다 (input.rs:33-35).
        assert!(b.len() >= 8, "input packet must carry NV_INPUT_HEADER");
        let size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let magic = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        (size, magic, b[8..].to_vec())
    }

    #[test]
    fn input_mouse_position_layout() {
        let p = encode_input(InputEvent::MousePosition {
            x: 300,
            y: -20,
            ref_w: 1920,
            ref_h: 1080,
        });
        let (size, magic, body) = split_input(&p);
        // NV_ABS_MOUSE_MOVE_PACKET = 18B, size 필드는 자기 자신(4B) 제외.
        assert_eq!(p.as_slice().len(), 18);
        assert_eq!(size, 14);
        assert_eq!(magic, IN_MAGIC_MOUSE_MOVE_ABS);
        // 호스트는 body.len() >= 10 을 요구한다 (input.rs:49).
        assert_eq!(body.len(), 10);
        assert_eq!(i16::from_be_bytes([body[0], body[1]]), 300);
        assert_eq!(i16::from_be_bytes([body[2], body[3]]), -20);
        assert_eq!(&body[4..6], &[0, 0]); // unused
        assert_eq!(i16::from_be_bytes([body[6], body[7]]), 1920);
        assert_eq!(i16::from_be_bytes([body[8], body[9]]), 1080);
    }

    #[test]
    fn input_mouse_button_layout() {
        let down = encode_input(InputEvent::MouseButton { button: 3, down: true });
        let (size, magic, body) = split_input(&down);
        assert_eq!(down.as_slice().len(), 9);
        assert_eq!(size, 5);
        assert_eq!(magic, IN_MAGIC_MOUSE_BTN_DOWN);
        assert_eq!(body, vec![3]);

        let up = encode_input(InputEvent::MouseButton { button: 5, down: false });
        let (_, magic, body) = split_input(&up);
        assert_eq!(magic, IN_MAGIC_MOUSE_BTN_UP);
        assert_eq!(body, vec![5]);
    }

    #[test]
    fn input_key_layout() {
        let p = encode_input(InputEvent::Key {
            key_code: 0x41, // VK_A
            down: true,
            modifiers: 0x02,
        });
        let (size, magic, body) = split_input(&p);
        // NV_KEYBOARD_PACKET = 14B.
        assert_eq!(p.as_slice().len(), 14);
        assert_eq!(size, 10);
        assert_eq!(magic, IN_MAGIC_KEY_DOWN);
        assert_eq!(body.len(), 6);
        assert_eq!(body[0], 0); // flags
        // 호스트는 body[1..3] 을 LE u16 로 읽는다 (input.rs:92) — 여기가 BE 면 키가 깨진다.
        assert_eq!(u16::from_le_bytes([body[1], body[2]]), 0x41);
        assert_eq!(body[3], 0x02); // modifiers
        assert_eq!(&body[4..6], &[0, 0]); // zero2

        let up = encode_input(InputEvent::Key { key_code: 0x41, down: false, modifiers: 0 });
        let (_, magic, _) = split_input(&up);
        assert_eq!(magic, IN_MAGIC_KEY_UP);
    }

    #[test]
    fn input_scroll_layout() {
        let p = encode_input(InputEvent::Scroll { amount: -120 });
        let (size, magic, body) = split_input(&p);
        // NV_SCROLL_PACKET = 14B.
        assert_eq!(p.as_slice().len(), 14);
        assert_eq!(size, 10);
        assert_eq!(magic, IN_MAGIC_SCROLL);
        assert_eq!(body.len(), 6);
        assert_eq!(i16::from_be_bytes([body[0], body[1]]), -120);
        assert_eq!(i16::from_be_bytes([body[2], body[3]]), -120); // scrollAmt2 == scrollAmt1
        assert_eq!(&body[4..6], &[0, 0]); // zero3
    }

    #[test]
    fn input_survives_the_encrypted_round_trip() {
        // 입력이 호스트 inject 에 도달하는 전체 경로: 암호화 → 호스트 복호 → 안쪽 헤더.
        let ev = InputEvent::Key { key_code: 0x1B, down: true, modifiers: 0 };
        let buf = encode_encrypted(&cipher(), 5, MSG_INPUT_DATA, encode_input(ev).as_slice())
            .expect("encode");
        let (ty, payload) = host_unwrap(&buf);
        assert_eq!(ty, MSG_INPUT_DATA);
        assert_eq!(payload.len(), 14);
        assert_eq!(
            u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]),
            IN_MAGIC_KEY_DOWN
        );
    }

    #[test]
    fn every_payload_fits_the_inline_buffer() {
        assert_eq!(FrameFecStatus::SIZE, 21);
        assert!(FrameFecStatus::SIZE <= MAX_PAYLOAD);
        for ev in [
            InputEvent::MousePosition { x: 0, y: 0, ref_w: 0, ref_h: 0 },
            InputEvent::MouseButton { button: 1, down: true },
            InputEvent::Key { key_code: 0, down: false, modifiers: 0 },
            InputEvent::Scroll { amount: 0 },
        ] {
            assert!(encode_input(ev).as_slice().len() <= MAX_PAYLOAD);
        }
    }

    #[test]
    fn parse_header_rejects_runt() {
        assert!(parse_header(&[0u8; 3]).is_none());
        let (ty, body) = parse_header(&[0x02, 0x03, 0x01, 0x00, 0xAB]).expect("header");
        assert_eq!(ty, MSG_REQUEST_IDR);
        assert_eq!(body, &[0xAB]);
    }

    #[test]
    fn decrypt_rejects_runt() {
        assert!(decrypt_message(&[0u8; 23], &cipher()).is_none());
    }
}
