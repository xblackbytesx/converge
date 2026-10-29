//! Login throttling and the `/v1` limits, in memory (single instance,
//! AGENTS.md decision 5).
//!
//! Logins: only failures count. More than `PER_ADDR` failures from one address, or
//! `PER_ACCOUNT` for one email, within `WINDOW` blocks that address or email
//! for `BLOCK`. Accounts are never locked for good: the cost of guessing goes
//! up, and the owner can always get in again after five minutes.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use converge_core::limits::{Bucket, DayCount, over, retry_secs};
use sqlx::PgPool;
use uuid::Uuid;

pub const WINDOW: Duration = Duration::from_secs(60);
pub const BLOCK: Duration = Duration::from_secs(300);
pub const PER_ADDR: u32 = 10;
pub const PER_ACCOUNT: u32 = 5;
/// Past this many tracked keys, expired entries are swept on the next write.
const SWEEP_AT: usize = 10_000;

#[derive(Clone, Copy)]
struct Entry {
    window_start: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

#[derive(Default)]
pub struct LoginLimiter {
    entries: Mutex<HashMap<String, Entry>>,
}

fn addr_key(addr: Option<IpAddr>) -> Option<String> {
    addr.map(|a| format!("ip:{a}"))
}

fn account_key(email: &str) -> String {
    format!("email:{}", email.trim().to_lowercase())
}

impl LoginLimiter {
    /// Whether a login attempt may go ahead at all.
    pub fn allowed(&self, addr: Option<IpAddr>, email: &str, now: Instant) -> bool {
        let map = self.entries.lock().expect("limiter lock");
        let blocked = |k: &str| map.get(k).and_then(|e| e.blocked_until).is_some_and(|t| now < t);
        !(addr_key(addr).is_some_and(|k| blocked(&k)) || blocked(&account_key(email)))
    }

    pub fn failed(&self, addr: Option<IpAddr>, email: &str, now: Instant) {
        let mut map = self.entries.lock().expect("limiter lock");
        if map.len() > SWEEP_AT {
            map.retain(|_, e| now.duration_since(e.window_start) < WINDOW || e.blocked_until.is_some_and(|t| now < t));
        }
        let mut bump = |key: String, limit: u32| {
            let e = map.entry(key).or_insert(Entry { window_start: now, failures: 0, blocked_until: None });
            if now.duration_since(e.window_start) >= WINDOW {
                *e = Entry { window_start: now, failures: 0, blocked_until: e.blocked_until };
            }
            e.failures += 1;
            if e.failures > limit {
                e.blocked_until = Some(now + BLOCK);
            }
        };
        if let Some(k) = addr_key(addr) {
            bump(k, PER_ADDR);
        }
        bump(account_key(email), PER_ACCOUNT);
    }

    /// A successful login clears the account's failures (not the address's:
    /// one right password must not reset a spray across many accounts).
    pub fn succeeded(&self, email: &str) {
        self.entries.lock().expect("limiter lock").remove(&account_key(email));
    }
}

/// One person's `/v1` limits, from the snapshot. `None`: no limit.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub rpm: Option<u32>,
    pub tokens_per_day: Option<i64>,
    pub parallel_per_key: Option<u32>,
    /// Where the person's day starts and ends.
    pub tz: Tz,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { rpm: None, tokens_per_day: None, parallel_per_key: None, tz: chrono_tz::UTC }
    }
}

/// Why a request was turned away, with what the client is told.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    Rpm { limit: u32, wait: Duration },
    Tokens { used: i64, limit: i64, wait: Duration, tz: Tz },
    Parallel { limit: u32 },
}

impl Refused {
    /// What `usage.limited` and the metrics call it.
    pub fn kind(&self) -> &'static str {
        match self {
            Refused::Rpm { .. } => "rpm",
            Refused::Tokens { .. } => "tokens",
            Refused::Parallel { .. } => "parallel",
        }
    }

    pub fn retry_after(&self) -> u64 {
        match self {
            Refused::Rpm { wait, .. } | Refused::Tokens { wait, .. } => retry_secs(*wait),
            Refused::Parallel { .. } => 1,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Refused::Rpm { limit, .. } => {
                format!("Requests per minute: the limit of {limit} is reached; try again in {} s", self.retry_after())
            }
            Refused::Tokens { used, limit, tz, .. } => {
                format!("Tokens per day: {used} used of {limit}, resets at 00:00 {tz}")
            }
            Refused::Parallel { limit } => {
                format!("Parallel requests for this key: {limit} are already running, the limit")
            }
        }
    }
}

