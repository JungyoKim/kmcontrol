//! 스트리밍 호스트 오케스트레이션: 인증서 로드/생성 → 웹서버(HTTP/HTTPS) + RTSP 기동.
//!
//! R2: 페어링 + serverinfo + RTSP 협상까지. Moonlight 클라이언트가 이 호스트를 발견·페어링하고
//! 스트림 파라미터를 협상하는 지점까지 동작. 실제 미디어 송출은 R3.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::clients::ClientManager;
use crate::crypto;
use crate::rtsp::{RtspServer, StreamPorts};
use crate::webserver::{ServerConfig, Webserver};

/// 호스트 실행 설정.
pub struct HostConfig {
    pub name: String,
    pub bind_ip: String,
    pub http_port: u16,
    pub https_port: u16,
    pub rtsp_port: u16,
    /// 서버 인증서 PEM 경로 (없으면 생성).
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// 페어링 클라이언트 영속 상태 파일.
    pub state_path: PathBuf,
    /// 서버 고유 id (serverinfo).
    pub unique_id: String,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            name: "KMC Streamhost".into(),
            bind_ip: "0.0.0.0".into(),
            http_port: 47989,
            https_port: 47984,
            rtsp_port: 48010,
            cert_path: "streamhost-cert.pem".into(),
            key_path: "streamhost-key.pem".into(),
            state_path: "streamhost-clients.json".into(),
            unique_id: "0123456789ABCDEF".into(),
        }
    }
}

/// 서버 인증서/키 로드, 없으면 생성해 저장.
fn load_or_create_cert(cert_path: &PathBuf, key_path: &PathBuf) -> Result<(String, String)> {
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read_to_string(cert_path).context("read cert")?;
        let key = std::fs::read_to_string(key_path).context("read key")?;
        return Ok((cert, key));
    }
    tracing::info!("generating self-signed server certificate");
    let (cert, key) = crypto::create_certificate()?;
    std::fs::write(cert_path, &cert).context("write cert")?;
    std::fs::write(key_path, &key).context("write key")?;
    Ok((cert, key))
}

