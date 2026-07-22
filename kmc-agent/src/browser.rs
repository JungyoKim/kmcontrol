//! 전용 CDP Chrome 수명주기 관리 (투트랙의 "전용 브라우저" 트랙).
//!
//! agent가 원격 디버깅 포트를 연 격리 프로필 Chrome을 1회 띄워 유지한다. AI(MCP)는
//! cua-driver browser_* 도구로 이 브라우저를 DOM 기반(스크린샷 없이) 조작한다.
//! headless가 아니므로 스트리밍 화면에 보여 관리자가 감독할 수 있다.
//!
//! 이 모듈은 "브라우저가 떠 있게 보장"만 하고 그 pid+window_id를 반환한다.
//! 실제 navigate/click 오케스트레이션은 MCP 서버가 browser_* 도구로 수행한다.

use std::sync::{LazyLock, Mutex};

const CDP_PORT: u16 = 9222;

static STATE: LazyLock<Mutex<Option<u32>>> = LazyLock::new(|| Mutex::new(None));

fn cdp_port() -> u16 {
    std::env::var("KMC_BROWSER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CDP_PORT)
}

fn profile_dir() -> String {
    if let Ok(p) = std::env::var("KMC_BROWSER_PROFILE") {
        return p;
    }
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\Temp".to_string());
    format!("{base}\\kmc\\dedicated-chrome")
}

/// Chrome 실행 파일 경로 탐색.
fn chrome_path() -> Option<String> {
    if let Ok(p) = std::env::var("KMC_CHROME") {
        return Some(p);
    }
    let candidates = [
        "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
        "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    ];
    candidates.iter().find(|p| std::path::Path::new(p).exists()).map(|s| s.to_string())
}

/// CDP 엔드포인트가 살아있는지 확인.
fn cdp_alive(port: u16) -> bool {
    // 동기 TCP 연결 시도로 리스너 여부만 확인(HTTP까지 안 가도 충분).
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        std::time::Duration::from_millis(500),
    )
    .is_ok()
}

/// 전용 CDP Chrome을 보장하고 (pid, window_id)를 JSON으로 반환.
/// 이미 떠 있으면 재사용, 아니면 spawn.
pub fn ensure() -> serde_json::Value {
    let port = cdp_port();
    // window_id 조회는 cua-driver(list_windows)에 의존하므로 데몬을 먼저 보장.
    crate::cua::ensure_daemon();

    // 이미 살아있으면 캐시된 pid 재사용.
    if cdp_alive(port) {
        if let Some(pid) = *STATE.lock().unwrap() {
            if let Some(wid) = window_id_for(pid) {
                return serde_json::json!({ "pid": pid, "window_id": wid, "port": port, "reused": true });
            }
        }
        // 포트는 살아있으나 pid 미상 → 리스너 소유 pid 조회.
        if let Some(pid) = listener_pid(port) {
            *STATE.lock().unwrap() = Some(pid);
            if let Some(wid) = window_id_for(pid) {
                return serde_json::json!({ "pid": pid, "window_id": wid, "port": port, "reused": true });
            }
        }
    }

    // spawn.
    let Some(chrome) = chrome_path() else {
        return serde_json::json!({ "error": "chrome.exe not found (set KMC_CHROME)" });
    };
    let profile = profile_dir();
    let _ = std::fs::create_dir_all(&profile);
    let spawn = std::process::Command::new(&chrome)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={profile}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("about:blank")
        .spawn();
    if let Err(e) = spawn {
        return serde_json::json!({ "error": format!("spawn chrome: {e}") });
    }

    // CDP 준비 대기 (최대 ~8s).
    for _ in 0..40 {
        if cdp_alive(port) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let Some(pid) = listener_pid(port) else {
        return serde_json::json!({ "error": "chrome launched but CDP port not listening" });
    };
    *STATE.lock().unwrap() = Some(pid);
    // 창이 뜰 시간을 조금 더 준 뒤 window_id 조회.
    let mut wid = None;
    for _ in 0..15 {
        wid = window_id_for(pid);
        if wid.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    match wid {
        Some(w) => serde_json::json!({ "pid": pid, "window_id": w, "port": port, "reused": false }),
        None => serde_json::json!({ "pid": pid, "port": port, "reused": false, "warning": "window_id not found yet" }),
    }
}

/// 지정 포트를 LISTEN하는 프로세스 pid (PowerShell Get-NetTCPConnection).
fn listener_pid(port: u16) -> Option<u32> {
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-NetTCPConnection -State Listen -LocalPort {port} -ErrorAction SilentlyContinue | Select -First 1 -Expand OwningProcess)"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()
}

/// cua-driver list_windows에서 pid의 top-level window_id 조회.
fn window_id_for(pid: u32) -> Option<u64> {
    let exe = crate::exec::cua_driver_path();
    let out = std::process::Command::new(&exe)
        .arg("call")
        .arg("list_windows")
        .arg("{}")
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let wins = v.get("windows").and_then(|w| w.as_array())?;
    // 해당 pid의, 크기가 있는(실제) 창 우선.
    let mut fallback = None;
    for w in wins {
        if w.get("pid").and_then(|p| p.as_u64()) == Some(pid as u64) {
            let wid = w.get("window_id").and_then(|i| i.as_u64());
            let has_size = w
                .get("bounds")
                .and_then(|b| b.get("width"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                > 0;
            if has_size {
                return wid;
            }
            fallback = fallback.or(wid);
        }
    }
    fallback
}

/// (선택) 학생 Chrome 바로가기를 전용 CDP 프로필/포트로 통일한다 — per-user, 관리자 불필요.
/// WTG 프로비저닝이 아닌 '일반 설치'에서도 "사용자 Chrome == AI가 CDP로 조작하는 Chrome"이
/// 되도록. `ensure()`와 동일한 `profile_dir()`/`cdp_port()`를 써서 바로가기와 agent가 한
/// 프로세스를 공유하게 만든다. 개발/개인 PC의 바로가기를 함부로 건드리지 않도록
/// `KMC_UNIFY_BROWSER`가 설정된 경우에만 동작한다(프로비저닝/인스톨러가 세팅).
pub fn unify() {
    let on = std::env::var("KMC_UNIFY_BROWSER").map(|v| !v.is_empty() && v != "0").unwrap_or(false);
    if !on {
        return;
    }
    if chrome_path().is_none() {
        tracing::info!("browser unify skipped (chrome not found)");
        return;
    }
    let prof = profile_dir();
    let port = cdp_port();
    let _ = std::fs::create_dir_all(&prof);
    // 백슬래시/따옴표 안전을 위해 임시 .ps1로 실행. 바로가기 조작은 WScript.Shell COM.
    let script = format!(
        r#"$ErrorActionPreference='SilentlyContinue'
$prof='{prof}'; $port={port}
$chrome=@("$env:ProgramFiles\Google\Chrome\Application\chrome.exe","${{env:ProgramFiles(x86)}}\Google\Chrome\Application\chrome.exe")|Where-Object{{Test-Path $_}}|Select-Object -First 1
if(-not $chrome){{exit 0}}
$chArgs="--remote-debugging-port=$port --user-data-dir=`"$prof`" --no-first-run --no-default-browser-check"
$w=New-Object -ComObject WScript.Shell
$cands=@(
 (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Google Chrome.lnk'),
 (Join-Path $env:USERPROFILE 'Desktop\Google Chrome.lnk'),
 'C:\Users\Public\Desktop\Google Chrome.lnk',
 (Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar\Google Chrome.lnk')
)
foreach($l in $cands){{ if(Test-Path $l){{ $s=$w.CreateShortcut($l); $s.Arguments=$chArgs; $s.Save() }} }}
$desk=Join-Path $env:USERPROFILE 'Desktop\Chrome.lnk'
$s=$w.CreateShortcut($desk); $s.TargetPath=$chrome; $s.Arguments=$chArgs; $s.Save()
"#
    );
    let tmp = std::env::temp_dir().join("kmc-unify.ps1");
    if std::fs::write(&tmp, script).is_err() {
        return;
    }
    let ok = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&tmp)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    tracing::info!(profile = %prof, port, success = ok, "browser unify (per-user shortcuts)");
}
