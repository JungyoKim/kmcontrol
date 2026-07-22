//! moonlight-common-c 연결 드라이버: FFI 콜백 브릿지 + LiStartConnection.
//!
//! submitDecodeUnit 등 디코더 콜백은 사용자 컨텍스트 인자가 없으므로,
//! 단일 활성 스트림 전제하에 전역 슬롯으로 Decoder에 접근한다(우리 설계는 단일 세션).

use anyhow::{bail, Result};
use ffi::*;
use kmc_mooncommon as ffi;
use parking_lot::Mutex;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use crate::pair::{LaunchResult, PairedHost, ServerInfo};

// Limelight.h: DR_OK=0, DR_NEED_IDR=-1 (bindgen이 #define을 상수로 안 냄 → 수작업 미러).
const DR_OK: c_int = 0;

/// 인코딩된 H.264 access unit (Annex-B). ffmpeg 디코드 없이 프론트(WebCodecs)로 그대로 전달한다.
pub struct AuFrame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// 인코딩 AU 싱크 (단일 활성 세션). start_stream에서 설정, submitDecodeUnit에서 send, cleanup에서 해제.
/// std mpsc라 tokio 비의존; admin이 rx를 드레인해 로컬 WS로 팬아웃한다.
static AU_SINK: Mutex<Option<std::sync::mpsc::Sender<AuFrame>>> = Mutex::new(None);
/// 종료 코드 관찰용.
static LAST_TERMINATION: Mutex<Option<i32>> = Mutex::new(None);
/// Opus 오디오 프레임 싱크(단일 활성 세션). decodeAndPlaySample에서 send, admin이 드레인해 WS로 팬아웃.
static AUDIO_SINK: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>> = Mutex::new(None);
/// dr_setup 이 관찰한 협상 video_format (VIDEO_FORMAT_H265 이면 HEVC). 프론트가 조회.
static NEGOTIATED_VIDEO_FORMAT: Mutex<i32> = Mutex::new(0);

/// 협상된 코덱 문자열("hevc" 또는 "h264"). admin 커맨드가 프론트에 전달.
pub fn negotiated_codec() -> &'static str {
    let vf = *NEGOTIATED_VIDEO_FORMAT.lock();
    if vf & (VIDEO_FORMAT_MASK_H265 as i32) != 0 { "hevc" } else { "h264" }
}

// ---- DECODER_RENDERER_CALLBACKS ----

extern "C" fn dr_setup(
    video_format: c_int,
    _width: c_int,
    _height: c_int,
    _redraw_rate: c_int,
    _context: *mut c_void,
    _dr_flags: c_int,
) -> c_int {
    // 디코드는 프론트(WebCodecs)가 한다. 협상된 video_format 을 기록해 프론트가 코덱을 맞추게 한다.
    *NEGOTIATED_VIDEO_FORMAT.lock() = video_format;
    tracing::info!(video_format, codec = negotiated_codec(), "decoder setup ok (passthrough → WebCodecs)");
    0
}

extern "C" fn dr_start() {}
extern "C" fn dr_stop() {}
extern "C" fn dr_cleanup() {
    *AU_SINK.lock() = None;
    tracing::info!("decoder cleanup");
}

/// 버퍼체인(LENTRY)을 단일 Annex-B 버퍼로 연결해 인코딩 AU 싱크로 전달. 디코드는 프론트가 한다.
extern "C" fn dr_submit_decode_unit(du: *mut DECODE_UNIT) -> c_int {
    if du.is_null() {
        return DR_OK;
    }
    let du = unsafe { &*du };
    // 버퍼체인 순회 → 단일 Annex-B 버퍼 (data에 start code 내장).
    let mut annexb: Vec<u8> = Vec::with_capacity(du.fullLength.max(0) as usize);
    let mut entry = du.bufferList;
    while !entry.is_null() {
        let e = unsafe { &*entry };
        if !e.data.is_null() && e.length > 0 {
            let slice = unsafe { std::slice::from_raw_parts(e.data as *const u8, e.length as usize) };
            annexb.extend_from_slice(slice);
        }
        entry = e.next;
    }
    let keyframe = du.frameType == FRAME_TYPE_IDR as c_int;
    if let Some(tx) = AU_SINK.lock().as_ref() {
        // 수신자(WS 클라이언트)가 없으면 드롭됨 — 재연결 시 IDR을 다시 요청하므로 무해.
        let _ = tx.send(AuFrame { keyframe, data: annexb });
    }
    DR_OK
}

