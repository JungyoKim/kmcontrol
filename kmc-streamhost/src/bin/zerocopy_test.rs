//! Zero-copy GPU 인코더 로컬 성능 테스트 (dev PC Intel 어댑터).
//! NV12 D3D11 텍스처를 만들어 ZeroCopyEncoder 로 반복 인코딩, fps/비트레이트 측정.
//! 사용: streamhost-zerocopy-test [adapter_index] [w] [h] [codec]

use anyhow::{anyhow, Result};
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

use kmc_streamhost::gpu_encode::ZeroCopyEncoder;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let args: Vec<String> = std::env::args().collect();
    let adapter_idx: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let w: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2880);
    let h: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1800);
    let codec = args.get(4).map(|s| s.as_str()).unwrap_or("h264_qsv");

    unsafe {
        // 지정 어댑터로 D3D11 디바이스 생성.
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter = factory.EnumAdapters(adapter_idx)?;
        let desc = adapter.GetDesc()?;
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        println!("adapter {adapter_idx}: {name}");

        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device = device.ok_or_else(|| anyhow!("no device"))?;
        let ctx = ctx.ok_or_else(|| anyhow!("no ctx"))?;

        // 소스 NV12 텍스처(내용 무관, 성능용).
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut nv12: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12))?;
        let nv12 = nv12.ok_or_else(|| anyhow!("no nv12 tex"))?;

        println!("creating encoder {codec} {w}x{h}...");
        let mut enc = ZeroCopyEncoder::new(codec, &device, &ctx, w, h, 120, 30_000_000)?;

        // 워밍업 + 측정.
        let frames = 600u32;
        let mut total_bytes = 0usize;
        let mut key = 0u32;
        let start = Instant::now();
        for i in 0..frames {
            let pkts = enc.encode(&nv12, i == 0)?;
            for p in &pkts {
                total_bytes += p.data.len();
                if p.is_key_frame {
                    key += 1;
                }
            }
        }
        let secs = start.elapsed().as_secs_f64();
        let fps = frames as f64 / secs;
        let mbps = (total_bytes as f64 * 8.0) / secs / 1e6;
        println!("\n===== ZERO-COPY ENCODE RESULT =====");
        println!("{codec} {w}x{h}: {frames} frames in {secs:.2}s");
        println!("ENCODE FPS: {fps:.1}");
        println!("bitrate: {mbps:.1} Mbps, keyframes: {key}, total: {total_bytes} bytes");
    }
    Ok(())
}
