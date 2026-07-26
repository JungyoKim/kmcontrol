//! RTSP(48010) 스트림 협상 클라이언트 — 호스트 `kmc-streamhost/src/rtsp.rs`의 대칭 구현.
//!
//! `OPTIONS → DESCRIBE → SETUP(video/audio/control) → ANNOUNCE → PLAY` 순으로
//! 해상도/fps/비트레이트/패킷크기/코덱을 협상하고, 호스트가 SETUP 응답에 실어준
//! 미디어 포트를 돌려준다. 미디어 전송·컨트롤 채널은 이 모듈 범위 밖이다.
//!
//! # 연결 수명
//! 요청 하나당 TCP 연결 하나를 새로 연다. 호스트가 응답을 쓴 직후 write half를
//! shutdown하기 때문이다(host rtsp.rs:155 — "Moonlight은 요청당 연결을 기대").
//!
//! # 비표준 요청 라인 (host rtsp.rs:116-119)
//! 호스트는 rtsp_types로 파싱하기 전에 raw 요청 문자열을 이렇게 치환한다.
//!
//! ```text
//! "streamid" -> "rtsp://localhost?streamid"
//! "PLAY /"   -> "PLAY rtsp://localhost/"
//! ```
//!
//! 즉 호스트가 기대하는 것은 **치환 전** 형태 — URI가 아닌 날것의
//! `streamid=video/0/0`(host rtsp.rs:182-190이 `streamid` 쿼리로 되읽음)과
//! `/`(GFE 3.22 이후 Moonlight의 단일 PLAY 타깃, RtspConnection.c:1358) — 이다.
//! rtsp_types의 request URI는 `Url`만 받으므로, URI 없이 직렬화한 뒤(직렬화기가
//! placeholder `*`를 쓴다 — serializer.rs:58) 요청 라인의 URI 토큰만 원본 문자열로
//! 되돌린다(`rewrite_request_target`). 이 되돌리기가 호스트 치환의 정확한 역함수다.
//!
//! # ANNOUNCE SDP
//! 협상 결과를 결정하는 것은 호스트 `announce()`(rtsp.rs:209-237)가
//! `sdp_attr`(rtsp.rs:264-270)로 읽는 여섯 개 속성뿐이다. 속성 이름은
//! moonlight-common-c `SdpGenerator.c`와 바이트 단위로 동일하다.
//!
//! | SDP 속성                                  | 호스트 소비 위치     |
//! |-------------------------------------------|----------------------|
//! | `x-nv-video[0].clientViewportWd`          | host rtsp.rs:218     |
//! | `x-nv-video[0].clientViewportHt`          | host rtsp.rs:219     |
//! | `x-nv-video[0].maxFPS`                    | host rtsp.rs:220     |
//! | `x-nv-video[0].packetSize`                | host rtsp.rs:221     |
//! | `x-ml-video.configuredBitrateKbps`        | host rtsp.rs:222     |
//! | `x-nv-vqos[0].bitStreamFormat`            | host rtsp.rs:223     |
//!
//! SDP 라인은 반드시 `x=...` 꼴이어야 한다(sdp-types LineParser가 `=`를 인덱스 1에서만
//! 허용). 또 세션 레벨 속성은 첫 `m=` 라인보다 앞에 와야 `get_first_attribute_value`가
//! 찾는다.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use rtsp_types::{headers, HeaderName, Method, StatusCode, Version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::pair::LaunchResult;

/// 호스트 `StreamPorts::default()`(host rtsp.rs:32)와 동일한 관례 포트 —
/// SETUP 응답에 `server_port=`가 없을 때의 폴백.
const DEFAULT_VIDEO_PORT: u16 = 47998;
const DEFAULT_AUDIO_PORT: u16 = 48000;
const DEFAULT_CONTROL_PORT: u16 = 47999;

/// Sunshine/Moonlight 공용 암호화 플래그 (Limelight-internal.h:48).
/// 호스트는 DESCRIBE에서 `encryptionSupported:1`을 광고한다(host rtsp.rs:171).
const SS_ENC_CONTROL_V2: u32 = 0x01;

/// GFE 7.x 세대에 해당하는 RTSP 클라이언트 버전 (RtspConnection.c:1004-1007).
const RTSP_CLIENT_VERSION: u32 = 14;

