//! cua-driver 데몬 수명주기 — agent가 상시 보장한다.
//!
//! GUI/브라우저 자동화는 전부 cua-driver 데몬(`\\.\pipe\cua-driver`)을 거치므로,
//! 데몬이 죽으면 모든 조작이 실패한다. agent가 (1) startup에 데몬을 보장하고,
//! (2) 로그온 자동 기동(스케줄 작업)을 등록하며, (3) 조작 중 데몬이 죽으면
//! exec 레이어가 이 모듈로 되살려 재시도한다.

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

/// 로그온 시 데몬 자동 기동 등록(Windows 스케줄 작업). best-effort — 실패해도 계속.
pub fn enable_autostart() {
    let exe = crate::exec::cua_driver_path();
    match Command::new(&exe).args(["autostart", "enable"]).output() {
        Ok(o) => tracing::info!(ok = o.status.success(), "cua-driver autostart enable"),
        Err(e) => tracing::warn!(error = %e, "cua-driver autostart enable failed"),
    }
}