/// The day number `DayCount` uses: days since year 1 in `tz`.
pub fn local_day(wall: DateTime<Utc>, tz: Tz) -> i32 {
    wall.with_timezone(&tz).date_naive().num_days_from_ce()
}

/// Until the next midnight in `tz` (DST gaps fall back to the first
/// moment that exists, or a plain day).
fn until_midnight(wall: DateTime<Utc>, tz: Tz) -> Duration {
    let next = wall.with_timezone(&tz).date_naive().succ_opt().and_then(|d| d.and_hms_opt(0, 0, 0));
    let at = next.and_then(|n| tz.from_local_datetime(&n).earliest()).map(|t| t.with_timezone(&Utc));
    at.and_then(|t| (t - wall).to_std().ok()).unwrap_or(Duration::from_secs(86_400))
}

#[derive(Default)]
struct State {
    buckets: HashMap<Uuid, Bucket>,
    days: HashMap<Uuid, DayCount>,
    /// Requests running per key, while they run.
    running: HashMap<Uuid, u32>,
}

/// The `/v1` limits: requests per minute and tokens per day per person,
/// parallel requests per key. Survives snapshot reloads; a changed limit
/// applies to the next request.
#[derive(Default)]
pub struct ApiLimiter {
    state: Mutex<State>,
}

/// One running request on a key; dropping it frees the place.
pub struct Permit {
    limiter: Arc<ApiLimiter>,
    key: Option<Uuid>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let Some(k) = self.key else { return };
        let mut s = self.limiter.state.lock().expect("limiter lock");
        if let Some(n) = s.running.get_mut(&k) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.running.remove(&k);
            }
        }
    }
}

