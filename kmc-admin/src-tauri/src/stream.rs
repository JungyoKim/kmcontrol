//! admin 스트림 클라이언트: 호스트에 붙어 인코딩된 H.264 access unit을 받아
//! 로컬 WebSocket으로 프론트에 순서대로 전달한다. 디코드는 프론트(WebCodecs)가 GPU로 수행한다.
//!
//! 와이어 계약: WS 바이너리 메시지 = `[1바이트 프레임타입: 1=key/0=delta][Annex-B H.264]`.
//! 프론트는 stream_port()로 포트를 받아 `ws://127.0.0.1:PORT`에 붙는다.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use kmc_moonclient::{conn, pair, AuFrame, Identity};
use parking_lot::Mutex;
use tokio::sync::broadcast;

/// 활성 스트림 상태.
#[derive(Default)]
pub struct StreamState {
    session: Mutex<Option<conn::StreamSession>>,
    /// 비디오 WS 클라이언트로 팬아웃할 브로드캐스트(한 번 만들어 재사용).
    bcast: Mutex<Option<broadcast::Sender<Arc<Vec<u8>>>>>,
    /// 비디오 로컬 WS 서버 포트(한 번 바인딩 후 고정).
    port: Mutex<Option<u16>>,
    /// 오디오(Opus) WS 팬아웃 브로드캐스트.
    audio_bcast: Mutex<Option<broadcast::Sender<Arc<Vec<u8>>>>>,
    /// 오디오 로컬 WS 서버 포트.
    audio_port: Mutex<Option<u16>>,
}

pub type SharedStream = Arc<StreamState>;

fn identity_path() -> PathBuf {
    let dir = dirs_config().unwrap_or_else(|| PathBuf::from("."));
    dir.join("kmc-admin-client-identity.json")
}

fn dirs_config() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("kmcontrol"))
}

impl StreamState {
    /// 스트림 시작. paired 상태면 재연결, 아니면 PIN 페어링.
    pub fn start(&self, address: &str, width: u32, height: u32, fps: u32, pin: Option<String>) -> Result<()> {
        // 이전 세션 정리.
        self.stop();

        // WS 서버(비디오+오디오 팬아웃)를 먼저 확보 — 프론트가 포트로 붙을 수 있어야 한다.
        self.ensure_server()?;
        let bcast = self
            .bcast
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("video ws server not initialized"))?;
        let audio_bcast = self
            .audio_bcast
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("audio ws server not initialized"))?;

        let id_path = identity_path();
        if let Some(parent) = id_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let identity = Identity::load_or_generate(&id_path)?;
        let (http, https) = (47989u16, 47984u16);

        let info = pair::query_server_info(&identity, address, http, https)?;
        // 페어링 제거: 호스트가 모든 클라를 신뢰하므로 address/포트만으로 바로 launch한다.
        // (rikey는 launch 시 클라가 생성; 서버 인증서 검증은 danger_accept_invalid_certs로 스킵.)
        let _ = &pin; // 더 이상 PIN 불필요.
        let host = pair::PairedHost {
            address: address.to_string(),
            http_port: http,
            https_port: https,
            server_cert_pem: String::new(),
        };

        let launch = pair::launch(&identity, &host, width, height, fps, info.current_game != 0)?;

        // moonclient는 인코딩 AU/Opus를 std mpsc로 보낸다(tokio 비의존). 여기서 드레인해 브로드캐스트로 팬아웃.
        let (au_tx, au_rx) = std::sync::mpsc::channel::<AuFrame>();
        std::thread::spawn(move || {
            // tx가 dr_cleanup에서 드롭되면 recv가 끝나 스레드 종료.
            for au in au_rx {
                let mut framed = Vec::with_capacity(1 + au.data.len());
                framed.push(if au.keyframe { 1u8 } else { 0u8 });
                framed.extend_from_slice(&au.data);
                let _ = bcast.send(Arc::new(framed));
            }
        });
        // Opus 오디오 프레임 팬아웃 (프레임 = Opus 패킷 그대로).
        let (audio_tx, audio_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for opus in audio_rx {
                let _ = audio_bcast.send(Arc::new(opus));
            }
        });

        let session = conn::start_stream(&info, &host, &launch, width, height, fps, 20_000, au_tx, audio_tx)?;
        *self.session.lock() = Some(session);
        Ok(())
    }

    /// 스트림 종료 (StreamSession drop → LiStopConnection). WS 서버는 유지(재연결 재사용).
    pub fn stop(&self) {
        *self.session.lock() = None;
    }

    /// 비디오 로컬 WS 서버 포트. start() 이후 유효.
    pub fn port(&self) -> Option<u16> {
        *self.port.lock()
    }

    /// 오디오 로컬 WS 서버 포트. start() 이후 유효.
    pub fn audio_port(&self) -> Option<u16> {
        *self.audio_port.lock()
    }

    /// 비디오+오디오 WS 팬아웃 서버를 최초 1회 바인딩한다. 이후 포트/브로드캐스트 재사용.
    fn ensure_server(&self) -> Result<()> {
        if self.port.lock().is_some() {
            return Ok(());
        }
        // 비디오: 새 클라이언트 연결 시 IDR 요청(디코더 키프레임 동기).
        let (v_tx, v_port) = spawn_ws_server(true).context("bind video ws server")?;
        // 오디오: IDR 무관.
        let (a_tx, a_port) = spawn_ws_server(false).context("bind audio ws server")?;
        *self.port.lock() = Some(v_port);
        *self.bcast.lock() = Some(v_tx);
        *self.audio_port.lock() = Some(a_port);
        *self.audio_bcast.lock() = Some(a_tx);
        Ok(())
    }
}

