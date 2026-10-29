//! Request and token limits, pure: the caller passes the time in, the server
//! keeps the state (AGENTS.md, M6 plan).

use std::time::{Duration, Instant};

/// Requests per minute as a token bucket: holds `rpm`, refills `rpm / 60`
/// per second, so a burst at a window edge cannot double the rate.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    level: f64,
    at: Instant,
}

impl Bucket {
    pub fn full(rpm: u32, now: Instant) -> Self {
        Bucket { level: f64::from(rpm), at: now }
    }

    /// Takes one request, or says how long until one is free.
    pub fn take(&mut self, rpm: u32, now: Instant) -> Result<(), Duration> {
        let cap = f64::from(rpm.max(1));
        let rate = cap / 60.0;
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.level = (self.level + elapsed * rate).min(cap);
        self.at = now;
        if self.level >= 1.0 {
            self.level -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - self.level) / rate))
        }
    }
}

/// Tokens counted for one day. `day` is any day number the caller picks
/// (the server uses days since 1970 in the person's time zone); a new day
/// starts the count again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DayCount {
    pub day: i32,
    pub tokens: i64,
}

impl DayCount {
    pub fn add(&mut self, day: i32, tokens: i64) {
        if self.day != day {
            *self = DayCount { day, tokens: 0 };
        }
        self.tokens = self.tokens.saturating_add(tokens.max(0));
    }

    pub fn used(&self, day: i32) -> i64 {
        if self.day == day { self.tokens } else { 0 }
    }
}

/// Tokens are known only after an answer, so the request that crosses the
/// line finishes and the next one is refused.
pub fn over(used: i64, limit: i64) -> bool {
    used >= limit
}

/// Whole seconds for `Retry-After`, never 0 (0 invites a tight retry loop).
pub fn retry_secs(d: Duration) -> u64 {
    (d.as_secs() + u64::from(d.subsec_nanos() > 0)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_allows_a_burst_then_refills() {
        let t0 = Instant::now();
        let mut b = Bucket::full(3, t0);
        assert!(b.take(3, t0).is_ok());
        assert!(b.take(3, t0).is_ok());
        assert!(b.take(3, t0).is_ok());
        let wait = b.take(3, t0).unwrap_err();
        assert_eq!(wait, Duration::from_secs(20), "3 per minute: one every 20 s");
        assert_eq!(retry_secs(wait), 20);
        assert!(b.take(3, t0 + Duration::from_secs(10)).is_err(), "half refilled is not one");
        assert!(b.take(3, t0 + Duration::from_secs(20)).is_ok());
        // Long idle refills to the cap, not past it.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(b.take(3, later).is_ok());
        }
        assert!(b.take(3, later).is_err());
    }

    #[test]
    fn a_lowered_limit_applies_at_once() {
        let t0 = Instant::now();
        let mut b = Bucket::full(100, t0);
        assert!(b.take(1, t0).is_ok(), "capped to the new limit, one left");
        assert!(b.take(1, t0).is_err());
    }

    #[test]
    fn days_roll_over_and_the_line_is_at_or_over() {
        let mut d = DayCount::default();
        d.add(20_000, 150);
        d.add(20_000, 50);
        assert_eq!(d.used(20_000), 200);
        assert!(over(d.used(20_000), 200));
        assert!(!over(d.used(20_000), 201));
        assert_eq!(d.used(20_001), 0, "a new day has nothing used");
        d.add(20_001, 7);
        assert_eq!(d, DayCount { day: 20_001, tokens: 7 });
        d.add(20_001, -5);
        assert_eq!(d.tokens, 7, "never counts down");
    }

    #[test]
    fn retry_after_rounds_up() {
        assert_eq!(retry_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_secs(Duration::from_millis(1500)), 2);
        assert_eq!(retry_secs(Duration::ZERO), 1);
        assert_eq!(retry_secs(Duration::from_secs(5)), 5);
    }
}
