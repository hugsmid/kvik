//! Retries with exponential backoff and jitter, and the seams that let tests
//! run them without waiting.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// How a transient fault is retried. Each attempt is signed afresh.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Attempts in all, the first included (at least 1).
    pub attempts: u32,
    /// The pause before the second attempt; it doubles for each later one.
    pub base: Duration,
    /// The longest pause.
    pub max: Duration,
}

impl Default for RetryPolicy {
    /// Six attempts, pausing about 0.2, 0.4, 0.8, 1.6 and 3.2 seconds.
    fn default() -> Self {
        RetryPolicy {
            attempts: 6,
            base: Duration::from_millis(200),
            max: Duration::from_secs(20),
        }
    }
}

impl RetryPolicy {
    /// A policy.
    pub fn new(attempts: u32, base: Duration, max: Duration) -> Self {
        RetryPolicy {
            attempts: attempts.max(1),
            base,
            max,
        }
    }

    /// The pause after failed attempt `n` (1-based), before jitter.
    pub fn backoff(&self, n: u32) -> Duration {
        let factor = 1u32.checked_shl(n.saturating_sub(1)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.max)
    }
}

/// The wall clock that dates signatures.
pub trait WallClock: Send + Sync {
    /// Now.
    fn now(&self) -> SystemTime;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// How the worker waits between attempts. It runs on the sink's own worker
/// thread, never in a future's `poll`.
pub trait Pause: Send + Sync {
    /// Waits for `d`.
    fn pause(&self, d: Duration);
}

/// `std::thread::sleep`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleep;

impl Pause for ThreadSleep {
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// "Equal jitter": half the backoff, plus up to as much again at random, so
/// that writers that failed together do not retry together.
pub(crate) struct Jitter(Mutex<u64>);

impl Jitter {
    pub(crate) fn new(seed: u64) -> Self {
        Jitter(Mutex::new(seed | 1))
    }

    pub(crate) fn apply(&self, d: Duration) -> Duration {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // xorshift64
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        let frac = (*s >> 11) as f64 / (1u64 << 53) as f64;
        let half = d / 2;
        half + half.mul_f64(frac)
    }
}

/// A seed that differs between processes.
pub(crate) fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let p = RetryPolicy::new(10, Duration::from_millis(100), Duration::from_secs(1));
        let got: Vec<u128> = (1..=6).map(|n| p.backoff(n).as_millis()).collect();
        assert_eq!(got, vec![100, 200, 400, 800, 1000, 1000]);
        assert_eq!(p.backoff(200), Duration::from_secs(1));
        assert_eq!(RetryPolicy::new(0, p.base, p.max).attempts, 1);
    }

    #[test]
    fn jitter_stays_between_half_and_all() {
        let j = Jitter::new(42);
        let d = Duration::from_millis(1000);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1000 {
            let x = j.apply(d);
            assert!(x >= d / 2 && x <= d, "{x:?}");
            seen.insert(x.as_millis());
        }
        assert!(seen.len() > 100);
    }
}