// ---- CONNECTION_LISTENER_CALLBACKS ----

extern "C" fn cl_connection_terminated(error_code: c_int) {
    *LAST_TERMINATION.lock() = Some(error_code as i32);
    tracing::warn!(error_code, "connection terminated");
}

extern "C" fn cl_stage_failed(stage: c_int, error_code: c_int) {
    tracing::error!(stage, error_code, "connection stage failed");
}

extern "C" fn cl_connection_status_update(status: c_int) {
    tracing::info!(status, "connection status update (0=OK,1=POOR)");
}

// ---- AUDIO_RENDERER_CALLBACKS ----

extern "C" fn ar_init(
    _audio_config: c_int,
    _opus_config: POPUS_MULTISTREAM_CONFIGURATION,
    _context: *mut c_void,
    _ar_flags: c_int,
) -> c_int {
    // 디코드는 프론트(WebCodecs)가 한다. 여기선 Opus 프레임을 그대로 전달만.
    tracing::info!("audio renderer init (passthrough → WebCodecs)");
    0
}
extern "C" fn ar_start() {}
extern "C" fn ar_stop() {}
extern "C" fn ar_cleanup() {
    *AUDIO_SINK.lock() = None;
    tracing::info!("audio renderer cleanup");
}

/// moonlight이 디코드된(=우리 경우 인코딩된 Opus) 샘플을 넘긴다. 그대로 싱크로 전달.
extern "C" fn ar_decode_and_play(sample_data: *mut c_char, sample_length: c_int) {
    if sample_data.is_null() || sample_length <= 0 {
        return; // PLC 플레이스홀더(빈 프레임) — 프론트가 갭으로 처리.
    }
    let slice = unsafe { std::slice::from_raw_parts(sample_data as *const u8, sample_length as usize) };
    if let Some(tx) = AUDIO_SINK.lock().as_ref() {
        let _ = tx.send(slice.to_vec());
    }
}

/// 실행 중인 스트림 핸들. drop 시 LiStopConnection.
pub struct StreamSession {
    _keep_alive: (),
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        unsafe { ffi::LiStopConnection() };
        *AU_SINK.lock() = None;
        *AUDIO_SINK.lock() = None;
        tracing::info!("stream session stopped");
    }
}

