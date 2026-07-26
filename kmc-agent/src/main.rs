mod browser;
mod config;
mod cua;
mod exec;
mod provision;
mod run;
mod tailscale;
mod sysstat;

use anyhow::Result;

/// 이 로그온 세션에서 agent 가 유일한 인스턴스임을 보장한다.
///
/// 중복 실행(자동시작 HKCU Run + 사용자가 직접 한 번 더 실행)이 실제로 발생했고,
/// 그때 뒤에 뜬 쪽은 GameStream 포트(47984/47989/48010, 47998~48000)를 전부
/// 선점당해 `GameStream host failed to start` 로 죽으면서도 제어 평면만 살아남아
/// 진단이 어려운 반쪽 상태가 됐다. 포트 충돌로 사후 감지하지 말고 부작용(cua 자동시작
/// 등록, 브라우저 통일, hub 접속) 이전에 먼저 끊는다.
///
/// 스코프가 `Local\`(로그온 세션)인 이유: agent 는 비관리자로 도니 `Global\` 은
/// SeCreateGlobalPrivilege 가 없어 만들지 못한다.
#[cfg(windows)]
fn claim_single_instance() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;

    unsafe {
        let Ok(_handle) = CreateMutexW(None, false, w!("Local\\kmc-agent-singleton")) else {
            // 뮤텍스를 못 만드는 환경이면 가드를 포기하고 그대로 진행한다
            // (가드는 안전장치일 뿐, 이것 때문에 agent 가 안 뜨면 더 나쁘다).
            return true;
        };
        // 이미 누가 쥐고 있으면 우리가 두 번째다. CreateMutexW 는 이 경우에도 Ok 를
        // 주므로(기존 객체 핸들) 반드시 GetLastError 로 판별해야 한다.
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return false;
        }
        // 핸들을 닫지 않고 흘린다 = 뮤텍스가 프로세스 종료까지 유지된다.
        // `HANDLE` 은 Copy 이고 Drop 이 없어 별도 조치가 필요 없다(=`mem::forget` 불필요).
        true
    }
}

#[cfg(not(windows))]
fn claim_single_instance() -> bool {
    true
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_agent=debug".into()),
        )
        .init();

    if !claim_single_instance() {
        tracing::warn!(
            "kmc-agent 가 이미 실행 중이다 - 이 인스턴스는 종료한다 \
             (중복 실행 시 GameStream 포트를 선점당해 스트리밍 호스트가 죽는다)"
        );
        return Ok(());
    }

    let state = provision::provision().await?;
    // tailnet 연결 대기(최대 20s). 설치와 `up`은 elevated 인스톨러 책임이고 agent는 조회만
    // 한다 — 비관리자 `up`은 UAC/로그인 GUI를 띄우고 실패하므로 절대 부르지 않는다.
    // Hello 전에 100.x를 확보해야 hub가 스트리밍 타겟을 tailnet 주소로 잡는다.
    tailscale::wait_ready(std::time::Duration::from_secs(20));
    // cua-driver 데몬을 보장(GUI/브라우저 자동화의 필수 백엔드).
    // 스케줄 작업 등록은 하지 않는다 — UAC 승격을 부른다(`cua.rs` 모듈 문서 참고).
    cua::ensure_daemon();

    // (선택) 학생 Chrome 바로가기를 전용 CDP 프로필/포트로 통일 — WTG가 아닌 일반 설치에서도
    // "사용자 Chrome == AI가 조작하는 Chrome"이 되게. KMC_UNIFY_BROWSER 설정 시에만 동작.
    browser::unify();

    // 자체 GameStream 호스트를 in-process로 기동(Sunshine 대체). admin이 세션 승인 후
    // 이 노트북 주소로 직접 P2P 연결해 화면/오디오/입력을 주고받는다. hub는 영상을 프록시하지 않는다.
    // 실패해도(예: GPU/캡처 불가) agent 제어플레인은 계속 동작하도록 격리.
    match kmc_streamhost::host::start(kmc_streamhost::host::HostConfig::default()).await {
        Ok(rtsp) => {
            // rtsp 핸들을 leak해 프로세스 수명 내내 호스트를 유지(지속 파이프라인).
            std::mem::forget(rtsp);
            tracing::info!("GameStream host started (in-process)");
        }
        Err(e) => tracing::error!(error=%e, "GameStream host failed to start (control plane continues)"),
    }

    run::run(state).await
}
