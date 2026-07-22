//! GameStream HTTP(47989)/HTTPS(47984) 웹서버 — serverinfo, 페어링, PIN.
//!
//! 참조: hgaiser/moonshine (BSD-2), Sunshine. Moonlight 클라이언트가 이 엔드포인트로
//! 서버 발견·페어링·앱 목록·스트림 시작을 수행한다. R2 범위는 페어링 + serverinfo까지.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::clients::{ClientManager, PendingClient};
use crate::tls;

const SERVERINFO_APP_VERSION: &str = "7.1.431.-1";
const SERVERINFO_GFE_VERSION: &str = "3.23.0.74";

/// serverinfo/RTSP가 참조하는 서버 식별·포트 설정.
#[derive(Clone)]
pub struct ServerConfig {
    pub name: String,
    pub unique_id: String,
    pub http_port: u16,
    pub https_port: u16,
    pub rtsp_port: u16,
}

#[derive(Clone)]
pub struct Webserver {
    config: ServerConfig,
    clients: ClientManager,
    server_cert_pem: String,
    server_key_pem: String,
    session: crate::session::SessionState,
}

impl Webserver {
    pub fn new(
        config: ServerConfig,
        clients: ClientManager,
        server_cert_pem: String,
        server_key_pem: String,
        session: crate::session::SessionState,
    ) -> Self {
        Self { config, clients, server_cert_pem, server_key_pem, session }
    }

