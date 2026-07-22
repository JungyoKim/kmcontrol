// kmc-proto JSON 형태의 수작업 미러. 필드명/태그는 crates/proto/src/lib.rs 와 정확히 일치해야 함.

export interface ProcessInfo {
  pid: number;
  name: string;
  cpu: number;
  mem_bytes: number;
}

export interface StatusReport {
  battery_percent: number | null;
  battery_charging: boolean | null;
  disk_free_bytes: number;
  disk_total_bytes: number;
  processes: ProcessInfo[];
  reported_at: string; // RFC3339
  encoder_ok: boolean | null; // Intel QSV 인코더 가용성(null=미확인/구버전)
}

export interface AgentView {
  agent_id: string; // uuid
  name: string;
  online: boolean;
  status: StatusReport | null;
  controlled_by: string | null;
  tailscale_addr: string | null;
}

export interface CommandResult {
  command_id: string;
  exit_code: number | null;
  stdout: string;
  stderr: string;
  error: string | null;
}

export type AlertLevel = "info" | "warning" | "critical";

export interface AlertPayload {
  agent_id: string;
  level: AlertLevel;
  message: string;
}
