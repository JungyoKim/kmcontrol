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
    // tailnet 연결 보장(네이티브 tailscaled). hub가 tailnet에서 도달되면 이 노드의 100.x가
    // 세션 주소/스트리밍 타겟으로 자동 사용됨. 설치는 elevated 인스톨러/provision 책임.
    tailscale::ensure_up(&state.name);
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