    /// HTTP(평문) + HTTPS(mTLS) 리스너를 spawn하고 즉시 반환.
    pub async fn serve(self, bind_ip: &str) -> Result<()> {
        let http_addr: SocketAddr = format!("{bind_ip}:{}", self.config.http_port)
            .parse()
            .context("parse http addr")?;
        let https_addr: SocketAddr = format!("{bind_ip}:{}", self.config.https_port)
            .parse()
            .context("parse https addr")?;

        // HTTP.
        {
            let server = self.clone();
            let listener = TcpListener::bind(http_addr).await.context("bind http")?;
            tracing::info!(%http_addr, "GameStream HTTP listening");
            tokio::spawn(async move {
                loop {
                    let Ok((conn, peer)) = listener.accept().await else { continue };
                    let local = conn.local_addr().ok();
                    let server = server.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(conn);
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                io,
                                service_fn(|req| server.route(req, local, peer, false, None)),
                            )
                            .await;
                    });
                }
            });
        }

        // HTTPS (mTLS: 클라이언트 인증서 요청, lenient 검증).
        {
            let server = self.clone();
            let acceptor = tls::make_acceptor(&self.server_cert_pem, &self.server_key_pem)
                .context("build tls acceptor")?;
            let listener = TcpListener::bind(https_addr).await.context("bind https")?;
            tracing::info!(%https_addr, "GameStream HTTPS listening");
            tokio::spawn(async move {
                loop {
                    let Ok((conn, peer)) = listener.accept().await else { continue };
                    let local = conn.local_addr().ok();
                    let acceptor = acceptor.clone();
                    let server = server.clone();
                    tokio::spawn(async move {
                        let tls_conn = match acceptor.accept(conn).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::debug!(error=%e, "tls handshake failed");
                                return;
                            }
                        };
                        // peer 인증서 지문 추출 (mTLS 인가).
                        let fingerprint = tls::peer_cert_fingerprint(tls_conn.get_ref().1);
                        let io = TokioIo::new(tls_conn);
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                io,
                                service_fn(|req| {
                                    server.route(req, local, peer, true, fingerprint.clone())
                                }),
                            )
                            .await;
                    });
                }
            });
        }

        Ok(())
    }

    async fn route(
        &self,
        req: Request<hyper::body::Incoming>,
        local: Option<SocketAddr>,
        _peer: SocketAddr,
        https: bool,
        peer_fingerprint: Option<String>,
    ) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
        let params: HashMap<String, String> = req
            .uri()
            .query()
            .map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default();
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        tracing::debug!(%method, %path, https, "http request");

        let resp = match (https, &method, path.as_str()) {
            (_, &Method::GET, "/serverinfo") => self.server_info(&params, local, https),
            (false, &Method::GET, "/pair") => self.handle_pair(req, params, local).await,
            (true, &Method::GET, "/pair") => self.handle_pair(req, params, local).await,
            (false, &Method::GET, "/pin") => self.pin_page(&params),
            (false, &Method::POST, "/submit-pin") => self.submit_pin(req).await,
            (true, &Method::GET, "/applist") => match self.require_paired(&peer_fingerprint) {
                Some(deny) => deny,
                None => self.app_list(),
            },
            (true, &Method::GET, "/launch") => match self.require_paired(&peer_fingerprint) {
                Some(deny) => deny,
                None => self.launch(&params, local),
            },
            (true, &Method::GET, "/resume") => match self.require_paired(&peer_fingerprint) {
                Some(deny) => deny,
                None => self.launch(&params, local),
            },
            _ => {
                tracing::warn!(%method, %path, https, "unhandled request");
                not_found()
            }
        };
        Ok(resp)
    }

    fn require_paired(&self, fingerprint: &Option<String>) -> Option<Response<Full<Bytes>>> {
        // 페어링 제거: 자체 스트리밍 스택(hub가 세션 인증을 담당)이라 클라 인증서 페어링은 불필요.
        // 모든 클라이언트를 신뢰한다. (LAN/Tailscale ACL이 접근 경계를 담당.)
        let _ = fingerprint;
        None
    }

    fn server_info(
        &self,
        params: &HashMap<String, String>,
        local: Option<SocketAddr>,
        https: bool,
    ) -> Response<Full<Bytes>> {
        // 페어링 제거 — 항상 paired로 광고. 클라이언트는 페어링을 건너뛰고 바로 launch한다.
        let paired = "1";
        let _ = (params, https);
        let local_ip = local.map(|a| a.ip().to_string()).unwrap_or_default();

        // H.264 + HEVC(Main) 광고. Moonlight 클라가 HEVC 가능하면 HEVC 로 협상하고,
        // 그 경우 streamhost 가 hevc_qsv 로 인코딩한다(video_format=1). 클라가 H.264 만 되면 H.264.
        // ServerCodecModeSupport: bit0=H264, bit1=HEVC Main → 0x0003.
        let codec_support: u32 = 0x0003;

        let mut xml = String::from("<root status_code=\"200\">");
        xml += &format!("<hostname>{}</hostname>", escape_xml(&self.config.name));
        xml += &format!("<appversion>{SERVERINFO_APP_VERSION}</appversion>");
        xml += &format!("<GfeVersion>{SERVERINFO_GFE_VERSION}</GfeVersion>");
        xml += &format!("<uniqueid>{}</uniqueid>", self.config.unique_id);
        xml += &format!("<HttpsPort>{}</HttpsPort>", self.config.https_port);
        xml += &format!("<ExternalPort>{}</ExternalPort>", self.config.http_port);
        xml += "<mac>00:00:00:00:00:00</mac>";
        xml += "<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"; // HEVC 지원(≥4K 허용 값)
        xml += &format!("<LocalIP>{}</LocalIP>", escape_xml(&local_ip));
        xml += &format!("<ServerCodecModeSupport>{codec_support}</ServerCodecModeSupport>");
        xml += "<SupportedDisplayMode></SupportedDisplayMode>";
        xml += &format!("<PairStatus>{paired}</PairStatus>");
        xml += "<currentgame>0</currentgame>";
        xml += "<state>KMC_SERVER_FREE</state>";
        xml += "</root>";
        xml_ok(&xml)
    }

    fn app_list(&self) -> Response<Full<Bytes>> {
        // R2: 단일 "Desktop" 앱만 광고 (스트림 시작은 R3에서).
        let xml = "<root status_code=\"200\"><App><IsHdrSupported>0</IsHdrSupported>\
            <AppTitle>Desktop</AppTitle><ID>1</ID></App></root>";
        xml_ok(xml)
    }

    /// /launch, /resume — 세션을 열고 sessionUrl0(RTSP)를 반환. Moonlight은 이 URL로 RTSP 협상 시작.
    fn launch(&self, params: &HashMap<String, String>, local: Option<SocketAddr>) -> Response<Full<Bytes>> {
        // mode=WxHxR 파싱.
        let (mut width, mut height, mut refresh) = (0u32, 0u32, 60u32);
        if let Some(mode) = params.get("mode") {
            let parts: Vec<&str> = mode.split('x').collect();
            if parts.len() == 3 {
                width = parts[0].parse().unwrap_or(0);
                height = parts[1].parse().unwrap_or(0);
                refresh = parts[2].parse().unwrap_or(60);
            }
        }
        // 원격 입력 키 (제어 채널, R4에서 사용).
        let remote_input_key = params
            .get("rikey")
            .and_then(|h| hex::decode(h).ok())
            .unwrap_or_default();
        let remote_input_key_id = params
            .get("rikeyid")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let rikey_len = remote_input_key.len();
        self.session.set(crate::session::LaunchSession {
            remote_input_key,
            remote_input_key_id,
            width,
            height,
            refresh_rate: refresh,
        });
        tracing::info!(width, height, refresh, rikey_len, rikeyid = remote_input_key_id, "session launched — advertising RTSP sessionUrl0");

        // sessionUrl0가 있어야 Moonlight이 RTSP로 진행한다.
        let host_ip = local.map(|a| a.ip().to_string()).unwrap_or_else(|| "127.0.0.1".to_string());
        let xml = format!(
            "<root status_code=\"200\"><gamesession>1</gamesession>\
             <sessionUrl0>rtsp://{host_ip}:{}</sessionUrl0></root>",
            self.config.rtsp_port
        );
        xml_ok(&xml)
    }

    // ---- 페어링 ----

    async fn handle_pair(
        &self,
        _req: Request<hyper::body::Incoming>,
        mut params: HashMap<String, String>,
        local: Option<SocketAddr>,
    ) -> Response<Full<Bytes>> {
        if let Some(phrase) = params.remove("phrase") {
            match phrase.as_str() {
                "getservercert" => self.pair_get_server_cert(params, local).await,
                "pairchallenge" => paired_xml(""),
                other => bad_request(&format!("unknown pair phrase: {other}")),
            }
        } else if params.contains_key("clientchallenge") {
            self.pair_client_challenge(params)
        } else if params.contains_key("serverchallengeresp") {
            self.pair_server_challenge_response(params)
        } else if params.contains_key("clientpairingsecret") {
            self.pair_client_secret(params)
        } else {
            bad_request(&format!("unknown pair command: {:?}", params.keys()))
        }
    }

    async fn pair_get_server_cert(
        &self,
        mut params: HashMap<String, String>,
        _local: Option<SocketAddr>,
    ) -> Response<Full<Bytes>> {
        let Some(client_cert_hex) = params.remove("clientcert") else {
            return bad_request("missing clientcert");
        };
        let Ok(client_cert_bytes) = hex::decode(&client_cert_hex) else {
            return bad_request("clientcert not hex");
        };
        let Ok(client_pem) = String::from_utf8(client_cert_bytes) else {
            return bad_request("clientcert not utf8 pem");
        };
        let Some(unique_id) = params.remove("uniqueid") else {
            return bad_request("missing uniqueid");
        };
        let Some(salt_hex) = params.remove("salt") else {
            return bad_request("missing salt");
        };
        let Ok(salt_vec) = hex::decode(&salt_hex) else {
            return bad_request("salt not hex");
        };
        let Ok(salt) = <[u8; 16]>::try_from(salt_vec.as_slice()) else {
            return bad_request("salt not 16 bytes");
        };

        let notify = self.clients.start_pairing(PendingClient {
            id: unique_id.clone(),
            pem: client_pem,
            salt,
            pin_notify: Arc::new(Notify::new()),
            key: None,
            server_secret: None,
            server_challenge: None,
            client_hash: None,
        });

        let pin_url = format!("http://127.0.0.1:{}/pin?uniqueid={unique_id}", self.config.http_port);
        tracing::info!(%unique_id, "pairing started — enter PIN at {pin_url}");

        // PIN 입력 대기 (submit-pin이 notify_waiters 호출).
        wait_for_pin(notify).await;

        // 서버 인증서 PEM을 hex로 반환.
        paired_xml(&format!("<plaincert>{}</plaincert>", hex::encode(self.server_cert_pem.as_bytes())))
    }

    fn pair_client_challenge(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
        let (Some(id), Some(chal_hex)) = (params.remove("uniqueid"), params.remove("clientchallenge"))
        else {
            return bad_request("missing uniqueid/clientchallenge");
        };
        let Ok(challenge) = hex::decode(&chal_hex) else {
            return bad_request("clientchallenge not hex");
        };
        match self.clients.client_challenge(&id, challenge) {
            Ok(resp) => paired_xml(&format!("<challengeresponse>{}</challengeresponse>", hex::encode(resp))),
            Err(e) => {
                tracing::warn!(error=%e, "client_challenge failed");
                bad_request("client challenge failed")
            }
        }
    }

    fn pair_server_challenge_response(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
        let (Some(id), Some(resp_hex)) =
            (params.remove("uniqueid"), params.remove("serverchallengeresp"))
        else {
            return bad_request("missing uniqueid/serverchallengeresp");
        };
        let Ok(challenge_response) = hex::decode(&resp_hex) else {
            return bad_request("serverchallengeresp not hex");
        };
        match self.clients.server_challenge_response(&id, challenge_response) {
            Ok(secret) => paired_xml(&format!("<pairingsecret>{}</pairingsecret>", hex::encode(secret))),
            Err(e) => {
                tracing::warn!(error=%e, "server_challenge_response failed");
                bad_request("server challenge response failed")
            }
        }
    }

    fn pair_client_secret(&self, mut params: HashMap<String, String>) -> Response<Full<Bytes>> {
        let (Some(id), Some(secret_hex)) =
            (params.remove("uniqueid"), params.remove("clientpairingsecret"))
        else {
            return bad_request("missing uniqueid/clientpairingsecret");
        };
        let Ok(client_secret) = hex::decode(&secret_hex) else {
            return bad_request("clientpairingsecret not hex");
        };
        match self.clients.check_client_pairing_secret(&id, client_secret) {
            Ok(()) => {
                tracing::info!(%id, "client paired successfully");
                paired_xml("")
            }
            Err(e) => {
                tracing::warn!(error=%e, "pairing secret verification failed");
                bad_request("pairing secret check failed")
            }
        }
    }

    // ---- PIN (평문 HTTP) ----

    fn pin_page(&self, params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        let id = params.get("uniqueid").cloned().unwrap_or_default();
        let html = format!(
            "<!doctype html><html><body><h3>KMC 페어링 PIN 입력</h3>\
             <form method=POST action=/submit-pin>\
             <input type=hidden name=uniqueid value=\"{id}\">\
             PIN: <input name=pin autofocus> <button>확인</button></form></body></html>"
        );
        let mut r = Response::new(Full::new(Bytes::from(html)));
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        r
    }

    async fn submit_pin(&self, req: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
        let body = match req.into_body().collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => return bad_request(&format!("read body: {e}")),
        };
        let form: HashMap<String, String> =
            url::form_urlencoded::parse(&body).into_owned().collect();
        let (Some(id), Some(pin)) = (form.get("uniqueid"), form.get("pin")) else {
            return bad_request("missing uniqueid/pin");
        };
        match self.clients.register_pin(id, pin) {
            Ok(()) => {
                tracing::info!(%id, "pin submitted");
                let mut r = Response::new(Full::new(Bytes::from("<html><body>페어링 진행 중… 창을 닫아도 됩니다.</body></html>")));
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                r
            }
            Err(e) => bad_request(&format!("register pin: {e}")),
        }
    }
}

/// submit-pin의 notify를 기다리되, 90초 타임아웃.
async fn wait_for_pin(notify: Arc<Notify>) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(90), notify.notified()).await;
}

// ---- XML / 응답 헬퍼 ----

fn xml_ok(body: &str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    r
}

/// `<root status_code="200"><paired>1</paired>{inner}</root>`
fn paired_xml(inner: &str) -> Response<Full<Bytes>> {
    xml_ok(&format!("<root status_code=\"200\"><paired>1</paired>{inner}</root>"))
}

fn bad_request(msg: &str) -> Response<Full<Bytes>> {
    tracing::warn!("bad request: {msg}");
    let mut r = Response::new(Full::new(Bytes::from(
        "<root status_code=\"400\"><paired>0</paired></root>".to_string(),
    )));
    *r.status_mut() = StatusCode::BAD_REQUEST;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    r
}

fn not_found() -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from_static(b"not found")));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