impl ApiLimiter {
    /// Checks every limit and, when none is hit, takes a request from the
    /// bucket and a place on the key. Nothing is taken when refused.
    pub fn check(
        self: &Arc<Self>,
        principal: Uuid,
        key: Option<Uuid>,
        l: &Limits,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Result<Permit, Refused> {
        let mut s = self.state.lock().expect("limiter lock");
        if let (Some(k), Some(limit)) = (key, l.parallel_per_key)
            && s.running.get(&k).copied().unwrap_or(0) >= limit
        {
            return Err(Refused::Parallel { limit });
        }
        if let Some(limit) = l.tokens_per_day {
            let used = s.days.get(&principal).map_or(0, |d| d.used(local_day(wall, l.tz)));
            if over(used, limit) {
                return Err(Refused::Tokens { used, limit, wait: until_midnight(wall, l.tz), tz: l.tz });
            }
        }
        if let Some(rpm) = l.rpm {
            let b = s.buckets.entry(principal).or_insert_with(|| Bucket::full(rpm, now));
            b.take(rpm, now).map_err(|wait| Refused::Rpm { limit: rpm, wait })?;
        }
        if let Some(k) = key {
            *s.running.entry(k).or_default() += 1;
        }
        Ok(Permit { limiter: self.clone(), key })
    }

    /// Counted for everyone, limit or not, so a limit set mid-day starts
    /// from what the day already used.
    pub fn add_tokens(&self, principal: Uuid, tz: Tz, wall: DateTime<Utc>, tokens: i64) {
        if tokens <= 0 {
            return;
        }
        let mut s = self.state.lock().expect("limiter lock");
        s.days.entry(principal).or_default().add(local_day(wall, tz), tokens);
    }

    pub fn used_today(&self, principal: Uuid, tz: Tz, wall: DateTime<Utc>) -> i64 {
        let s = self.state.lock().expect("limiter lock");
        s.days.get(&principal).map_or(0, |d| d.used(local_day(wall, tz)))
    }

    /// Today's tokens per person from the usage table, so a restart does
    /// not hand out a fresh day. Run once at start, before serving.
    pub async fn seed(&self, pool: &PgPool) -> anyhow::Result<()> {
        let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
            "SELECT u.principal_id, p.tz, SUM(u.prompt_tokens + u.completion_tokens)::bigint
               FROM usage u JOIN users p ON p.id = u.principal_id
              WHERE u.at >= (date_trunc('day', now() AT TIME ZONE p.tz) AT TIME ZONE p.tz)
              GROUP BY u.principal_id, p.tz",
        )
        .fetch_all(pool)
        .await?;
        let wall = Utc::now();
        let mut s = self.state.lock().expect("limiter lock");
        for (principal, tz, tokens) in rows {
            let tz: Tz = tz.parse().unwrap_or(chrono_tz::UTC);
            s.days.insert(principal, DayCount { day: local_day(wall, tz), tokens });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn account_blocks_after_five_failures_then_recovers() {
        let l = LoginLimiter::default();
        let t = Instant::now();
        for i in 0..PER_ACCOUNT {
            assert!(l.allowed(ip("10.0.0.1"), "a@x.nl", t), "attempt {i}");
            l.failed(ip("10.0.0.1"), "A@x.nl ", t);
        }
        assert!(l.allowed(ip("10.0.0.1"), "a@x.nl", t), "five failures are still allowed");
        l.failed(ip("10.0.0.1"), "a@x.nl", t);
        assert!(!l.allowed(ip("10.0.0.2"), "a@x.nl", t), "blocked for that email from anywhere");
        assert!(l.allowed(ip("10.0.0.1"), "b@x.nl", t), "the address itself is not blocked yet");
        assert!(l.allowed(ip("10.0.0.1"), "a@x.nl", t + BLOCK + Duration::from_secs(1)));
    }

    #[test]
    fn address_blocks_a_spray_across_accounts() {
        let l = LoginLimiter::default();
        let t = Instant::now();
        for i in 0..=PER_ADDR {
            l.failed(ip("10.0.0.9"), &format!("u{i}@x.nl"), t);
        }
        assert!(!l.allowed(ip("10.0.0.9"), "new@x.nl", t));
        assert!(l.allowed(ip("10.0.0.8"), "new@x.nl", t));
        l.succeeded("u1@x.nl");
        assert!(!l.allowed(ip("10.0.0.9"), "new@x.nl", t), "success elsewhere keeps the address blocked");
    }

    #[test]
    fn failures_spread_over_windows_never_block() {
        let l = LoginLimiter::default();
        let mut t = Instant::now();
        for _ in 0..20 {
            l.failed(None, "slow@x.nl", t);
            assert!(l.allowed(None, "slow@x.nl", t), "4 per window stays under 5");
            t += WINDOW / 4;
        }
    }

    fn limits(rpm: Option<u32>, tokens: Option<i64>, parallel: Option<u32>) -> Limits {
        Limits { rpm, tokens_per_day: tokens, parallel_per_key: parallel, tz: "Europe/Amsterdam".parse().unwrap() }
    }

    #[test]
    fn parallel_places_come_back_when_requests_end() {
        let l = Arc::new(ApiLimiter::default());
        let (p, k) = (Uuid::new_v4(), Uuid::new_v4());
        let lim = limits(None, None, Some(2));
        let (t, w) = (Instant::now(), Utc::now());
        let a = l.check(p, Some(k), &lim, t, w).unwrap();
        let b = l.check(p, Some(k), &lim, t, w).unwrap();
        assert_eq!(l.check(p, Some(k), &lim, t, w).err(), Some(Refused::Parallel { limit: 2 }));
        assert!(l.check(p, Some(Uuid::new_v4()), &lim, t, w).is_ok(), "per key, not per person");
        drop(a);
        let _c = l.check(p, Some(k), &lim, t, w).unwrap();
        drop(b);
        assert!(l.check(p, None, &lim, t, w).is_ok(), "no key (Try it): no parallel cap");
    }

    #[test]
    fn tokens_reset_at_the_persons_midnight() {
        let l = Arc::new(ApiLimiter::default());
        let p = Uuid::new_v4();
        let lim = limits(None, Some(1000), None);
        // 23:30 in Amsterdam (CET, UTC+1).
        let w: DateTime<Utc> = "2026-11-02T22:30:00Z".parse().unwrap();
        l.add_tokens(p, lim.tz, w, 1000);
        let r = l.check(p, None, &lim, Instant::now(), w).err().unwrap();
        assert_eq!(r.kind(), "tokens");
        assert_eq!(r.retry_after(), 1800);
        assert_eq!(r.message(), "Tokens per day: 1000 used of 1000, resets at 00:00 Europe/Amsterdam");
        let after: DateTime<Utc> = "2026-11-02T23:00:01Z".parse().unwrap();
        assert!(l.check(p, None, &lim, Instant::now(), after).is_ok());
        assert_eq!(l.used_today(p, lim.tz, w), 1000);
    }

    #[test]
    fn a_refused_request_takes_nothing() {
        let l = Arc::new(ApiLimiter::default());
        let (p, k) = (Uuid::new_v4(), Uuid::new_v4());
        let lim = limits(Some(1), None, Some(5));
        let t = Instant::now();
        let _a = l.check(p, Some(k), &lim, t, Utc::now()).unwrap();
        let r = l.check(p, Some(k), &lim, t, Utc::now()).err().unwrap();
        assert_eq!((r.kind(), r.retry_after()), ("rpm", 60));
        assert_eq!(l.state.lock().unwrap().running[&k], 1, "the refused one holds no place");
    }
}
