//! R3b 검증: 합성 프레임을 H.264로 인코딩해 NAL 출력을 확인.

use anyhow::Result;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_streamhost=debug".into()),
        )
        .init();

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    kmc_streamhost::encoder::mf_startup()?;

    let (w, h, fps) = (1280u32, 720u32, 60u32);
    let mut enc = kmc_streamhost::encoder::H264Encoder::new_software(w, h, fps, 8_000_000)?;
    tracing::info!(w, h, fps, "software H264 encoder ready");

    // 합성 BGRA 프레임(그라디언트) → NV12 → 인코딩. 여러 프레임 투입해 IDR + 후속 프레임 확보.
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut nv12 = Vec::new();
    let mut total_nal = 0usize;
    let mut key_frames = 0usize;
    let mut out_file: Vec<u8> = Vec::new();
    let frame_dur = 10_000_000i64 / fps as i64;

    for f in 0..30i64 {
        // 프레임마다 색이 변하는 그라디언트.
        for y in 0..h as usize {
            for x in 0..w as usize {
                let idx = (y * w as usize + x) * 4;
                bgra[idx] = ((x + f as usize * 4) & 0xff) as u8; // B
                bgra[idx + 1] = ((y + f as usize * 2) & 0xff) as u8; // G
                bgra[idx + 2] = ((x + y) & 0xff) as u8; // R
                bgra[idx + 3] = 255;
            }
        }
        kmc_streamhost::encoder::bgra_to_nv12(&bgra, w as usize, h as usize, &mut nv12);
        let time = f * frame_dur;
        let packets = enc.encode_nv12(&nv12, time, frame_dur)?;
        for p in &packets {
            total_nal += p.data.len();
            if p.is_key_frame {
                key_frames += 1;
            }
            out_file.extend_from_slice(&p.data);
        }
    }

    std::fs::write("encoder-out.h264", &out_file)?;
    tracing::info!(total_nal_bytes = total_nal, key_frames, file_bytes = out_file.len(), "encoding smoke test complete → encoder-out.h264");
    if total_nal == 0 {
        anyhow::bail!("encoder produced no NAL data");
    }
    Ok(())
}
