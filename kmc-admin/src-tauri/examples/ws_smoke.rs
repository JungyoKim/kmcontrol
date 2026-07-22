//! 헤드리스 스모크: StreamState.start → 로컬 WS 서버 → ws 클라이언트로 인코딩 AU 수신 확인.
//! 프론트(WebCodecs)가 받을 것과 동일한 바이트스트림([타입][Annex-B])이 흐르는지 증명한다.
use futures_util::StreamExt;
use kmc_admin_lib::stream::StreamState;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let st = StreamState::default();
    st.start("127.0.0.1", 1920, 1080, 60, None)?;
    let port = st.port().expect("ws port");
    println!("ws server port = {port}");

    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await?;
    let (_sink, mut stream) = ws.split();

    let mut frames = 0usize;
    let mut keyframes = 0usize;
    let mut total_bytes = 0usize;
    let mut first_bytes = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while std::time::Instant::now() < deadline && frames < 120 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                frames += 1;
                total_bytes += b.len();
                let key = b.first() == Some(&1u8);
                if key {
                    keyframes += 1;
                }
                if first_bytes.len() < 12 {
                    first_bytes.push(if key { 'K' } else { 'p' });
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    println!(
        "recv frames={frames}, keyframes={keyframes}, total={total_bytes} bytes, seq={:?}",
        first_bytes.iter().collect::<String>()
    );
    println!(
        "→ avg {:.1} KB/frame; keyframe present = {}",
        if frames > 0 { total_bytes as f64 / frames as f64 / 1024.0 } else { 0.0 },
        keyframes > 0
    );
    st.stop();
    Ok(())
}
