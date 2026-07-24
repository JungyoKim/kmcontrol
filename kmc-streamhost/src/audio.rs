//! 오디오 스트림: WASAPI 루프백 캡처 → Opus 인코딩 → RTP(payload type 97) → UDP 48000.
//!
//! Moonlight 클라이언트가 오디오 포트로 "PING"을 보내면 그 주소를 등록하고, 이후 인코딩된
//! Opus 프레임을 RTP로 감싸 송출한다. FEC(RS 4,2)는 손실 복구용이라 무손실 LAN/로컬에선
//! 데이터 샤드만 보내도 재생된다 → 데이터 패킷만 순번대로 전송.
//!
//! 오디오 파라미터(우리 클라이언트 협상값과 일치): 48kHz, 스테레오, 5ms 프레임(240 samples/ch).

use std::net::SocketAddr;
use anyhow::{anyhow, Context, Result};
use tokio::net::UdpSocket;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_MS: u32 = 5;
const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize; // 240/ch
const RTP_HEADER_LEN: usize = 12;
const AUDIO_PAYLOAD_TYPE: u8 = 97;

/// 오디오 스트림을 spawn한다. 캡처+인코딩은 전용 std 스레드(WASAPI는 COM/이벤트 기반),
/// UDP 송출은 tokio 태스크. 지속 파이프라인: 프로세스 수명 내내 유지.
pub async fn start(bind_ip: &str, port: u16) -> Result<()> {
    let addr: SocketAddr = format!("{bind_ip}:{port}").parse().context("parse audio addr")?;
    let socket = UdpSocket::bind(addr).await.context("bind audio udp")?;
    tracing::info!(%addr, "audio UDP listening (waiting for client PING)");

    // 캡처+인코딩 스레드 → Opus 프레임을 tokio mpsc로 보냄(sync 컨텍스트에서 UnboundedSender 사용 가능).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            // 캡처 루프가 죽으면(장치 변경/절전으로 WASAPI 이벤트 실패 등) 자동 재초기화.
            // 그냥 두면 오디오만 영구 무음이 되고 pipeline_started 가드 때문에 재시작도 안 되므로,
            // 여기서 감시·재시도해 스스로 복구한다. tx(수신자)가 살아있는 한 계속 시도.
            loop {
                match capture_encode_loop(tx.clone()) {
                    Ok(()) => {
                        // 정상 종료 = 수신자(tx) 드롭 → 스트림 자체가 끝난 것. 감시 종료.
                        tracing::info!("audio capture loop ended normally (receiver gone)");
                        break;
                    }
                    Err(e) => {
                        if tx.is_closed() {
                            break; // 수신자 없음 → 재시도 무의미.
                        }
                        tracing::warn!(error=%e, "audio capture loop died — restarting in 1s");
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        })
        .context("spawn audio capture thread")?;

    tokio::spawn(async move {
        let mut client_addr: Option<SocketAddr> = None;
        let mut recv_buf = [0u8; 256];
        let mut seq: u16 = 0;
        let mut timestamp: u32 = 0;
        let ssrc: u32 = 0;
        let mut last_ping = std::time::Instant::now();
        const OWNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        loop {
            tokio::select! {
                // 클라이언트 PING 수신 → 주소 등록.
                r = socket.recv_from(&mut recv_buf) => match r {
                    Ok((len, addr)) => {
                        if &recv_buf[..len] == b"PING" {
                            let owner_active = client_addr.is_some()
                                && client_addr != Some(addr)
                                && last_ping.elapsed() < OWNER_TIMEOUT;
                            if !owner_active {
                                if client_addr != Some(addr) {
                                    tracing::info!(%addr, "audio client registered via PING");
                                    seq = 0;
                                    timestamp = 0;
                                }
                                client_addr = Some(addr);
                                last_ping = std::time::Instant::now();
                            }
                        }
                    }
                    Err(e) => {
                        // WSAECONNRESET(10054): 클라이언트 수신 중단 → 송출 정지, 재-PING까지 대기.
                        if e.raw_os_error() == Some(10054) && client_addr.is_some() {
                            tracing::info!("audio client stopped receiving; pausing until re-PING");
                            client_addr = None;
                        }
                    }
                },
                // Opus 프레임 → RTP(type 97) → 송출.
                frame = rx.recv() => {
                    let Some(frame) = frame else {
                        tracing::debug!("audio frame channel closed");
                        break;
                    };
                    timestamp = timestamp.wrapping_add(SAMPLES_PER_FRAME as u32);
                    let Some(dst) = client_addr else {
                        continue; // 아직 PING 없음 — 드롭.
                    };
                    let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + frame.len());
                    pkt.push(0x80); // version 2.
                    pkt.push(AUDIO_PAYLOAD_TYPE);
                    pkt.extend_from_slice(&seq.to_be_bytes());
                    pkt.extend_from_slice(&timestamp.to_be_bytes());
                    pkt.extend_from_slice(&ssrc.to_be_bytes());
                    pkt.extend_from_slice(&frame);
                    seq = seq.wrapping_add(1);
                    if let Err(e) = socket.send_to(&pkt, dst).await {
                        tracing::warn!(error=%e, "audio send failed");
                    }
                }
            }
        }
    });

    Ok(())
}