/// 요청 하나(연결 + 송신 + 응답 수신)에 허용하는 최대 시간.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// 협상할 스트림 파라미터.
#[derive(Clone, Debug)]
pub struct StreamCfg {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub packet_size: u32,
    /// 0=H264, 1=HEVC (moonlight bitStreamFormat).
    pub video_format: u32,
}

/// RTSP 협상 결과 (서버가 SETUP 응답으로 알려준 포트).
#[derive(Clone, Debug)]
pub struct RtspNegotiated {
    pub video_port: u16,
    pub audio_port: u16,
    pub control_port: u16,
}

/// OPTIONS→DESCRIBE→SETUP×3→ANNOUNCE→PLAY 를 순서대로 수행한다.
///
/// `host_ip`/`rtsp_port`는 TCP 접속 대상이고, 요청 라인에 실리는 타깃 URL은
/// launch 응답의 `sessionUrl0`(`LaunchResult::rtsp_session_url`)을 그대로 쓴다.
/// 비어 있으면 `rtsp://{host_ip}:{rtsp_port}`로 재구성한다
/// (RtspConnection.c:973-983과 동일한 폴백).
pub async fn handshake(
    host_ip: &str,
    rtsp_port: u16,
    launch: &LaunchResult,
    cfg: &StreamCfg,
) -> Result<RtspNegotiated> {
    let target_url = resolve_target_url(host_ip, rtsp_port, &launch.rtsp_session_url)?;
    let mut client = RtspClient::new(host_ip, rtsp_port, target_url);

    // 1) OPTIONS — 호스트가 살아 있고 세션이 열려 있는지 확인.
    //    (host handle_connection()은 활성 세션이 없으면 응답 없이 끊는다 — rtsp.rs:103-106)
    let url = client.target_url.clone();
    let opts = client.exchange("OPTIONS", Method::Options, &url, Vec::new(), Vec::new()).await?;
    tracing::debug!(
        public = opts.header(&headers::PUBLIC).map(|v| v.as_str()).unwrap_or(""),
        "RTSP OPTIONS ok"
    );

    // 2) DESCRIBE — 호스트가 광고하는 기능 플래그 수신 (host rtsp.rs:166-179).
    let describe = client
        .exchange(
            "DESCRIBE",
            Method::Describe,
            &url,
            vec![(headers::ACCEPT, "application/sdp".to_string())],
            Vec::new(),
        )
        .await?;
    // 호스트 DESCRIBE 본문은 `v=0`이 없는 비표준 SDP 조각이라 sdp-types로 파싱할 수 없다.
    // Moonlight과 동일하게 문자열 스캔으로만 읽는다(RtspConnection.c:1139-1155).
    let describe_sdp = String::from_utf8_lossy(describe.body()).into_owned();
    let enc_supported = scan_sdp_uint(&describe_sdp, "x-ss-general.encryptionSupported").unwrap_or(0);
    // 컨트롤 채널 암호화는 오버헤드가 작아 지원되면 항상 켠다(SdpGenerator.c:277-278).
    let enc_enabled = enc_supported & SS_ENC_CONTROL_V2;
    tracing::debug!(enc_supported, enc_enabled, sdp = %describe_sdp.trim(), "RTSP DESCRIBE ok");

    // 3) SETUP ×3 — streamid별로 개별 요청. 응답 Transport의 server_port를 읽는다.
    let video_port = client.setup("video", "streamid=video/0/0", DEFAULT_VIDEO_PORT).await?;
    let audio_port = client.setup("audio", "streamid=audio/0/0", DEFAULT_AUDIO_PORT).await?;
    let control_port = client.setup("control", CONTROL_STREAM_ID, DEFAULT_CONTROL_PORT).await?;

    // 4) ANNOUNCE — 협상 파라미터를 SDP로 통보. 호스트가 StreamContext로 저장한다.
    let sdp = sdp_body(host_ip, cfg, video_port, enc_enabled);
    debug_assert!(!sdp.contains("streamid"), "SDP must not trip the host's streamid substitution");
    client
        .exchange(
            "ANNOUNCE",
            Method::Announce,
            CONTROL_STREAM_ID,
            vec![(headers::CONTENT_TYPE, "application/sdp".to_string())],
            sdp.into_bytes(),
        )
        .await?;

    // 5) PLAY — 호스트가 play_hook을 돌려 미디어 송출을 시작한다(host rtsp.rs:239-251).
    client.exchange("PLAY", Method::Play, PLAY_TARGET, Vec::new(), Vec::new()).await?;

    let negotiated = RtspNegotiated { video_port, audio_port, control_port };
    tracing::info!(?negotiated, "RTSP handshake complete");
    Ok(negotiated)
}

