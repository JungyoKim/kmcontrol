//! RTSP(48010) 스트림 협상 서버.
//!
//! Moonlight은 OPTIONS→DESCRIBE→SETUP(video/audio/control)→ANNOUNCE→PLAY 순으로
//! 스트림 파라미터(해상도/fps/비트레이트/포트)를 협상한다. R2 범위는 협상까지 —
//! PLAY 수신 시 실제 미디어 송출은 R3에서 시작한다. 협상된 파라미터는 `StreamContext`에
//! 저장돼 R3가 사용한다.
//!
//! 참조: hgaiser/moonshine (BSD-2). Moonlight의 비표준 RTSP를 rtsp_types로 파싱하기 위한
//! 문자열 치환 워크어라운드를 그대로 사용한다.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rtsp_types::{headers, Method};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 스트림 포트 설정 (SETUP 응답에 사용).
#[derive(Clone)]
pub struct StreamPorts {
    pub video: u16,
    pub audio: u16,
    pub control: u16,
}

impl Default for StreamPorts {
    fn default() -> Self {
        // Sunshine/Moonlight 관례 포트.
        Self { video: 47998, audio: 48000, control: 47999 }
    }
}

/// ANNOUNCE에서 협상된 스트림 파라미터 (R3가 소비).
#[derive(Clone, Debug, Default)]
pub struct StreamContext {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub packet_size: u32,
    pub bitrate_bps: u32,
    /// 0=H264, 1=HEVC (moonlight bitStreamFormat).
    pub video_format: u32,
}

type PlayHook = Arc<dyn Fn(StreamContext) + Send + Sync>;

#[derive(Clone)]
pub struct RtspServer {
    rtsp_port: u16,
    ports: StreamPorts,
    context: Arc<Mutex<Option<StreamContext>>>,
    session: crate::session::SessionState,
    play_hook: Arc<Mutex<Option<PlayHook>>>,
}

impl RtspServer {
    pub fn new(rtsp_port: u16, ports: StreamPorts, session: crate::session::SessionState) -> Self {
        Self {
            rtsp_port,
            ports,
            context: Arc::new(Mutex::new(None)),
            session,
            play_hook: Arc::new(Mutex::new(None)),
        }
    }

    /// PLAY 수신 시 협상된 컨텍스트로 호출될 훅 설정 (호스트가 비디오 스트림 시작).
    pub fn set_play_hook(&self, hook: PlayHook) {
        *self.play_hook.lock() = Some(hook);
    }

    /// 마지막으로 협상된 컨텍스트 (R3 검증/사용).
    pub fn last_context(&self) -> Option<StreamContext> {
        self.context.lock().clone()
    }