/// WS 팬아웃 서버 1개를 127.0.0.1:임의포트에 바인딩하고 (브로드캐스트 sender, 포트)를 반환.
/// `idr_on_connect`면 새 클라이언트 연결/랙 시 conn::request_idr()로 키프레임을 요청(비디오용).
fn spawn_ws_server(idr_on_connect: bool) -> Result<(broadcast::Sender<Arc<Vec<u8>>>, u16)> {
    let (tx, _rx) = broadcast::channel::<Arc<Vec<u8>>>(512);
    let server_tx = tx.clone();
    let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("stream ws runtime build failed: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0u16)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("stream ws bind failed: {e}");
                    return;
                }
            };
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            let _ = port_tx.send(port);
            eprintln!("stream ws server listening on 127.0.0.1:{port} (idr={idr_on_connect})");

            loop {
                let (sock, _peer) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let sub = server_tx.subscribe();
                tokio::spawn(async move { serve_client(sock, sub, idr_on_connect).await });
            }
        });
    });

    let port = port_rx.recv().context("ws server failed to bind")?;
    Ok((tx, port))
}

/// 한 WS 클라이언트를 서비스: 브로드캐스트 프레임을 바이너리로 흘리고, 랙/닫힘을 처리한다.
async fn serve_client(
    sock: tokio::net::TcpStream,
    mut sub: broadcast::Receiver<Arc<Vec<u8>>>,
    idr_on_connect: bool,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let ws = match tokio_tungstenite::accept_async(sock).await {
        Ok(w) => w,
        Err(_) => return,
    };
    let (mut sink, mut stream) = ws.split();

    // 비디오: 새 클라이언트 → 키프레임부터 시작하도록 IDR 요청.
    if idr_on_connect {
        conn::request_idr();
    }

    loop {
        tokio::select! {
            msg = sub.recv() => match msg {
                Ok(bytes) => {
                    if sink.send(Message::binary(bytes.as_ref().clone())).await.is_err() {
                        break;
                    }
                }
                // 랙으로 프레임을 놓치면 디코더 동기가 깨지므로 IDR 재요청(비디오만).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if idr_on_connect {
                        conn::request_idr();
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            inc = stream.next() => match inc {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}
