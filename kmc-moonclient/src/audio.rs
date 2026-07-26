//! 오디오 수신: UDP RTP(payload type 97) → Opus 프레임을 **그대로** 상위로 전달.
//!
//! 생산자는 우리 호스트 `kmc-streamhost/src/audio.rs`다. 아래는 전부 그 소스에서 확인한 사실이며,
//! moonlight 문서가 아니라 호스트 코드가 규격이다.
//!
//! * 포트: 기본값 48000 (`kmc-streamhost/src/rtsp.rs:32` `StreamPorts::default()`, `host.rs:151-152`).
//!   단, 실제로 바인드할 주소는 RTSP 협상 결과(`RtspNegotiated::audio_port`)에서 온다 —
//!   이 모듈은 포트를 하드코딩하지 않고 [`AudioReceiver::bind`] 인자로 받는다.
//! * RTP 헤더: 12바이트 고정 (`audio.rs:17` `RTP_HEADER_LEN`, 조립은 `audio.rs:101-107`).
//!   `[0]=0x80`(version 2, P/X/CC 없음), `[1]=97`(payload type, marker 미사용),
//!   `[2..4]=seq(BE u16)`, `[4..8]=timestamp(BE u32)`, `[8..12]=ssrc(BE u32, 항상 0)`.
//! * payload type: 97 (`audio.rs:18` `AUDIO_PAYLOAD_TYPE`, 기록은 `audio.rs:103`).
//! * **FEC 패리티는 전선에 실리지 않는다.** 호스트 모듈 주석 `audio.rs:4-5`가
//!   "FEC(RS 4,2)는 손실 복구용이라 무손실 LAN/로컬에선 데이터 샤드만 보내도 재생된다
//!   → 데이터 패킷만 순번대로 전송"이라고 명시하고, 실제 송출 경로(`audio.rs:91-112`)에도
//!   패리티 샤드를 만드는 코드가 전혀 없다. 그래서 이 모듈에는 FEC 복구 코드를 두지 않는다
//!   (죽은 코드 금지). 시퀀스 갭은 **아무것도 emit 하지 않고** 통계로만 센다 —
//!   PLC(끊김 은닉)는 프론트 몫이다(`kmc-moonclient/src/conn.rs:131` 참조: 빈 샘플 = 갭).
//! * **암호화 없음.** `audio.rs:101-107`에서 Opus 프레임을 그대로 이어붙일 뿐이고,
//!   호스트 audio.rs 어디에도 AES/GCM 호출이 없다. (참고로 C 레퍼런스
//!   moonlight-common-c `AudioStream.c:178-195`는 `AudioEncryptionEnabled`일 때만
//!   AES-**CBC**로 복호화한다 — GCM이 아니고, 우리 호스트는 그 플래그에 해당하는 동작 자체가 없다.)
//! * PING: 클라이언트가 오디오 포트로 정확히 `b"PING"` 4바이트를 보내면 호스트가 그 주소를
//!   등록하고 송출을 시작한다(`audio.rs:66-81`). 등록 시 seq/timestamp가 0으로 리셋된다
//!   (`audio.rs:75-76`). 유휴 타임아웃으로 스트림이 멈추지는 **않는다** — 2초짜리
//!   `OWNER_TIMEOUT`(`audio.rs:61`)은 *다른* 주소가 소유권을 뺏을 수 있는 창일 뿐이다.
//!   다만 WSAECONNRESET(10054)를 만나면 호스트가 등록을 해제하고 **재-PING**을 기다리므로
//!   (`audio.rs:84-88`), 클라이언트는 주기적으로 PING을 계속 보내야 복구된다.
//!   → 2초보다 충분히 짧은 [`PING_INTERVAL`](500ms) 주기로 재전송한다.
//! * Opus 파라미터: 48kHz / 스테레오 / 5ms 프레임 = 채널당 240샘플
//!   (`audio.rs:13-16`, 인코더 생성 `audio.rs:159`).
//!
//! 디코딩은 하지 않는다. 프론트(WebCodecs)가 Opus를 디코드하므로 여기선 프레임 바이트를
//! 그대로 mpsc로 넘긴다 — 구 FFI 경로 `conn.rs:117,128-137`(`ar_decode_and_play`)와 동일한 계약.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