/// WASAPI 루프백 캡처 → 240샘플 프레임 누적 → Opus 인코딩 → tx.
fn capture_encode_loop(tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> Result<()> {
    use audiopus::{coder::Encoder, Application, Channels, SampleRate};
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

    wasapi::initialize_mta()
        .ok()
        .map_err(|e| anyhow!("wasapi COM init: {e:?}"))?;

    // 기본 출력 장치를 루프백(Render 장치를 Capture 방향으로)으로 연다.
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|e| anyhow!("wasapi device enumerator: {e:?}"))?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|e| anyhow!("get default render device: {e:?}"))?;
    let mut audio_client = device
        .get_iaudioclient()
        .map_err(|e| anyhow!("get audio client: {e:?}"))?;
    // autoconvert로 48kHz/스테레오/i16 강제(믹스 포맷과 달라도 WASAPI가 변환).
    let format = WaveFormat::new(16, 16, &SampleType::Int, SAMPLE_RATE as usize, CHANNELS, None);
    let (_def, min_time) = audio_client
        .get_device_period()
        .map_err(|e| anyhow!("get device period: {e:?}"))?;
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: min_time };
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| anyhow!("initialize loopback client: {e:?}"))?;

    let h_event = audio_client
        .set_get_eventhandle()
        .map_err(|e| anyhow!("get event handle: {e:?}"))?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|e| anyhow!("get capture client: {e:?}"))?;
    audio_client
        .start_stream()
        .map_err(|e| anyhow!("start stream: {e:?}"))?;
    tracing::info!("audio WASAPI loopback capture started (48kHz stereo, Opus 5ms)");

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::LowDelay)
        .map_err(|e| anyhow!("opus encoder: {e:?}"))?;

    let block_align = CHANNELS * 2; // i16 스테레오 = 4바이트/프레임.
    let frame_bytes = SAMPLES_PER_FRAME * block_align; // 240 × 4 = 960바이트.
    let mut queue: std::collections::VecDeque<u8> = std::collections::VecDeque::with_capacity(frame_bytes * 16);
    let mut pcm = vec![0i16; SAMPLES_PER_FRAME * CHANNELS];
    let mut out = vec![0u8; 4000];

    loop {
        // 캡처된 바이트를 큐에 적재.
        if let Err(e) = capture_client.read_from_device_to_deque(&mut queue) {
            tracing::warn!(error=%format!("{e:?}"), "audio read failed");
        }
        // 프레임 단위로 Opus 인코딩.
        while queue.len() >= frame_bytes {
            for s in pcm.iter_mut() {
                let lo = queue.pop_front().unwrap();
                let hi = queue.pop_front().unwrap();
                *s = i16::from_le_bytes([lo, hi]);
            }
            match encoder.encode(&pcm, &mut out) {
                Ok(n) => {
                    if tx.send(out[..n].to_vec()).is_err() {
                        return Ok(()); // 수신 종료.
                    }
                }
                Err(e) => tracing::warn!(error=%format!("{e:?}"), "opus encode failed"),
            }
        }
        if h_event.wait_for_event(1_000_000).is_err() {
            // 이벤트 대기 실패 = 장치가 바뀌었거나 클라이언트가 죽음. Err 로 반환해 감시 스레드가
            // 장치를 다시 열어 재개하도록 한다(Ok 로 끝내면 영구 무음).
            let _ = audio_client.stop_stream();
            return Err(anyhow!("audio capture event wait failed (device changed?)"));
        }
    }
}
