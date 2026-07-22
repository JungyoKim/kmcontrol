//! FFI 스모크: moonlight-common-c 함수를 실제로 링크·호출해 바인딩 동작 확인.
use kmc_mooncommon as ffi;

fn main() {
    unsafe {
        // STREAM_CONFIGURATION 제로화 후 필드 접근.
        let mut cfg: ffi::STREAM_CONFIGURATION = std::mem::zeroed();
        ffi::LiInitializeStreamConfiguration(&mut cfg);
        cfg.width = 1920;
        cfg.height = 1080;
        cfg.fps = 60;

        // 콜백 구조체 초기화 함수도 호출.
        let mut vcb: ffi::DECODER_RENDERER_CALLBACKS = std::mem::zeroed();
        ffi::LiInitializeVideoCallbacks(&mut vcb);

        // 스테이지 이름 조회 (문자열 반환 함수 링크 확인).
        let name = ffi::LiGetStageName(0);
        let s = if name.is_null() {
            "<null>".to_string()
        } else {
            std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned()
        };

        // launch 쿼리 파라미터 (Sunshine 확장) — 값 확인.
        let q = ffi::LiGetLaunchUrlQueryParameters();
        let qs = if q.is_null() {
            "<null>".to_string()
        } else {
            std::ffi::CStr::from_ptr(q).to_string_lossy().into_owned()
        };

        println!("OK: cfg={}x{}@{}, stage0='{}', launchQuery='{}'", cfg.width, cfg.height, cfg.fps, s, qs);
    }
}
