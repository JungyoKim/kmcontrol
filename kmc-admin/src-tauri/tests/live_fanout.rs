//! admin 팬아웃 라이브 검증: 프론트가 실제로 붙는 경로(`StreamState::start` → 순수 Rust
//! 클라이언트 → broadcast → 로컬 WS)를 UI 없이 그대로 태운다. WebCodecs 는 이 WS 에서 온
//! 바이트를 그대로 먹으므로, 여기서 프레이밍이 맞으면 프론트에서도 맞는다.
//!
//! 실제 호스트가 필요하므로 기본은 skip. 실행:
//! ```text
//! KMC_BASE_PORT=48989 streamhost-host-test
//! KMC_LIVE_HOST=127.0.0.1:48989 cargo test --test live_fanout -- --nocapture
//! ```

use futures_util::StreamExt;
use kmc_admin_lib::stream::StreamState;
use std::time::Duration;

/// AU 한 장의 계약: `data[0]` 이 타입 바이트(1=key/0=delta), 이후는 Annex-B 스타트코드.
fn framing_ok(buf: &[u8]) -> bool {
    matches!(buf.split_first(), Some((&ty, rest))
        if (ty == 0 || ty == 1) && (rest.starts_with(&[0, 0, 0, 1]) || rest.starts_with(&[0, 0, 1])))
}

#[test]
fn fanout_delivers_decodable_aus_over_the_frontend_websocket() {
    let Ok(host) = std::env::var("KMC_LIVE_HOST") else {
        eprintln!("skip: set KMC_LIVE_HOST=<addr[:base_port]> with a streamhost running");
        return;
    };

    let state = StreamState::default();
    state
        .start(&host, 1280, 720, 60, None, false)
        .expect("start stream");
    let video_port = state.port().expect("video ws port");
    let audio_port = state.audio_port().expect("audio ws port");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let (video, audio) = rt.block_on(async {
        tokio::join!(
            collect(video_port, Duration::from_secs(6), 30),
            collect(audio_port, Duration::from_secs(6), 30),
        )
    });
    state.stop();

    // 비디오: 프레임이 오고, 전부 self-framed 이며, 키프레임이 최소 1장(디코더 시작 가능).
    assert!(!video.is_empty(), "no AU reached the video websocket");
    let malformed = video.iter().filter(|b| !framing_ok(b)).count();
    assert_eq!(malformed, 0, "{malformed}/{} AUs broke the framing contract", video.len());
    let keyframes = video.iter().filter(|b| b[0] == 1).count();
    assert!(keyframes > 0, "no keyframe in {} AUs - decoder could never start", video.len());

    // 오디오: Opus 프레임이 온다(무음이어도 WASAPI 루프백은 프레임을 낸다 —
    // 0 이면 호스트에서 아무것도 재생 중이 아니라는 뜻이므로 진단만 남긴다).
    if audio.is_empty() {
        eprintln!("warning: no opus frames - is anything playing on the host?");
    }
    println!("video {} AU ({keyframes} key), audio {} frames", video.len(), audio.len());
}

/// WS 에 붙어 바이너리 메시지를 `limit` 개 또는 `budget` 시간까지 모은다.
async fn collect(port: u16, budget: Duration, limit: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Ok(Ok((mut ws, _))) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")),
    )
    .await
    else {
        return out;
    };
    let deadline = tokio::time::Instant::now() + budget;
    while out.len() < limit {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b)))) => out.push(b),
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    out
}
