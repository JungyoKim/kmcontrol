//! 동적 비트레이트 적응 (AIMD).
//!
//! 클라이언트(moonlight-common-c)는 패킷 손실이 발생한 프레임에 대해서만
//! `SS_FRAME_FEC_STATUS`(control 0x5502)를 보낸다 — 무손실 네트워크에서는 아무것도 안 온다.
//! 따라서 "메시지 도착 = 지금 손실 중"이라는 신호로 삼아 TCP 식 AIMD 로 조정한다:
//!   - 손실 신호 → 곱셈 감소(target *= DECREASE_FACTOR), 버스트 방지로 스로틀.
//!   - 무손실 지속 → 시간 기반 덧셈 증가(초당 max 의 일부).
//!
//! pull 방식: 인코드 루프가 매 반복 `poll_target()` 을 호출하면 그 안에서 회복 증가가
//! 자연히 일어난다(별도 스레드 불필요). control 채널은 `on_loss()` 만 호출한다.
//!
//! QSV 인코더는 런타임 재구성이 불안정하므로(드라이버 크래시 보고 다수) 실제 비트레이트
//! 반영은 인코더 컨텍스트 재생성으로 처리한다(capture.rs). 이 컨트롤러는 목표값만 관리.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

/// 손실 시 목표를 이 비율로 낮춘다(60% 로 = -40%).
const DECREASE_FACTOR: f64 = 0.6;
/// 감소 스로틀: 손실 프레임이 연속으로 와도 이 간격 안에서는 한 번만 낮춘다.
const DECREASE_THROTTLE_MS: u128 = 400;
/// 마지막 손실 이후 이만큼 지나야 회복(증가)을 시작한다.
const RECOVER_DELAY_MS: u128 = 1500;
/// 회복 증가 주기.
const RECOVER_INTERVAL_MS: u128 = 1000;
/// 회복 스텝 = ceiling(max) 의 이 비율만큼 매 주기 덧셈(≈20s 만에 floor→max).
const RECOVER_STEP_FRAC: f64 = 0.05;
/// FEC parity 하한(%): 무손실~경미 손실 시. Moonlight 기본과 동일.
const FEC_MIN_PCT: u32 = 20;
/// FEC parity 상한(%): 심한 손실 시. parity 를 늘려 버스트 손실을 복구한다.
/// (셀룰러/핫스팟처럼 손실률이 FEC 복구 한계를 넘나드는 회선 대응.)
const FEC_MAX_PCT: u32 = 50;
/// 손실률 EWMA 평활 계수(0~1). 클수록 최근 값에 민감. 프레임당 손실은 튀므로 완만하게.
const LOSS_EWMA_ALPHA: f64 = 0.3;
/// 이 EWMA 손실률(0~1)에서 FEC 가 상한에 도달한다. 그 이하는 선형 보간.
const FEC_SATURATION_LOSS: f64 = 0.30;

struct Timing {
    last_loss: Option<Instant>,
    last_decrease: Instant,
    last_recover: Instant,
}

/// 스로틀 윈도우보다 충분히 과거인 시각(첫 손실/회복이 즉시 통과하도록 초기화용).
fn stale_instant() -> Instant {
    Instant::now()
        .checked_sub(std::time::Duration::from_secs(3600))
        .unwrap_or_else(Instant::now)
}

/// 목표 비트레이트(bps)를 AIMD 로 관리. Arc 로 control 채널과 인코드 루프가 공유한다.
pub struct BitrateController {
    /// 현재 목표 비트레이트(bps). 0 = 미설정(아직 PLAY 협상 전).
    target: AtomicU32,
    /// 하한(bps). 협상 대역폭의 일부.
    min: AtomicU32,
    /// 상한(bps) = 협상된 비트레이트(ceiling).
    max: AtomicU32,
    timing: Mutex<Timing>,
    /// 최근 손실률 EWMA(0.0~1.0) 를 1e6 스케일 고정소수로 저장(원자적). FEC% 계산에 사용.
    loss_ewma_ppm: AtomicU32,
}

