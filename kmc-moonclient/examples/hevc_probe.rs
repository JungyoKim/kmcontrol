//! HEVC 협상 검증 프로브: 노트북 streamhost 에 admin 과 동일하게 연결해
//! allow_hevc=true 로 협상하고, 첫 AU 들의 NAL 타입을 분류해 실제 HEVC 인코딩을 확인한다.
//!
//! 실행: cargo run --release --example hevc_probe -- <laptop-tailnet-ip>

use anyhow::Result;
use kmc_moonclient::{conn, pair};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Annex-B 버퍼를 순회하며 NAL 타입을 수집. hevc=true 면 HEVC(2바이트 헤더), 아니면 H.264.
fn classify_nals(buf: &[u8], hevc: bool) -> Vec<u8> {
    let mut types = Vec::new();
    let n = buf.len();
    let mut i = 0;
    while i + 3 < n {
        // start code 00 00 01 또는 00 00 00 01
        let sc3 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        let sc4 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1;
        if sc3 || sc4 {
            let hdr = if sc4 { i + 4 } else { i + 3 };
            if hdr < n {
                let t = if hevc {
                    (buf[hdr] >> 1) & 0x3f
                } else {
                    buf[hdr] & 0x1f
                };
                types.push(t);
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    types
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let addr = std::env::args().nth(1).unwrap_or_else(|| "100.127.176.115".into());
    let (http, https) = (47989u16, 47984u16);

    let id_path = std::env::temp_dir().join("kmc-hevc-probe-identity.json");
    let identity = pair::Identity::load_or_generate(&id_path)?;

    println!("[probe] query_server_info {addr}:{http}/{https}");
    let info = pair::query_server_info(&identity, &addr, http, https)?;
    println!(
        "[probe] serverinfo: codec_mode_support={} current_game={}",
        info.codec_mode_support, info.current_game
    );

    let host = pair::PairedHost {
        address: addr.clone(),
        http_port: http,
        https_port: https,
        server_cert_pem: String::new(),
    };
    let (w, h, fps) = (1920u32, 1080u32, 60u32);
    let launch = pair::launch(&identity, &host, w, h, fps, info.current_game != 0)?;

    let (au_tx, au_rx) = mpsc::channel::<conn::AuFrame>();
    let (audio_tx, _audio_rx) = mpsc::channel::<Vec<u8>>();

    println!("[probe] start_stream allow_hevc=true …");
    let _session = conn::start_stream(&info, &host, &launch, w, h, fps, 20_000, au_tx, audio_tx, true)?;
    println!("[probe] connected. negotiated_codec = {}", conn::negotiated_codec());

    let hevc = conn::negotiated_codec() == "hevc";
    let deadline = Instant::now() + Duration::from_secs(6);
    let (mut frames, mut keyframes) = (0u32, 0u32);
    let mut seen: std::collections::BTreeSet<u8> = Default::default();
    let mut first_key_nals: Vec<u8> = Vec::new();

    while Instant::now() < deadline {
        match au_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(au) => {
                frames += 1;
                let nals = classify_nals(&au.data, hevc);
                for t in &nals {
                    seen.insert(*t);
                }
                if au.keyframe {
                    keyframes += 1;
                    if first_key_nals.is_empty() {
                        first_key_nals = nals;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }

    println!("\n===== HEVC PROBE RESULT =====");
    println!("negotiated_codec : {}", conn::negotiated_codec());
    println!("frames received  : {frames}  (keyframes {keyframes})");
    println!("NAL types seen   : {seen:?}");
    println!("first keyframe NALs: {first_key_nals:?}");
    if hevc {
        // HEVC: VPS=32 SPS=33 PPS=34 IDR_W_RADL=19 IDR_N_LP=20
        let has_hevc_params = seen.contains(&32) || seen.contains(&33) || seen.contains(&34);
        let has_hevc_idr = seen.contains(&19) || seen.contains(&20);
        println!(
            "VERDICT: {}",
            if has_hevc_params && has_hevc_idr {
                "PASS — real HEVC bitstream (VPS/SPS/PPS + IDR) from laptop streamhost"
            } else if has_hevc_params {
                "PASS(partial) — HEVC parameter sets present"
            } else {
                "INCONCLUSIVE — no HEVC param NALs observed"
            }
        );
    } else {
        println!("VERDICT: negotiated H.264 (server did not pick HEVC)");
    }
    Ok(())
}
