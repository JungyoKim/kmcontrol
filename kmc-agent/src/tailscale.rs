//! Tailscale 런타임 연결 보장 — 네이티브 tailscaled를 agent가 tailnet에 붙여둔다.
//!
//! 설치와 **로그인(`up`)은 모두 elevated 인스톨러의 책임**이다(WTG: `provision.ps1`,
//! 비-WTG: `install.ps1`/MSI). 인스톨러가 `up --unattended`로 등록해두면 tailscaled가
//! 시스템 서비스로 부팅마다 스스로 재연결하므로, 런타임에 `up`을 다시 부를 이유가 없다.
//!
//! **agent는 절대 `tailscale up`을 호출하지 않는다.** Windows의 tailscaled는 SYSTEM
//! 서비스여서 비관리자 컨텍스트의 `up`은 UAC/로그인 GUI를 띄우고 실패한다(= 학생 계정에서
//! agent 기동마다 권한 창이 뜨던 원인). 상태 *조회*는 무권한으로 되므로, agent는
//! `wait_ready()`로 연결이 설 때까지 유한 대기만 하고 안 되면 경고 후 진행한다
//! (제어플레인은 LAN/공개 hub로 계속 동작).
//!
//! 유한 대기가 필요한 이유: 로그온 직후엔 tailscaled가 아직 handshake 중일 수 있는데,
//! `self_ip()`가 그때 None이면 Hello의 `stream_addr`가 비어 hub가 공인 NAT IP로 폴백해
//! P2P 스트리밍이 깨진다. Hello 전에 100.x를 확보하는 게 목적이다.

use std::process::Command;
use std::time::Duration;

/// tailscale.exe 경로. env override(KMC_TAILSCALE) 우선, 없으면 표준 설치 경로.
fn tailscale_path() -> String {
    std::env::var("KMC_TAILSCALE")
        .unwrap_or_else(|_| "C:\\Program Files\\Tailscale\\tailscale.exe".to_string())
}

/// tailnet에 연결(Running)돼 있는지(`tailscale status --json`의 BackendState).
fn is_up(exe: &str) -> bool {
    Command::new(exe)
        .args(["status", "--json"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v.get("BackendState").and_then(|s| s.as_str()).map(|s| s == "Running"))
        .unwrap_or(false)
}

/// tailnet 연결(Running)이 설 때까지 최대 `timeout` 대기한다. 연결되면 `true`.
///
/// 미설치면 즉시 `false`(대기하지 않음). `up`은 호출하지 않는다 — 모듈 문서 참고.
pub fn wait_ready(timeout: Duration) -> bool {
    let exe = tailscale_path();
    if !std::path::Path::new(&exe).exists() {
        tracing::warn!(%exe, "tailscale not installed — elevated installer must install + `up`; continuing on LAN");
        return false;
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut waited = false;
    loop {
        if is_up(&exe) {
            if waited {
                tracing::info!("tailscale connected after wait");
            } else {
                tracing::info!("tailscale already connected");
            }
            return true;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                timeout_s = timeout.as_secs(),
                "tailscale not connected in time (logged out? installer must run `up --unattended` as admin) — continuing on LAN"
            );
            return false;
        }
        waited = true;
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// agent 자신의 tailnet IPv4(100.x)를 반환한다. 미연결/미설치면 None.
/// hub 에 Hello 로 보고해 세션 주소(스트리밍 타겟)로 쓰이게 한다.
pub fn self_ip() -> Option<String> {
    let exe = tailscale_path();
    if !std::path::Path::new(&exe).exists() {
        return None;
    }
    let out = Command::new(&exe).args(["ip", "-4"]).output().ok()?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().lines().next()?.trim().to_string();
    // 100.64.0.0/10 (CGNAT, tailnet 대역)만 유효로 간주.
    if ip.starts_with("100.") {
        Some(ip)
    } else {
        None
    }
}
