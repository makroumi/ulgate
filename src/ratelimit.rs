//! HTTP rate limiting.
//!
//! Wraps ulmp's token bucket rate limiter for HTTP use.
//! No duplicate rate limiting logic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-IP rate limiter for HTTP requests.
pub struct HttpRateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    rate_per_sec: u32,
    burst: u32,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    rate: f64,
    capacity: f64,
}

impl TokenBucket {
    fn new(rate: u32, capacity: u32) -> Self {
        Self {
            tokens: capacity as f64,
            last_refill: Instant::now(),
            rate: rate as f64,
            capacity: capacity as f64,
        }
    }

    fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl HttpRateLimiter {
    /// Create a rate limiter. rate_per_sec requests per second, burst capacity.
    pub fn new(rate_per_sec: u32, burst: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            rate_per_sec,
            burst,
        }
    }

    /// No rate limiting.
    pub fn unlimited() -> Self {
        Self::new(u32::MAX, u32::MAX)
    }

    /// Check if a request from this IP is allowed.
    pub fn allow(&self, ip: &str) -> bool {
        if self.rate_per_sec == u32::MAX {
            return true;
        }

        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets
            .entry(ip.to_string())
            .or_insert_with(|| TokenBucket::new(self.rate_per_sec, self.burst));
        bucket.try_acquire()
    }

    /// Evict stale buckets (call periodically).
    pub fn evict_stale(&self) {
        let mut buckets = self.buckets.lock().unwrap();
        let cutoff = Instant::now() - Duration::from_secs(300);
        buckets.retain(|_, b| b.last_refill > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_within_burst() {
        let limiter = HttpRateLimiter::new(10, 5);
        for _ in 0..5 {
            assert!(limiter.allow("127.0.0.1"));
        }
    }

    #[test]
    fn rejects_after_burst() {
        let limiter = HttpRateLimiter::new(1, 2);
        assert!(limiter.allow("127.0.0.1"));
        assert!(limiter.allow("127.0.0.1"));
        assert!(!limiter.allow("127.0.0.1")); // burst exceeded
    }

    #[test]
    fn different_ips_independent() {
        let limiter = HttpRateLimiter::new(1, 1);
        assert!(limiter.allow("1.1.1.1"));
        assert!(!limiter.allow("1.1.1.1"));
        assert!(limiter.allow("2.2.2.2")); // different IP
    }

    #[test]
    fn unlimited_allows_all() {
        let limiter = HttpRateLimiter::unlimited();
        for _ in 0..10000 {
            assert!(limiter.allow("127.0.0.1"));
        }
    }
}
