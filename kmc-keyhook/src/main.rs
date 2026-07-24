//! kmc-keyhook — 저수준 키보드 + 마우스 캡처 sidecar.
//!
//! WHY 별도 프로세스: admin(Tauri/WebView2)이 포그라운드일 때 admin "내부"의 WH_KEYBOARD_LL
//! 콜백은 호출되지 않는다(실측: 내부 훅 0회, 별도 프로세스 훅 227회). 그래서 훅을 admin 밖
//! 이 작은 프로세스가 소유하고, 캡처한 입력을 stdout 으로 admin 에 넘긴다. admin 은 자기
//! 프로세스의 moonlight 연결로 원격 전송한다(연결은 LiStartConnection 을 부른 admin 에만 존재).
//!
//! 게이트: active = STREAMING && FOCUSED && HOVERING.
//!   - STREAMING/FOCUSED 는 admin 이 stdin 으로 알려준다.
//!   - HOVERING(마우스가 영상 위인가)은 sidecar 가 마우스 절대좌표를 canvas 화면 사각형(admin 이
//!     stdin 으로 전달)과 비교해 스스로 판정한다. hover 변화는 stdout 으로 admin 에 보고
//!     (admin 이 배지 UI 갱신 등에 사용).
//!
//! 키보드: active 면 키를 stdout 으로 보고 + 로컬 차단(return 1). 아니면 통과.
//! 마우스: 이동/버튼/휠은 로컬 차단하지 않고 통과시키되(로컬 커서 자연스럽게 유지),
//!   active 일 때만 원격 좌표로 변환해 stdout 으로 미러링한다.
//!
//! 프로토콜 stdin(admin→sidecar), 한 줄에 하나:
//!   s1|s0            STREAMING on/off
//!   f1|f0            FOCUSED on/off
//!   r<l> <t> <r> <b> canvas 화면 절대 사각형(물리 px). 넷 다 0 이면 rect 미설정(hover 항상 false).
//! 프로토콜 stdout(sidecar→admin), 한 줄에 하나:
//!   k <vk> <down>        키보드(down=1|0)
//!   mm <x> <y> <w> <h>   마우스 이동(원격 좌표계 x/y, 참조 해상도 w/h)
//!   mb <button> <down>   마우스 버튼(button=1..5)
//!   ms <amount>          휠(WHEEL_DELTA=120 단위, 위=양수)
//!   h <0|1>              hover 상태 변화

#![cfg(windows)]

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
    KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

// 게이트 입력.
static STREAMING: AtomicBool = AtomicBool::new(false);
static FOCUSED: AtomicBool = AtomicBool::new(false);
static HOVERING: AtomicBool = AtomicBool::new(false);
// canvas 화면 절대 사각형(물리 px). RECT_SET 이 false 면 hover 판정 안 함.
static RECT_SET: AtomicBool = AtomicBool::new(false);
static RECT_L: AtomicI32 = AtomicI32::new(0);
static RECT_T: AtomicI32 = AtomicI32::new(0);
static RECT_R: AtomicI32 = AtomicI32::new(0);
static RECT_B: AtomicI32 = AtomicI32::new(0);

/// 훅 콜백 → 출력 워커. 콜백은 send 후 즉시 반환(경량 유지 → LowLevelHooksTimeout 회피).
static OUT_TX: OnceLock<Sender<String>> = OnceLock::new();

// 키/버튼 소유권: DOWN 이 원격으로 갔으면(=그때 active), UP 도 반드시 원격으로 보낸다.
// 경계를 넘는 순간(active 전환) 눌린 키의 UP 이 반대쪽으로 새어 stuck 되는 것을 막는 핵심.
// vk 0..=255 각각 "원격에 down 상태로 보낸 적 있음". 마우스 버튼 1..=5 은 별도.
static KEY_REMOTE: [AtomicBool; 256] = {
    const F: AtomicBool = AtomicBool::new(false);
    [F; 256]
};
static BTN_REMOTE: [AtomicBool; 6] = {
    const F: AtomicBool = AtomicBool::new(false);
    [F; 6]
};

fn active() -> bool {
    STREAMING.load(Ordering::Relaxed)
        && FOCUSED.load(Ordering::Relaxed)
        && HOVERING.load(Ordering::Relaxed)
}

/// 원격에 down 으로 보낸 모든 키/버튼에 up 을 보내 정리(스트림 종료 등 경계에서 stuck 방지).
fn release_all_remote() {
    for vk in 0..256 {
        if KEY_REMOTE[vk].swap(false, Ordering::Relaxed) {
            emit(format!("k {vk} 0"));
        }
    }
    for b in 1..6 {
        if BTN_REMOTE[b].swap(false, Ordering::Relaxed) {
            emit(format!("mb {b} 0"));
        }
    }
}

fn emit(line: String) {
    if let Some(tx) = OUT_TX.get() {
        let _ = tx.send(line);
    }
}

