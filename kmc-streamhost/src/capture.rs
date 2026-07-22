//! 라이브 캡처 → GPU 색변환 → (최신프레임 슬롯) → 고정 fps 인코딩 타이머 → QSV → 패킷화.
//!
//! 캡처 콜백(windows-capture)은 GPU 변환한 NV12를 공유 슬롯에 저장만 한다(빠름).
//! 별도 인코딩 타이머 스레드가 협상 fps로 슬롯의 최신 NV12(변화 없으면 직전 것)를 꺼내
//! h264_qsv로 인코딩·전송한다. 이 디커플링이 Moonlight이 요구하는 안정적 프레임 cadence를
//! 만든다 — 캡처 공급이 불규칙(정적 화면 시 급감)해도 인코딩은 일정 속도 유지.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::gpuconvert::GpuConverter;
use crate::qsv::QsvEncoder;
use crate::video::EncodedFrame;

#[derive(Clone)]
pub struct CaptureFlags {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// QSV 인코더 이름: "h264_qsv" 또는 "hevc_qsv".
    pub codec: &'static str,
    pub sender: crate::video::FrameSender,
    pub stop_rx: Arc<AtomicBool>,
    pub idr_req: Arc<AtomicBool>,
    /// 캡처+인코드 스레드가 완전히 종료되면 true (세션 전환 시 이전 세션 종료 대기용).
    pub done: Arc<AtomicBool>,
}

/// 최신 NV12 프레임 공유 슬롯 (캡처 → 인코더). Condvar 로 새 프레임 도착을 즉시 통지 →
/// 인코더가 sleep 폴링(≈15ms 그래뉼 = ~64fps 상한) 없이 무제한으로 최신 프레임을 인코딩.
#[derive(Clone)]
pub struct FrameSlot {
    inner: Arc<Mutex<Option<Vec<u8>>>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    cv: Arc<parking_lot::Condvar>,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cv: Arc::new(parking_lot::Condvar::new()),
        }
    }
    fn store(&self, nv12: &[u8]) {
        let mut g = self.inner.lock();
        match g.as_mut() {
            Some(buf) if buf.len() == nv12.len() => buf.copy_from_slice(nv12),
            _ => *g = Some(nv12.to_vec()),
        }
        self.generation.fetch_add(1, Ordering::Release);
        self.cv.notify_all();
    }
    /// 현재 프레임 복사 + generation.
    fn load(&self, out: &mut Vec<u8>) -> Option<u64> {
        let g = self.inner.lock();
        let buf = g.as_ref()?;
        if out.len() != buf.len() {
            out.resize(buf.len(), 0);
        }
        out.copy_from_slice(buf);
        Some(self.generation.load(Ordering::Acquire))
    }
    /// `last_gen` 이후 새 프레임을 최대 `timeout` 까지 대기하며 복사. 반환 = 새 generation(있으면).
    /// 새 프레임 없으면(타임아웃) None → 호출부가 직전 프레임 재전송(keepalive) 판단.
    fn wait_new(&self, out: &mut Vec<u8>, last_gen: u64, timeout: Duration) -> Option<u64> {
        let mut g = self.inner.lock();
        if self.generation.load(Ordering::Acquire) == last_gen {
            self.cv.wait_for(&mut g, timeout);
        }
        let cur = self.generation.load(Ordering::Acquire);
        let buf = g.as_ref()?;
        if out.len() != buf.len() {
            out.resize(buf.len(), 0);
        }
        out.copy_from_slice(buf);
        Some(cur)
    }
}

/// 캡처 핸들러: GPU 변환 후 슬롯에 저장.
pub struct LiveCapture {
    converter: Option<GpuConverter>,
    flags: CaptureFlags,
    slot: FrameSlot,
}

