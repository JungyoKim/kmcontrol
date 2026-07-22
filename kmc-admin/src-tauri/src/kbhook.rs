//! 저수준 키보드 훅(WH_KEYBOARD_LL). 스트리밍 중 + 우리 창이 포그라운드일 때 모든 키를
//! OS 레벨에서 가로채 원격으로 전달하고 로컬 처리를 차단한다 — 윈도우키/한영키/Alt+Tab 등
//! 시스템 키가 admin 로컬이 아니라 원격에 먹도록(Moonlight 키보드 그랩과 동일).
//!
//! 포그라운드가 아니거나 스트리밍이 아니면 그대로 통과(로컬 키보드 정상). 마우스로 다른 창을
//! 클릭해 포커스를 옮기면 자동으로 로컬 키보드가 복구된다.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    HC_ACTION, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

static ENABLED: AtomicBool = AtomicBool::new(false); // 스트리밍 중인가.
static INSTALLED: AtomicBool = AtomicBool::new(false); // 훅 설치됨(1회).
static HWND_VAL: AtomicIsize = AtomicIsize::new(0); // 우리 창 HWND.

/// 스트리밍 시작/종료 시 호출 — 키보드 그랩 on/off.
pub fn set_streaming(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 창 HWND 등록 + 훅 설치(프로세스 1회). 이후 ENABLED + 포그라운드일 때만 그랩.
pub fn install(hwnd: isize) {
    HWND_VAL.store(hwnd, Ordering::Relaxed);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("kbd-hook".into()).spawn(|| unsafe {
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("kbhook: SetWindowsHookExW failed: {e:?}");
                INSTALLED.store(false, Ordering::SeqCst);
                return;
            }
        };
        // LL 훅 콜백은 이 스레드의 메시지 펌프를 통해 전달된다.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ENABLED.load(Ordering::Relaxed) {
        let ours = HWND_VAL.load(Ordering::Relaxed);
        // 우리 창이 포그라운드일 때만 그랩(아니면 로컬 키보드 통과).
        if ours != 0 && GetForegroundWindow().0 as isize == ours {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if down || up {
                // vkCode 를 그대로 원격에 전달(LWIN/RWIN/HANGUL/좌우 modifier 구분 포함).
                kmc_moonclient::send_key(kb.vkCode as i16, down, 0);
                return LRESULT(1); // 로컬 처리 차단.
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
