//! cua-driver 데몬 수명주기 — agent가 상시 보장한다.
//!
//! GUI/브라우저 자동화는 전부 cua-driver 데몬(`\\.\pipe\cua-driver`)을 거치므로,
//! 데몬이 죽으면 모든 조작이 실패한다. 보장은 3중이다: (1) agent startup에서
//! `ensure_daemon`, (2) 브라우저 조작 직전 `browser.rs`, (3) 조작 중 데몬이 죽으면
//! `exec.rs`가 되살려 재시도.
//!
//! **로그온 스케줄 작업(`cua-driver autostart enable`)은 일부러 쓰지 않는다.**
//! agent는 HKCU Run으로 이미 로그온마다 뜨고 그때 (1)이 데몬을 띄우므로 순수 잉여인데,
//! 등록되는 작업은 `RunLevel=Highest`(승격 실행)라 특권 경계를 건드린다.
//! 게다가 startup마다 이 호출을 하면 그만큼 기동이 늦는다 — 실측: 학생 노트북 로그에서
//! `tailscale already connected`(15:28:14.770) -> `autostart enable ok=true`(15:28:38.708),
//! **이 한 줄이 23.9초**. Hello 가 그만큼 밀린다.
//! 특권 작업은 elevated 인스톨러만 한다는 규칙(`tailscale.rs` 참고)을 여기에도 적용한다.

use std::os::windows::process::CommandExt;
use std::process::Command;

/// DETACHED_PROCESS — 데몬을 콘솔에 묶지 않고 독립 실행.
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// 데몬이 응답하는지(`cua-driver status`).
pub fn is_running() -> bool {
    let exe = crate::exec::cua_driver_path();
    Command::new(&exe)
        .arg("status")
        .output()
        .map(|o| {
            let s = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            s.contains("daemon is running")
        })
        .unwrap_or(false)
}

/// 데몬을 보장한다. 이미 떠 있으면 no-op, 아니면 `serve`를 detached로 기동하고 준비 대기.
/// 성공 시 true.
pub fn ensure_daemon() -> bool {
    if is_running() {
        return true;
    }
    let exe = crate::exec::cua_driver_path();
    if let Err(e) = Command::new(&exe).arg("serve").creation_flags(DETACHED_PROCESS).spawn() {
        tracing::warn!(error = %e, "cua-driver serve spawn failed");
        return false;
    }
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        if is_running() {
            tracing::info!("cua-driver daemon started");
            return true;
        }
    }
    tracing::warn!("cua-driver daemon did not become ready");
    false
}
