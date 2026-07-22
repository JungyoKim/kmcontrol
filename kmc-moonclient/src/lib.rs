//! GameStream 클라이언트: 페어링 + HTTP launch + moonlight-common-c 연결 드라이버.
//!
//! 국면1(이 크레이트가 직접 HTTP): 인증서 신원, 5단계 페어링, serverinfo, launch.
//! 국면2(moonlight-common-c 인수): LiStartConnection 이후 RTSP/control/RTP/FEC.

pub mod conn;
pub mod crypto;
pub mod pair;

pub use conn::{
    negotiated_codec, request_idr, send_key, send_mouse_button, send_mouse_position, send_scroll,
    start_stream, AuFrame, StreamSession,
};
pub use pair::{Identity, LaunchResult, PairedHost, ServerInfo};
