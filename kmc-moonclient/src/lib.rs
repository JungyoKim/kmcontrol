//! GameStream 클라이언트 (**순수 Rust**): 페어링 + HTTP launch + 미디어 세션 구동.
//!
//! 국면1: 인증서 신원, 5단계 페어링, serverinfo, launch (`pair`).
//! 국면2: RTSP 핸드셰이크(`rtsp`) → control 채널(`control`) + 비디오/오디오 RTP 수신
//!        (`video`/`audio`). `session` 이 이들을 묶어 호스트 스트림을 구동한다.
//!
//! 와이어 포맷(RTP/NvVideoPacket/FEC 상태)은 호스트 `kmc-streamhost` 와 `kmc-gsproto`
//! 크레이트로 공유한다 — 한쪽만 고치면 런타임에서만 드러나는 불일치가 되기 때문이다.

pub mod audio;
pub mod control;
pub mod crypto;
pub mod pair;
pub mod rtsp;
pub mod session;
pub mod video;

pub use pair::{Identity, LaunchResult, PairedHost, ServerInfo};
pub use session::{
    last_termination, negotiated_codec, request_idr, send_key, send_mouse_button,
    send_mouse_position, send_scroll, start_stream, StreamSession,
};
pub use video::AuFrame;
