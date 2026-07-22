use chrono::Utc;
use kmc_proto::{ProcessInfo, StatusReport};
use sysinfo::{Disks, ProcessesToUpdate, System};

use crate::config;

/// System 인스턴스를 재사용해 상태를 수집(문서 권고).
pub struct Collector {
    sys: System,
}

impl Collector {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Collector { sys }
    }

    pub fn collect(&mut self) -> StatusReport {
        // 프로세스 갱신 (CPU 사용률은 두 번째 리프레시부터 유효).
        self.sys.refresh_processes(ProcessesToUpdate::All, true);

        let mut processes: Vec<ProcessInfo> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcessInfo {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage(),
                mem_bytes: p.memory(),
            })
            .collect();
        // 메모리 상위 15개.
        processes.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes));
        processes.truncate(15);

        // 디스크: 시스템 드라이브(C:) 우선, 없으면 첫 디스크.
        let disks = Disks::new_with_refreshed_list();
        let (disk_total_bytes, disk_free_bytes) = pick_system_disk(&disks);

        let (battery_percent, battery_charging) = read_battery();

        StatusReport {
            battery_percent,
            battery_charging,
            disk_free_bytes,
            disk_total_bytes,
            processes,
            reported_at: Utc::now(),
        }
    }
}

fn pick_system_disk(disks: &Disks) -> (u64, u64) {
    // 마운트 지점이 C:\ 또는 / 인 디스크 우선.
    let mut chosen: Option<&sysinfo::Disk> = None;
    for d in disks.list() {
        let mp = d.mount_point().to_string_lossy();
        if mp.starts_with("C:") || mp == "/" {
            chosen = Some(d);
            break;
        }
    }
    let disk = chosen.or_else(|| disks.list().first());
    match disk {
        Some(d) => (d.total_space(), d.available_space()),
        None => (0, 0),
    }
}

fn read_battery() -> (Option<f32>, Option<bool>) {
    if let Some(fake) = config::fake_battery() {
        // 가짜 배터리: 방전 상태로 보고(Alert 경로 결정적 검증용).
        return (Some(fake), Some(false));
    }
    match battery::Manager::new() {
        Ok(manager) => match manager.batteries() {
            Ok(mut iter) => match iter.next() {
                Some(Ok(bat)) => {
                    let percent = bat.state_of_charge().value * 100.0;
                    let charging = matches!(bat.state(), battery::State::Charging | battery::State::Full);
                    (Some(percent), Some(charging))
                }
                _ => (None, None),
            },
            Err(_) => (None, None),
        },
        Err(_) => (None, None),
    }
}