impl BitrateController {
    pub fn new() -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            target: AtomicU32::new(0),
            min: AtomicU32::new(0),
            max: AtomicU32::new(0),
            timing: Mutex::new(Timing {
                last_loss: None,
                last_decrease: now,
                last_recover: now,
            }),
            loss_ewma_ppm: AtomicU32::new(0),
        })
    }

    /// PLAY 협상 후 경계값 설정. `ceiling` = 협상된 비트레이트(상한). floor 는 ceiling 의 1/8
    /// (하한 500kbps 보장). 목표는 ceiling 에서 시작(좋은 네트워크 가정, 손실 시 낮아짐).
    pub fn configure(&self, ceiling_bps: u32) {
        let ceiling = ceiling_bps.max(500_000);
        let floor = (ceiling / 8).max(500_000).min(ceiling);
        self.max.store(ceiling, Ordering::Release);
        self.min.store(floor, Ordering::Release);
        self.target.store(ceiling, Ordering::Release);
        let mut t = self.timing.lock();
        // 타이머는 과거로 초기화 — 스트림 시작 직후 첫 손실이 스로틀에 막히지 않도록.
        t.last_loss = None;
        t.last_decrease = stale_instant();
        t.last_recover = stale_instant();
        self.loss_ewma_ppm.store(0, Ordering::Release);
    }

    /// 손실 신호(FEC status 도착). `loss_fraction` = 0.0~1.0(수신 실패 패킷 비율).
    /// EWMA 손실률을 갱신(FEC% 계산용)하고, 스로틀 밖이면 비트레이트를 곱셈 감소한다.
    pub fn on_loss(&self, loss_fraction: f32) {
        let max = self.max.load(Ordering::Acquire);
        if max == 0 {
            return; // 미설정(협상 전).
        }
        // EWMA 갱신은 스로틀과 무관하게 항상 수행 — FEC 는 모든 손실 신호를 반영해야 한다.
        let lf = loss_fraction.clamp(0.0, 1.0) as f64;
        let prev = self.loss_ewma_ppm.load(Ordering::Acquire) as f64 / 1e6;
        let ewma = LOSS_EWMA_ALPHA * lf + (1.0 - LOSS_EWMA_ALPHA) * prev;
        self.loss_ewma_ppm.store((ewma * 1e6) as u32, Ordering::Release);

        let min = self.min.load(Ordering::Acquire);
        let mut t = self.timing.lock();
        let now = Instant::now();
        t.last_loss = Some(now);
        if now.duration_since(t.last_decrease).as_millis() < DECREASE_THROTTLE_MS {
            return; // 버스트 스로틀 — 이미 최근에 낮췄음(EWMA 는 위에서 이미 갱신).
        }
        // 손실이 심하면 더 공격적으로(0.5), 경미하면 완만하게(0.75). 기본 0.6.
        let factor = if loss_fraction >= 0.3 {
            0.5
        } else if loss_fraction <= 0.05 {
            0.75
        } else {
            DECREASE_FACTOR
        };
        let cur = self.target.load(Ordering::Acquire);
        let next = ((cur as f64 * factor) as u32).max(min);
        self.target.store(next, Ordering::Release);
        t.last_decrease = now;
        tracing::info!(
            loss = loss_fraction,
            from = cur,
            to = next,
            "bitrate: loss → decrease"
        );
    }

    /// 목표 비트레이트를 읽는다. 무손실이 지속되면 이 호출 안에서 시간 기반 덧셈 회복을 수행한다.
    /// 인코드 루프가 매 반복 호출한다(pull 방식).
    pub fn poll_target(&self) -> u32 {
        let max = self.max.load(Ordering::Acquire);
        if max == 0 {
            return 0;
        }
        let cur = self.target.load(Ordering::Acquire);
        if cur >= max {
            return cur;
        }
        let now = Instant::now();
        let mut t = self.timing.lock();
        // 마지막 손실 후 충분히 지났고, 회복 주기가 됐으면 한 스텝 올린다.
        let since_loss = t.last_loss.map_or(u128::MAX, |l| now.duration_since(l).as_millis());
        if since_loss < RECOVER_DELAY_MS {
            return cur;
        }
        if now.duration_since(t.last_recover).as_millis() < RECOVER_INTERVAL_MS {
            return cur;
        }
        let step = ((max as f64 * RECOVER_STEP_FRAC) as u32).max(250_000);
        let next = cur.saturating_add(step).min(max);
        self.target.store(next, Ordering::Release);
        t.last_recover = now;
        tracing::debug!(from = cur, to = next, "bitrate: recover → increase");
        next
    }

    /// 현재 목표 FEC parity 백분율(20~50). EWMA 손실률에 비례해 선형 상향.
    /// 손실이 마지막으로 관측된 지 오래면 EWMA 를 시간 기반으로 감쇠시켜 FEC 를 하한으로 되돌린다
    /// (무손실 신호는 안 오므로 여기서 능동 감쇠). 비디오 송출 루프가 프레임마다 호출한다.
    pub fn poll_fec_percentage(&self) -> u32 {
        // 시간 기반 감쇠: 마지막 손실 이후 경과에 따라 EWMA 를 지수 감쇠(반감기 ~2s).
        let since_loss_ms = {
            let t = self.timing.lock();
            t.last_loss.map_or(u128::MAX, |l| Instant::now().duration_since(l).as_millis())
        };
        let mut ewma = self.loss_ewma_ppm.load(Ordering::Acquire) as f64 / 1e6;
        if since_loss_ms > 500 && ewma > 0.0 {
            // 반감기 2000ms: factor = 0.5^(dt/2000). 마지막 손실 기준으로 계산.
            let decay = 0.5_f64.powf(since_loss_ms as f64 / 2000.0);
            ewma *= decay;
            self.loss_ewma_ppm.store((ewma * 1e6) as u32, Ordering::Release);
        }
        // EWMA 0 → FEC_MIN, EWMA≥saturation → FEC_MAX, 사이는 선형.
        let frac = (ewma / FEC_SATURATION_LOSS).clamp(0.0, 1.0);
        FEC_MIN_PCT + ((FEC_MAX_PCT - FEC_MIN_PCT) as f64 * frac) as u32
    }
}

