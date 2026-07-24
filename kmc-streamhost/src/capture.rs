//! 라이브 캡처 → 인코딩 → 패킷화. 두 경로:
//!
//! **zero-copy (Intel/Arc):** 캡처 콜백(디바이스 A=WGC)은 WGC BGRA 프레임을 공유 텍스처(keyed
//! mutex)에 CopyResource 만 하고 즉시 반환. 별도 인코드 스레드가 자기 디바이스 B(같은 어댑터,
//! 별도 D3D11)로 공유 텍스처를 로컬 복사→즉시 반납→색변환(NV12)→QSV 인코딩. A/B 디바이스 분리로
//! WGC vs VideoProcessor vs mfx 의 디바이스 락 경합이 사라진다(Sunshine 방식).
//!
//! **RAM 폴백:** 크로스-디바이스 설정 실패 시 — 캡처 콜백이 디바이스 A 에서 색변환+CPU readback
//! 해 바이트 슬롯에 저장, 인코드 스레드가 슬롯을 읽어 QsvEncoder 로 인코딩(기존 검증 경로).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::gpu_encode::ZeroCopyEncoder;
use crate::gpuconvert::GpuConverter;
use crate::qsv::QsvEncoder;
use crate::video::EncodedFrame;
use crate::xdevice::{self, KEY_CAPTURE, KEY_ENCODE};

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
    /// 클라이언트가 현재 프레임을 수신 중인가(video PING 활성). false 면 인코딩을 건너뛰어
    /// idle 시 CPU/GPU/배터리를 아낀다(파이프라인 리소스는 유지, 무거운 작업만 정지).
    pub client_active: Arc<AtomicBool>,
}

/// 최신 NV12 프레임 공유 슬롯 (RAM 폴백: 캡처 → 인코더). Condvar 로 새 프레임 도착을 즉시 통지.
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
    /// `last_gen` 이후 새 프레임을 최대 `timeout` 까지 대기하며 복사.
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

/// 크로스-디바이스 setup 메시지(캡처 첫 프레임 → 인코드 스레드). 필드 전부 Send.
/// `handle == 0` 이면 캡처가 공유 텍스처를 못 만든 것 → 인코드는 즉시 RAM 폴백.
struct SetupMsg {
    luid: i64,
    handle: isize,
    src_w: u32,
    src_h: u32,
}

/// 캡처 핸들러 초기화 데이터(Settings::new 로 전달).
pub struct CaptureInit {
    flags: CaptureFlags,
    slot: FrameSlot,
    setup_tx: mpsc::Sender<SetupMsg>,
    mode_rx: mpsc::Receiver<bool>,
}

/// 캡처 핸들러(디바이스 A). 첫 프레임에 모드 결정 후 zero-copy(공유 텍스처 복사) 또는 RAM(변환+슬롯).
pub struct LiveCapture {
    flags: CaptureFlags,
    slot: FrameSlot,
    setup_tx: Option<mpsc::Sender<SetupMsg>>,
    mode_rx: Option<mpsc::Receiver<bool>>,
    init_done: bool,
    zerocopy: bool,
    ctx_a: Option<ID3D11DeviceContext>,
    shared: Option<xdevice::SharedTex>,
    converter: Option<GpuConverter>,
}

impl GraphicsCaptureApiHandler for LiveCapture {
    type Flags = CaptureInit;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let CaptureInit { flags, slot, setup_tx, mode_rx } = ctx.flags;
        Ok(Self {
            flags,
            slot,
            setup_tx: Some(setup_tx),
            mode_rx: Some(mode_rx),
            init_done: false,
            zerocopy: false,
            ctx_a: None,
            shared: None,
            converter: None,
        })
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