/// launch 결과 + 협상 파라미터로 LiStartConnection 구동. 인코딩 AU는 `au_tx`로 흐른다.
///
/// 이 함수는 LiStartConnection이 반환할 때까지(연결 수립 또는 실패) 블록한다.
/// 성공 시 StreamSession을 반환하며, drop하면 스트림이 종료된다.
pub fn start_stream(
    server: &ServerInfo,
    host: &PairedHost,
    launch: &LaunchResult,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    au_tx: std::sync::mpsc::Sender<AuFrame>,
    audio_tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<StreamSession> {
    *AU_SINK.lock() = Some(au_tx);
    *AUDIO_SINK.lock() = Some(audio_tx);
    *LAST_TERMINATION.lock() = None;

    unsafe {
        // STREAM_CONFIGURATION.
        let mut cfg: STREAM_CONFIGURATION = std::mem::zeroed();
        LiInitializeStreamConfiguration(&mut cfg);
        cfg.width = width as c_int;
        cfg.height = height as c_int;
        cfg.fps = fps as c_int;
        cfg.bitrate = bitrate_kbps as c_int;
        cfg.packetSize = 1392;
        cfg.streamingRemotely = STREAM_CFG_LOCAL as c_int;
        cfg.audioConfiguration = make_stereo_audio_config();
        // H.264 + HEVC 둘 다 지원 요청. 서버(streamhost)가 HEVC 가능하면 HEVC로 협상되고,
        // 실제 협상된 포맷은 dr_setup(video_format)으로 확인해 프론트에 전달한다.
        cfg.supportedVideoFormats = (VIDEO_FORMAT_H264 | VIDEO_FORMAT_H265) as c_int;
        cfg.encryptionFlags = 0;
        cfg.remoteInputAesKey = std::mem::transmute::<[u8; 16], [c_char; 16]>(launch.rikey);
        cfg.remoteInputAesIv = std::mem::transmute::<[u8; 16], [c_char; 16]>(launch.rikey_iv);

        // SERVER_INFORMATION.
        let address = CString::new(host.address.clone())?;
        let app_version = CString::new(server.app_version.clone())?;
        let gfe_version = CString::new(server.gfe_version.clone())?;
        let rtsp_url = CString::new(launch.rtsp_session_url.clone())?;
        let mut si: SERVER_INFORMATION = std::mem::zeroed();
        si.address = address.as_ptr();
        si.serverInfoAppVersion = app_version.as_ptr();
        si.serverInfoGfeVersion = if server.gfe_version.is_empty() {
            std::ptr::null()
        } else {
            gfe_version.as_ptr()
        };
        si.rtspSessionUrl = rtsp_url.as_ptr();
        si.serverCodecModeSupport = server.codec_mode_support as c_int;

        // DECODER_RENDERER_CALLBACKS.
        let mut dr: DECODER_RENDERER_CALLBACKS = std::mem::zeroed();
        LiInitializeVideoCallbacks(&mut dr);
        dr.setup = Some(dr_setup);
        dr.start = Some(dr_start);
        dr.stop = Some(dr_stop);
        dr.cleanup = Some(dr_cleanup);
        dr.submitDecodeUnit = Some(dr_submit_decode_unit);
        dr.capabilities = 0; // push 모델 (DIRECT_SUBMIT 아님 → 라이브러리 내부 디코드 스레드 사용)

        // CONNECTION_LISTENER_CALLBACKS.
        let mut cl: CONNECTION_LISTENER_CALLBACKS = std::mem::zeroed();
        LiInitializeConnectionCallbacks(&mut cl);
        cl.connectionTerminated = Some(cl_connection_terminated);
        cl.stageFailed = Some(cl_stage_failed);
        cl.connectionStatusUpdate = Some(cl_connection_status_update);

        // AUDIO_RENDERER_CALLBACKS.
        let mut ar: AUDIO_RENDERER_CALLBACKS = std::mem::zeroed();
        LiInitializeAudioCallbacks(&mut ar);
        ar.init = Some(ar_init);
        ar.start = Some(ar_start);
        ar.stop = Some(ar_stop);
        ar.cleanup = Some(ar_cleanup);
        ar.decodeAndPlaySample = Some(ar_decode_and_play);

        let rc = LiStartConnection(
            &mut si,
            &mut cfg,
            &mut cl,
            &mut dr,
            &mut ar,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        );
        if rc != 0 {
            bail!("LiStartConnection failed: rc={rc}");
        }
    }
    Ok(StreamSession { _keep_alive: () })
}

/// 키프레임(IDR) 재요청. 새 WS 클라이언트가 붙거나 브로드캐스트 랙으로 프레임을 놓쳤을 때,
/// 프론트(WebCodecs)가 키프레임부터 다시 디코드할 수 있게 호스트에 IDR을 요청한다.
/// 활성 연결이 있을 때만 의미가 있다(연결 없으면 라이브러리 내부에서 무시).
pub fn request_idr() {
    unsafe { LiRequestIdrFrame() };
}

// ---- 원격 입력 (control 채널로 암호화 전송; moonlight-common-c가 처리) ----

/// 절대 마우스 위치. x/y는 참조 해상도(ref_w×ref_h) 기준.
pub fn send_mouse_position(x: i16, y: i16, ref_w: i16, ref_h: i16) {
    unsafe { LiSendMousePositionEvent(x, y, ref_w, ref_h) };
}

/// 마우스 버튼. button: 1=Left 2=Middle 3=Right 4=X1 5=X2.
pub fn send_mouse_button(button: u8, down: bool) {
    let action: c_char = if down { 0x07 } else { 0x08 }; // BUTTON_ACTION_PRESS/RELEASE
    unsafe { LiSendMouseButtonEvent(action, button as c_int) };
}

/// 키보드 이벤트. key_code = Windows VK, modifiers = MODIFIER_* 비트.
pub fn send_key(key_code: i16, down: bool, modifiers: u8) {
    let action: c_char = if down { 0x03 } else { 0x04 }; // KEY_ACTION_DOWN/UP
    unsafe { LiSendKeyboardEvent(key_code, action, modifiers as c_char) };
}

/// 세로 스크롤 (WHEEL_DELTA=120 단위, 위=양수).
pub fn send_scroll(amount: i16) {
    unsafe { LiSendHighResScrollEvent(amount) };
}

/// 최근 종료 코드 (관찰용).
pub fn last_termination() -> Option<i32> {
    *LAST_TERMINATION.lock()
}

fn make_stereo_audio_config() -> c_int {
    // MAKE_AUDIO_CONFIGURATION(2, 0x3) = (0x3<<16)|(2<<8)|0xCA (함수형 매크로 수작업 미러).
    ((0x3 << 16) | (2 << 8) | 0xCA) as c_int
}