/// ANNOUNCE 타깃이자 control SETUP의 streamid (RtspConnection.c:648, 953).
const CONTROL_STREAM_ID: &str = "streamid=control/13/0";

/// GFE 3.22 이후 Moonlight은 단일 PLAY를 타깃 `/`로 보낸다 (RtspConnection.c:1358).
/// 호스트의 `"PLAY /" -> "PLAY rtsp://localhost/"` 치환이 노리는 형태다.
const PLAY_TARGET: &str = "/";

/// launch 응답의 sessionUrl0을 검증하고, 없으면 재구성한다.
fn resolve_target_url(host_ip: &str, rtsp_port: u16, session_url: &str) -> Result<String> {
    let url = session_url.trim();
    if url.is_empty() {
        return Ok(format!("rtsp://{host_ip}:{rtsp_port}"));
    }
    if url.starts_with("rtspenc://") {
        bail!("RTSP: host advertised encrypted RTSP ({url}) — not supported by this client");
    }
    if url.starts_with("rtspru://") {
        bail!("RTSP: host advertised ENet RTSP ({url}) — only plain TCP RTSP is supported");
    }
    if !url.starts_with("rtsp://") {
        bail!("RTSP: unexpected sessionUrl0 scheme in {url:?}");
    }
    Ok(url.to_string())
}

/// 요청마다 새 TCP 연결을 여는 RTSP 클라이언트. CSeq/Session을 이어서 유지한다.
struct RtspClient {
    host_ip: String,
    rtsp_port: u16,
    target_url: String,
    /// 다음 요청에 실을 CSeq. Moonlight과 동일하게 1부터 단조 증가(RtspConnection.c:81).
    cseq: u32,
    /// 첫 SETUP 응답의 Session 헤더에서 뽑은 세션 토큰.
    session_id: Option<String>,
}

impl RtspClient {
    fn new(host_ip: &str, rtsp_port: u16, target_url: String) -> Self {
        Self {
            host_ip: host_ip.to_string(),
            rtsp_port,
            target_url,
            cseq: 1,
            session_id: None,
        }
    }

    /// SETUP 한 건. 응답 Transport의 `server_port=`를 파싱하고, 없으면 관례 포트로 폴백한다
    /// (RtspConnection.c:1276-1279 / 1192-1195 / 1320-1325와 동일한 정책).
    async fn setup(&mut self, stream: &str, streamid: &str, fallback: u16) -> Result<u16> {
        let label = format!("SETUP {stream}");
        let resp = self
            .exchange(&label, Method::Setup, streamid, Vec::new(), Vec::new())
            .await?;

        if self.session_id.is_none() {
            if let Some(v) = resp.header(&headers::SESSION) {
                // "KmcSession;timeout = 90" -> "KmcSession" (host rtsp.rs:204)
                let token = v.as_str().split(';').next().unwrap_or("").trim();
                if !token.is_empty() {
                    self.session_id = Some(token.to_string());
                }
            }
        }

        let port = resp
            .header(&headers::TRANSPORT)
            .and_then(|v| parse_server_port(v.as_str()))
            .unwrap_or_else(|| {
                tracing::warn!(stream, fallback, "no server_port in SETUP Transport — using default");
                fallback
            });
        tracing::debug!(stream, port, "RTSP SETUP ok");
        Ok(port)
    }

    /// 요청을 직렬화해 새 연결로 보내고, 완전한 응답 하나를 받아 200을 확인한다.
    async fn exchange(
        &mut self,
        label: &str,
        method: Method,
        target: &str,
        extra: Vec<(HeaderName, String)>,
        body: Vec<u8>,
    ) -> Result<rtsp_types::Response<Vec<u8>>> {
        let wire = self.encode(method, target, extra, body)?;
        let resp = tokio::time::timeout(EXCHANGE_TIMEOUT, self.round_trip(label, &wire))
            .await
            .with_context(|| format!("RTSP {label}: timed out after {EXCHANGE_TIMEOUT:?}"))??;

        let status = resp.status();
        if status != StatusCode::Ok {
            bail!("RTSP {label}: host returned status {} ({status:?})", u16::from(status));
        }
        Ok(resp)
    }