impl GraphicsCaptureApiHandler for LiveCapture {
    type Flags = (CaptureFlags, FrameSlot);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (flags, slot) = ctx.flags;
        Ok(Self { converter: None, flags, slot })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.stop_rx.load(Ordering::Relaxed) {
            capture_control.stop();
            return Ok(());
        }
        if self.converter.is_none() {
            let device: ID3D11Device = frame.device().clone();
            let context: ID3D11DeviceContext = frame.device_context().clone();
            tracing::info!(
                frame_w = frame.width(),
                frame_h = frame.height(),
                target_w = self.flags.width,
                target_h = self.flags.height,
                "capture frame dims vs encoder target"
            );
            let conv = GpuConverter::new(device, context, frame.width(), frame.height(), self.flags.width, self.flags.height)
                .map_err(|e| format!("gpu converter init: {e}"))?;
            self.converter = Some(conv);
        }
        let tex: ID3D11Texture2D = frame.as_raw_texture().clone();
        let nv12 = self
            .converter
            .as_mut()
            .unwrap()
            .convert(&tex)
            .map_err(|e| format!("gpu convert: {e}"))?;
        self.slot.store(nv12);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("capture session closed");
        Ok(())
    }
}

/// 캡처 스레드 + 인코딩 타이머 스레드를 함께 시작.
/// 감독 스레드가 두 워커를 join한 뒤 `flags.done`을 set해, 세션 전환 시
/// 호출부가 이전 세션의 완전한 종료를 기다릴 수 있게 한다 (GraphicsCapture 중복 시작 크래시 방지).
pub fn spawn_capture(flags: CaptureFlags) {
    let done = flags.done.clone();
    let done_outer = flags.done.clone();
    let supervisor = std::thread::Builder::new()
        .name("capture-supervisor".into())
        .spawn(move || {
            let slot = FrameSlot::new();

            // 인코딩 타이머 스레드 (panic 격리).
            let encode_handle = {
                let flags = flags.clone();
                let slot = slot.clone();
                std::thread::Builder::new()
                    .name("encode-timer".into())
                    .spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            unsafe {
                                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                            }
                            encode_loop(flags, slot);
                        }));
                        if r.is_err() {
                            tracing::error!("encode loop panicked (isolated) — session degraded, process alive");
                        }
                    })
            };
            let encode_handle = match encode_handle {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::error!(error=%e, "failed to spawn encode thread");
                    None
                }
            };

            // 캡처 스레드 (panic 격리). start_free_threaded + stop_rx 폴링.
            let capture_handle = {
                let flags = flags.clone();
                let slot = slot.clone();
                std::thread::Builder::new()
                    .name("live-capture".into())
                    .spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let monitor = match Monitor::primary() {
                                Ok(m) => m,
                                Err(e) => {
                                    tracing::error!(error=%e, "no primary monitor");
                                    return;
                                }
                            };
                            let stop_rx = flags.stop_rx.clone();
                            // WGC 의 MinUpdateInterval Default 는 사실상 60Hz(≈16.6ms)로 프레임 전달을
                            // 캡한다. target fps 간격으로 낮춰(예 120fps→8.3ms) 캡처가 그 상한까지 프레임을
                            // 공급하게 한다. 이 속성 미지원 플랫폼이면 Default 로 폴백(에러 방지).
                            let target_fps = flags.fps.max(1);
                            let min_interval = std::time::Duration::from_secs_f64(1.0 / target_fps as f64);
                            let mui = match windows_capture::graphics_capture_api::GraphicsCaptureApi::is_minimum_update_interval_supported() {
                                Ok(true) => MinimumUpdateIntervalSettings::Custom(min_interval),
                                _ => MinimumUpdateIntervalSettings::Default,
                            };
                            let settings = Settings::new(
                                monitor,
                                CursorCaptureSettings::WithCursor,
                                DrawBorderSettings::Default,
                                SecondaryWindowSettings::Default,
                                mui,
                                DirtyRegionSettings::Default,
                                ColorFormat::Bgra8,
                                (flags, slot),
                            );
                            match LiveCapture::start_free_threaded(settings) {
                                Ok(control) => {
                                    while !stop_rx.load(Ordering::Relaxed) {
                                        if control.is_finished() {
                                            break;
                                        }
                                        std::thread::sleep(Duration::from_millis(20));
                                    }
                                    if let Err(e) = control.stop() {
                                        tracing::warn!(error=%e, "capture stop error");
                                    }
                                }
                                Err(e) => tracing::error!(error=%e, "live capture failed to start"),
                            }
                        }));
                        if r.is_err() {
                            tracing::error!("capture thread panicked (isolated) — session degraded, process alive");
                        }
                    })
            };
            let capture_handle = match capture_handle {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::error!(error=%e, "failed to spawn capture thread");
                    None
                }
            };

            if let Some(h) = capture_handle {
                let _ = h.join();
            }
            if let Some(h) = encode_handle {
                let _ = h.join();
            }
            done.store(true, Ordering::Release);
            tracing::info!("capture session fully torn down");
        });
    if let Err(e) = supervisor {
        tracing::error!(error=%e, "failed to spawn capture supervisor");
        done_outer.store(true, Ordering::Release);
    }
}

