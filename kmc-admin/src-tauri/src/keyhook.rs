//! 입력 캡처 sidecar(kmc-keyhook.exe) 매니저.
//!
//! WHY sidecar: admin(WebView2)이 포그라운드일 때 admin 내부의 WH_KEYBOARD_LL 콜백은
//! 호출되지 않는다(실측: 내부 훅 0회 vs 별도 프로세스 227회). 그래서 저수준 키보드+마우스
//! 훅을 별도 프로세스가 소유하고, 캡처한 입력을 stdout 으로 받아 이 프로세스의 moonlight
//! 연결로 원격 전송한다(연결은 LiStartConnection 을 부른 admin 프로세스에만 존재).
//!
//! 게이트: active = STREAMING && FOCUSED && HOVERING. HOVERING(마우스가 영상 위)은 sidecar 가
//! canvas 화면 사각형과 마우스 절대좌표를 비교해 스스로 판정하고, 변화를 stdout("h 0|1")으로
//! 알려준다 — admin 은 이를 프론트로 emit 해 배지 UI 를 갱신한다.
//!
//! 인터페이스:
//!   - install(app): sidecar spawn + stdout 리더 스레드 기동(프로세스 1회). app 은 hover 이벤트
//!     emit 용.
//!   - set_streaming(on) / set_focused(on): 게이트 상태를 sidecar 로 전달.
//!   - set_canvas_rect(l,t,r,b): canvas 화면 절대 사각형(물리 px)을 sidecar 로 전달.
#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tauri::{AppHandle, Emitter};

use std::os::windows::process::CommandExt;
const CREATE_NO_WINDOW: u32 = 0x0800_0000; // sidecar 콘솔창 숨김.

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// sidecar stdin 핸들. 자식 프로세스 핸들도 붙잡아 살려둔다.
static SIDECAR: Mutex<Option<Sidecar>> = Mutex::new(None);

struct Sidecar {
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
}

/// kmc-keyhook.exe 경로: admin.exe 와 같은 디렉터리(번들 동봉).
fn sidecar_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("kmc-keyhook.exe")))
        .unwrap_or_else(|| std::path::PathBuf::from("kmc-keyhook.exe"))
}

/// sidecar 를 자식으로 띄우고 stdout 리더 스레드를 기동(프로세스 1회).
pub fn install(app: AppHandle) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let path = sidecar_path();
    let mut child = match Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("keyhook: sidecar spawn failed ({}): {e:?}", path.display());
            INSTALLED.store(false, Ordering::SeqCst);
            return;
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");

    // stdout 리더: sidecar 가 보낸 입력 이벤트를 파싱해 원격 전송/이벤트 emit.
    std::thread::Builder::new()
        .name("keyhook-reader".into())
        .spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                handle_line(&app, &line);
            }
        })
        .ok();

    *SIDECAR.lock() = Some(Sidecar { child, stdin });
}

/// sidecar stdout 한 줄 처리.
fn handle_line(app: &AppHandle, line: &str) {
    let mut it = line.split_whitespace();
    match it.next() {
        // 키보드: k <vk> <down>
        Some("k") => {
            if let (Some(vk), Some(d)) = (it.next(), it.next()) {
                if let (Ok(vk), Ok(d)) = (vk.parse::<i32>(), d.parse::<u8>()) {
                    kmc_moonclient::send_key(vk as i16, d == 1, 0);
                }
            }
        }
        // 마우스 이동: mm <x> <y> <w> <h> (원격 참조 좌표계)
        Some("mm") => {
            let v: Vec<i32> = it.filter_map(|s| s.parse().ok()).collect();
            if v.len() == 4 {
                kmc_moonclient::send_mouse_position(v[0] as i16, v[1] as i16, v[2] as i16, v[3] as i16);
            }
        }
        // 마우스 버튼: mb <button> <down>
        Some("mb") => {
            if let (Some(b), Some(d)) = (it.next(), it.next()) {
                if let (Ok(b), Ok(d)) = (b.parse::<u8>(), d.parse::<u8>()) {
                    kmc_moonclient::send_mouse_button(b, d == 1);
                }
            }
        }
        // 휠: ms <delta> (원시 WHEEL_DELTA 단위, 위=양수)
        Some("ms") => {
            if let Some(a) = it.next() {
                if let Ok(a) = a.parse::<i16>() {
                    kmc_moonclient::send_scroll(a);
                }
            }
        }
        // hover 변화: h <0|1> — 프론트 배지 갱신용.
        Some("h") => {
            if let Some(v) = it.next() {
                let on = v == "1";
                let _ = app.emit("hover", on);
            }
        }
        _ => {}
    }
}

fn send(cmd: &str) {
    let mut guard = SIDECAR.lock();
    if let Some(sc) = guard.as_mut() {
        let _ = sc.stdin.write_all(cmd.as_bytes());
        let _ = sc.stdin.write_all(b"\n");
        let _ = sc.stdin.flush();
    }
}

/// 스트리밍 시작/종료.
pub fn set_streaming(on: bool) {
    send(if on { "s1" } else { "s0" });
}

/// admin 창 포커스 변화(Tauri WindowEvent::Focused).
pub fn set_focused(on: bool) {
    send(if on { "f1" } else { "f0" });
}

/// canvas 화면 절대 사각형(물리 px). 프론트가 리사이즈/이동/스크롤 시 갱신.
pub fn set_canvas_rect(l: i32, t: i32, r: i32, b: i32) {
    send(&format!("r {l} {t} {r} {b}"));
}
