//! 헤드리스 오디오 스모크: 세션 수립 후 오디오 WS에 붙어 Opus 프레임이 흐르는지 확인.
//! 전체 경로 증명: WASAPI 루프백 캡처 → Opus 인코딩 → RTP(48000) → moonlight 수신 →
//! decodeAndPlaySample 콜백 → admin WS 팬아웃 → (프론트 WebCodecs가 받을) Opus 바이트.
//! 무음이어도 WASAPI 루프백은 프레임을 생성하므로 프레임 수신 자체로 경로가 증명된다.
use futures_util::StreamExt;
use kmc_admin_lib::stream::StreamState;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let st = StreamState::default();
    st.start("127.0.0.1", 1920, 1080, 60, None)?;
    let port = st.audio_port().expect("audio ws port");
    println!("audio ws port = {port}");

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await?;
    let (_sink, mut stream) = ws.split();

    let mut frames = 0usize;
    let mut total = 0usize;
    let mut sizes = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && frames < 100 {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                frames += 1;
                total += b.len();
                if sizes.len() < 6 {
                    sizes.push(b.len());
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    println!("recv opus frames={frames}, total={total} bytes, first sizes={sizes:?}");
    println!(
        "→ audio path working = {}; avg {:.0} bytes/frame",
        frames > 0,
        if frames > 0 { total as f64 / frames as f64 } else { 0.0 }
    );
    st.stop();
    Ok(())
}
