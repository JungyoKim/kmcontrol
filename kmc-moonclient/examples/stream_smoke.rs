//! 순수 Rust 클라이언트 E2E 스모크: 실제 `kmc-streamhost` 에 붙어 비디오/오디오를 받는다.
//!
//! ```text
//! streamhost-host-test                          # 다른 창에서 호스트 기동
//! cargo run --example stream_smoke 127.0.0.1 10
//! ```
//!
//! 검증 항목(하나라도 실패하면 exit 1):
//! - AU 가 도착하고 **키프레임이 최소 1장** 있다 (FEC 재조립 + Annex-B 프레이밍이 살아있다는 뜻).
//! - 모든 AU 가 self-framed 이다 (`data[0]` 이 0/1, `data[1..]` 는 Annex-B 스타트코드로 시작).
//! - Opus 프레임이 도착한다 (오디오 PING 등록 + RTP 파싱).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use kmc_moonclient::{pair, AuFrame, Identity};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_moonclient=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    // 포트 베이스. GameStream 관례는 47989(http)/47984(https)/48010(rtsp) 이지만, 같은 머신에
    // Sunshine 등이 떠 있으면 비켜서 띄워야 한다 - 호스트도 HostConfig 로 옮길 수 있다.
    let http: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(47989);
    let https: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(47984);
    let rtsp_port: u16 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48010);
    let allow_hevc = std::env::var("KMC_HEVC").is_ok();

    let id_path = std::env::temp_dir().join("kmc-smoke-identity.json");
    let identity = Identity::load_or_generate(&id_path)?;
    tracing::info!(http, https, rtsp_port, "target ports");

    let info = pair::query_server_info(&identity, &address, http, https)
        .context("query_server_info (호스트가 떠 있는가?)")?;
    tracing::info!(
        app_version = %info.app_version,
        codec_mode_support = info.codec_mode_support,
        "serverinfo ok"
    );

    let host = pair::PairedHost {
        address: address.clone(),
        http_port: http,
        https_port: https,
        rtsp_port,
        server_cert_pem: String::new(),
    };
    let launch = pair::launch(&identity, &host, 1280, 720, 60, info.current_game != 0)
        .context("launch")?;

    let (au_tx, au_rx) = mpsc::channel::<AuFrame>();
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>();

    let session = kmc_moonclient::start_stream(
        &info, &host, &launch, 1280, 720, 60, 10_000, au_tx, audio_tx, allow_hevc,
    )
    .context("start_stream")?;
    tracing::info!(codec = kmc_moonclient::negotiated_codec(), "stream started");

    let hevc = kmc_moonclient::negotiated_codec() == "hevc";
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut frames, mut keyframes, mut bytes, mut malformed) = (0u32, 0u32, 0usize, 0u32);
    let mut first_frame: Option<Duration> = None;
    let mut nal_types: std::collections::BTreeSet<u8> = Default::default();
    let started = Instant::now();

    while Instant::now() < deadline {
        match au_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(au) => {
                if first_frame.is_none() {
                    first_frame = Some(started.elapsed());
                }
                frames += 1;
                bytes += au.data.len();
                if au.keyframe {
                    keyframes += 1;
                }
                if !is_self_framed(&au) {
                    malformed += 1;
                }
                nal_types.extend(classify_nals(&au.data[1..], hevc));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let audio_frames = audio_rx.try_iter().count();
    drop(session);

    let elapsed = started.elapsed().as_secs_f64();
    println!("--- {secs}s @ {address} ---");
    println!("video : {frames} AU ({keyframes} key), {bytes} B, {:.1} fps, {:.2} Mbps",
        f64::from(frames) / elapsed,
        (bytes as f64 * 8.0) / elapsed / 1e6);
    println!("audio : {audio_frames} opus frames");
    println!("first frame: {first_frame:?}");
    println!("NAL types  : {nal_types:?}  (codec {})", if hevc { "hevc" } else { "h264" });
    println!("termination: {:?}", kmc_moonclient::last_termination());

    if frames == 0 {
        bail!("no access units received");
    }
    if keyframes == 0 {
        bail!("no keyframe received ({frames} delta AUs) - decoder could never start");
    }
    if malformed != 0 {
        bail!("{malformed}/{frames} AUs were not self-framed Annex-B");
    }
    if audio_frames == 0 {
        bail!("no opus frames received");
    }
    // 파라미터 세트가 없으면 디코더를 초기화할 수 없다: H.264 SPS=7/PPS=8, HEVC VPS=32/SPS=33/PPS=34.
    let params: &[u8] = if hevc { &[32, 33, 34] } else { &[7, 8] };
    if !params.iter().any(|t| nal_types.contains(t)) {
        bail!("no parameter-set NALs ({params:?}) in {nal_types:?} - decoder cannot initialize");
    }
    println!("OK");
    Ok(())
}

/// `data[0]` 은 타입 바이트(1=key/0=delta), 이후는 Annex-B 스타트코드여야 한다.
/// admin 은 이 버퍼를 재가공 없이 브라우저로 넘기므로 여기서 깨지면 프론트가 디코드하지 못한다.
fn is_self_framed(au: &AuFrame) -> bool {
    let Some((&ty, rest)) = au.data.split_first() else {
        return false;
    };
    if ty != u8::from(au.keyframe) {
        return false;
    }
    rest.starts_with(&[0, 0, 0, 1]) || rest.starts_with(&[0, 0, 1])
}

/// Annex-B 버퍼를 순회하며 NAL 타입을 수집. hevc=true 면 HEVC(2바이트 헤더), 아니면 H.264.
fn classify_nals(buf: &[u8], hevc: bool) -> Vec<u8> {
    let mut types = Vec::new();
    let n = buf.len();
    let mut i = 0;
    while i + 3 < n {
        let sc3 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        let sc4 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1;
        if sc3 || sc4 {
            let hdr = if sc4 { i + 4 } else { i + 3 };
            if hdr < n {
                types.push(if hevc { (buf[hdr] >> 1) & 0x3f } else { buf[hdr] & 0x1f });
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    types
}
