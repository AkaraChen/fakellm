//! Server time. Everything that stamps times (`created`, request log) or paces output (time to first
//! token, token interval, `retry-after`) reads [`Clock::now`], so the control plane can freeze or move
//! time for one server without touching the host clock. Each server owns its clock: several
//! in-process servers in one test binary stay independent.
//!
//! The state lives in one atomic word, so `now()` is a single load and every change is an atomic
//! read-modify-write: a reader never sees a half-applied freeze, and concurrent `advance` calls add up
//! instead of overwriting each other. Every change also wakes tasks parked in [`Clock::wait_until`],
//! which is how a frozen clock lets a streaming response make progress: nothing happens until the
//! test advances time, and then the next chunk is released at once.

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

/// `value << 1 | frozen`. Running: `value` is the nanoseconds added to the host clock.
/// Frozen: `value` is the absolute time in nanoseconds since the epoch. Both fit in 63 bits,
/// which covers ±146 years (the years 1824 to 2116 when frozen).
#[derive(Default)]
pub struct Clock {
    state: AtomicI64,
    changed: Notify,
}

/// A requested time, or offset, does not fit the clock's range of about ±146 years.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange;

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("time is out of the clock's range (years 1824 to 2116)")
    }
}

impl std::error::Error for OutOfRange {}

fn pack(value: i64, frozen: bool) -> Result<i64, OutOfRange> {
    Ok(value.checked_mul(2).ok_or(OutOfRange)? | frozen as i64)
}

fn unpack(state: i64) -> (i64, bool) {
    (state >> 1, state & 1 == 1)
}

impl Clock {
    pub fn now(&self) -> SystemTime {
        let (value, frozen) = unpack(self.state.load(Ordering::Relaxed));
        if frozen {
            return from_nanos(value);
        }
        let host = SystemTime::now();
        if value >= 0 {
            host + Duration::from_nanos(value as u64)
        } else {
            host - Duration::from_nanos(value.unsigned_abs())
        }
    }

    /// Seconds since the epoch, as APIs report `created`.
    pub fn unix_secs(&self) -> i64 {
        to_nanos(self.now()).div_euclid(1_000_000_000)
    }

    /// Stops time at `at` (or at the current server time).
    pub fn freeze(&self, at: Option<SystemTime>) -> Result<(), OutOfRange> {
        let at = at.map(to_nanos);
        self.update(|value, frozen| {
            let at = at.unwrap_or(if frozen { value } else { host_nanos() + value });
            (at, true)
        })
    }

    /// Lets time run again from the current server time.
    pub fn unfreeze(&self) -> Result<(), OutOfRange> {
        self.update(|value, frozen| if frozen { (value - host_nanos(), false) } else { (value, false) })
    }

    /// Jumps to `at`, keeping the frozen/running state.
    pub fn set(&self, at: SystemTime) -> Result<(), OutOfRange> {
        let at = to_nanos(at);
        self.update(|_, frozen| (if frozen { at } else { at - host_nanos() }, frozen))
    }

    /// Moves time forward (or backward, for negative `nanos`).
    pub fn advance(&self, nanos: i64) -> Result<(), OutOfRange> {
        self.update(|value, frozen| (value.saturating_add(nanos), frozen))
    }

    /// Back to the host clock.
    pub fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    pub fn is_frozen(&self) -> bool {
        unpack(self.state.load(Ordering::Relaxed)).1
    }

    /// Whether the clock differs from the host clock (frozen, or running with an offset).
    pub fn is_virtual(&self) -> bool {
        self.state.load(Ordering::Relaxed) != 0
    }

    /// Resolves once the server time reaches `deadline`. While the clock is frozen this only
    /// returns when the control plane moves time past the deadline; while it runs, it sleeps on the
    /// host clock but re-checks after every change, so `advance` also shortens a running wait.
    pub async fn wait_until(&self, deadline: SystemTime) {
        loop {
            // Register before reading the time, so a change between the read and the await is seen.
            let changed = self.changed.notified();
            let remaining = match deadline.duration_since(self.now()) {
                Ok(r) if !r.is_zero() => r,
                _ => return,
            };
            if self.is_frozen() {
                changed.await;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(remaining) => {}
                    _ = changed => {}
                }
            }
        }
    }

    fn update(&self, f: impl Fn(i64, bool) -> (i64, bool)) -> Result<(), OutOfRange> {
        let mut result = Ok(());
        let _ = self.state.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
            let (value, frozen) = unpack(state);
            let (value, frozen) = f(value, frozen);
            match pack(value, frozen) {
                Ok(next) => Some(next),
                Err(e) => {
                    result = Err(e);
                    None
                }
            }
        });
        if result.is_ok() {
            self.changed.notify_waiters();
        }
        result
    }
}

