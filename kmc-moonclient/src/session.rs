//! 세션 오케스트레이션 — `LiStartConnection` 을 대체하는 순수 Rust 구동부.
//!
//! 슬라이스들을 하나의 스트림으로 묶는다:
//!
//! ```text
//! rtsp::handshake ──> RtspNegotiated{video,audio,control 포트}
//!        │
//!        ├─> video::run   (UDP, PING + FEC 재조립) ──> au_tx      ──> admin WS 팬아웃
//!        │        ├─ fec_tx  ─┐
//!        │        └─ lost_tx ─┤
//!        ├─> audio::run   (UDP, PING)              ──> audio_tx   ──> admin WS 팬아웃
//!        └─> control::connect (ENet + StartB) <────┘  (FEC 상태 보고 / IDR 재요청)
//! ```
//!
//! # 순서가 계약이다
//! 호스트는 control 채널의 `StartB`(0x0307)를 받아야 송출을 시작하고
//! (`kmc-streamhost/src/control.rs:158-162`), 비디오/오디오 UDP 는 클라이언트의 `PING` 으로
//! 목적지 주소를 배운다(`video/mod.rs:61`, `audio.rs:66-81`). 그래서 **수신기를 먼저 띄워
//! PING 을 흘린 뒤** control 을 연결한다. 반대로 하면 첫 프레임들이 갈 곳을 잃는다.
//!
//! # 스레드 모델
//! admin 의 `StreamState::start` 는 동기 함수다(Tauri 커맨드). 그래서 이 모듈이 전용 tokio
//! 런타임을 소유하고 `block_on` 으로 핸드셰이크만 마친 뒤, 수신 루프는 그 위에 spawn 한다.
//! 입력 전송 API 는 런타임 없이 호출 가능해야 하므로(키훅 사이드카 스레드에서 초당 수백 회)
//! 프로세스 전역 핸들에 큐잉만 한다.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use crate::audio::AudioReceiver;
use crate::control::{ControlChannel, InputEvent};
use crate::pair::{LaunchResult, PairedHost, ServerInfo};
use crate::rtsp::{self, StreamCfg};
use crate::video::{self, AuFrame};

/// shard 페이로드 크기(바이트). moonlight 의 LAN 기본값이며 호스트는 ANNOUNCE 로 받은 값을
/// 그대로 쓴다(`kmc-streamhost/src/rtsp.rs:221`).
pub const PACKET_SIZE: u32 = 1392;

/// `ServerCodecModeSupport` bit1 = HEVC (호스트 `webserver.rs:190-191` 의 비트 규약).
const SCM_HEVC: i32 = 0x0002;

/// IDR 재요청 최소 간격. 한 프레임이 깨지면 뒤따르는 프레임들도 연쇄로 깨지므로, 억제하지 않으면
/// 손실 구간마다 IDR 폭풍이 일어 오히려 대역을 더 먹는다.
const IDR_MIN_INTERVAL: Duration = Duration::from_millis(300);

/// 현재(또는 마지막) 세션의 control 채널. 입력/IDR API 가 런타임 없이 접근한다.
///
/// `stop` 후에도 비우지 않는다 — 종료 코드([`last_termination`])를 사후에 읽어야 하기 때문이다.
/// 죽은 채널에 대한 송신은 조용한 no-op 이다(`ControlChannel::enqueue`).
static CONTROL: Mutex<Option<Arc<ControlChannel>>> = Mutex::new(None);

/// 협상된 코덱이 HEVC 인가. 프론트(WebCodecs)가 디코더 설정 전에 조회한다.
static NEGOTIATED_HEVC: AtomicBool = AtomicBool::new(false);

/// 협상된 코덱 문자열("hevc" 또는 "h264").
pub fn negotiated_codec() -> &'static str {
    if NEGOTIATED_HEVC.load(Ordering::Relaxed) {
        "hevc"
    } else {
        "h264"
    }
}

/// 호스트가 통보한 종료 코드(있으면). 세션이 끝난 뒤에도 유효하다.
pub fn last_termination() -> Option<i32> {
    CONTROL.lock().as_ref().and_then(|c| c.termination())
}

/// 키프레임 재전송 요청. 새 WS 뷰어가 붙거나 프레임을 복구하지 못했을 때 호출한다.
pub fn request_idr() {
    if let Some(c) = CONTROL.lock().as_ref() {
        c.request_idr();
    }
}

