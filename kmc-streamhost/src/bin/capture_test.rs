//! R1 검증 바이너리: 주 모니터를 N초간 캡처해 mp4로 저장.
//! 사용법: streamhost-capture-test [output.mp4] [duration_secs]

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kmc_streamhost=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "capture-test.mp4".to_string());
    let duration: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    tracing::info!(%output, duration, "starting capture test");
    kmc_streamhost::record_primary_monitor(&output, duration)?;
    tracing::info!(%output, "done");
    Ok(())
}