impl Default for BitrateController {
    fn default() -> Self {
        // Arc 없이 단독 인스턴스(테스트/편의). 실제 공유는 new() 사용.
        let now = Instant::now();
        Self {
            target: AtomicU32::new(0),
            min: AtomicU32::new(0),
            max: AtomicU32::new(0),
            timing: Mutex::new(Timing {
                last_loss: None,
                last_decrease: now,
                last_recover: now,
            }),
            loss_ewma_ppm: AtomicU32::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_is_noop() {
        let c = BitrateController::default();
        assert_eq!(c.poll_target(), 0);
        c.on_loss(0.5);
        assert_eq!(c.poll_target(), 0);
    }

    #[test]
    fn configure_starts_at_ceiling() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        assert_eq!(c.poll_target(), 20_000_000);
    }

    #[test]
    fn loss_decreases_multiplicatively() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.on_loss(0.1); // 기본 0.6 → 12M
        assert_eq!(c.poll_target(), 12_000_000);
    }

    #[test]
    fn severe_loss_drops_harder() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.on_loss(0.5); // 0.5 factor → 10M
        assert_eq!(c.poll_target(), 10_000_000);
    }

    #[test]
    fn decrease_is_throttled() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.on_loss(0.1); // 20M → 12M
        c.on_loss(0.1); // 스로틀 내 → 변화 없음
        assert_eq!(c.poll_target(), 12_000_000);
    }

    #[test]
    fn floor_is_respected() {
        let c = BitrateController::default();
        c.configure(4_000_000); // floor = 500k
        // 여러 번 강제로 낮추되 스로틀 우회를 위해 타이밍 조작.
        for _ in 0..20 {
            {
                let mut t = c.timing.lock();
                t.last_decrease = Instant::now() - std::time::Duration::from_secs(10);
            }
            c.on_loss(0.5);
        }
        assert_eq!(c.poll_target(), 500_000);
    }

    #[test]
    fn recovery_increases_after_delay() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.on_loss(0.1); // → 12M
        assert_eq!(c.poll_target(), 12_000_000);
        // 손실/회복 타이밍을 과거로 밀어 회복 조건 충족.
        {
            let mut t = c.timing.lock();
            let past = Instant::now() - std::time::Duration::from_secs(5);
            t.last_loss = Some(past);
            t.last_recover = past;
        }
        let after = c.poll_target();
        assert!(after > 12_000_000, "expected recovery increase, got {after}");
        // 스텝 = max*0.05 = 1M → 13M.
        assert_eq!(after, 13_000_000);
    }

    #[test]
    fn recovery_caps_at_ceiling() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.on_loss(0.5); // → 10M
        for _ in 0..100 {
            {
                let mut t = c.timing.lock();
                let past = Instant::now() - std::time::Duration::from_secs(5);
                t.last_loss = Some(past);
                t.last_recover = past;
            }
            c.poll_target();
        }
        assert_eq!(c.poll_target(), 20_000_000);
    }

    #[test]
    fn fec_default_is_min_when_clean() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        assert_eq!(c.poll_fec_percentage(), FEC_MIN_PCT);
    }

    #[test]
    fn fec_rises_under_sustained_loss() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        // 심한 손실을 여러 번 먹여 EWMA 를 saturation 이상으로 올린다.
        for _ in 0..10 {
            c.on_loss(0.5);
        }
        let fec = c.poll_fec_percentage();
        assert!(fec > FEC_MIN_PCT, "expected FEC above min under loss, got {fec}");
        assert!(fec <= FEC_MAX_PCT, "FEC must not exceed max, got {fec}");
    }

    #[test]
    fn fec_saturates_at_max() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        // 손실 1.0 을 반복 → EWMA → 1.0 >> saturation(0.3) → FEC_MAX.
        for _ in 0..30 {
            c.on_loss(1.0);
        }
        assert_eq!(c.poll_fec_percentage(), FEC_MAX_PCT);
    }

    #[test]
    fn fec_decays_after_loss_stops() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        for _ in 0..30 {
            c.on_loss(1.0);
        }
        assert_eq!(c.poll_fec_percentage(), FEC_MAX_PCT);
        // 손실을 과거로 밀어 시간 감쇠가 작동하게 한다(반감기 2s → 10s 후 거의 0).
        {
            let mut t = c.timing.lock();
            t.last_loss = Some(Instant::now() - std::time::Duration::from_secs(10));
        }
        let fec = c.poll_fec_percentage();
        assert!(fec < FEC_MAX_PCT, "FEC should decay after loss stops, got {fec}");
    }

    #[test]
    fn fec_unconfigured_returns_min() {
        let c = BitrateController::default();
        // 미설정이어도 안전한 하한을 반환(패킷타이저가 항상 유효 값을 받도록).
        assert_eq!(c.poll_fec_percentage(), FEC_MIN_PCT);
    }
}
