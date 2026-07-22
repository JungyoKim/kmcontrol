//! R3d 로컬 벤치: 캡처 → GPU 변환 → QSV 인코딩 파이프라인의 순수 처리량 측정.
//! Moonlight/네트워크 없이 병목(convert vs encode vs 캡처 도착)을 격리 계측.
//! 사용법: streamhost-pipeline-bench [target_w] [target_h] [fps] [seconds]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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

use kmc_streamhost::gpuconvert::GpuConverter;
use kmc_streamhost::qsv::QsvEncoder;

#[derive(Clone)]
struct BenchFlags {
    tw: u32,
    th: u32,
    fps: u32,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    conv_us: Arc<AtomicU64>,
    enc_us: Arc<AtomicU64>,
    arrive_us: Arc<AtomicU64>,
}

struct Bench {
    conv: Option<GpuConverter>,
    enc: QsvEncoder,
    f: BenchFlags,
    last: Instant,
}

impl GraphicsCaptureApiHandler for Bench {
    type Flags = BenchFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags;
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
        let mut enc = QsvEncoder::new(f.tw, f.th, f.fps, 20_000_000).map_err(|e| format!("qsv: {e}"))?;
        enc.request_idr();
        Ok(Self { conv: None, enc, f, last: Instant::now() })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, cc: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.f.stop.load(Ordering::Relaxed) { cc.stop(); return Ok(()); }
        // 프레임 도착 간격 (windows-capture 콜백 주기).
        let arrive = self.last.elapsed();
        self.last = Instant::now();

        if self.conv.is_none() {
            let dev: ID3D11Device = frame.device().clone();
            let cxt: ID3D11DeviceContext = frame.device_context().clone();
            self.conv = Some(GpuConverter::new(dev, cxt, frame.width(), frame.height(), self.f.tw, self.f.th)
                .map_err(|e| format!("conv init: {e}"))?);
        }
        let tex: ID3D11Texture2D = frame.as_raw_texture().clone();
        let t0 = Instant::now();
        let nv12 = self.conv.as_mut().unwrap().convert(&tex).map_err(|e| format!("conv: {e}"))?;
        let cus = t0.elapsed().as_micros() as u64;
        let t1 = Instant::now();
        let _pkts = self.enc.encode(nv12).map_err(|e| format!("enc: {e}"))?;
        let eus = t1.elapsed().as_micros() as u64;

        self.f.frames.fetch_add(1, Ordering::Relaxed);
        self.f.conv_us.fetch_add(cus, Ordering::Relaxed);
        self.f.enc_us.fetch_add(eus, Ordering::Relaxed);
        self.f.arrive_us.fetch_add(arrive.as_micros() as u64, Ordering::Relaxed);
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let mut a = std::env::args().skip(1);
    let tw: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(1920);
    let th: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(1080);
    let fps: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let secs: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    let stop = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let conv_us = Arc::new(AtomicU64::new(0));
    let enc_us = Arc::new(AtomicU64::new(0));
    let arrive_us = Arc::new(AtomicU64::new(0));
    let flags = BenchFlags { tw, th, fps, stop: stop.clone(), frames: frames.clone(),
        conv_us: conv_us.clone(), enc_us: enc_us.clone(), arrive_us: arrive_us.clone() };

    let handle = std::thread::spawn(move || {
        let monitor = Monitor::primary().expect("monitor");
        let settings = Settings::new(monitor, CursorCaptureSettings::WithCursor,
            DrawBorderSettings::Default, SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default, DirtyRegionSettings::Default,
            ColorFormat::Bgra8, flags);
        let _ = Bench::start(settings);
    });

    std::thread::sleep(std::time::Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(300));

    let n = frames.load(Ordering::Relaxed).max(1);
    println!("=== pipeline bench: {}x{} @ {}fps target, {}s ===", tw, th, fps, secs);
    println!("frames captured: {}  ({:.1} fps effective)", n, n as f64 / secs as f64);
    println!("avg frame-arrival interval: {:.2} ms  (=> capture supply {:.1} fps)",
        arrive_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0,
        1_000_000.0 / (arrive_us.load(Ordering::Relaxed) as f64 / n as f64));
    println!("avg GPU convert: {:.2} ms", conv_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0);
    println!("avg QSV encode:  {:.2} ms", enc_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0);
    let _ = handle;
    Ok(())
}
