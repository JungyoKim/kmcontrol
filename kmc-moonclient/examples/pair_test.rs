//! 페어링 + serverinfo + launch 검증 (실행 중인 kmc-streamhost 대상).
//! 사용법: pair_test <address> <pin>
//! 페어링 중 호스트에 PIN을 제출해야 함: curl -X POST http://addr:47989/submit-pin -d "uniqueid=<UID>&pin=<PIN>"
//! 이 예제는 UID를 먼저 출력하므로, 별도 터미널에서 submit-pin 하거나 호스트 웹UI 사용.

use kmc_moonclient::{pair, Identity};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").try_init().ok();
    let args: Vec<String> = std::env::args().collect();
    let address = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1");
    let pin = args.get(2).map(|s| s.as_str()).unwrap_or("1234");
    let (http, https) = (47989u16, 47984u16);

    let identity = Identity::generate()?;
    println!("CLIENT_UNIQUEID={}", identity.unique_id);
    println!("PIN={pin}  (submit to host: /submit-pin uniqueid={}&pin={pin})", identity.unique_id);

    // serverinfo (페어링 전).
    let info = pair::query_server_info(&identity, address, http, https)?;
    println!("serverinfo: pair_status={} appversion={} codec={} current_game={}",
        info.pair_status, info.app_version, info.codec_mode_support, info.current_game);

    // 페어링. 호스트가 PIN을 기다리므로 이 호출은 submit-pin 될 때까지 블록.
    println!("pairing... (submit PIN to host now)");
    let host = pair::pair(&identity, address, http, https, pin)?;
    println!("PAIRED. server_cert fingerprint len={}", host.server_cert_pem.len());

    // launch.
    let launch = pair::launch(&identity, &host, 1920, 1080, 60, false)?;
    println!("LAUNCH OK: rtsp={}", launch.rtsp_session_url);
    println!("rikey[0..4]={:02x?} rikeyid_iv[0..4]={:02x?}", &launch.rikey[..4], &launch.rikey_iv[..4]);
    Ok(())
}
