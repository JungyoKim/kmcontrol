//! R2 검증 바이너리: GameStream 호스트를 기동해 Moonlight 클라이언트 페어링/협상을 수용.
//! 종료: Ctrl+C.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_streamhost=debug".into()),
        )
        .init();

    // 전역 panic hook: 어떤 스레드가 panic해도 로그만 남기고 프로세스는 유지.
    // (Rust 기본 unwind라 스레드만 죽지만, 명시적으로 로깅해 진단을 돕는다.)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("PANIC (isolated, process continues): {info}");
        default_hook(info);
    }));

    let config = kmc_streamhost::host::HostConfig::default();
    let rtsp = kmc_streamhost::host::start(config).await?;

    tracing::info!("host running — pair from Moonlight, then Ctrl+C to inspect negotiated context");
    tokio::signal::ctrl_c().await?;

    if let Some(ctx) = rtsp.last_context() {
        tracing::info!(?ctx, "last negotiated stream context");
    } else {
        tracing::info!("no stream context negotiated (no PLAY reached)");
    }
    Ok(())
}