    /// TCP 연결 → 송신 → 완전한 RTSP 메시지가 파싱될 때까지 수신.
    async fn round_trip(&self, label: &str, wire: &[u8]) -> Result<rtsp_types::Response<Vec<u8>>> {
        let mut conn = TcpStream::connect((self.host_ip.as_str(), self.rtsp_port))
            .await
            .with_context(|| {
                format!("RTSP {label}: connect to {}:{}", self.host_ip, self.rtsp_port)
            })?;
        conn.set_nodelay(true).ok();
        conn.write_all(wire)
            .await
            .with_context(|| format!("RTSP {label}: write request"))?;
        conn.flush().await.ok();

        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 4096];
        let message = loop {
            // 빈 버퍼는 Incomplete로 떨어지므로 첫 바퀴에서 곧바로 read로 넘어간다.
            let parsed = match rtsp_types::Message::<Vec<u8>>::parse(&buf) {
                Ok((msg, _consumed)) => Some(msg),
                Err(rtsp_types::ParseError::Incomplete(_)) => None,
                Err(e) => bail!("RTSP {label}: response parse failed: {e}"),
            };
            if let Some(msg) = parsed {
                break msg;
            }
            let n = conn
                .read(&mut chunk)
                .await
                .with_context(|| format!("RTSP {label}: read response"))?;
            if n == 0 {
                bail!(
                    "RTSP {label}: host closed connection after {} bytes without a complete response",
                    buf.len()
                );
            }
            buf.extend_from_slice(&chunk[..n]);
        };

        match message {
            rtsp_types::Message::Response(resp) => Ok(resp),
            other => bail!("RTSP {label}: expected a response, got {other:?}"),
        }
    }

    /// 요청 바이트 생성. CSeq를 소비(증가)하므로 요청당 정확히 한 번만 부른다.
    fn encode(
        &mut self,
        method: Method,
        target: &str,
        extra: Vec<(HeaderName, String)>,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let cseq = self.cseq;
        self.cseq += 1;

        let mut builder = rtsp_types::Request::builder(method, Version::V1_0)
            .header(headers::CSEQ, cseq.to_string())
            .header(hname("X-GS-ClientVersion"), RTSP_CLIENT_VERSION.to_string())
            .header(hname("Host"), self.host_ip.clone());
        if let Some(session) = &self.session_id {
            builder = builder.header(headers::SESSION, session.clone());
        }
        for (name, value) in extra {
            builder = builder.header(name, value);
        }

        // request URI는 일부러 비워 둔다 — 직렬화기가 `*`를 쓰고, 그 토큰만 아래에서
        // 호스트가 기대하는 비표준 타깃으로 되돌린다.
        let request = builder.build(body);
        let mut wire = Vec::with_capacity(request.write_len() as usize);
        request
            .write(&mut wire)
            .context("RTSP: serialize request")?;
        rewrite_request_target(wire, target)
    }
}

/// 정적 ASCII 문자열을 헤더 이름으로. rtsp_types에 상수가 없는 헤더용.
fn hname(name: &'static str) -> HeaderName {
    // 호출부는 모두 ASCII 리터럴이므로 실패할 수 없다.
    HeaderName::from_static_str(name).expect("ASCII header name")
}

/// 직렬화된 요청의 첫 줄에서 URI 토큰만 `target`으로 교체한다.
///
/// 요청 라인은 `METHOD SP URI SP VERSION CRLF`이고 METHOD/URI(placeholder `*`)/VERSION
/// 어디에도 공백이 없으므로 `splitn(3, ' ')`이 안전하다. 본문은 건드리지 않는다.
fn rewrite_request_target(wire: Vec<u8>, target: &str) -> Result<Vec<u8>> {
    let eol = wire
        .windows(2)
        .position(|w| w == b"\r\n")
        .context("RTSP: serialized request has no request line")?;
    let line = std::str::from_utf8(&wire[..eol]).context("RTSP: request line not utf8")?;

    let mut parts = line.splitn(3, ' ');
    let method = parts.next().context("RTSP: request line missing method")?;
    let _placeholder = parts.next().context("RTSP: request line missing uri")?;
    let version = parts.next().context("RTSP: request line missing version")?;

    let head = format!("{method} {target} {version}");
    let mut out = Vec::with_capacity(head.len() + wire.len() - eol);
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(&wire[eol..]);
    Ok(out)
}