/// 마우스 절대좌표가 canvas 사각형 안인지 판정하고 HOVERING 갱신. 변화 시 stdout 보고.
/// 반환: 안이면 Some((원격x, 원격y, 참조w, 참조h)), 밖이면 None.
fn update_hover(px: i32, py: i32) -> Option<(i32, i32, i32, i32)> {
    if !RECT_SET.load(Ordering::Relaxed) {
        if HOVERING.swap(false, Ordering::Relaxed) {
            emit("h 0".into());
        }
        return None;
    }
    let (l, t, r, b) = (
        RECT_L.load(Ordering::Relaxed),
        RECT_T.load(Ordering::Relaxed),
        RECT_R.load(Ordering::Relaxed),
        RECT_B.load(Ordering::Relaxed),
    );
    let inside = px >= l && px < r && py >= t && py < b;
    let was = HOVERING.swap(inside, Ordering::Relaxed);
    if inside != was {
        emit(format!("h {}", if inside { 1 } else { 0 }));
    }
    if !inside || r <= l || b <= t {
        return None;
    }
    // canvas 픽셀 좌표(=원격 참조 해상도)로 변환. w/h 는 canvas 물리 크기.
    let w = r - l;
    let h = b - t;
    let x = (px - l).clamp(0, w - 1);
    let y = (py - t).clamp(0, h - 1);
    Some((x, y, w, h))
}

unsafe extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = wparam.0 as u32;
        let down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
        let up = m == WM_KEYUP || m == WM_SYSKEYUP;
        if down || up {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = (kb.vkCode & 0xff) as usize;
            let owned_remote = &KEY_REMOTE[vk];
            if down {
                // DOWN 은 현재 게이트로 판정. 원격이면 소유권 표시 + 로컬 차단.
                if active() {
                    owned_remote.store(true, Ordering::Relaxed);
                    emit(format!("k {} 1", kb.vkCode));
                    return LRESULT(1);
                }
                // 로컬로 통과(원격 소유 아님).
            } else {
                // UP 은 DOWN 이 간 곳으로 보낸다(게이트 무관) — 경계 넘어도 stuck 방지.
                if owned_remote.swap(false, Ordering::Relaxed) {
                    emit(format!("k {} 0", kb.vkCode));
                    return LRESULT(1); // 원격으로만, 로컬 차단.
                }
                // 로컬로 통과.
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let m = wparam.0 as u32;
        // 항상 hover 판정(게이트 무관) — 마우스 위치는 늘 추적해야 hover 가 정확.
        let mapped = update_hover(ms.pt.x, ms.pt.y);
        let act = active();
        // 버튼 down/up 을 (버튼번호, is_down) 으로 정규화.
        let btn: Option<(usize, bool)> = match m {
            WM_LBUTTONDOWN => Some((1, true)),
            WM_LBUTTONUP => Some((1, false)),
            WM_RBUTTONDOWN => Some((3, true)),
            WM_RBUTTONUP => Some((3, false)),
            WM_MBUTTONDOWN => Some((2, true)),
            WM_MBUTTONUP => Some((2, false)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let xb = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                Some((if xb == 1 { 4 } else { 5 }, m == WM_XBUTTONDOWN))
            }
            _ => None,
        };
        if let Some((b, is_down)) = btn {
            // 버튼도 키처럼 소유권 추적: DOWN 이 원격이면 UP 도 원격(게이트 무관) → stuck 방지.
            let owned = &BTN_REMOTE[b];
            if is_down {
                if act {
                    owned.store(true, Ordering::Relaxed);
                    emit(format!("mb {b} 1"));
                }
            } else if owned.swap(false, Ordering::Relaxed) {
                emit(format!("mb {b} 0"));
            }
        } else if act {
            match m {
                WM_MOUSEMOVE => {
                    if let Some((x, y, w, h)) = mapped {
                        emit(format!("mm {x} {y} {w} {h}"));
                    }
                }
                WM_MOUSEWHEEL => {
                    let delta = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                    emit(format!("ms {delta}"));
                }
                WM_MOUSEHWHEEL => { /* 가로 휠 미지원(무시) */ }
                _ => {}
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn main() {
    // 출력 워커: 콜백이 넘긴 라인을 stdout 으로. 콜백과 분리해 콜백을 가볍게 유지.
    let (tx, rx) = channel::<String>();
    let _ = OUT_TX.set(tx);
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        while let Ok(line) = rx.recv() {
            let mut h = stdout.lock();
            let _ = writeln!(h, "{line}");
            let _ = h.flush();
        }
    });

    // stdin 리더: admin 의 상태/사각형 갱신.
    std::thread::spawn(|| {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line == "s1" {
                STREAMING.store(true, Ordering::Relaxed);
            } else if line == "s0" {
                STREAMING.store(false, Ordering::Relaxed);
                release_all_remote(); // 스트림 종료 시 눌린 원격 키/버튼 정리.
            } else if line == "f1" {
                FOCUSED.store(true, Ordering::Relaxed);
            } else if line == "f0" {
                FOCUSED.store(false, Ordering::Relaxed);
            } else if let Some(rest) = line.strip_prefix("r ") {
                let n: Vec<i32> = rest.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                if n.len() == 4 {
                    RECT_L.store(n[0], Ordering::Relaxed);
                    RECT_T.store(n[1], Ordering::Relaxed);
                    RECT_R.store(n[2], Ordering::Relaxed);
                    RECT_B.store(n[3], Ordering::Relaxed);
                    RECT_SET.store(n[2] > n[0] && n[3] > n[1], Ordering::Relaxed);
                }
            }
        }
        std::process::exit(0); // stdin 닫힘(admin 종료) → 종료.
    });

    // 훅 설치 + 메시지 펌프(이 스레드). LL 훅 콜백은 이 스레드 펌프로 배달된다.
    unsafe {
        let khook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("kmc-keyhook: kbd hook failed: {e:?}");
                std::process::exit(1);
            }
        };
        let mhook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("kmc-keyhook: mouse hook failed: {e:?}");
                std::process::exit(1);
            }
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(khook);
        let _ = UnhookWindowsHookEx(mhook);
    }
}