/// 호스트 오디오 RTP 헤더 길이(`kmc-streamhost/src/audio.rs:17`).
pub const RTP_HEADER_LEN: usize = 12;
/// 오디오 payload type(`kmc-streamhost/src/audio.rs:18`).
pub const AUDIO_PAYLOAD_TYPE: u8 = 97;
/// 호스트가 등록 트리거로 기대하는 정확한 바이트열(`kmc-streamhost/src/audio.rs:68`).
pub const PING_PAYLOAD: &[u8] = b"PING";
/// PING 재전송 주기. 호스트 `OWNER_TIMEOUT`(2초, `audio.rs:61`)보다 충분히 짧아야 하고,
/// CONNRESET 후 재등록(`audio.rs:84-88`)도 이 주기 안에 회복된다.
pub const PING_INTERVAL: Duration = Duration::from_millis(500);

/// Opus 샘플레이트(`kmc-streamhost/src/audio.rs:13`).
pub const SAMPLE_RATE: u32 = 48_000;
/// 채널 수(`kmc-streamhost/src/audio.rs:14`).
pub const CHANNELS: usize = 2;
/// 프레임 길이(ms) (`kmc-streamhost/src/audio.rs:15`).
pub const FRAME_MS: u32 = 5;
/// 채널당 프레임 샘플 수 = 240 (`kmc-streamhost/src/audio.rs:16`).
pub const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;

/// 호스트 인코더 출력 버퍼 상한(`kmc-streamhost/src/audio.rs:166`)에 헤더를 더한 값.
pub const MAX_AUDIO_DATAGRAM: usize = RTP_HEADER_LEN + 4000;

/// 재정렬로 볼지 스트림 재시작으로 볼지 가르는 경계. 호스트는 새 클라이언트 등록 시
/// seq를 0으로 되돌리므로(`audio.rs:75-76`) 큰 역방향 점프는 재동기화로 처리한다.
const RESYNC_THRESHOLD: i16 = 1024;

/// RTP 헤더 파싱 실패 사유. 어느 경우에도 패닉하지 않고 datagram을 버린다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioParseError {
    /// 12바이트 헤더도 못 채운 runt 패킷.
    TooShort(usize),
    /// RTP version이 2가 아님.
    BadVersion(u8),
    /// CSRC/extension이 붙어 있음 — 호스트는 항상 0x80만 쓴다(`audio.rs:102`).
    UnsupportedHeader(u8),
    /// payload type이 97이 아님. 호스트가 보내지 않는 타입(FEC 패리티 포함)은 전부 여기서 걸린다.
    WrongPayloadType(u8),
    /// 헤더만 있고 Opus 바이트가 없음 — 전달해봐야 프론트가 갭으로 버린다(`conn.rs:131`).
    EmptyPayload,
}

impl std::fmt::Display for AudioParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort(n) => write!(f, "runt audio datagram: {n} bytes < {RTP_HEADER_LEN}"),
            Self::BadVersion(b) => write!(f, "bad RTP version in byte0 {b:#04x}"),
            Self::UnsupportedHeader(b) => write!(f, "unsupported RTP header byte0 {b:#04x}"),
            Self::WrongPayloadType(t) => {
                write!(f, "wrong audio payload type {t} (expected {AUDIO_PAYLOAD_TYPE})")
            }
            Self::EmptyPayload => write!(f, "audio RTP packet carries no Opus payload"),
        }
    }
}

impl std::error::Error for AudioParseError {}

/// 파싱된 오디오 RTP 헤더.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRtpHeader {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

