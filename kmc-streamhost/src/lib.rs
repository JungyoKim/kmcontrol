//! kmc-streamhost — Windows GameStream 호스트 (R1: 캡처→인코딩 검증 단계).
//!
//! R1 범위: `windows-capture`(Graphics Capture API + Media Foundation 하드웨어 인코더)로
//! 주 모니터를 캡처해 H.264 mp4로 인코딩. Intel iGPU에서는 MF H.264 인코더가 자동으로
//! QSV(Quick Sync) 하드웨어 경로를 사용한다. 외부 의존성(ffmpeg/libclang) 없음 — OS 내장.
//!
//! 이후 단계(R3)에서 파일 저장 대신 raw H.264 NAL 출력 + RTP 패킷화로 확장한다.

pub mod crypto;
pub mod clients;
pub mod state;
pub mod tls;
pub mod webserver;
pub mod rtsp;
pub mod session;
pub mod video;
pub mod control;
pub mod encoder;
pub mod qsv;
pub mod gpuconvert;
pub mod capture;
pub mod input;
pub mod audio;
pub mod host;

use std::time::Instant;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

/// 캡처 핸들러로 전달되는 설정 플래그.
#[derive(Clone)]
pub struct RecordFlags {
    pub width: u32,
    pub height: u32,
    pub output_path: String,
    pub duration_secs: u64,
}

/// 고정 시간 동안 주 모니터를 캡처해 mp4로 저장하는 핸들러 (R1 검증용).
pub struct FileRecorder {
    encoder: Option<VideoEncoder>,
    start: Instant,
    duration_secs: u64,
    frames: u64,
}

impl GraphicsCaptureApiHandler for FileRecorder {
    type Flags = RecordFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags;
        tracing::info!(
            width = f.width,
            height = f.height,
            output = %f.output_path,
            duration = f.duration_secs,
            "initializing hardware video encoder (MF/QSV)"
        );
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(f.width, f.height),
            AudioSettingsBuilder::default().disabled(true),
            ContainerSettingsBuilder::default(),
            &f.output_path,
        )?;

        Ok(Self {
            encoder: Some(encoder),
            start: Instant::now(),
            duration_secs: f.duration_secs,
            frames: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        self.frames += 1;
        if let Some(enc) = self.encoder.as_mut() {
            enc.send_frame(frame)?;
        }

        if self.start.elapsed().as_secs() >= self.duration_secs {
            if let Some(enc) = self.encoder.take() {
                enc.finish()?;
            }
            let elapsed = self.start.elapsed().as_secs_f64();
            tracing::info!(
                frames = self.frames,
                elapsed_secs = elapsed,
                fps = self.frames as f64 / elapsed,
                "capture finished; encoder flushed"
            );
            capture_control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::warn!("capture item closed");
        Ok(())
    }
}

/// 주 모니터를 `duration_secs`초 동안 캡처해 `output_path`(mp4)로 저장한다.
/// 현재 스레드를 점유하며, 완료 시 반환.
pub fn record_primary_monitor(output_path: &str, duration_secs: u64) -> anyhow::Result<()> {
    let monitor = Monitor::primary().map_err(|e| anyhow::anyhow!("get primary monitor: {e}"))?;
    let (width, height) = monitor_resolution(&monitor)?;
    tracing::info!(width, height, "primary monitor selected");

    let flags = RecordFlags {
        width,
        height,
        output_path: output_path.to_string(),
        duration_secs,
    };

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        flags,
    );

    FileRecorder::start(settings).map_err(|e| anyhow::anyhow!("capture failed: {e}"))?;
    Ok(())
}

/// 모니터 해상도를 짝수로 맞춰 반환 (H.264 인코더는 짝수 치수를 요구).
fn monitor_resolution(monitor: &Monitor) -> anyhow::Result<(u32, u32)> {
    let width = monitor.width().map_err(|e| anyhow::anyhow!("monitor width: {e}"))?;
    let height = monitor.height().map_err(|e| anyhow::anyhow!("monitor height: {e}"))?;
    Ok((width & !1, height & !1))
}