        if !self.init_done {
            self.init_done = true;
            let device: ID3D11Device = frame.device().clone();
            let context: ID3D11DeviceContext = frame.device_context().clone();
            let (fw, fh) = (frame.width(), frame.height());
            tracing::info!(fw, fh, target_w = self.flags.width, target_h = self.flags.height, "capture dims vs encoder target");

            let tx = self.setup_tx.take();
            let rx = self.mode_rx.take();
            let mut chose_zc = false;
            if let (Some(tx), Some(rx)) = (tx, rx) {
                // 디바이스 A 에 공유 BGRA 텍스처 생성 시도.
                match xdevice::create_shared_bgra(&device, fw, fh).and_then(|st| {
                    let luid = xdevice::device_luid(&device)?;
                    Ok((st, luid))
                }) {
                    Ok((st, luid)) => {
                        let _ = tx.send(SetupMsg { luid, handle: st.handle, src_w: fw, src_h: fh });
                        // 인코드 스레드의 모드 결정 대기(디바이스 B/인코더 init 시간 포함).
                        match rx.recv_timeout(Duration::from_secs(5)) {
                            Ok(true) => {
                                tracing::info!("zero-copy cross-device path active");
                                self.zerocopy = true;
                                self.shared = Some(st);
                                self.ctx_a = Some(context.clone());
                                chose_zc = true;
                            }
                            other => tracing::warn!(?other, "encode chose RAM fallback"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=%e, "shared texture create failed; RAM");
                        let _ = tx.send(SetupMsg { luid: 0, handle: 0, src_w: fw, src_h: fh });
                    }
                }
            }

            if !chose_zc {
                match GpuConverter::new(device, context, fw, fh, self.flags.width, self.flags.height) {
                    Ok(c) => self.converter = Some(c),
                    Err(e) => return Err(format!("gpu converter init: {e}").into()),
                }
            }
        }

        let wgc_tex: ID3D11Texture2D = frame.as_raw_texture().clone();
        if self.zerocopy {
            let st = self.shared.as_ref().unwrap();
            let ctx = self.ctx_a.as_ref().unwrap();
            // KEY_CAPTURE 획득(인코드가 반납할 때까지, 짧은 타임아웃 → WGC 무한 블록 방지).
            match xdevice::acquire_sync(&st.mutex, KEY_CAPTURE, 8) {
                Ok(true) => {
                    unsafe {
                        ctx.CopyResource(&st.tex, &wgc_tex);
                        let _ = st.mutex.ReleaseSync(KEY_ENCODE);
                    }
                }
                Ok(false) => {} // 타임아웃: 인코드가 아직 이전 프레임 처리 중 → 이 프레임 스킵(WGC 계속).
                Err(e) => tracing::warn!(error=%e, "capture AcquireSync"),
            }
        } else if let Some(c) = self.converter.as_mut() {
            match c.convert(&wgc_tex) {
                Ok(nv12) => self.slot.store(nv12),
                Err(e) => tracing::warn!(error=%e, "gpu convert"),
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("capture session closed");
        Ok(())
    }
}

/// 인코드 디바이스 B 상태(zero-copy 경로).
struct ZcState {
    ctx_b: ID3D11DeviceContext,
    shared_b: ID3D11Texture2D,
    mutex_b: IDXGIKeyedMutex,
    local_b: ID3D11Texture2D,
    converter: GpuConverter,
    encoder: ZeroCopyEncoder,
    _dev_b: ID3D11Device,
}

fn setup_zerocopy(flags: &CaptureFlags, msg: &SetupMsg) -> anyhow::Result<ZcState> {
    let (dev_b, ctx_b) = xdevice::create_device_for_luid(msg.luid)?;
    let (shared_b, mutex_b) = xdevice::open_shared(&dev_b, msg.handle)?;
    let local_b = xdevice::create_bgra(&dev_b, msg.src_w, msg.src_h)?;
    let converter = GpuConverter::new(dev_b.clone(), ctx_b.clone(), msg.src_w, msg.src_h, flags.width, flags.height)?;
    let encoder = ZeroCopyEncoder::new(flags.codec, &dev_b, &ctx_b, flags.width, flags.height, flags.fps.max(1), flags.bitrate_bps)?;
    Ok(ZcState { ctx_b, shared_b, mutex_b, local_b, converter, encoder, _dev_b: dev_b })
}

/// 인코드 스레드 진입점: 캡처의 setup 을 받아 zero-copy 시도, 실패 시 RAM 폴백.
fn encode_thread(
    flags: CaptureFlags,
    setup_rx: mpsc::Receiver<SetupMsg>,
    mode_tx: mpsc::Sender<bool>,
    slot: FrameSlot,
) {
    let msg = match setup_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(m) => m,
        Err(_) => {
            tracing::warn!("no capture setup received; encode thread exiting");
            return;
        }
    };
    if msg.handle == 0 {
        tracing::warn!("capture can't share texture; RAM encode loop");
        encode_loop(flags, slot);
        return;
    }
    match setup_zerocopy(&flags, &msg) {
        Ok(state) => {
            let _ = mode_tx.send(true);
            zerocopy_loop(flags, state);
        }
        Err(e) => {
            tracing::warn!(error=%e, "device B / zero-copy setup failed; RAM fallback");
            let _ = mode_tx.send(false);
            encode_loop(flags, slot);
        }
    }
}

/// zero-copy 인코드 루프(디바이스 B). 공유 텍스처를 로컬 복사→즉시 반납→변환→인코딩.
fn zerocopy_loop(flags: CaptureFlags, mut s: ZcState) {
    let start = Instant::now();
    let mut frames: u64 = 0;
    tracing::info!("zero-copy encode loop started (device B)");
    loop {
        if flags.stop_rx.load(Ordering::Relaxed) {
            break;
        }
        // 클라이언트가 없으면(아무도 스트림을 안 봄) 인코딩을 건너뛰고 절전 대기.
        // 캡처/변환/QSV 인코딩이 idle 노트북의 CPU·GPU·배터리를 태우는 걸 막는다.
        // 파이프라인 리소스(D3D11/QSV)는 그대로 유지하므로 재개 시 재생성 비용/크래시 없음.
        if !flags.client_active.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        let idr = flags.idr_req.swap(false, Ordering::Relaxed);
        match xdevice::acquire_sync(&s.mutex_b, KEY_ENCODE, 100) {
            Ok(true) => {}
            Ok(false) => continue, // 타임아웃(정적 화면 = 새 프레임 없음).
            Err(e) => {
                tracing::warn!(error=%e, "encode AcquireSync");
                continue;
            }
        }
        // 공유 → 로컬 복사(빠름) 후 즉시 반납 → 캡처가 다음 프레임을 바로 쓸 수 있음.
        unsafe {
            s.ctx_b.CopyResource(&s.local_b, &s.shared_b);
            let _ = s.mutex_b.ReleaseSync(KEY_CAPTURE);
        }
        // 디바이스 B 에서 색변환 + QSV 인코딩(WGC 디바이스와 경합 없음).
        let rtp_ts = (start.elapsed().as_secs_f64() * 90_000.0) as u32;
        let nv12 = match s.converter.convert_gpu(&s.local_b) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error=%e, "convert_gpu");
                continue;
            }
        };
        if let Err(e) = s.encoder.stage(nv12) {
            tracing::warn!(error=%e, "encoder stage");
            continue;
        }
        match s.encoder.submit(idr) {
            Ok(packets) => {
                for p in packets {
                    let ef = EncodedFrame { data: p.data, is_key_frame: p.is_key_frame, rtp_timestamp: rtp_ts };
                    if flags.sender.blocking_send(ef).is_err() {
                        tracing::info!("video channel closed; zero-copy loop stopping");
                        return;
                    }
                }
            }
            Err(e) => tracing::warn!(error=%e, "encoder submit"),
        }
        frames += 1;
        if frames % 120 == 0 {
            let secs = start.elapsed().as_secs_f64();
            tracing::info!(frames, fps = frames as f64 / secs, "xdevice capture+encode throughput");
        }
    }
    tracing::info!("zero-copy encode loop stopped");
}