/// 오디오 RTP datagram을 헤더와 Opus payload로 쪼갠다.
///
/// 호스트 조립 순서(`kmc-streamhost/src/audio.rs:101-107`)를 그대로 역으로 읽는다.
/// payload는 복사 없이 원본 슬라이스를 빌려준다.
pub fn parse_rtp_audio(datagram: &[u8]) -> Result<(AudioRtpHeader, &[u8]), AudioParseError> {
    if datagram.len() < RTP_HEADER_LEN {
        return Err(AudioParseError::TooShort(datagram.len()));
    }
    let b0 = datagram[0];
    if b0 >> 6 != 2 {
        return Err(AudioParseError::BadVersion(b0));
    }
    // X(0x10) 또는 CC(0x0F)가 있으면 헤더 길이가 12가 아니게 된다 — 호스트는 쓰지 않는다.
    if b0 & 0x1F != 0 {
        return Err(AudioParseError::UnsupportedHeader(b0));
    }
    // 최상위 비트는 marker. 호스트는 세우지 않지만 표준대로 마스킹해서 비교한다.
    let payload_type = datagram[1] & 0x7F;
    if payload_type != AUDIO_PAYLOAD_TYPE {
        return Err(AudioParseError::WrongPayloadType(payload_type));
    }
    let payload = &datagram[RTP_HEADER_LEN..];
    if payload.is_empty() {
        return Err(AudioParseError::EmptyPayload);
    }
    let header = AudioRtpHeader {
        sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
        timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
        ssrc: u32::from_be_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]),
    };
    Ok((header, payload))
}

/// 손실 통계 스냅샷.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LossStats {
    /// 실제로 받아 전달한 오디오 프레임 수.
    pub received: u64,
    /// seq 진행으로 유추한, 받았어야 할 프레임 수.
    pub expected: u64,
    /// 파싱 단계에서 버린 datagram 수(잘못된 payload type / runt 등).
    pub rejected: u64,
}

impl LossStats {
    /// 유실 추정치. 재정렬로 received가 expected를 넘길 수 있으므로 saturating.
    pub fn lost(&self) -> u64 {
        self.expected.saturating_sub(self.received)
    }

    /// 0.0 ~ 1.0 손실률.
    pub fn loss_ratio(&self) -> f64 {
        if self.expected == 0 {
            return 0.0;
        }
        self.lost() as f64 / self.expected as f64
    }
}

/// 공유 가능한 원자 카운터. `session.rs`가 `Arc`만 들고 값싸게 조회한다.
#[derive(Debug, Default)]
pub struct AudioStats {
    received: AtomicU64,
    expected: AtomicU64,
    rejected: AtomicU64,
}

impl AudioStats {
    /// 카운터 스냅샷. 락 없이 relaxed 로드 3회.
    pub fn snapshot(&self) -> LossStats {
        LossStats {
            received: self.received.load(Ordering::Relaxed),
            expected: self.expected.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }
}

/// 16비트 시퀀스 추적기. 랩어라운드는 `wrapping_sub` 후 `i16` 부호로 판정한다.
#[derive(Debug, Default)]
struct SeqTracker {
    last: Option<u16>,
}

/// [`SeqTracker::advance`] 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeqStep {
    /// 첫 패킷 또는 재시작 후 첫 패킷.
    Resync,
    /// 정상 전진. `gap`은 이번 패킷 직전에 빠진 슬롯 수(연속이면 0).
    Forward { gap: u16 },
    /// 이미 지나간 seq — 재정렬 도착이거나 중복.
    Stale,
}

impl SeqTracker {
    fn advance(&mut self, seq: u16) -> SeqStep {
        let Some(last) = self.last else {
            self.last = Some(seq);
            return SeqStep::Resync;
        };
        // 랩어라운드 안전: 65535 → 0 은 diff 1.
        let diff = seq.wrapping_sub(last) as i16;
        if diff > 0 {
            self.last = Some(seq);
            SeqStep::Forward { gap: diff as u16 - 1 }
        } else if diff <= -RESYNC_THRESHOLD {
            // 호스트가 새 클라이언트를 등록하며 seq를 0으로 되돌린 경우(`audio.rs:75-76`).
            self.last = Some(seq);
            SeqStep::Resync
        } else {
            SeqStep::Stale
        }
    }
}

