use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rate limiter that tracks requests per address using a fixed window approach.
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (u32, Instant)>>,
    max_requests: u32,
    window_duration: Duration,
}

impl RateLimiter {
    pub fn new(max_requests_per_second: u32) -> Self {
        RateLimiter {
            windows: Mutex::new(HashMap::new()),
            max_requests: max_requests_per_second,
            window_duration: Duration::from_secs(1),
        }
    }

    /// Returns `true` if the request is allowed, `false` if rate limit exceeded.
    pub fn check_rate_limit(&self, address: &str) -> bool {
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap();

        if windows.len() > 10000 {
            windows.retain(|_, (_, window_start)| {
                now.duration_since(*window_start) < self.window_duration * 2
            });
        }

        let address_lower = address.to_lowercase();

        match windows.get_mut(&address_lower) {
            Some((count, window_start)) => {
                if now.duration_since(*window_start) >= self.window_duration {
                    *count = 1;
                    *window_start = now;
                    true
                } else if *count >= self.max_requests {
                    false
                } else {
                    *count += 1;
                    true
                }
            }
            None => {
                windows.insert(address_lower, (1, now));
                true
            }
        }
    }

    #[allow(dead_code)]
    pub fn tracked_addresses(&self) -> usize {
        self.windows.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_allows_under_limit() {
        let limiter = RateLimiter::new(5);
        let addr = "0x1234567890abcdef";
        for _ in 0..5 {
            assert!(limiter.check_rate_limit(addr));
        }
    }

    #[test]
    fn test_blocks_over_limit() {
        let limiter = RateLimiter::new(5);
        let addr = "0x1234567890abcdef";
        for _ in 0..5 {
            assert!(limiter.check_rate_limit(addr));
        }
        assert!(!limiter.check_rate_limit(addr));
    }

    #[test]
    fn test_different_addresses_independent() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check_rate_limit("0xaaa"));
        assert!(limiter.check_rate_limit("0xaaa"));
        assert!(!limiter.check_rate_limit("0xaaa"));
        assert!(limiter.check_rate_limit("0xbbb"));
        assert!(limiter.check_rate_limit("0xbbb"));
    }

    #[test]
    fn test_window_resets_after_duration() {
        let limiter = RateLimiter::new(2);
        let addr = "0x1234";
        assert!(limiter.check_rate_limit(addr));
        assert!(limiter.check_rate_limit(addr));
        assert!(!limiter.check_rate_limit(addr));
        thread::sleep(Duration::from_millis(1100));
        assert!(limiter.check_rate_limit(addr));
    }

    #[test]
    fn test_case_insensitive() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check_rate_limit("0xABC"));
        assert!(limiter.check_rate_limit("0xabc"));
        assert!(!limiter.check_rate_limit("0xAbC"));
    }
}