/// 절대 마우스 위치(참조 해상도 `ref_w`×`ref_h` 기준).
pub fn send_mouse_position(x: i16, y: i16, ref_w: i16, ref_h: i16) {
    send_input(InputEvent::MousePosition { x, y, ref_w, ref_h });
}

/// 마우스 버튼(1=L 2=M 3=R 4=X1 5=X2).
pub fn send_mouse_button(button: u8, down: bool) {
    send_input(InputEvent::MouseButton { button, down });
}

/// 키보드(`key_code` = Windows VK, `modifiers` = MODIFIER_* 비트).
pub fn send_key(key_code: i16, down: bool, modifiers: u8) {
    send_input(InputEvent::Key { key_code, down, modifiers });
}

/// 세로 스크롤(WHEEL_DELTA=120 단위).
pub fn send_scroll(amount: i16) {
    send_input(InputEvent::Scroll { amount });
}

fn send_input(ev: InputEvent) {
    if let Some(c) = CONTROL.lock().as_ref() {
        c.send_input(ev);
    }
}

/// 실행 중인 스트림. drop 하면 control 을 끊고 수신 루프를 세운다.
pub struct StreamSession {
    /// 미디어 수신 루프를 얹은 전용 런타임. drop 순서 제어를 위해 `Option`.
    rt: Option<Runtime>,
    /// 수신 루프 종료 플래그(런타임 종료보다 먼저 관측되어 깔끔히 빠져나간다).
    shutdown: Arc<AtomicBool>,
    control: Arc<ControlChannel>,
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.control.shutdown();
        if let Some(rt) = self.rt.take() {
            // 런타임 drop 은 런타임 스레드 안에서 호출하면 패닉한다. 호출자가 Tauri 커맨드인지
            // 워커인지 알 수 없으므로 항상 별도 스레드에 넘긴다(호출자를 막지도 않는다).
            std::thread::spawn(move || rt.shutdown_timeout(Duration::from_secs(2)));
        }
        tracing::info!("stream session stopped");
    }
}

/// HEVC 를 쓸지 결정한다. 클라이언트(WebCodecs)가 디코드 가능하고(`allow_hevc`) 호스트가
/// 광고했을 때만 참이다 — 둘 중 하나라도 아니면 H.264 로 안전 폴백한다.
fn pick_hevc(codec_mode_support: i32, allow_hevc: bool) -> bool {
    allow_hevc && (codec_mode_support & SCM_HEVC) != 0
}

/// `host` 를 IP 리터럴 또는 이름으로 해석한다.
fn resolve(address: &str, port: u16) -> Result<SocketAddr> {
    (address, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {address}:{port}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {address}:{port}"))
}

/// 스트림 시작: RTSP 협상 → 미디어 수신기 기동 → control 연결(StartB).
///
/// 반환 시점에 호스트는 이미 송출을 시작했다. 인코딩 AU 는 `au_tx`, Opus 프레임은 `audio_tx`
/// 로 흐른다(둘 다 self-framed — 호출자는 재가공 없이 그대로 팬아웃하면 된다).
#[allow(clippy::too_many_arguments)]
pub fn start_stream(
    server: &ServerInfo,
    host: &PairedHost,
    launch: &LaunchResult,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    au_tx: Sender<AuFrame>,
    audio_tx: Sender<Vec<u8>>,
    allow_hevc: bool,
) -> Result<StreamSession> {
    let hevc = pick_hevc(server.codec_mode_support, allow_hevc);
    let cfg = StreamCfg {
        width,
        height,
        fps,
        bitrate_kbps,
        packet_size: PACKET_SIZE,
        video_format: u32::from(hevc),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .thread_name("kmc-media")
        .build()
        .context("build media runtime")?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let address = host.address.clone();
    let rtsp_port = host.rtsp_port;

    let control = rt.block_on(async {
        let neg = rtsp::handshake(&address, rtsp_port, launch, &cfg).await?;
        tracing::info!(
            video = neg.video_port,
            audio = neg.audio_port,
            control = neg.control_port,
            codec = if hevc { "hevc" } else { "h264" },
            "RTSP negotiated"
        );

        let video_addr = resolve(&address, neg.video_port)?;
        let audio_addr = resolve(&address, neg.audio_port)?;
        let local_bind: SocketAddr = if video_addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("literal")
        } else {
            "[::]:0".parse().expect("literal")
        };

        // 수신기 먼저 — PING 이 흐르기 시작해야 호스트가 목적지를 배운다.
        let (fec_tx, fec_rx) = tokio::sync::mpsc::unbounded_channel();
        let (lost_tx, lost_rx) = tokio::sync::mpsc::unbounded_channel();
        let vid_shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = video::run(local_bind, video_addr, au_tx, fec_tx, lost_tx, vid_shutdown).await {
                tracing::error!(error = %e, "video receiver stopped");
            }
        });

        let audio = AudioReceiver::bind(audio_addr, audio_tx)
            .await
            .context("bind audio receiver")?;
        tokio::spawn(async move {
            if let Err(e) = audio.run().await {
                tracing::error!(error = %e, "audio receiver stopped");
            }
        });

        // control 연결이 곧 StartB 전송이다 — 이 줄에서 호스트 송출이 시작된다.
        let control = Arc::new(
            ControlChannel::connect(&address, neg.control_port, &launch.rikey, &launch.rikey_iv)
                .await
                .context("connect control channel")?,
        );

        // 손실 보고 → 호스트 적응 비트레이트. 복구 실패 → IDR 재요청(폭풍 억제).
        let fec_ctl = control.clone();
        tokio::spawn(async move { pump_fec(fec_rx, fec_ctl).await });
        let idr_ctl = control.clone();
        tokio::spawn(async move { pump_lost(lost_rx, idr_ctl).await });

        Ok::<_, anyhow::Error>(control)
    })?;

    NEGOTIATED_HEVC.store(hevc, Ordering::Relaxed);
    *CONTROL.lock() = Some(control.clone());

    Ok(StreamSession { rt: Some(rt), shutdown, control })
}

