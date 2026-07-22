//! Tailscale 런타임 연결 보장 — 네이티브 tailscaled를 agent가 tailnet에 붙여둔다.
//!
//! 설치(시스템 서비스 + WinTun 드라이버)는 admin이 필요하므로 elevated 인스톨러
//! (WTG: `provision.ps1`, 비-WTG: MSI/NSIS)의 책임이다. 그 단계에서 학생 계정을
//! Tailscale operator로 지정해두면(=`tailscale set --operator`), 여기(비관리자 런타임)서
//! `tailscale up`/status를 권한 없이 호출할 수 있다.
//!
//! agent는 startup에 `ensure_up()`으로 tailnet 연결을 자가보장한다(cua 데몬과 동일 패턴).
//! hub가 tailnet에서 도달되면, agent가 hub의 tailnet 주소로 WS를 맺어 hub가 캡처하는
//! peer_ip가 곧 이 노드의 100.x 주소가 되고 → 세션 주소/스트리밍 타겟이 자동 tailnet화된다.

use std::process::Command;

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

/// tailnet 연결을 보장한다. 이미 Running이면 no-op. 아니면 authkey로 `up`.
/// 설치가 안 돼 있으면(비관리자 런타임에선 설치 불가) 경고만 남기고 넘어간다 —
/// 제어플레인(hub WS)은 LAN/localhost로도 계속 동작하므로 치명적이지 않다.
pub fn ensure_up(hostname: &str) {
    let exe = tailscale_path();
    if !std::path::Path::new(&exe).exists() {
        tracing::warn!(%exe, "tailscale not installed — elevated installer/provision must install it; skipping");
        return;
    }
    if is_up(&exe) {
        tracing::info!("tailscale already connected");
        return;
    }
    let Ok(authkey) = std::env::var("KMC_TS_AUTHKEY") else {
        tracing::warn!("tailscale not connected and KMC_TS_AUTHKEY unset — skipping up (assume provisioned elsewhere)");
        return;
    };
    let mut cmd = Command::new(&exe);
    cmd.arg("up")
        .arg(format!("--authkey={authkey}"))
        .arg(format!("--advertise-tags={}", kmc_proto::CAMP_LAPTOP_TAG))
        .arg("--unattended");
    if !hostname.is_empty() {
        cmd.arg(format!("--hostname={hostname}"));
    }
    match cmd.output() {
        Ok(o) if o.status.success() => tracing::info!(%hostname, "tailscale up ok"),
        Ok(o) => tracing::warn!(
            stderr = %String::from_utf8_lossy(&o.stderr),
            "tailscale up failed (operator not set? admin needed once)"
        ),
        Err(e) => tracing::warn!(error = %e, "tailscale up spawn failed"),
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
