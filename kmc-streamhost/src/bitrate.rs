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
/// 마지막 손실 이후 이만큼 지나야 회복(증가)을 시작한다. 셀룰러 진동 방지를 위해 길게(5s):
/// 손실 직후 성급히 올리면 회선이 회복 안 된 상태에서 재손실 → 비트레이트 진동 → freeze/burst.
const RECOVER_DELAY_MS: u128 = 5000;
/// 회복 증가 주기(느리게).
const RECOVER_INTERVAL_MS: u128 = 2000;
/// 회복 스텝 = ceiling(max) 의 이 비율만큼 매 주기 덧셈. 작게(0.03) 잡아 완만히 상승.
const RECOVER_STEP_FRAC: f64 = 0.03;
/// 이 EWMA 손실률 이상이면 회복(증가)을 아예 보류한다 — 잔여 손실이 있으면 올리지 않는다.
const RECOVER_LOSS_GATE: f64 = 0.02;
/// 손실 없이 이만큼 지나면 학습 상한을 재탐색(느린 상향). 30s 로 길게 — 자주 한계를 안 건드리게.
const REPROBE_AFTER_MS: u128 = 30000;
/// FEC parity 하한(%): 무손실~경미 손실 시. Moonlight 기본보다 약간 낮춰 평상시 전송량 절감.
const FEC_MIN_PCT: u32 = 15;
/// FEC parity 상한(%): 경미한 랜덤 손실 복구용. 혼잡 회선에선 FEC 증가가 오히려 독이므로
/// 상한을 25% 로 억제한다(과거 50% 는 shard 2배 → 혼잡 악화 → fps 붕괴를 유발했다).
/// 심한 손실의 정답은 parity 증가가 아니라 비트레이트 하향이다.
const FEC_MAX_PCT: u32 = 25;
/// 손실률 EWMA 평활 계수(0~1). 클수록 최근 값에 민감. 프레임당 손실은 튀므로 완만하게.
const LOSS_EWMA_ALPHA: f64 = 0.3;
/// 이 EWMA 손실률(0~1)에서 FEC 가 상한에 도달한다. 그 이하는 선형 보간.
/// 낮게(0.10) 잡아 경미한 손실엔 빠르게 반응하되 상한 자체가 25% 라 폭증하지 않는다.
const FEC_SATURATION_LOSS: f64 = 0.10;

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
    /// 학습된 안전 상한(bps): 손실로 붕괴한 지점 기록. 회복은 이 값을 넘지 않아 톱니파를 막는다.
    /// 손실 없이 오래 안정되면 서서히 상향(회선이 좋아졌을 수도 있으므로 재탐색).
    learned_ceiling: AtomicU32,
    /// 재탐색 상한(bps): 관측된 붕괴 지점들의 상한. reprobe 가 learned_ceiling 을 이 값 위로는
    /// 못 올린다 — 5M 회선에서 20M 까지 무한 상승하던 톱니파를 막는다. 초기엔 max(제약 없음),
    /// 손실 붕괴 시 붕괴 지점의 1.2배로 낮아진다(그 이상은 또 무너질 게 뻔하므로 탐색 안 함).
    probe_cap: AtomicU32,
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
            learned_ceiling: AtomicU32::new(0),
            probe_cap: AtomicU32::new(0),
        })
    }

    pub fn configure(&self, ceiling_bps: u32) {
        let ceiling = ceiling_bps.max(1_000_000);
        // floor 는 절대 하한(1Mbps) — ceiling/8 은 고대역 협상 시 6M+ 로 과도해져 혼잡 회선에서
        // 못 내려가는 문제가 있었다. 셀룰러/핫스팟이 감당할 최저치까지 내려가게 한다.
        let floor = 1_000_000u32.min(ceiling);
        // 시작 목표는 보수적으로(3Mbps 또는 ceiling/4 중 작은 값). ceiling/2 는 초반에 회선 용량을
        // 모른 채 과전송해 큰 손실 붕괴(16M→5M 같은)를 유발했다. 낮게 시작해 회복 로직이 천천히 올린다.
        let start = 3_000_000u32.min(ceiling / 4).max(floor).min(ceiling);
        self.max.store(ceiling, Ordering::Release);
        self.min.store(floor, Ordering::Release);
        self.target.store(start, Ordering::Release);
        let mut t = self.timing.lock();
        // 타이머는 과거로 초기화 — 스트림 시작 직후 첫 손실이 스로틀에 막히지 않도록.
        t.last_loss = None;
        t.last_decrease = stale_instant();
        t.last_recover = Instant::now();
        self.loss_ewma_ppm.store(0, Ordering::Release);
        // 학습 상한/재탐색 상한을 ceiling 으로 초기화(아직 실패 미관측 → 전체 대역 탐색 허용).
        self.learned_ceiling.store(ceiling, Ordering::Release);
        self.probe_cap.store(ceiling, Ordering::Release);
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
        // 학습 상한 갱신: 유의미한 손실(≥10%)이 난 현재 비트레이트는 지속 불가능하다고 보고
        // 그 85% 를 안전 상한으로 기록(하향 래칫만 — 더 낮은 실패점만 반영). 회복이 이 위로
        // 다시 올라가 또 무너지는 톱니파를 막는다. floor 밑으로는 안 내림.
        if loss_fraction >= 0.10 {
            // 0.70: 실패 지점의 70% 로 안전 상한 설정(여유 크게). 85% 는 회선 한계에 너무 붙어
            // 지속 손실→마우스 잔상(부분 프레임)을 유발했다. 여유를 둬 손실 없는 대역에서 안정.
            let collapse = (cur as f64 * 0.70) as u32;
            let prev_lc = self.learned_ceiling.load(Ordering::Acquire);
            let new_lc = collapse.max(min).min(prev_lc);
            self.learned_ceiling.store(new_lc, Ordering::Release);
            // 재탐색 상한도 낮춘다: 붕괴 지점(cur)의 1.2배까지만 이후 탐색 허용. 이래야 reprobe 가
            // 5M 회선에서 20M 까지 무한히 기어올라 또 무너지는 것(톱니파)을 근본적으로 막는다.
            let new_pc = ((cur as f64 * 1.2) as u32).max(new_lc).min(self.probe_cap.load(Ordering::Acquire));
            self.probe_cap.store(new_pc, Ordering::Release);
        }
        tracing::info!(
            loss = loss_fraction,
            from = cur,
            to = next,
            learned_ceiling = self.learned_ceiling.load(Ordering::Acquire),
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
        let now = Instant::now();
        let mut t = self.timing.lock();
        let since_loss = t.last_loss.map_or(u128::MAX, |l| now.duration_since(l).as_millis());
        if since_loss < RECOVER_DELAY_MS {
            return cur;
        }
        // 잔여 손실 게이트: EWMA 손실이 남아있으면 회복하지 않는다(진동 방지).
        let ewma = self.loss_ewma_ppm.load(Ordering::Acquire) as f64 / 1e6;
        if ewma >= RECOVER_LOSS_GATE {
            return cur;
        }
        if now.duration_since(t.last_recover).as_millis() < RECOVER_INTERVAL_MS {
            return cur;
        }
        // 재탐색: 손실 없이 오래(REPROBE_AFTER) 안정되면 학습 상한을 조금씩 올린다(회선이
        // 좋아졌을 수 있으므로). 단, probe_cap(붕괴 지점 기반) 을 넘지 않는다 — 무한 상승 금지.
        let probe_cap = self.probe_cap.load(Ordering::Acquire);
        let mut lc = self.learned_ceiling.load(Ordering::Acquire);
        if lc < probe_cap && since_loss >= REPROBE_AFTER_MS {
            lc = ((lc as f64 * 1.02) as u32).min(probe_cap);
            self.learned_ceiling.store(lc, Ordering::Release);
        }
        // 회복 상한 = 학습된 안전 상한(협상 max 가 아니라). 여기서 멈춰 톱니파를 막는다.
        let cap = lc.min(max);
        if cur >= cap {
            return cur;
        }
        // 회복 스텝: max 의 3% 와 현재값의 10% 중 큰 값. 저대역(예 1M)에 갇혔을 때 현재 비례
        // 스텝으로 빠르게 제 대역까지 올라오고(1M→1.1M→1.2M... 가 아니라 +10%씩), 고대역에선
        // max 비례로 완만히. 손실 나면 즉시 하향되므로 공격적 회복이 안전하다.
        let step = ((max as f64 * RECOVER_STEP_FRAC) as u32).max((cur as f64 * 0.10) as u32).max(250_000);
        let next = cur.saturating_add(step).min(cap);
        self.target.store(next, Ordering::Release);
        t.last_recover = now;
        tracing::debug!(from = cur, to = next, cap, "bitrate: recover → increase");
        next
    }

    /// 현재 목표 FEC parity 백분율(15~25). EWMA 손실률에 비례하되, **혼잡 인지**:
    /// 비트레이트가 floor 근처(대역폭 여유 없음)면 FEC 를 하한으로 억제한다 — 이 경우 손실은
    /// 랜덤이 아니라 혼잡 때문이라 parity 증가는 전송량만 늘려 악화시킨다(측정으로 확인).
    /// FEC 는 "대역폭 여유가 있는데 랜덤 손실이 있을 때"만 올린다. 프레임마다 호출.
    pub fn poll_fec_percentage(&self) -> u32 {
        // 시간 기반 감쇠: 마지막 손실 이후 경과에 따라 EWMA 를 지수 감쇠(반감기 ~2s).
        let since_loss_ms = {
            let t = self.timing.lock();
            t.last_loss.map_or(u128::MAX, |l| Instant::now().duration_since(l).as_millis())
        };
        let mut ewma = self.loss_ewma_ppm.load(Ordering::Acquire) as f64 / 1e6;
        if since_loss_ms > 500 && ewma > 0.0 {
            let decay = 0.5_f64.powf(since_loss_ms as f64 / 2000.0);
            ewma *= decay;
            self.loss_ewma_ppm.store((ewma * 1e6) as u32, Ordering::Release);
        }
        // 혼잡 게이트: target 이 floor 의 1.5배 이하(=대역폭 여유 거의 없음)면 FEC 최소.
        // 이때 손실은 혼잡 신호이므로 비트레이트 하향(poll_target)에 맡기고 parity 는 안 늘린다.
        let target = self.target.load(Ordering::Acquire);
        let min = self.min.load(Ordering::Acquire);
        if min == 0 || target <= min * 3 / 2 {
            return FEC_MIN_PCT;
        }
        // 여유가 있을 때만: EWMA 0 → FEC_MIN, EWMA≥saturation → FEC_MAX, 사이는 선형.
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
            learned_ceiling: AtomicU32::new(0),
            probe_cap: AtomicU32::new(0),
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
    fn configure_starts_conservative() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        // 시작은 보수적으로 min(3M, ceiling/4) = 3M (초반 과전송 붕괴 방지).
        assert_eq!(c.poll_target(), 3_000_000);
    }

    #[test]
    fn loss_decreases_multiplicatively() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.target.store(10_000_000, Ordering::Release); // 감소 계약을 결정적으로 테스트
        c.on_loss(0.1); // 기본 0.6 → 6M
        assert_eq!(c.poll_target(), 6_000_000);
    }

    #[test]
    fn severe_loss_drops_harder() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.target.store(10_000_000, Ordering::Release);
        c.on_loss(0.5); // 0.5 factor → 5M
        assert_eq!(c.poll_target(), 5_000_000);
    }

    #[test]
    fn decrease_is_throttled() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.target.store(10_000_000, Ordering::Release);
        c.on_loss(0.1); // 10M → 6M
        c.on_loss(0.1); // 스로틀 내 → 변화 없음
        assert_eq!(c.poll_target(), 6_000_000);
    }

    #[test]
    fn floor_is_respected() {
        let c = BitrateController::default();
        c.configure(4_000_000); // floor = 1M (절대 하한)
        for _ in 0..20 {
            {
                let mut t = c.timing.lock();
                t.last_decrease = Instant::now() - std::time::Duration::from_secs(10);
            }
            c.on_loss(0.5);
        }
        assert_eq!(c.poll_target(), 1_000_000);
    }

    #[test]
    fn recovery_increases_after_delay() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.target.store(10_000_000, Ordering::Release);
        c.on_loss(0.1); // → 6M
        assert_eq!(c.poll_target(), 6_000_000);
        {
            let mut t = c.timing.lock();
            let past = Instant::now() - std::time::Duration::from_secs(10);
            t.last_loss = Some(past);
            t.last_recover = past;
        }
        c.loss_ewma_ppm.store(0, Ordering::Release);
        let after = c.poll_target();
        assert!(after > 6_000_000, "expected recovery increase, got {after}");
        // 스텝 = max*0.03 = 600K → 6.6M.
        assert_eq!(after, 6_600_000);
    }

    #[test]
    fn recovery_caps_at_ceiling() {
        // 손실이 전혀 없으면(probe_cap=ceiling 유지) reprobe 로 ceiling 까지 회복 가능.
        let c = BitrateController::default();
        c.configure(20_000_000);
        c.target.store(5_000_000, Ordering::Release);
        for _ in 0..400 {
            {
                let mut t = c.timing.lock();
                let past = Instant::now() - std::time::Duration::from_secs(40);
                t.last_loss = Some(past);
                t.last_recover = past;
            }
            c.loss_ewma_ppm.store(0, Ordering::Release);
            c.poll_target();
        }
        assert_eq!(c.poll_target(), 20_000_000);
    }

    #[test]
    fn learned_ceiling_caps_recovery_after_collapse() {
        let c = BitrateController::default();
        c.configure(20_000_000); // start 10M, learned_ceiling 20M
        // 8M 부근에서 손실 붕괴 시뮬레이션: target 을 8M 로 두고 큰 손실.
        c.target.store(8_000_000, Ordering::Release);
        c.on_loss(0.4); // learned_ceiling → min(20M, 8M*0.70=5.6M) = 5.6M
        assert_eq!(c.learned_ceiling.load(Ordering::Acquire), 5_600_000);
        // 손실 없이 회복시켜도 학습 상한(5.6M) 근처에서 멈춘다(20M 로 안 올라감).
        // last_loss 를 6s 로 유지: RECOVER_DELAY(5s) 는 넘되 REPROBE(15s) 는 안 넘겨 재탐색 억제.
        for _ in 0..50 {
            {
                let mut t = c.timing.lock();
                let past = Instant::now() - std::time::Duration::from_millis(6000);
                t.last_loss = Some(past);
                t.last_recover = past;
            }
            c.loss_ewma_ppm.store(0, Ordering::Release);
            c.poll_target();
        }
        let settled = c.poll_target();
        assert!(settled <= 5_600_000, "recovery must cap at learned ceiling, got {settled}");
        assert!(settled >= 5_000_000, "should recover up to near learned ceiling, got {settled}");
    }

    #[test]
    fn fec_default_is_min_when_clean() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        assert_eq!(c.poll_fec_percentage(), FEC_MIN_PCT);
    }

    #[test]
    fn fec_rises_under_loss_with_headroom() {
        let c = BitrateController::default();
        c.configure(20_000_000); // floor 1M
        // headroom 을 명시적으로 보장: target 을 ceiling 으로 고정(게이트 통과 조건).
        c.target.store(20_000_000, Ordering::Release);
        // EWMA 를 직접 saturation 이상으로 올린다(손실 관측 시각도 최신으로).
        c.loss_ewma_ppm.store((0.20 * 1e6) as u32, Ordering::Release);
        {
            let mut t = c.timing.lock();
            t.last_loss = Some(Instant::now());
        }
        let fec = c.poll_fec_percentage();
        assert!(fec > FEC_MIN_PCT, "expected FEC above min with headroom+loss, got {fec}");
        assert!(fec <= FEC_MAX_PCT, "FEC must not exceed max, got {fec}");
    }

    #[test]
    fn fec_suppressed_at_floor_congestion() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        // 심한 손실 반복 → 비트레이트가 floor(1M)까지 떨어짐 = 혼잡 상황.
        for _ in 0..20 {
            {
                let mut t = c.timing.lock();
                t.last_decrease = Instant::now() - std::time::Duration::from_secs(10);
            }
            c.on_loss(0.9);
        }
        // 혼잡(floor 근처)에선 손실이 심해도 FEC 를 올리지 않는다 — parity 증가는 악화만 시킴.
        assert_eq!(c.poll_fec_percentage(), FEC_MIN_PCT);
    }

    #[test]
    fn fec_decays_after_loss_stops() {
        let c = BitrateController::default();
        c.configure(20_000_000);
        // headroom 고정 + EWMA saturation → FEC_MAX.
        c.target.store(20_000_000, Ordering::Release);
        c.loss_ewma_ppm.store((0.20 * 1e6) as u32, Ordering::Release);
        {
            let mut t = c.timing.lock();
            t.last_loss = Some(Instant::now());
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
