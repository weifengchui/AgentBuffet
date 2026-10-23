//! M2 供应商运行时状态：熔断、degraded、额度软上限、429 被动校准。
//! 无定时器：一切按 now（epoch 秒）惰性判断；额度按 UTC 日惰性翻转。

use crate::config::{CircuitBreaker, Provider};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn day_of(secs: u64) -> u64 {
    secs / 86_400
}

#[derive(Debug, Clone)]
struct ProviderState {
    /// 滑动窗口内的失败时刻（epoch 秒）；成功即清空（"连续失败"语义）。
    fail_times: Vec<u64>,
    /// 熔断/隔离截止点；惰性判断 open_until <= now 即恢复。
    open_until: Option<u64>,
    /// 401/403 或缺 key：需人工修 key，持续至配置重载。
    degraded: bool,
    /// 当前记账日（UTC epoch 日），跨日自动清零。
    day: u64,
    /// 当日估算 tokens 累计（来自响应 usage 字段）。
    tokens_used: u64,
    /// 429（无 retry-after）收紧系数：每收到一次乘 conservative_quota_ratio。
    tighten: f64,
}

impl ProviderState {
    fn new(now: u64) -> Self {
        Self {
            fail_times: Vec::new(),
            open_until: None,
            degraded: false,
            day: day_of(now),
            tokens_used: 0,
            tighten: 1.0,
        }
    }
}

/// 候选不可用的原因。
#[derive(Debug, Clone)]
pub enum Unavailable {
    CircuitOpen { until: u64 },
    Degraded,
    QuotaExhausted { used: u64, cap: u64 },
}

impl Unavailable {
    /// fail chain 里的短标签。
    pub fn tag(&self) -> &'static str {
        match self {
            Unavailable::CircuitOpen { .. } => "open",
            Unavailable::Degraded => "degraded",
            Unavailable::QuotaExhausted { .. } => "quota",
        }
    }
}

/// 全部 provider 的运行时状态表（存于 AppState，热重载后由新表替代）。
pub struct StateTable {
    inner: Mutex<HashMap<String, ProviderState>>,
}

