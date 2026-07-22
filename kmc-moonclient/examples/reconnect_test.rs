//! 재연결 검증: 저장된 신원으로 페어링 건너뛰고 serverinfo→launch만.
//! 1회차: 페어링 후 신원+호스트 저장. 2회차 이후: 저장분 로드해 재연결.
//! 사용법: reconnect_test <address> <pin>

use kmc_moonclient::{pair, Identity};
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let address = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1");
    let pin = args.get(2).map(|s| s.as_str()).unwrap_or("1234");
    let (http, https) = (47989u16, 47984u16);

    let id_path = Path::new("D:/Users/G433m/Documents/kmcontrol/kmc-moonclient/client-identity.json");
    let host_path = Path::new("D:/Users/G433m/Documents/kmcontrol/kmc-moonclient/paired-host.json");

    let identity = Identity::load_or_generate(id_path)?;
    println!("identity uid={} (loaded or generated)", identity.unique_id);

    let info = pair::query_server_info(&identity, address, http, https)?;
    println!("serverinfo: pair_status={} appversion={}", info.pair_status, info.app_version);

    let host = if info.pair_status && host_path.exists() {
        // 이미 페어링됨 → 저장된 호스트 재사용 (재연결 경로).
        println!(">>> RECONNECT PATH: skipping pairing, using stored host");
        let bytes = std::fs::read(host_path)?;
        serde_json::from_slice::<pair::PairedHost>(&bytes)?
    } else {
        // 최초 페어링.
        println!(">>> FIRST PAIR PATH (submit PIN to host now)");
        println!("    submit: uniqueid={}&pin={pin}", identity.unique_id);
        let host = pair::pair(&identity, address, http, https, pin)?;
        std::fs::write(host_path, serde_json::to_vec_pretty(&host)?)?;
        println!("paired + saved host");
        host
    };

    let launch = pair::launch(&identity, &host, 1920, 1080, 60, info.current_game != 0)?;
    println!("LAUNCH OK: rtsp={}", launch.rtsp_session_url);
    Ok(())
}