/// 호스트를 기동한다. 웹서버·RTSP 리스너를 spawn하고 `RtspServer` 핸들을 반환한다
/// (협상된 StreamContext 조회용). 호출자는 이후 종료 시그널을 기다린다.
pub async fn start(config: HostConfig) -> Result<RtspServer> {
    let (cert_pem, key_pem) = load_or_create_cert(&config.cert_path, &config.key_path)?;

    let clients = ClientManager::new(config.state_path.clone(), cert_pem.clone(), key_pem.clone())
        .context("init client manager")?;

    let server_config = ServerConfig {
        name: config.name.clone(),
        unique_id: config.unique_id.clone(),
        http_port: config.http_port,
        https_port: config.https_port,
        rtsp_port: config.rtsp_port,
    };

    let session = crate::session::SessionState::new();

    let webserver = Webserver::new(server_config, clients, cert_pem, key_pem, session.clone());
    webserver.serve(&config.bind_ip).await.context("start webserver")?;

    let rtsp = RtspServer::new(config.rtsp_port, StreamPorts::default(), session.clone());

    // 제어 채널(ENet 47999)을 호스트 시작 시 1회 기동. StartB 수신 시 video_trigger 발동.
    let video_trigger = crate::control::VideoTrigger::new();
    // IDR 요청 플래그 — control 채널(클라이언트 IDR 요청)과 캡처/인코더가 공유한다.
    let idr_req = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::control::start(
        &config.bind_ip,
        StreamPorts::default().control,
        session.clone(),
        video_trigger.clone(),
        idr_req.clone(),
    )
    .await
    .context("start control channel")?;

    // PLAY 훅: 지속 파이프라인 설계.
    // 캡처(GraphicsCapture)+GPU변환+QSV 인코더+UDP 송출은 첫 PLAY에서 1회만 생성해
    // 프로세스 수명 내내 유지한다. native 리소스(D3D11/MFX/GraphicsCapture)를 세션마다
    // 재생성하면 두 번째 세션부터 검은 화면/크래시가 발생하므로, 재사용이 유일하게 안정적이다.
    // 세션 전환(새 PLAY)은 IDR 강제 요청 + 프레임 카운터 리셋만 수행한다.
    let bind_ip = config.bind_ip.clone();
    let video_port = StreamPorts::default().video;
    let dummy_video = std::env::var("KMC_DUMMY_VIDEO").is_ok();
    let trigger_for_hook = video_trigger.clone();
    // 파이프라인 1회 생성 가드 + 공유 IDR 요청 플래그.
    let pipeline_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // video 송출 태스크에 세션 리셋을 알리는 플래그 (새 세션마다 프레임 카운터 리셋).
    let session_reset = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 클라이언트 수신 활성 플래그: video PING 이 있으면 true. 인코더가 idle 시 인코딩을 멈추는 데 쓴다.
    let client_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    rtsp.set_play_hook(std::sync::Arc::new(move |ctx| {
        let bind_ip = bind_ip.clone();
        let trigger = trigger_for_hook.clone();
        let pipeline_started = pipeline_started.clone();
        let idr_req = idr_req.clone();
        let session_reset = session_reset.clone();
        let client_active = client_active.clone();
        tokio::spawn(async move {
            // 이미 파이프라인이 살아있으면: 세션 전환 처리만 (재생성하지 않음).
            if pipeline_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
                tracing::info!("PLAY on existing pipeline — requesting IDR + session reset");
                session_reset.store(true, std::sync::atomic::Ordering::Release);
                idr_req.store(true, std::sync::atomic::Ordering::Release);
                return;
            }

            // 첫 PLAY: UDP 송출 소켓을 1회 bind.
            let packet_size = if ctx.packet_size == 0 { 1024 } else { ctx.packet_size as usize };
            let sender = match crate::video::start(&bind_ip, video_port, packet_size, session_reset.clone(), client_active.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error=%e, "failed to start video stream");
                    pipeline_started.store(false, std::sync::atomic::Ordering::Release);
                    return;
                }
            };
            tracing::info!("video UDP stream ready — awaiting StartB");

            // 오디오 스트림도 1회 시작(WASAPI 루프백 → Opus → RTP 48000). 지속.
            let audio_port = StreamPorts::default().audio;
            if let Err(e) = crate::audio::start(&bind_ip, audio_port).await {
                tracing::error!(error=%e, "failed to start audio stream (continuing video-only)");
            }

            // StartB 이후 캡처/인코더 1회 생성 (이후 유지).
            let idr_req = idr_req.clone();
            let client_active = client_active.clone();
            tokio::spawn(async move {
                trigger.wait().await;
                tracing::info!("StartB received — beginning frame emission");
                if dummy_video {
                    crate::video::spawn_dummy_generator(sender, ctx.fps.max(1));
                    return;
                }
                // 해상도/비트레이트/코덱은 1회만 계산(세션 내 고정).
                let (nw, nh) = match windows_capture::monitor::Monitor::primary() {
                    Ok(m) => match (m.width(), m.height()) {
                        (Ok(mw), Ok(mh)) => (mw.max(2), mh.max(2)),
                        _ => (ctx.width.max(2), ctx.height.max(2)),
                    },
                    Err(_) => (ctx.width.max(2), ctx.height.max(2)),
                };
                let (bw, bh) = (ctx.width, ctx.height);
                let (w, h) = if bw == 0 || bh == 0 || (bw >= nw && bh >= nh) {
                    (nw, nh)
                } else {
                    let s = (bw as f64 / nw as f64).min(bh as f64 / nh as f64);
                    (((nw as f64 * s) as u32).max(2), ((nh as f64 * s) as u32).max(2))
                };
                let (w, h) = (w & !1, h & !1);
                let fps = if ctx.fps == 0 { 120 } else { ctx.fps };
                let px_rate = (w as u64) * (h as u64) * (fps as u64);
                let bitrate_floor = ((px_rate as f64 * 0.10) as u64).min(60_000_000) as u32;
                let negotiated = if ctx.bitrate_bps == 0 { 15_000_000 } else { ctx.bitrate_bps };
                let bitrate = negotiated.max(bitrate_floor);
                let codec = if ctx.video_format == 1 { "hevc_qsv" } else { "h264_qsv" };
                tracing::info!(w, h, fps, bitrate, codec, "stream encoder selected (on-demand pipeline)");

                // 생명주기 감시: 클라이언트가 볼 때만 캡처+인코드 파이프라인을 띄운다.
                // idle 이면 완전히 종료 → WGC 캡처 세션도 닫혀 노란 "캡처 중" 테두리가 사라진다.
                // 재개 시 spawn_capture 로 새로 생성(리소스 재생성 — 첫 프레임까지 ~1초).
                // (stop_flag, done_flag) of the live pipeline; None when nothing running.
                let mut running: Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::atomic::AtomicBool>)> = None;
                loop {
                    let want = client_active.load(std::sync::atomic::Ordering::Relaxed);
                    // 종료 진행 중이던 파이프라인이 완전히 내려갔으면 슬롯 비우기.
                    if let Some((stop, done)) = &running {
                        if stop.load(std::sync::atomic::Ordering::Relaxed)
                            && done.load(std::sync::atomic::Ordering::Acquire)
                        {
                            running = None;
                        }
                    }
                    let alive = running.as_ref().map_or(false, |(stop, _)| {
                        !stop.load(std::sync::atomic::Ordering::Relaxed)
                    });
                    match (want, alive, running.is_some()) {
                        // 클라이언트 있음 + 살아있는 파이프라인 없음 + 이전 것 완전 종료됨 → 새로 시작.
                        (true, false, false) => {
                            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let flags = crate::capture::CaptureFlags {
                                width: w, height: h, fps, bitrate_bps: bitrate, codec,
                                sender: sender.clone(),
                                stop_rx: stop.clone(),
                                idr_req: idr_req.clone(),
                                done: done.clone(),
                                client_active: client_active.clone(),
                            };
                            tracing::info!("client watching — starting capture pipeline");
                            crate::capture::spawn_capture(flags);
                            running = Some((stop, done));
                        }
                        // 클라이언트 없음 + 파이프라인 살아있음 → 종료 신호(WGC 세션 닫힘 → 노란 테두리 제거).
                        (false, true, _) => {
                            tracing::info!("no client — tearing down capture pipeline (removes capture border)");
                            if let Some((stop, _)) = &running {
                                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            });
        });
    }));

    rtsp.clone().serve(&config.bind_ip).await.context("start rtsp")?;

    tracing::info!(
        name = %config.name,
        http = config.http_port,
        https = config.https_port,
        rtsp = config.rtsp_port,
        "streamhost started (R2: pairing + negotiation)"
    );
    Ok(rtsp)
}