/// 소켓과 무관한 순수 depacketizer. 테스트는 이걸 직접 두드린다.
pub struct AudioDepacketizer {
    sink: Sender<Vec<u8>>,
    stats: Arc<AudioStats>,
    tracker: SeqTracker,
}

impl AudioDepacketizer {
    /// Opus 프레임을 흘려보낼 싱크를 받아 생성한다.
    pub fn new(sink: Sender<Vec<u8>>) -> Self {
        Self { sink, stats: Arc::new(AudioStats::default()), tracker: SeqTracker::default() }
    }

    /// 통계 핸들. 복제해서 `session.rs`가 들고 있으면 된다.
    pub fn stats(&self) -> Arc<AudioStats> {
        Arc::clone(&self.stats)
    }

    /// datagram 하나를 처리한다.
    ///
    /// * `Ok(true)` — Opus 프레임을 싱크로 보냈다.
    /// * `Ok(false)` — 유효하지만 전달하지 않았다(중복/재정렬 도착).
    /// * `Err(_)` — 파싱 실패. 호출자는 계속 수신하면 된다.
    ///
    /// 갭이 생겨도 대체 프레임을 만들어 넣지 않는다 — FEC 패리티가 전선에 없고
    /// (`kmc-streamhost/src/audio.rs:4-5`), PLC는 프론트 몫이다(`conn.rs:131`).
    pub fn ingest(&mut self, datagram: &[u8]) -> Result<bool, AudioParseError> {
        let (header, payload) = parse_rtp_audio(datagram).inspect_err(|_| {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
        })?;

        match self.tracker.advance(header.sequence) {
            SeqStep::Resync => {
                self.stats.expected.fetch_add(1, Ordering::Relaxed);
            }
            SeqStep::Forward { gap } => {
                self.stats.expected.fetch_add(u64::from(gap) + 1, Ordering::Relaxed);
                if gap > 0 {
                    tracing::debug!(gap, seq = header.sequence, "audio sequence gap (no FEC parity on the wire)");
                }
            }
            SeqStep::Stale => {
                // expected는 이미 셌던 슬롯 — 중복 가산 금지. 그래도 프레임은 살려서 보낸다.
            }
        }

        self.stats.received.fetch_add(1, Ordering::Relaxed);
        // 싱크가 닫혔으면 스트림이 끝난 것 — 호출자가 루프를 접도록 false 대신 에러 없이 알린다.
        Ok(self.sink.send(payload.to_vec()).is_ok())
    }
}

/// UDP 소켓 + PING keepalive + depacketizer를 묶은 수신기.
pub struct AudioReceiver {
    socket: UdpSocket,
    host: SocketAddr,
    depacketizer: AudioDepacketizer,
}

impl AudioReceiver {
    /// 임시 포트에 바인드하고 호스트 오디오 주소를 기억한다.
    ///
    /// `connect`는 쓰지 않는다. 호스트가 `0.0.0.0`에 바인드하면 응답 source IP가
    /// 우리가 보낸 목적지 IP와 다를 수 있어(멀티홈) 커널 필터에 걸릴 수 있기 때문이다.
    /// 우리 임시 포트로 오는 건 호스트뿐이므로 source 필터링은 하지 않는다.
    pub async fn bind(host: SocketAddr, sink: Sender<Vec<u8>>) -> Result<Self> {
        let local: SocketAddr = if host.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
        let socket = UdpSocket::bind(local).await.context("bind audio udp")?;
        Ok(Self { socket, host, depacketizer: AudioDepacketizer::new(sink) })
    }

