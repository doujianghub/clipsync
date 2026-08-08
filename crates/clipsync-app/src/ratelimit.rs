//! 发送速率限制（令牌桶）。
//!
//! **为什么需要**：修复后文件传输可达 135 MB/s，足以占满千兆链路——传大文件
//! 时会明显影响正常上网。限速让用户可以把带宽留一部分给其它用途。
//!
//! **只限文件内容**：文本/图片剪贴板同步数据量小且对延迟敏感，不参与限速，
//! 因此即使在限速状态下，复制一段文字仍然瞬时同步。
//!
//! **非阻塞设计**：`take` 不会睡眠等待令牌，而是返回"此刻允许发多少"。这一点
//! 很重要——收发泵是单线程的，若在此阻塞就无法及时响应"剪贴板已更新需取代
//! 传输"以及对端发来的消息。令牌不足时本轮少发或不发，下一轮再来。

use std::time::Instant;

/// 令牌桶限速器。
#[derive(Debug)]
pub struct RateLimiter {
    /// 每秒补充的字节数；`None` 表示不限速。
    rate: Option<f64>,
    /// 桶容量（允许的突发量）。取 1 秒的量，兼顾突发与平滑。
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// `bytes_per_sec` 为 `None` 或 0 时不限速。
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        let rate = match bytes_per_sec {
            Some(r) if r > 0 => Some(r as f64),
            _ => None,
        };
        let capacity = rate.unwrap_or(0.0);
        Self {
            rate,
            capacity,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// 是否处于限速状态。
    pub fn is_limited(&self) -> bool {
        self.rate.is_some()
    }

    /// 请求���送 `want` 字节，返回此刻实际允许发送的字节数（0..=want）。
    ///
    /// 不限速时原样返回 `want`。返回 0 表示当前令牌耗尽，调用方应本轮跳过发送。
    pub fn take(&mut self, want: u64) -> u64 {
        let rate = match self.rate {
            Some(r) => r,
            None => return want, // 不限速
        };

        // 按流逝时间补充令牌。
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * rate).min(self.capacity);

        let allowed = self.tokens.min(want as f64);
        if allowed < 1.0 {
            return 0;
        }
        let allowed = allowed.floor();
        self.tokens -= allowed;
        allowed as u64
    }

    /// 令牌耗尽时建议的等待时长——用于让收发泵睡得恰到好处，
    /// 既不空转耗 CPU，也不会睡过头拖慢传输。
    pub fn suggested_wait(&self) -> std::time::Duration {
        match self.rate {
            // 攒够一个分块所需的时间，上限 50ms 以保证仍能及时响应取代与收包。
            Some(rate) if rate > 0.0 => {
                let secs = (64.0 * 1024.0 / rate).min(0.05);
                std::time::Duration::from_secs_f64(secs)
            }
            _ => std::time::Duration::from_millis(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_always_grants_full_amount() {
        let mut l = RateLimiter::new(None);
        assert!(!l.is_limited());
        assert_eq!(l.take(1_000_000), 1_000_000);
        assert_eq!(l.take(u32::MAX as u64), u32::MAX as u64);
    }

    #[test]
    fn zero_rate_means_unlimited() {
        let mut l = RateLimiter::new(Some(0));
        assert!(!l.is_limited());
        assert_eq!(l.take(999), 999);
    }

    #[test]
    fn initial_burst_is_bounded_by_capacity() {
        // 1 MB/s：初始桶满，最多允许一次突发 1MB。
        let mut l = RateLimiter::new(Some(1_000_000));
        assert!(l.is_limited());
        let got = l.take(10_000_000);
        assert!(got <= 1_000_000, "突发不应超过桶容量，实得 {got}");
        assert!(got > 0);
    }

    #[test]
    fn exhausted_bucket_returns_zero() {
        let mut l = RateLimiter::new(Some(1_000_000));
        // 抽干桶。
        let _ = l.take(1_000_000);
        // 立刻再要（几乎没有时间补充），应几乎无令牌。
        let second = l.take(1_000_000);
        assert!(second < 1_000_000, "桶被抽干后不应再给出满额");
    }

    #[test]
    fn tokens_refill_over_time() {
        let mut l = RateLimiter::new(Some(1_000_000)); // 1 MB/s
        let _ = l.take(1_000_000); // 抽干
        std::thread::sleep(std::time::Duration::from_millis(120));
        let got = l.take(1_000_000);
        // 120ms 应补充约 120KB；放宽区间避免调度抖动导致偶发失败。
        assert!(
            (50_000..=400_000).contains(&got),
            "补充量应与流逝时间成正比，实得 {got}"
        );
    }

    #[test]
    fn never_grants_more_than_requested() {
        let mut l = RateLimiter::new(Some(10_000_000));
        assert!(l.take(1000) <= 1000);
    }

    #[test]
    fn suggested_wait_is_bounded() {
        // 极慢的限速也不应让泵睡太久，否则无法及时响应取代请求。
        let l = RateLimiter::new(Some(1024));
        assert!(l.suggested_wait() <= std::time::Duration::from_millis(50));
        // 不限速时无需等待。
        assert_eq!(
            RateLimiter::new(None).suggested_wait(),
            std::time::Duration::ZERO
        );
    }
}