/// FEC 수신 통계를 control 채널로 그대로 넘긴다(호스트 `BitrateController::on_loss` 의 입력).
async fn pump_fec(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<kmc_gsproto::FrameFecStatus>,
    control: Arc<ControlChannel>,
) {
    while let Some(st) = rx.recv().await {
        control.send_fec_status(st);
    }
}

/// 복구 실패 프레임 → IDR 재요청. [`IDR_MIN_INTERVAL`] 이내 중복 요청은 버린다.
async fn pump_lost(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
    control: Arc<ControlChannel>,
) {
    let mut last: Option<Instant> = None;
    while let Some(frame_index) = rx.recv().await {
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < IDR_MIN_INTERVAL) {
            continue;
        }
        last = Some(now);
        tracing::debug!(frame_index, "frame unrecoverable - requesting IDR");
        control.request_idr();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_requires_both_sides_to_agree() {
        // 호스트가 H.264 만 광고하는 현재 기본값(webserver.rs:191 = 0x0001).
        assert!(!pick_hevc(0x0001, true));
        assert!(!pick_hevc(0x0001, false));
        // 호스트가 둘 다 광고해도 클라가 디코드 못 하면 H.264.
        assert!(!pick_hevc(0x0003, false));
        // 둘 다 되면 HEVC.
        assert!(pick_hevc(0x0003, true));
        assert!(pick_hevc(0x0002, true));
    }

    #[test]
    fn video_format_flag_matches_moonlight_bitstreamformat() {
        // ANNOUNCE 의 x-nv-vqos[0].bitStreamFormat: 0=H264, 1=HEVC (host rtsp.rs:223).
        assert_eq!(u32::from(pick_hevc(0x0003, true)), 1);
        assert_eq!(u32::from(pick_hevc(0x0001, true)), 0);
    }

    #[test]
    fn resolve_accepts_ip_literals_without_dns() {
        let a = resolve("127.0.0.1", 48010).expect("ipv4 literal");
        assert_eq!(a, "127.0.0.1:48010".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn resolve_reports_the_failing_target() {
        let e = resolve("this-host-does-not-exist.invalid", 48010).unwrap_err();
        assert!(
            format!("{e:#}").contains("this-host-does-not-exist.invalid:48010"),
            "error should name the target: {e:#}"
        );
    }

    /// 손실이 몰아쳐도 IDR 은 억제 간격을 지켜야 한다 — 안 그러면 손실 구간마다 IDR 폭풍이 난다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idr_storm_is_suppressed() {
        // ControlChannel 없이 억제 로직만 검증한다(연결은 E2E 몫).
        let mut last: Option<Instant> = None;
        let mut sent = 0;
        for _ in 0..100 {
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < IDR_MIN_INTERVAL) {
                continue;
            }
            last = Some(now);
            sent += 1;
        }
        assert_eq!(sent, 1, "100 consecutive losses must collapse to one IDR");
    }
}