    /// 로컬 바인드 주소(테스트/로깅용).
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.local_addr().context("audio local_addr")
    }

    /// 통계 핸들.
    pub fn stats(&self) -> Arc<AudioStats> {
        self.depacketizer.stats()
    }

    /// 수신 루프. 즉시 PING을 한 번 보내고, 이후 [`PING_INTERVAL`] 주기로 재전송하면서 수신한다.
    /// 싱크가 닫히면(수신자 드롭) 정상 종료한다.
    pub async fn run(mut self) -> Result<()> {
        let mut buf = vec![0u8; MAX_AUDIO_DATAGRAM];
        // 첫 tick은 즉시 발화 → 시작하자마자 PING.
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if let Err(e) = self.socket.send_to(PING_PAYLOAD, self.host).await {
                        // 호스트가 아직 안 떠 있으면 흔하다. 다음 주기에 재시도.
                        tracing::debug!(error = %e, "audio PING send failed");
                    }
                }
                r = self.socket.recv_from(&mut buf) => match r {
                    Ok((len, _from)) => {
                        match self.depacketizer.ingest(&buf[..len]) {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::info!("audio sink closed — stopping receiver");
                                return Ok(());
                            }
                            Err(e) => tracing::debug!(error = %e, "dropping audio datagram"),
                        }
                    }
                    Err(e) => {
                        // Windows WSAECONNRESET(10054): 이전 PING에 대한 ICMP unreachable.
                        // 호스트가 아직 안 떴을 때 정상적으로 발생 → 무시하고 계속.
                        if e.raw_os_error() == Some(10054) {
                            tracing::debug!("audio socket got ICMP unreachable; continuing");
                            continue;
                        }
                        return Err(e).context("audio recv_from");
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    /// 호스트 `kmc-streamhost/src/audio.rs:101-107`의 조립을 **그대로** 복제한 인코더.
    /// 여기가 어긋나면 테스트가 아니라 이 함수가 틀린 것이므로 라인 단위로 맞춰 둔다.
    fn host_encode_rtp(seq: u16, timestamp: u32, ssrc: u32, frame: &[u8]) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + frame.len());
        pkt.push(0x80); // version 2.
        pkt.push(97); // AUDIO_PAYLOAD_TYPE.
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&timestamp.to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(frame);
        pkt
    }

    fn depacketizer() -> (AudioDepacketizer, Receiver<Vec<u8>>) {
        let (tx, rx) = channel();
        (AudioDepacketizer::new(tx), rx)
    }

    #[test]
    fn host_layout_roundtrips_to_identical_opus_bytes() {
        let frame: Vec<u8> = (0u8..=200).collect();
        let pkt = host_encode_rtp(0xBEEF, 240 * 7, 0, &frame);
        assert_eq!(pkt.len(), RTP_HEADER_LEN + frame.len());

        let (header, payload) = parse_rtp_audio(&pkt).expect("well-formed host packet must parse");
        assert_eq!(header.sequence, 0xBEEF);
        assert_eq!(header.timestamp, 240 * 7);
        assert_eq!(header.ssrc, 0);
        // 바이트 단위 동일성 — 잘림/오프셋 실수를 잡는다.
        assert_eq!(payload, &frame[..]);
    }

    #[test]
    fn ingest_emits_raw_opus_unmodified() {
        let (mut dp, rx) = depacketizer();
        let frame = b"\x78\x9c\xde\xad\xbe\xef opus payload".to_vec();
        assert!(dp.ingest(&host_encode_rtp(0, 0, 0, &frame)).unwrap());
        assert_eq!(rx.try_recv().unwrap(), frame);
        // 디코딩/재포장 금지 — 정확히 한 프레임만 나온다.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rejects_wrong_payload_type_without_panicking() {
        let (mut dp, rx) = depacketizer();
        let mut pkt = host_encode_rtp(0, 0, 0, b"payload");
        pkt[1] = 127; // moonlight의 오디오 FEC 타입 — 우리 호스트는 절대 보내지 않는다.
        assert_eq!(parse_rtp_audio(&pkt), Err(AudioParseError::WrongPayloadType(127)));
        assert_eq!(dp.ingest(&pkt), Err(AudioParseError::WrongPayloadType(127)));
        assert!(rx.try_recv().is_err());

        // marker 비트가 서 있어도 payload type 자체는 97이면 받아들인다.
        let mut marked = host_encode_rtp(1, 0, 0, b"payload");
        marked[1] |= 0x80;
        assert!(parse_rtp_audio(&marked).is_ok());

        let stats = dp.stats().snapshot();
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.received, 0);
    }

    #[test]
    fn rejects_runt_and_malformed_headers_without_panicking() {
        let (mut dp, rx) = depacketizer();
        for len in 0..RTP_HEADER_LEN {
            let runt = vec![0x80u8; len];
            assert_eq!(dp.ingest(&runt), Err(AudioParseError::TooShort(len)));
        }
        // 헤더만 있고 payload 없음.
        assert_eq!(dp.ingest(&host_encode_rtp(0, 0, 0, b"")), Err(AudioParseError::EmptyPayload));
        // RTP version 1.
        let mut bad_ver = host_encode_rtp(0, 0, 0, b"x");
        bad_ver[0] = 0x40;
        assert_eq!(dp.ingest(&bad_ver), Err(AudioParseError::BadVersion(0x40)));
        // CSRC count가 붙어 헤더 길이가 12가 아닌 경우.
        let mut with_csrc = host_encode_rtp(0, 0, 0, b"x");
        with_csrc[0] = 0x81;
        assert_eq!(dp.ingest(&with_csrc), Err(AudioParseError::UnsupportedHeader(0x81)));

        assert!(rx.try_recv().is_err());
        let stats = dp.stats().snapshot();
        assert_eq!(stats.rejected, RTP_HEADER_LEN as u64 + 3);
        assert_eq!(stats.received, 0);
        assert_eq!(stats.expected, 0);
    }

    #[test]
    fn sequence_accounting_survives_16bit_wraparound() {
        let (mut dp, rx) = depacketizer();
        // 65530 → 65535 연속 6개, 그 다음 0 → 3 (랩어라운드 직후 연속 4개).
        for seq in [65530u16, 65531, 65532, 65533, 65534, 65535, 0, 1, 2, 3] {
            assert!(dp.ingest(&host_encode_rtp(seq, 0, 0, b"f")).unwrap());
        }
        let stats = dp.stats().snapshot();
        assert_eq!(stats.received, 10);
        assert_eq!(stats.expected, 10, "랩어라운드를 갭으로 오인하면 안 된다");
        assert_eq!(stats.lost(), 0);
        assert_eq!(rx.try_iter().count(), 10);
    }

    #[test]
    fn gap_across_wraparound_is_counted_and_emits_nothing_for_the_hole() {
        let (mut dp, rx) = depacketizer();
        assert!(dp.ingest(&host_encode_rtp(65534, 0, 0, b"a")).unwrap());
        // 65535, 0, 1 세 개가 유실된 뒤 2가 도착.
        assert!(dp.ingest(&host_encode_rtp(2, 0, 0, b"b")).unwrap());
        let stats = dp.stats().snapshot();
        assert_eq!(stats.received, 2);
        assert_eq!(stats.expected, 5); // 65534,65535,0,1,2
        assert_eq!(stats.lost(), 3);
        assert!((stats.loss_ratio() - 0.6).abs() < 1e-9);
        // 구멍 자리에 가짜 프레임을 만들어 넣지 않는다 — 프론트가 갭으로 처리한다.
        let got: Vec<Vec<u8>> = rx.try_iter().collect();
        assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn reordered_and_duplicate_packets_do_not_inflate_expected() {
        let (mut dp, rx) = depacketizer();
        for seq in [10u16, 11, 12] {
            dp.ingest(&host_encode_rtp(seq, 0, 0, b"f")).unwrap();
        }
        // 늦게 도착한 재정렬 패킷 + 중복.
        dp.ingest(&host_encode_rtp(11, 0, 0, b"late")).unwrap();
        dp.ingest(&host_encode_rtp(12, 0, 0, b"dup")).unwrap();
        let stats = dp.stats().snapshot();
        assert_eq!(stats.expected, 3, "이미 센 슬롯을 다시 세면 안 된다");
        assert_eq!(stats.received, 5);
        assert_eq!(stats.lost(), 0, "received > expected는 saturating으로 0");
        // 늦게 왔어도 프레임 자체는 버리지 않고 전달한다.
        assert_eq!(rx.try_iter().count(), 5);
    }

    #[test]
    fn host_seq_reset_resyncs_instead_of_counting_65k_losses() {
        let (mut dp, _rx) = depacketizer();
        for seq in [5000u16, 5001, 5002] {
            dp.ingest(&host_encode_rtp(seq, 0, 0, b"f")).unwrap();
        }
        // 호스트가 새 클라이언트를 등록하며 seq=0으로 리셋(kmc-streamhost/src/audio.rs:75-76).
        dp.ingest(&host_encode_rtp(0, 0, 0, b"f")).unwrap();
        dp.ingest(&host_encode_rtp(1, 0, 0, b"f")).unwrap();
        let stats = dp.stats().snapshot();
        assert_eq!(stats.received, 5);
        assert_eq!(stats.expected, 5);
        assert_eq!(stats.lost(), 0);
    }

    #[test]
    fn ingest_reports_closed_sink() {
        let (tx, rx) = channel();
        let mut dp = AudioDepacketizer::new(tx);
        drop(rx);
        assert_eq!(dp.ingest(&host_encode_rtp(0, 0, 0, b"f")), Ok(false));
    }

    /// 실제 소켓 왕복: 가짜 호스트가 PING을 받고, 호스트와 동일한 RTP를 되쏘면
    /// 수신기가 Opus 프레임을 그대로 싱크에 올린다.
    // rx.recv_timeout()이 스레드를 블록하므로 워커가 2개 이상이어야 수신 태스크가 돈다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receiver_pings_then_delivers_frames() {
        let fake_host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr = fake_host.local_addr().unwrap();

        let (tx, rx) = channel();
        let receiver = AudioReceiver::bind(host_addr, tx).await.unwrap();
        let stats = receiver.stats();
        let task = tokio::spawn(receiver.run());

        // 정확히 b"PING" 4바이트여야 호스트가 등록한다(kmc-streamhost/src/audio.rs:68).
        let mut buf = [0u8; 64];
        let (len, client_addr) = fake_host.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], PING_PAYLOAD);

        let frame = b"\x01\x02\x03 opus".to_vec();
        for seq in 0u16..3 {
            let pkt = host_encode_rtp(seq, u32::from(seq) * 240, 0, &frame);
            fake_host.send_to(&pkt, client_addr).await.unwrap();
        }

        let mut got = Vec::new();
        while got.len() < 3 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(f) => got.push(f),
                Err(e) => panic!("audio frame not delivered: {e}"),
            }
        }
        assert!(got.iter().all(|f| f == &frame));
        assert_eq!(stats.snapshot().received, 3);
        assert_eq!(stats.snapshot().lost(), 0);

        task.abort();
    }

    /// PING이 [`PING_INTERVAL`] 주기로 재전송되어야 CONNRESET 후 재등록이 된다
    /// (kmc-streamhost/src/audio.rs:84-88).
    #[tokio::test]
    async fn ping_is_repeated_within_host_owner_timeout() {
        let fake_host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr = fake_host.local_addr().unwrap();
        let (tx, _rx) = channel();
        let task = tokio::spawn(AudioReceiver::bind(host_addr, tx).await.unwrap().run());

        let mut buf = [0u8; 64];
        for _ in 0..3 {
            let (len, _) = tokio::time::timeout(Duration::from_secs(2), fake_host.recv_from(&mut buf))
                .await
                .expect("PING must repeat well inside the host's 2s OWNER_TIMEOUT")
                .unwrap();
            assert_eq!(&buf[..len], PING_PAYLOAD);
        }
        task.abort();
    }
}
