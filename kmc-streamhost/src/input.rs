//! 원격 입력 주입 (Windows SendInput).
//!
//! control 채널(0x0206, 암호화)로 받은 Moonlight 입력 패킷을 파싱해 로컬 데스크톱에 주입한다.
//! 입력 패킷 와이어 포맷(moonlight-common-c Input.h/InputStream.c):
//!   NV_INPUT_HEADER = size(u32 BE, 헤더 제외 크기) + magic(u32 LE, 패킷 타입)
//! 필드 엔디안은 패킷마다 다르다(주석 참조).

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

// 입력 패킷 magic (LE32).
const MAGIC_KEY_DOWN: u32 = 0x0000_0003;
const MAGIC_KEY_UP: u32 = 0x0000_0004;
const MAGIC_MOUSE_MOVE_ABS: u32 = 0x0000_0005;
const MAGIC_MOUSE_MOVE_REL: u32 = 0x0000_0006;
const MAGIC_MOUSE_MOVE_REL_GEN5: u32 = 0x0000_0007;
const MAGIC_MOUSE_BTN_DOWN_GEN5: u32 = 0x0000_0008;
const MAGIC_MOUSE_BTN_UP_GEN5: u32 = 0x0000_0009;
const MAGIC_SCROLL_GEN5: u32 = 0x0000_000A;
const MAGIC_HSCROLL: u32 = 0x5500_0001;

// XBUTTON1/XBUTTON2 (winuser.h). windows 크레이트가 이 모듈에 상수를 노출하지 않아 리터럴 사용.
const XBUTTON1: i32 = 0x0001;
const XBUTTON2: i32 = 0x0002;

/// 복호화된 입력 페이로드(NV_INPUT_HEADER + 본문)를 파싱해 주입한다.
pub fn inject(payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    // header.magic = payload[4..8] LE32.
    let magic = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let body = &payload[8..];
    let r = std::panic::catch_unwind(|| match magic {
        MAGIC_MOUSE_MOVE_REL | MAGIC_MOUSE_MOVE_REL_GEN5 => {
            if body.len() >= 4 {
                let dx = i16::from_be_bytes([body[0], body[1]]);
                let dy = i16::from_be_bytes([body[2], body[3]]);
                mouse_move_rel(dx as i32, dy as i32);
            }
        }
        MAGIC_MOUSE_MOVE_ABS => {
            // x(BE) y(BE) unused(2) width(BE) height(BE).
            if body.len() >= 10 {
                let x = i16::from_be_bytes([body[0], body[1]]) as i32;
                let y = i16::from_be_bytes([body[2], body[3]]) as i32;
                let w = i16::from_be_bytes([body[6], body[7]]) as i32;
                let h = i16::from_be_bytes([body[8], body[9]]) as i32;
                mouse_move_abs(x, y, w, h);
            }
        }
        MAGIC_MOUSE_BTN_DOWN_GEN5 => {
            if let Some(&b) = body.first() {
                mouse_button(b, true);
            }
        }
        MAGIC_MOUSE_BTN_UP_GEN5 => {
            if let Some(&b) = body.first() {
                mouse_button(b, false);
            }
        }
        MAGIC_KEY_DOWN => key_from_body(body, true),
        MAGIC_KEY_UP => key_from_body(body, false),
        MAGIC_SCROLL_GEN5 => {
            // scrollAmt1(BE i16) — WHEEL_DELTA(120) 단위.
            if body.len() >= 2 {
                let amt = i16::from_be_bytes([body[0], body[1]]);
                scroll(amt as i32, false);
            }
        }
        MAGIC_HSCROLL => {
            if body.len() >= 2 {
                let amt = i16::from_be_bytes([body[0], body[1]]);
                scroll(amt as i32, true);
            }
        }
        _ => {}
    });
    if r.is_err() {
        tracing::error!("input injection panicked (isolated)");
    }
}

// NV_KEYBOARD_PACKET body: flags(i8) + keyCode(i16 LE) + modifiers(i8) + zero2(i16).
fn key_from_body(body: &[u8], down: bool) {
    if body.len() >= 3 {
        let key_code = u16::from_le_bytes([body[1], body[2]]);
        key(key_code, down);
    }
}

fn send_one(input: INPUT) {
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse_event(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, mouse_data: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one(input);
}

fn mouse_move_rel(dx: i32, dy: i32) {
    mouse_event(MOUSEEVENTF_MOVE, dx, dy, 0);
}

fn mouse_move_abs(x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // MOUSEEVENTF_ABSOLUTE 좌표계는 주 모니터 0..=65535. 참조 해상도 기준으로 정규화.
    let nx = (x as i64 * 65535 / w as i64) as i32;
    let ny = (y as i64 * 65535 / h as i64) as i32;
    mouse_event(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, nx, ny, 0);
}

fn mouse_button(button: u8, down: bool) {
    // 1=Left 2=Middle 3=Right 4=X1 5=X2 (moonlight).
    let (flags, data) = match button {
        1 => (if down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0),
        2 => (if down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0),
        3 => (if down { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0),
        4 => (if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1),
        5 => (if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2),
        _ => return,
    };
    mouse_event(flags, 0, 0, data);
}

fn scroll(amount: i32, horizontal: bool) {
    let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
    mouse_event(flags, 0, 0, amount);
}

// 확장 키(오른쪽 modifier, 화살표/편집키 등)는 KEYEVENTF_EXTENDEDKEY 필요.
fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // PageUp/Down, End, Home, arrows
            | 0x2D | 0x2E // Insert, Delete
            | 0xA3 // RCONTROL
            | 0xA5 // RMENU (right alt)
            | 0x5B | 0x5C // LWIN/RWIN
            | 0x6F // Divide (numpad /)
            | 0x90 // NumLock
    )
}

fn key(vk: u16, down: bool) {
    let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one(input);
}