    /// RTSP 리스너를 spawn하고 즉시 반환.
    pub async fn serve(self, bind_ip: &str) -> Result<()> {
        let addr: SocketAddr = format!("{bind_ip}:{}", self.rtsp_port)
            .parse()
            .context("parse rtsp addr")?;
        let listener = TcpListener::bind(addr).await.context("bind rtsp")?;
        tracing::info!(%addr, "RTSP listening");
        tokio::spawn(async move {
            loop {
                let Ok((conn, peer)) = listener.accept().await else { continue };
                let server = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = server.handle_connection(conn, peer).await {
                        tracing::debug!(error=%e, "rtsp connection ended");
                    }
                });
            }
        });
        Ok(())
    }

    async fn handle_connection(&self, mut conn: TcpStream, peer: SocketAddr) -> Result<()> {
        // 인증된 launch로 세션이 열린 경우에만 RTSP 협상 진행 (Sunshine 동작).
        if !self.session.is_active() {
            tracing::warn!(%peer, "rejecting RTSP: no active session (launch first)");
            return Ok(());
        }
        let mut raw = String::new();
        let message = loop {
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).await.context("rtsp read")?;
            if n == 0 {
                return Ok(());
            }
            raw.push_str(std::str::from_utf8(&buf[..n]).context("rtsp utf8")?);

            // Moonlight의 비표준 요청 URI를 rtsp_types가 파싱하도록 치환.
            let fixed = raw
                .replace("streamid", "rtsp://localhost?streamid")
                .replace("PLAY /", "PLAY rtsp://localhost/");
            match rtsp_types::Message::parse(&fixed) {
                Ok((msg, _)) => break msg,
                Err(rtsp_types::ParseError::Incomplete(_)) => continue,
                Err(e) => {
                    tracing::warn!(error=%e, "rtsp parse failed");
                    return Ok(());
                }
            }
        };

        let response = match message {
            rtsp_types::Message::Request(req) => {
                let cseq: i32 = req
                    .header(&headers::CSEQ)
                    .and_then(|v| v.as_str().parse().ok())
                    .unwrap_or(0);
                tracing::debug!(method=?req.method(), cseq, %peer, "rtsp request");
                match req.method() {
                    Method::Options => self.options(&req, cseq),
                    Method::Describe => self.describe(&req, cseq),
                    Method::Setup => self.setup(&req, cseq),
                    Method::Announce => self.announce(&req, cseq),
                    Method::Play => self.play(&req, cseq),
                    m => {
                        tracing::warn!(method=?m, "unsupported rtsp method");
                        resp(cseq, req.version(), rtsp_types::StatusCode::BadRequest)
                    }
                }
            }
            _ => resp(0, rtsp_types::Version::V2_0, rtsp_types::StatusCode::BadRequest),
        };

        let mut out = Vec::new();
        response.write(&mut out).context("serialize rtsp response")?;
        conn.write_all(&out).await.context("rtsp write")?;
        conn.shutdown().await.ok(); // Moonlight은 요청당 연결을 기대.
        Ok(())
    }

    fn options(&self, req: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
        rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
            .header(headers::CSEQ, cseq.to_string())
            .header(headers::PUBLIC, "OPTIONS DESCRIBE SETUP PLAY ANNOUNCE")
            .build(Vec::new())
    }

    fn describe(&self, req: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
        // Moonlight이 요구하는 최소 SDP. H.264 + HEVC 광고.
        let mut sdp = String::new();
        sdp.push_str("a=x-ss-general.featureFlags:2\n");
        // SS_ENC_CONTROL_V2 = 0x01. control 채널 12바이트 seq IV를 유도(우리 복호와 일치).
        sdp.push_str("a=x-ss-general.encryptionSupported:1\n");
        sdp.push_str("a=x-ss-general.encryptionRequested:1\n");
        sdp.push_str("sprop-parameter-sets=AAAAAU\n");
        sdp.push_str("a=x-nv-video[0].refPicInvalidation:1\n");
        sdp.push_str("a=fmtp:96 packetization-mode=1\n");
        rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
            .header(headers::CSEQ, cseq.to_string())
            .build(sdp.into_bytes())
    }

    fn setup(&self, req: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
        // 요청 URI: rtsp://localhost?streamid=video/... (치환됨). streamid로 포트 결정.
        let stream = req
            .request_uri()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "streamid")
                    .map(|(_, v)| v.split('/').next().unwrap_or("").to_string())
            })
            .unwrap_or_default();

        let port = match stream.as_str() {
            "video" => self.ports.video,
            "audio" => self.ports.audio,
            "control" => self.ports.control,
            other => {
                tracing::warn!(stream=%other, "unknown streamid in SETUP");
                return resp(cseq, req.version(), rtsp_types::StatusCode::BadRequest);
            }
        };
        tracing::debug!(stream=%stream, port, "rtsp setup");
        rtsp_types::Response::builder(req.version(), rtsp_types::StatusCode::Ok)
            .header(headers::CSEQ, cseq.to_string())
            .header(headers::SESSION, "KmcSession;timeout = 90")
            .header(headers::TRANSPORT, format!("server_port={port}"))
            .build(Vec::new())
    }

    fn announce(&self, req: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
        let sdp = match sdp_types::Session::parse(req.body()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error=%e, "announce sdp parse failed");
                return resp(cseq, req.version(), rtsp_types::StatusCode::BadRequest);
            }
        };

        let width = sdp_attr(&sdp, "x-nv-video[0].clientViewportWd").unwrap_or(0);
        let height = sdp_attr(&sdp, "x-nv-video[0].clientViewportHt").unwrap_or(0);
        let fps = sdp_attr(&sdp, "x-nv-video[0].maxFPS").unwrap_or(60);
        let packet_size = sdp_attr(&sdp, "x-nv-video[0].packetSize").unwrap_or(1024);
        let bitrate_kbps: u32 = sdp_attr(&sdp, "x-ml-video.configuredBitrateKbps").unwrap_or(10000);
        let video_format = sdp_attr(&sdp, "x-nv-vqos[0].bitStreamFormat").unwrap_or(0);

        let ctx = StreamContext {
            width,
            height,
            fps,
            packet_size,
            bitrate_bps: bitrate_kbps.saturating_mul(1000),
            video_format,
        };
        tracing::info!(?ctx, "rtsp announce negotiated stream context");
        *self.context.lock() = Some(ctx);

        resp(cseq, req.version(), rtsp_types::StatusCode::Ok)
    }

    fn play(&self, req: &rtsp_types::Request<Vec<u8>>, cseq: i32) -> rtsp_types::Response<Vec<u8>> {
        tracing::info!("rtsp PLAY received — starting video stream (R3)");
        let ctx = self.context.lock().clone().unwrap_or_default();
        if let Some(hook) = self.play_hook.lock().clone() {
            // 훅 호출 격리 — 세션 시작 중 panic이 RTSP 연결 처리를 죽이지 않도록.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(ctx))).is_err() {
                tracing::error!("play hook panicked (isolated)");
            }
        } else {
            tracing::warn!("no play hook set — video will not start");
        }
        resp(cseq, req.version(), rtsp_types::StatusCode::Ok)
    }
}

fn resp(
    cseq: i32,
    version: rtsp_types::Version,
    status: rtsp_types::StatusCode,
) -> rtsp_types::Response<Vec<u8>> {
    rtsp_types::Response::builder(version, status)
        .header(headers::CSEQ, cseq.to_string())
        .build(Vec::new())
}

fn sdp_attr<F: FromStr>(sdp: &sdp_types::Session, attr: &str) -> Option<F> {
    sdp.get_first_attribute_value(attr)
        .ok()
        .flatten()
        .map(|s| s.trim())
        .and_then(|s| s.parse().ok())
}
