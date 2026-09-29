//! 重试 / 续写策略：数值与生产现状一致（瞬态 2s/4s 两次；续写 ≤3 次、抖动前 1.5s），
//! 仅把硬编码常量变成可注入、可单测的策略对象（学 DeepWrite agent-turn-retry 的做法）。

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 瞬态重试的退避序列（毫秒）；长度即最大重试次数。
    pub transient_delays_ms: Vec<u64>,
    /// 中途失败自动续写（拼接）的最大次数。
    pub continuation_max: u32,
    /// 非长度截断类续写前的退避（毫秒），避免立刻撞上同一波限流。
    pub continuation_backoff_ms: u64,
}

impl RetryPolicy {
    /// 交接 §10：重试次数语义明确——首次请求之外额外 N 次；允许设置 0（截断退避序列）。
    pub fn with_extra_retries(n: usize) -> Self {
        let mut p = Self::default();
        p.transient_delays_ms.truncate(n);
        p
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            transient_delays_ms: vec![2000, 4000],
            continuation_max: 3,
            continuation_backoff_ms: 1500,
        }
    }
}

impl RetryPolicy {
    /// 已重试 retries_so_far 次后，是否还允许再重试一次。
    pub fn can_retry_transient(&self, retries_so_far: u32) -> bool {
        (retries_so_far as usize) < self.transient_delays_ms.len()
    }

    /// 第 retries_so_far 次重试前应睡多久；越界返回 None（不应再重试）。
    pub fn delay_for_retry(&self, retries_so_far: u32) -> Option<u64> {
        self.transient_delays_ms
            .get(retries_so_far as usize)
            .copied()
    }

    /// 已续写 cont_so_far 次后，是否还允许再续写一次。
    pub fn can_continue(&self, cont_so_far: u32) -> bool {
        cont_so_far < self.continuation_max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁死生产数值：任何改动都必须显式改本测试（行为锁的一部分）。
    #[test]
    fn defaults_match_production_numbers() {
        let p = RetryPolicy::default();
        assert_eq!(p.transient_delays_ms, vec![2000, 4000]);
        assert_eq!(p.continuation_max, 3);
        assert_eq!(p.continuation_backoff_ms, 1500);
    }

    #[test]
    fn retry_window_and_delays() {
        let p = RetryPolicy::default();
        assert!(p.can_retry_transient(0));
        assert!(p.can_retry_transient(1));
        assert!(!p.can_retry_transient(2));
        assert_eq!(p.delay_for_retry(0), Some(2000));
        assert_eq!(p.delay_for_retry(1), Some(4000));
        assert_eq!(p.delay_for_retry(2), None);
    }

    #[test]
    fn continuation_window() {
        let p = RetryPolicy::default();
        assert!(p.can_continue(0));
        assert!(p.can_continue(2));
        assert!(!p.can_continue(3));
    }
}