/// 캡처 스레드 + 인코드 스레드를 함께 시작. 감독 스레드가 둘을 join 후 `flags.done` set.
pub fn spawn_capture(flags: CaptureFlags) {
    let done = flags.done.clone();
    let done_outer = flags.done.clone();
    let supervisor = std::thread::Builder::new()
        .name("capture-supervisor".into())
        .spawn(move || {
            let slot = FrameSlot::new();
            let (setup_tx, setup_rx) = mpsc::channel::<SetupMsg>();
            let (mode_tx, mode_rx) = mpsc::channel::<bool>();

            // 인코드 스레드(panic 격리).
            let encode_handle = {
                let flags = flags.clone();
                let slot = slot.clone();
                std::thread::Builder::new()
                    .name("encode".into())
                    .spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            unsafe {
                                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                            }
                            encode_thread(flags, setup_rx, mode_tx, slot);
                        }));
                        if r.is_err() {
                            tracing::error!("encode thread panicked (isolated) — session degraded, process alive");
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

            // 캡처 스레드(panic 격리). start_free_threaded + stop_rx 폴링.
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
                            // WGC MinUpdateInterval 을 target fps 간격으로 낮춰 프레임 공급 상한 해제.
                            let target_fps = flags.fps.max(1);
                            let min_interval = std::time::Duration::from_secs_f64(1.0 / target_fps as f64);
                            let mui = match windows_capture::graphics_capture_api::GraphicsCaptureApi::is_minimum_update_interval_supported() {
                                Ok(true) => MinimumUpdateIntervalSettings::Custom(min_interval),
                                _ => MinimumUpdateIntervalSettings::Default,
                            };
                            let init = CaptureInit { flags, slot, setup_tx, mode_rx };
                            let settings = Settings::new(
                                monitor,
                                CursorCaptureSettings::WithCursor,
                                DrawBorderSettings::Default,
                                SecondaryWindowSettings::Default,
                                mui,
                                DirtyRegionSettings::Default,
                                ColorFormat::Bgra8,
                                init,
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

/// RAM 폴백 인코딩 루프. `flags.fps` 상한까지 이벤트 구동, 정적 화면 floor(min_fps) 재인코딩.
/// RTP 타임스탬프는 실시간 경과(90kHz) 기반.
fn encode_loop(flags: CaptureFlags, slot: FrameSlot) {
    let target_fps = flags.fps.max(1);
    let min_fps = (target_fps / 2).max(2);
    let mut encoder = match QsvEncoder::new_codec(flags.codec, flags.width, flags.height, target_fps, flags.bitrate_bps) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error=%e, "qsv encoder init failed");
            return;
        }
    };
    encoder.request_idr();
    tracing::info!(width = flags.width, height = flags.height, target_fps, min_fps, "RAM encoder ready (event-driven, capped at target)");

    let mut nv12 = Vec::new();
    let mut frame_count: u64 = 0;
    let start = Instant::now();
    let min_interval = Duration::from_secs_f64(1.0 / target_fps as f64);
    let max_wait = Duration::from_secs_f64(1.0 / min_fps as f64);

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
        // 클라이언트 없으면 인코딩 건너뛰고 절전(idle CPU/GPU/배터리 절약).
        if !flags.client_active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if let Some(g) = slot.wait_new(&mut nv12, gen, max_wait) {
            gen = g;
        }
        let since = last_encode.elapsed();
        if since < min_interval {
            std::thread::sleep(min_interval - since);
        }
        last_encode = Instant::now();
        if flags.idr_req.swap(false, Ordering::Relaxed) {
            encoder.request_idr();
        }
        let rtp_ts = (start.elapsed().as_secs_f64() * 90_000.0) as u32;
        match encoder.encode(&nv12) {
            Ok(packets) => {
                for p in packets {
                    let ef = EncodedFrame { data: p.data, is_key_frame: p.is_key_frame, rtp_timestamp: rtp_ts };
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
