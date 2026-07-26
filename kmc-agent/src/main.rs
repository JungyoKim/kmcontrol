mod browser;
mod config;
mod cua;
mod exec;
mod provision;
mod run;
mod tailscale;
mod sysstat;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_agent=debug".into()),
        )
        .init();

    let state = provision::provision().await?;
    // tailnet 연결 대기(최대 20s). 설치와 `up`은 elevated 인스톨러 책임이고 agent는 조회만
    // 한다 — 비관리자 `up`은 UAC/로그인 GUI를 띄우고 실패하므로 절대 부르지 않는다.
    // Hello 전에 100.x를 확보해야 hub가 스트리밍 타겟을 tailnet 주소로 잡는다.
    tailscale::wait_ready(std::time::Duration::from_secs(20));
    // cua-driver 데몬을 보장(GUI/브라우저 자동화의 필수 백엔드) + 로그온 자동 기동 등록.
    cua::ensure_daemon();
    cua::enable_autostart();

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
