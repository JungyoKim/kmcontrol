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

/// 최신 NV12 프레임 공유 슬롯 (캡처 → 인코딩 타이머).
#[derive(Clone)]
pub struct FrameSlot {
    inner: Arc<Mutex<Option<Vec<u8>>>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
    fn store(&self, nv12: &[u8]) {
        let mut g = self.inner.lock();
        match g.as_mut() {
            Some(buf) if buf.len() == nv12.len() => buf.copy_from_slice(nv12),
            _ => *g = Some(nv12.to_vec()),
        }
        self.generation.fetch_add(1, Ordering::Release);
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
                            let settings = Settings::new(
                                monitor,
                                CursorCaptureSettings::WithCursor,
                                DrawBorderSettings::Default,
                                SecondaryWindowSettings::Default,
                                MinimumUpdateIntervalSettings::Default,
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

/// 고정 fps 인코딩 루프. 슬롯의 최신 NV12(없으면 직전)를 협상 fps로 인코딩·전송.
fn encode_loop(flags: CaptureFlags, slot: FrameSlot) {
    let mut encoder = match QsvEncoder::new_codec(flags.codec, flags.width, flags.height, flags.fps, flags.bitrate_bps) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error=%e, "qsv encoder init failed");
            return;
        }
    };
    encoder.request_idr(); // 첫 프레임 IDR.
    tracing::info!(width = flags.width, height = flags.height, fps = flags.fps, "encode timer + h264_qsv ready");

    let interval = Duration::from_secs_f64(1.0 / flags.fps.max(1) as f64);
    let mut next = Instant::now();
    let mut nv12 = Vec::new();
    let mut frame_count: u64 = 0;
    let start = Instant::now();

    // 첫 프레임이 슬롯에 들어올 때까지 잠깐 대기.
    loop {
        if flags.stop_rx.load(Ordering::Relaxed) {
            return;
        }
        if slot.load(&mut nv12).is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    loop {
        if flags.stop_rx.load(Ordering::Relaxed) {
            tracing::info!("encode loop stopping");
            return;
        }

        // 최신 프레임 로드 (변화 없으면 직전 nv12 유지 = 재전송).
        let _ = slot.load(&mut nv12);

        if flags.idr_req.swap(false, Ordering::Relaxed) {
            encoder.request_idr();
        }

        match encoder.encode(&nv12) {
            Ok(packets) => {
                let rtp_ts = (frame_count.wrapping_mul(90_000) / flags.fps.max(1) as u64) as u32;
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
            tracing::info!(frames = frame_count, fps = frame_count as f64 / secs, "encode timer throughput");
        }

        // 고정 fps 페이싱.
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}