fn host_nanos() -> i64 {
    to_nanos(SystemTime::now())
}

fn to_nanos(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

fn from_nanos(n: i64) -> SystemTime {
    if n >= 0 {
        UNIX_EPOCH + Duration::from_nanos(n as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(n.unsigned_abs())
    }
}

/// Parses a duration such as `30d`, `1h30m`, `500ms` or `-2w`. Units: `ns`, `us`, `ms`, `s`, `m`,
/// `h`, `d` (24 hours), `w`. A bare number is seconds. Returns signed nanoseconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let bad = || format!("invalid duration {s:?} (examples: 30d, 1h30m, 500ms, -2w)");
    let (neg, mut rest) = match s.trim().strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.trim().trim_start_matches('+')),
    };
    if rest.is_empty() {
        return Err(bad());
    }
    let mut total: i128 = 0;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let n: i128 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit()).unwrap_or(rest.len());
        let per: i128 = match &rest[..unit_len] {
            "ns" => 1,
            "us" | "µs" => 1_000,
            "ms" => 1_000_000,
            "" | "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3600 * 1_000_000_000,
            "d" => 86_400 * 1_000_000_000,
            "w" => 7 * 86_400 * 1_000_000_000,
            _ => return Err(bad()),
        };
        rest = &rest[unit_len..];
        total = total.saturating_add(n.saturating_mul(per));
    }
    let total = if neg { -total } else { total };
    i64::try_from(total).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn freeze_advance_unfreeze() {
        let c = Clock::default();
        let t = UNIX_EPOCH + Duration::from_secs(1_000_000);
        c.freeze(Some(t)).unwrap();
        assert_eq!(c.now(), t);
        c.advance(5_000_000_000).unwrap();
        assert_eq!(c.now(), t + Duration::from_secs(5));
        c.unfreeze().unwrap();
        let after = c.now();
        assert!(after >= t + Duration::from_secs(5) && after < t + Duration::from_secs(6));
        c.reset();
        assert!(!c.is_virtual());
        assert!(c.now().duration_since(SystemTime::now()).unwrap_or_default() < Duration::from_secs(1));
    }

    #[test]
    fn set_keeps_mode() {
        let c = Clock::default();
        let t = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        c.set(t).unwrap();
        assert!(!c.is_frozen());
        let now = c.now();
        assert!(now >= t && now < t + Duration::from_secs(1));
        c.freeze(None).unwrap();
        let frozen = c.now();
        assert_eq!(c.now(), frozen);
        c.set(t).unwrap();
        assert_eq!(c.now(), t);
    }

    #[test]
    fn advance_backward_and_range() {
        let c = Clock::default();
        c.advance(-3_600_000_000_000).unwrap();
        assert!(c.now() < SystemTime::now() - Duration::from_secs(3590));
        assert_eq!(c.advance(i64::MAX), Err(OutOfRange));
        let far = UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        assert_eq!(c.freeze(Some(far)), Err(OutOfRange));
        // A failed change leaves the state alone.
        assert!(c.now() < SystemTime::now());
    }

    #[test]
    fn concurrent_advances_add_up() {
        let c = Arc::new(Clock::default());
        c.freeze(Some(UNIX_EPOCH)).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || (0..1000).for_each(|_| c.advance(1_000).unwrap()))
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(c.now(), UNIX_EPOCH + Duration::from_micros(8000));
    }

    #[tokio::test]
    async fn frozen_wait_needs_an_advance() {
        let c = Arc::new(Clock::default());
        c.freeze(Some(UNIX_EPOCH)).unwrap();
        let deadline = UNIX_EPOCH + Duration::from_millis(300);
        let waiter = tokio::spawn({
            let c = c.clone();
            async move { c.wait_until(deadline).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        c.advance(100_000_000).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        c.advance(200_000_000).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        // Already past: returns at once.
        c.wait_until(UNIX_EPOCH).await;
    }

    #[tokio::test]
    async fn running_wait_sleeps_on_the_host_clock() {
        let c = Clock::default();
        let start = std::time::Instant::now();
        c.wait_until(c.now() + Duration::from_millis(30)).await;
        assert!(start.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30d"), Ok(30 * 86_400 * 1_000_000_000));
        assert_eq!(parse_duration("1h30m"), Ok(5400 * 1_000_000_000));
        assert_eq!(parse_duration("-500ms"), Ok(-500_000_000));
        assert_eq!(parse_duration("2w"), Ok(14 * 86_400 * 1_000_000_000));
        assert_eq!(parse_duration("90"), Ok(90 * 1_000_000_000));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("99999999999999999999d").is_err());
    }
}