/// 인코딩 루프. `flags.fps==0` 이면 **무제한**: Condvar 로 새 프레임 도착 즉시 인코딩(인코더가
/// 뽑는 만큼 = GPU 인코더 상한). 정적 화면(새 프레임 없음)에선 keepalive 주기로 직전 프레임 재전송.
/// `flags.fps>0` 이면 고정 fps 페이싱(하위호환). RTP 타임스탬프는 항상 실시간 경과(90kHz) 기반.
fn encode_loop(flags: CaptureFlags, slot: FrameSlot) {
    // target fps = 상한(클라 요청값, 예 60/120). Sunshine 처럼 "무제한"이 아니라 이벤트 구동으로
    // 이 상한까지 뽑는다. min_fps = 정적 화면 floor(연결 유지 + 화질 회복용 주기적 재인코딩).
    let target_fps = flags.fps.max(1);
    let min_fps = (target_fps / 2).max(2); // Sunshine 기본 ≈ target/2.
    let mut encoder = match QsvEncoder::new_codec(flags.codec, flags.width, flags.height, target_fps, flags.bitrate_bps) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error=%e, "qsv encoder init failed");
            return;
        }
    };
    encoder.request_idr(); // 첫 프레임 IDR.
    tracing::info!(width = flags.width, height = flags.height, target_fps, min_fps, "encoder ready (event-driven, capped at target)");

    let mut nv12 = Vec::new();
    let mut frame_count: u64 = 0;
    let start = Instant::now();
    // 상한 간격: 새 프레임이 아무리 빨리 와도 이보다 자주 인코딩하지 않음(target fps 상한).
    let min_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    // floor 간격: 정적 화면에서도 최소 이 주기로 재인코딩(연결 유지 + 화질 회복).
    let max_wait = Duration::from_secs_f64(1.0 / min_fps as f64);

    // 첫 프레임 대기.
    let mut gen: u64 = 0;
    loop {
        if flags.stop_rx.load(Ordering::Relaxed) {
            return;
        }
        if let Some(g) = slot.load(&mut nv12) {
            gen = g;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut last_encode = Instant::now();
    loop {
        if flags.stop_rx.load(Ordering::Relaxed) {
            tracing::info!("encode loop stopping");
            return;
        }

        // 새 프레임을 max_wait(=floor 주기)까지 대기. 새 프레임 오면 즉시 진행, 없으면
        // floor 로 직전 프레임 재인코딩(정적 화면 keepalive → 스트림 유지, 멈춤 방지).
        if let Some(g) = slot.wait_new(&mut nv12, gen, max_wait) {
            gen = g;
        }

        // 상한 페이싱: 직전 인코딩 후 min_interval 이 안 지났으면 남은 시간만 sleep(target fps 상한).
        let since = last_encode.elapsed();
        if since < min_interval {
            std::thread::sleep(min_interval - since);
        }
        last_encode = Instant::now();

        if flags.idr_req.swap(false, Ordering::Relaxed) {
            encoder.request_idr();
        }

        // RTP 타임스탬프 = 실시간 경과(초) × 90kHz. 가변 fps 에서 정확한 재생 페이싱.
        let rtp_ts = (start.elapsed().as_secs_f64() * 90_000.0) as u32;
        match encoder.encode(&nv12) {
            Ok(packets) => {
                for p in packets {
                    let ef = EncodedFrame {
                        data: p.data,
                        is_key_frame: p.is_key_frame,
                        rtp_timestamp: rtp_ts,
                    };
                    if flags.sender.blocking_send(ef).is_err() {
                        tracing::info!("video channel closed; encode loop stopping");
                        return;
                    }
                }
            }
            Err(e) => tracing::warn!(error=%e, "qsv encode error"),
        }

        frame_count += 1;
        if frame_count % 120 == 0 {
            let secs = start.elapsed().as_secs_f64();
            tracing::info!(frames = frame_count, fps = frame_count as f64 / secs, "encode throughput");
        }
    }
}