impl Default for StateTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StateTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn with_state<R>(&self, id: &str, now: u64, f: impl FnOnce(&mut ProviderState) -> R) -> R {
        let mut g = self.inner.lock().unwrap();
        let st = g
            .entry(id.to_string())
            .or_insert_with(|| ProviderState::new(now));
        let today = day_of(now);
        if st.day != today {
            st.day = today;
            st.tokens_used = 0;
            st.tighten = 1.0;
        }
        f(st)
    }

    /// 候选过滤：degraded / 熔断 open / 额度耗尽 均不可用。
    pub fn availability(
        &self,
        id: &str,
        p: &Provider,
        ratio: f64,
        now: u64,
    ) -> Result<(), Unavailable> {
        self.with_state(id, now, |st| {
            if st.degraded {
                return Err(Unavailable::Degraded);
            }
            if let Some(u) = st.open_until {
                if u > now {
                    return Err(Unavailable::CircuitOpen { until: u });
                }
            }
            if let Some(q) = &p.quota {
                let cap = (q.daily_tokens as f64 * ratio * st.tighten) as u64;
                if st.tokens_used >= cap {
                    return Err(Unavailable::QuotaExhausted {
                        used: st.tokens_used,
                        cap,
                    });
                }
            }
            Ok(())
        })
    }

    /// 请求成功（响应头/缓冲完成/流首 chunk 判定通过）：清连续失败计数。
    pub fn record_success(&self, id: &str, now: u64) {
        self.with_state(id, now, |st| {
            st.fail_times.clear();
            st.open_until = None;
        });
    }

    /// 失败信号（5xx/超时/连接错误/无 retry-after 的 429）计入熔断窗口。
    /// 熔断优先于重试：窗口内达阈值即隔离 open_seconds。
    pub fn record_failure(&self, id: &str, cb: &CircuitBreaker, now: u64) {
        self.with_state(id, now, |st| {
            st.fail_times
                .retain(|t| t.saturating_add(cb.window_seconds) >= now);
            st.fail_times.push(now);
            if st.fail_times.len() as u32 >= cb.threshold {
                st.open_until = Some(now + cb.open_seconds);
                st.fail_times.clear();
            }
        });
    }

    /// 401/403：标记需人工修 key，持续至重载。
    pub fn mark_degraded(&self, id: &str, now: u64) {
        self.with_state(id, now, |st| st.degraded = true);
    }

    /// 429 被动校准：有 retry-after 则隔离至重置点；否则按保守系数收紧当日软上限，
    /// 同时计入一次熔断失败信号。
    pub fn calibrate_429(
        &self,
        id: &str,
        retry_after: Option<u64>,
        cb: &CircuitBreaker,
        ratio: f64,
        now: u64,
    ) {
        match retry_after {
            Some(s) => self.with_state(id, now, |st| {
                st.open_until = Some(now + s.max(1));
                st.fail_times.clear();
            }),
            None => {
                self.with_state(id, now, |st| st.tighten *= ratio.clamp(0.1, 1.0));
                self.record_failure(id, cb, now);
            }
        }
    }

    /// tokens 记账（估算值），响应完成时累计。
    pub fn add_tokens(&self, id: &str, tokens: u64, now: u64) {
        if tokens == 0 {
            return;
        }
        self.with_state(id, now, |st| st.tokens_used += tokens);
    }

    /// 当日已用 tokens（测试与后续 UI 用）。
    pub fn tokens_used(&self, id: &str, now: u64) -> u64 {
        self.with_state(id, now, |st| st.tokens_used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Quota;

    fn prov(quota: Option<u64>) -> Provider {
        Provider {
            id: "p".into(),
            vendor: "v".into(),
            base_url: "https://x.test/v1".into(),
            api_key_env: "K".into(),
            models: vec!["m".into()],
            quota: quota.map(|daily_tokens| Quota { daily_tokens }),
            allow_private: false,
            disabled: false,
        }
    }

    fn cb() -> CircuitBreaker {
        CircuitBreaker {
            window_seconds: 60,
            threshold: 3,
            open_seconds: 10,
        }
    }

    #[test]
    fn circuit_opens_at_threshold_and_recovers_after_open_seconds() {
        let t = StateTable::new();
        let p = prov(None);
        let c = cb();
        let now = 1_000_000;
        for i in 0..3 {
            t.record_failure("p", &c, now + i);
        }
        assert!(matches!(
            t.availability("p", &p, 0.8, now + 3),
            Err(Unavailable::CircuitOpen { .. })
        ));
        // 隔离到期自动恢复
        assert!(t.availability("p", &p, 0.8, now + 13).is_ok());
    }

    #[test]
    fn success_clears_consecutive_failures() {
        let t = StateTable::new();
        let p = prov(None);
        let c = cb();
        let now = 1_000_000;
        t.record_failure("p", &c, now);
        t.record_failure("p", &c, now + 1);
        t.record_success("p", now + 2);
        t.record_failure("p", &c, now + 3);
        t.record_failure("p", &c, now + 4);
        // 清空后重新累计，未达阈值不隔离
        assert!(t.availability("p", &p, 0.8, now + 5).is_ok());
    }

    #[test]
    fn window_prunes_old_failures() {
        let t = StateTable::new();
        let p = prov(None);
        let c = cb();
        let now = 1_000_000;
        t.record_failure("p", &c, now);
        t.record_failure("p", &c, now + 1);
        // 窗口 60s 之外，旧失败被剪除
        t.record_failure("p", &c, now + 100);
        assert!(t.availability("p", &p, 0.8, now + 101).is_ok());
    }

    #[test]
    fn degraded_is_sticky_until_state_reset() {
        let t = StateTable::new();
        let p = prov(None);
        let now = 1_000_000;
        t.mark_degraded("p", now);
        assert!(matches!(
            t.availability("p", &p, 0.8, now + 10_000),
            Err(Unavailable::Degraded)
        ));
    }

    #[test]
    fn quota_cap_uses_ratio_and_resets_on_day_rollover() {
        let t = StateTable::new();
        let p = prov(Some(1000));
        let now = day_of(1_000_000) * 86_400 + 100;
        t.add_tokens("p", 700, now);
        assert!(t.availability("p", &p, 0.8, now).is_ok());
        t.add_tokens("p", 150, now); // 850 >= 1000*0.8
        assert!(matches!(
            t.availability("p", &p, 0.8, now),
            Err(Unavailable::QuotaExhausted { .. })
        ));
        // 次日清零
        let tomorrow = now + 86_400;
        assert!(t.availability("p", &p, 0.8, tomorrow).is_ok());
        assert_eq!(t.tokens_used("p", tomorrow), 0);
    }

    #[test]
    fn retry_after_isolates_until_reset_point() {
        let t = StateTable::new();
        let p = prov(None);
        let c = cb();
        let now = 1_000_000;
        t.calibrate_429("p", Some(5), &c, 0.8, now);
        assert!(matches!(
            t.availability("p", &p, 0.8, now + 4),
            Err(Unavailable::CircuitOpen { .. })
        ));
        assert!(t.availability("p", &p, 0.8, now + 6).is_ok());
    }

    #[test]
    fn forty_two_nine_without_retry_after_tightens_cap() {
        let t = StateTable::new();
        let p = prov(Some(1000));
        let c = cb();
        let now = day_of(1_000_000) * 86_400 + 100;
        t.add_tokens("p", 750, now);
        assert!(t.availability("p", &p, 0.8, now).is_ok()); // cap=800
        t.calibrate_429("p", None, &c, 0.8, now); // cap=640
        assert!(matches!(
            t.availability("p", &p, 0.8, now),
            Err(Unavailable::QuotaExhausted { .. })
        ));
    }
}