/// Transport 헤더에서 `server_port=<n>`을 뽑는다.
/// 예: `unicast;server_port=48000-48001;source=...` → 48000
/// (RtspConnection.c:718-745의 동작을 그대로 옮김. 호스트는 `server_port=47998`만 보낸다 —
/// host rtsp.rs:205)
fn parse_server_port(transport: &str) -> Option<u16> {
    const NEEDLE: &str = "server_port=";
    let start = transport.find(NEEDLE)? + NEEDLE.len();
    let digits: String = transport[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.parse::<u32>() {
        Ok(p) if p > 0 && p <= u16::MAX as u32 => Some(p as u16),
        _ => None,
    }
}

/// 비표준 SDP 조각(호스트 DESCRIBE 본문)에서 `<attr>:<uint>`를 문자열 스캔으로 읽는다.
/// sdp-types는 `v=0`이 없는 이 본문을 파싱하지 못한다.
fn scan_sdp_uint(text: &str, attr: &str) -> Option<u32> {
    let needle = format!("{attr}:");
    let start = text.find(&needle)? + needle.len();
    let digits: String = text[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// ANNOUNCE 본문 SDP 생성.
///
/// 골격(`v=`/`o=`/`s=`/`t=`/`m=`)은 moonlight-common-c SdpGenerator.c:548-563과 동일하고,
/// 속성은 호스트 `announce()`가 읽는 여섯 개 + Moonlight이 항상 함께 보내는 동반 속성이다.
/// 세션 레벨 속성은 반드시 `m=`보다 앞에 와야 한다(sdp-types는 첫 `m=`에서 세션 파트를 닫는다).
fn sdp_body(host_ip: &str, cfg: &StreamCfg, video_port: u16, enc_enabled: u32) -> String {
    let addrtype = match host_ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => "IPv6",
        _ => "IPv4",
    };
    // FEC 여유분 20%를 뺀 값이 인코더 목표 비트레이트 (SdpGenerator.c:337-338).
    let adjusted_kbps = (cfg.bitrate_kbps as u64 * 80 / 100) as u32;
    // 1=HEVC 일 때만 HEVC 지원을 광고 (SdpGenerator.c:437-451).
    let hevc = u32::from(cfg.video_format == 1);

    let mut sdp = String::with_capacity(768);
    sdp.push_str("v=0\r\n");
    sdp.push_str(&format!("o=android 0 {RTSP_CLIENT_VERSION} IN {addrtype} {host_ip}\r\n"));
    sdp.push_str("s=NVIDIA Streaming Client\r\n");

    // --- 호스트가 실제로 읽는 속성 (host rtsp.rs:218-223) ---
    sdp.push_str(&format!("a=x-nv-video[0].clientViewportWd:{}\r\n", cfg.width));
    sdp.push_str(&format!("a=x-nv-video[0].clientViewportHt:{}\r\n", cfg.height));
    sdp.push_str(&format!("a=x-nv-video[0].maxFPS:{}\r\n", cfg.fps));
    sdp.push_str(&format!("a=x-nv-video[0].packetSize:{}\r\n", cfg.packet_size));
    sdp.push_str(&format!("a=x-ml-video.configuredBitrateKbps:{}\r\n", cfg.bitrate_kbps));
    sdp.push_str(&format!("a=x-nv-vqos[0].bitStreamFormat:{}\r\n", cfg.video_format));

    // --- Moonlight 동반 속성 (호스트는 무시하지만 실제 GFE/Sunshine 호환용) ---
    sdp.push_str("a=x-nv-video[0].rateControlMode:4\r\n");
    sdp.push_str("a=x-nv-video[0].timeoutLengthMs:7000\r\n");
    sdp.push_str("a=x-nv-video[0].framesWithInvalidRefThreshold:0\r\n");
    sdp.push_str(&format!("a=x-nv-video[0].initialBitrateKbps:{adjusted_kbps}\r\n"));
    sdp.push_str(&format!("a=x-nv-video[0].initialPeakBitrateKbps:{adjusted_kbps}\r\n"));
    sdp.push_str(&format!("a=x-nv-vqos[0].bw.minimumBitrateKbps:{adjusted_kbps}\r\n"));
    sdp.push_str(&format!("a=x-nv-vqos[0].bw.maximumBitrateKbps:{adjusted_kbps}\r\n"));
    sdp.push_str(&format!("a=x-nv-clientSupportHevc:{hevc}\r\n"));
    // 호스트가 DESCRIBE에서 요청한 컨트롤 채널 암호화 수락 (SdpGenerator.c:303-304).
    sdp.push_str(&format!("a=x-ss-general.encryptionEnabled:{enc_enabled}\r\n"));

    sdp.push_str("t=0 0\r\n");
    // Moonlight은 proto/fmt를 비운 채 보낸다 (SdpGenerator.c:561-562).
    sdp.push_str(&format!("m=video {video_port}  \r\n"));
    sdp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StreamCfg {
        StreamCfg {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20000,
            packet_size: 1392,
            video_format: 1,
        }
    }

    /// 호스트 `sdp_attr`(host rtsp.rs:264-270)와 동일한 읽기.
    fn sdp_attr<F: std::str::FromStr>(sdp: &sdp_types::Session, attr: &str) -> Option<F> {
        sdp.get_first_attribute_value(attr)
            .ok()
            .flatten()
            .map(|s| s.trim())
            .and_then(|s| s.parse().ok())
    }

    /// 호스트 handle_connection()의 문자열 치환(host rtsp.rs:117-119)을 그대로 재현.
    fn host_substitution(raw: &str) -> String {
        raw.replace("streamid", "rtsp://localhost?streamid")
            .replace("PLAY /", "PLAY rtsp://localhost/")
    }

    #[test]
    fn sdp_round_trips_every_attribute_the_host_reads() {
        let c = cfg();
        let body = sdp_body("192.168.0.7", &c, 47998, SS_ENC_CONTROL_V2);
        let sdp = sdp_types::Session::parse(body.as_bytes()).expect("host must be able to parse our SDP");

        assert_eq!(sdp_attr::<u32>(&sdp, "x-nv-video[0].clientViewportWd"), Some(c.width));
        assert_eq!(sdp_attr::<u32>(&sdp, "x-nv-video[0].clientViewportHt"), Some(c.height));
        assert_eq!(sdp_attr::<u32>(&sdp, "x-nv-video[0].maxFPS"), Some(c.fps));
        assert_eq!(sdp_attr::<u32>(&sdp, "x-nv-video[0].packetSize"), Some(c.packet_size));
        assert_eq!(
            sdp_attr::<u32>(&sdp, "x-ml-video.configuredBitrateKbps"),
            Some(c.bitrate_kbps)
        );
        assert_eq!(sdp_attr::<u32>(&sdp, "x-nv-vqos[0].bitStreamFormat"), Some(c.video_format));
    }

    #[test]
    fn sdp_attributes_precede_the_media_line() {
        // 첫 `m=` 이후의 속성은 세션이 아니라 미디어에 붙어 호스트가 못 읽는다.
        let body = sdp_body("10.0.0.1", &cfg(), 47998, 0);
        let m = body.find("m=video").expect("m= line");
        for attr in [
            "x-nv-video[0].clientViewportWd",
            "x-nv-video[0].clientViewportHt",
            "x-nv-video[0].maxFPS",
            "x-nv-video[0].packetSize",
            "x-ml-video.configuredBitrateKbps",
            "x-nv-vqos[0].bitStreamFormat",
        ] {
            assert!(body.find(attr).expect(attr) < m, "{attr} must precede m=");
        }
    }

    #[test]
    fn every_sdp_line_is_key_equals_value() {
        // sdp-types LineParser는 `=`가 인덱스 1이 아니면 라인 전체를 거부한다.
        let body = sdp_body("10.0.0.1", &cfg(), 47998, 1);
        for line in body.split("\r\n").filter(|l| !l.is_empty()) {
            assert_eq!(line.as_bytes().iter().position(|b| *b == b'='), Some(1), "bad line: {line}");
        }
    }

    #[test]
    fn sdp_never_contains_the_streamid_token() {
        // 호스트는 본문을 포함한 raw 전체에 "streamid" 치환을 건다 — 본문이 걸리면 손상된다.
        let body = sdp_body("10.0.0.1", &cfg(), 47998, 1);
        assert!(!body.contains("streamid"));
    }

    #[test]
    fn hevc_flag_tracks_video_format() {
        let mut c = cfg();
        c.video_format = 0;
        assert!(sdp_body("10.0.0.1", &c, 47998, 0).contains("a=x-nv-clientSupportHevc:0\r\n"));
        c.video_format = 1;
        assert!(sdp_body("10.0.0.1", &c, 47998, 0).contains("a=x-nv-clientSupportHevc:1\r\n"));
    }

    #[test]
    fn cseq_increments_monotonically_across_the_scripted_sequence() {
        let mut client = RtspClient::new("192.168.0.7", 48010, "rtsp://192.168.0.7:48010".into());
        let url = client.target_url.clone();
        let sdp = sdp_body("192.168.0.7", &cfg(), 47998, 1).into_bytes();

        let script: Vec<Vec<u8>> = vec![
            client.encode(Method::Options, &url, Vec::new(), Vec::new()).unwrap(),
            client
                .encode(
                    Method::Describe,
                    &url,
                    vec![(headers::ACCEPT, "application/sdp".into())],
                    Vec::new(),
                )
                .unwrap(),
            client.encode(Method::Setup, "streamid=video/0/0", Vec::new(), Vec::new()).unwrap(),
            client.encode(Method::Setup, "streamid=audio/0/0", Vec::new(), Vec::new()).unwrap(),
            client.encode(Method::Setup, CONTROL_STREAM_ID, Vec::new(), Vec::new()).unwrap(),
            client
                .encode(
                    Method::Announce,
                    CONTROL_STREAM_ID,
                    vec![(headers::CONTENT_TYPE, "application/sdp".into())],
                    sdp,
                )
                .unwrap(),
            client.encode(Method::Play, PLAY_TARGET, Vec::new(), Vec::new()).unwrap(),
        ];

        for (i, wire) in script.iter().enumerate() {
            let raw = String::from_utf8(wire.clone()).unwrap();
            let fixed = host_substitution(&raw);
            let (msg, _) = rtsp_types::Message::<Vec<u8>>::parse(&fixed)
                .unwrap_or_else(|e| panic!("host cannot parse request {i}: {e}\n{raw}"));
            let rtsp_types::Message::Request(req) = msg else { panic!("not a request") };
            let cseq: u32 = req.header(&headers::CSEQ).unwrap().as_str().parse().unwrap();
            assert_eq!(cseq, i as u32 + 1, "CSeq must start at 1 and increment by one");
        }
        assert_eq!(client.cseq, 8);
    }

    #[test]
    fn setup_request_line_survives_the_host_substitution() {
        let mut client = RtspClient::new("192.168.0.7", 48010, "rtsp://192.168.0.7:48010".into());
        for (streamid, expected) in [
            ("streamid=video/0/0", "video"),
            ("streamid=audio/0/0", "audio"),
            (CONTROL_STREAM_ID, "control"),
        ] {
            let wire = client.encode(Method::Setup, streamid, Vec::new(), Vec::new()).unwrap();
            let raw = String::from_utf8(wire).unwrap();
            // 클라이언트는 치환 "전" 형태를 보낸다.
            assert!(raw.starts_with(&format!("SETUP {streamid} RTSP/1.0\r\n")), "{raw}");

            let (msg, _) =
                rtsp_types::Message::<Vec<u8>>::parse(&host_substitution(&raw)).expect("host parse");
            let rtsp_types::Message::Request(req) = msg else { panic!("not a request") };
            assert_eq!(req.method(), &Method::Setup);
            // 호스트 setup()의 streamid 추출 (host rtsp.rs:183-190).
            let stream = req
                .request_uri()
                .and_then(|u| {
                    u.query_pairs()
                        .find(|(k, _)| k == "streamid")
                        .map(|(_, v)| v.split('/').next().unwrap_or("").to_string())
                })
                .unwrap_or_default();
            assert_eq!(stream, expected);
        }
    }

    #[test]
    fn play_request_line_uses_the_bare_slash_target() {
        let mut client = RtspClient::new("192.168.0.7", 48010, "rtsp://192.168.0.7:48010".into());
        let wire = client.encode(Method::Play, PLAY_TARGET, Vec::new(), Vec::new()).unwrap();
        let raw = String::from_utf8(wire).unwrap();
        assert!(raw.starts_with("PLAY / RTSP/1.0\r\n"), "{raw}");

        let (msg, _) =
            rtsp_types::Message::<Vec<u8>>::parse(&host_substitution(&raw)).expect("host parse");
        let rtsp_types::Message::Request(req) = msg else { panic!("not a request") };
        assert_eq!(req.method(), &Method::Play);
    }

    #[test]
    fn announce_carries_content_length_and_body() {
        let mut client = RtspClient::new("192.168.0.7", 48010, "rtsp://192.168.0.7:48010".into());
        let sdp = sdp_body("192.168.0.7", &cfg(), 47998, 1);
        let wire = client
            .encode(
                Method::Announce,
                CONTROL_STREAM_ID,
                vec![(headers::CONTENT_TYPE, "application/sdp".into())],
                sdp.clone().into_bytes(),
            )
            .unwrap();
        let raw = String::from_utf8(wire).unwrap();
        let (msg, _) =
            rtsp_types::Message::<Vec<u8>>::parse(&host_substitution(&raw)).expect("host parse");
        let rtsp_types::Message::Request(req) = msg else { panic!("not a request") };
        assert_eq!(
            req.header(&headers::CONTENT_LENGTH).unwrap().as_str(),
            sdp.len().to_string()
        );
        // 호스트 announce()는 이 본문을 그대로 sdp_types에 넘긴다.
        assert_eq!(req.body().as_slice(), sdp.as_bytes());
        sdp_types::Session::parse(req.body()).expect("host must parse the transported body");
    }

    #[test]
    fn session_id_is_echoed_after_the_first_setup() {
        let mut client = RtspClient::new("192.168.0.7", 48010, "rtsp://192.168.0.7:48010".into());
        client.session_id = Some("KmcSession".into());
        let wire = client.encode(Method::Play, PLAY_TARGET, Vec::new(), Vec::new()).unwrap();
        let raw = String::from_utf8(wire).unwrap();
        assert!(raw.contains("Session: KmcSession\r\n"), "{raw}");
    }

    #[test]
    fn transport_server_port_parsing() {
        // 호스트가 실제로 보내는 형태 (host rtsp.rs:205).
        assert_eq!(parse_server_port("server_port=47998"), Some(47998));
        // GFE/Sunshine의 일반형.
        assert_eq!(parse_server_port("unicast;server_port=48000-48001;source=1.2.3.4"), Some(48000));
        assert_eq!(parse_server_port("unicast"), None);
        assert_eq!(parse_server_port("server_port=0"), None);
        assert_eq!(parse_server_port("server_port=99999"), None);
        assert_eq!(parse_server_port("server_port=abc"), None);
    }

    #[test]
    fn describe_flags_are_scanned_from_the_non_sdp_body() {
        // 호스트 DESCRIBE 본문 그대로 (host rtsp.rs:169-175).
        let body = "a=x-ss-general.featureFlags:2\n\
                    a=x-ss-general.encryptionSupported:1\n\
                    a=x-ss-general.encryptionRequested:1\n\
                    sprop-parameter-sets=AAAAAU\n\
                    a=x-nv-video[0].refPicInvalidation:1\n\
                    a=fmtp:96 packetization-mode=1\n";
        assert!(sdp_types::Session::parse(body.as_bytes()).is_err(), "not real SDP — must be scanned");
        assert_eq!(scan_sdp_uint(body, "x-ss-general.encryptionSupported"), Some(1));
        assert_eq!(scan_sdp_uint(body, "x-ss-general.featureFlags"), Some(2));
        assert_eq!(scan_sdp_uint(body, "x-ss-general.nope"), None);
    }

    #[test]
    fn target_url_resolution() {
        assert_eq!(
            resolve_target_url("10.0.0.5", 48010, "rtsp://10.0.0.5:48010").unwrap(),
            "rtsp://10.0.0.5:48010"
        );
        assert_eq!(
            resolve_target_url("10.0.0.5", 48010, "  ").unwrap(),
            "rtsp://10.0.0.5:48010"
        );
        assert!(resolve_target_url("10.0.0.5", 48010, "rtspenc://10.0.0.5:48010").is_err());
        assert!(resolve_target_url("10.0.0.5", 48010, "http://10.0.0.5").is_err());
    }
}
