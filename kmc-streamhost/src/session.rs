//! 웹서버 `/launch`와 RTSP 서버가 공유하는 세션 상태.
//!
//! `/launch`(HTTPS)에서 Moonlight이 원격 입력 키(rikey/rikeyid)를 넘기고 세션을 연다.
//! RTSP는 활성 세션이 있을 때만 협상을 진행한다(Sunshine 동작: 인증된 launch 없이는 RTSP 거부).

use std::sync::Arc;

use parking_lot::Mutex;

/// launch에서 수신한 세션 파라미터.
#[derive(Clone, Debug)]
pub struct LaunchSession {
    /// 원격 입력 AES 키 (제어 채널, R4에서 사용).
    pub remote_input_key: Vec<u8>,
    pub remote_input_key_id: i64,
    /// 요청 해상도/리프레시 (launch mode=WxHxR).
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

/// 웹서버·RTSP 공유 세션 핸들.
#[derive(Clone, Default)]
pub struct SessionState {
    inner: Arc<Mutex<Option<LaunchSession>>>,
}

impl SessionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, session: LaunchSession) {
        *self.inner.lock() = Some(session);
    }

    pub fn is_active(&self) -> bool {
        self.inner.lock().is_some()
    }

    pub fn get(&self) -> Option<LaunchSession> {
        self.inner.lock().clone()
    }

    pub fn clear(&self) {
        *self.inner.lock() = None;
    }
}
