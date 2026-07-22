use kmc_proto::{CommandKind, CommandRequest, CommandResult};

/// 명령 실행: PowerShell 셸아웃 + GUI 자동화(cua-driver).
pub async fn run(req: CommandRequest) -> CommandResult {
    match req.kind {
        CommandKind::PowerShell { script } => run_powershell(req.command_id, &script).await,
        CommandKind::Gui { tool, args } => run_gui(req.command_id, &tool, &args).await,
    }
}

async fn run_powershell(command_id: uuid::Uuid, script: &str) -> CommandResult {
    let output = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .await;

    match output {
        Ok(out) => CommandResult {
            command_id,
            exit_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            error: None,
        },
        Err(e) => CommandResult {
            command_id,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("spawn powershell: {e}")),
        },
    }
}

/// GUI 자동화: cua-driver CLI(`cua-driver call <tool> <json>`) 셸아웃.
/// 백그라운드 조작(포커스 안 뺏음)은 cua-driver가 보장. 결과 JSON을 stdout으로 반환.
async fn run_gui(command_id: uuid::Uuid, tool: &str, args: &serde_json::Value) -> CommandResult {
    // 특수 로컬 도구: 전용 CDP Chrome을 보장하고 pid+window_id를 반환(agent가 처리).
    if tool == "kmc_ensure_browser" {
        let v = crate::browser::ensure();
        return CommandResult {
            command_id,
            exit_code: Some(0),
            stdout: v.to_string(),
            stderr: String::new(),
            error: None,
        };
    }
    let exe = cua_driver_path();
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    let mut output = cua_call(&exe, tool, &args_json).await;
    // 데몬이 죽어 있으면(모든 조작 실패) 되살리고 1회 재시도 — 백엔드 자가교정(LLM은 실패를 못 봄).
    if daemon_down(&output) {
        let revived = tokio::task::spawn_blocking(crate::cua::ensure_daemon).await.unwrap_or(false);
        tracing::warn!(revived, "cua-driver daemon was down; retried");
        output = cua_call(&exe, tool, &args_json).await;
    }

    match output {
        Ok(out) => CommandResult {
            command_id,
            exit_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            error: None,
        },
        Err(e) => CommandResult {
            command_id,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("spawn cua-driver ({exe}): {e}")),
        },
    }
}

/// `cua-driver call <tool> <json>` 셸아웃.
async fn cua_call(exe: &str, tool: &str, args_json: &str) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new(exe)
        .arg("call")
        .arg(tool)
        .arg(args_json)
        .output()
        .await
}

/// 호출 결과가 "데몬 미기동" 신호인지.
fn daemon_down(output: &std::io::Result<std::process::Output>) -> bool {
    let Ok(o) = output else { return false };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    combined.contains("daemon is not running")
}

/// cua-driver 실행 파일 경로. env override(KMC_CUA_DRIVER) 우선, 없으면 표준 설치 경로.
pub fn cua_driver_path() -> String {
    if let Ok(p) = std::env::var("KMC_CUA_DRIVER") {
        return p;
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return format!("{local}\\Programs\\Cua\\cua-driver\\bin\\cua-driver.exe");
    }
    "cua-driver.exe".to_string()
}
