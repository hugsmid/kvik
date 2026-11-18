//! Batch-sync pacing (spec §3 Group commit, Q-980 to Q-985): the sync-rate budget, a token bucket that refills at
//! one batch sync per interval, and the smoothed batch-sync duration that sets the interval (Q-712).

use std::time::Duration;

/// The interval is this many times the smoothed batch-sync duration (Q-712).
pub(crate) const MULTIPLIER: u64 = 8;
/// A shorter sample moves the estimate down by this fraction of the difference (Q-712).
pub(crate) const FALL: u64 = 8;
/// The batch syncs the budget holds: how many may start back to back after a quiet period (Q-985).
pub(crate) const BURST: u64 = 4;
/// One batch sync of the budget, in the units the budget is counted in.
const TOKEN: u128 = 1_000_000_000;

pub(crate) fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The smoothed duration of the batch syncs that succeeded, in nanoseconds: it rises at once and falls by an eighth
/// of the difference per sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SyncPace {
    estimate: Option<u64>,
}

impl SyncPace {
    pub(crate) fn sample(&mut self, sample: u64) {
        self.estimate = Some(match self.estimate {
            Some(d) if sample < d => d - (d - sample) / FALL,
            _ => sample,
        });
    }

    pub(crate) fn estimate(&self) -> Option<Duration> {
        self.estimate.map(Duration::from_nanos)
    }

    /// The interval in force: the ceiling with no estimate, else the estimate times [`MULTIPLIER`] within
    /// `[min(floor, ceiling), ceiling]`; in `u64` nanoseconds either way (Q-713).
    pub(crate) fn interval(&self, floor: Duration, ceiling: Duration) -> Duration {
        let c = nanos(ceiling);
        let Some(d) = self.estimate else {
            return Duration::from_nanos(c);
        };
        let f = nanos(floor).min(c);
        Duration::from_nanos(d.saturating_mul(MULTIPLIER).clamp(f, c))
    }
}

/// The sync-rate budget (Q-980): [`BURST`] batch syncs, refilled at one per interval, counted in billionths of a
/// sync. Full at the start; a zero interval leaves it full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Budget {
    /// Billionths of a sync spent below full, as of `at`; `None`: full.
    spent: Option<(u64, Duration)>,
}

impl Budget {
    fn full() -> u128 {
        u128::from(BURST) * TOKEN
    }

    /// The budget left at `now` with `interval` in force.
    fn left(&self, now: Duration, interval: Duration) -> u128 {
        let Some((spent, at)) = self.spent else {
            return Self::full();
        };
        let i = u128::from(nanos(interval));
        if i == 0 {
            return Self::full();
        }
        let refill = u128::from(nanos(now.saturating_sub(at))) * TOKEN / i;
        (Self::full() - u128::from(spent) + refill).min(Self::full())
    }

    /// When the next batch sync may start: at once while a sync is left, else when the refill reaches one.
    pub(crate) fn ready_at(&self, interval: Duration) -> Duration {
        let Some((spent, at)) = self.spent else {
            return Duration::ZERO;
        };
        let left = Self::full() - u128::from(spent);
        if left >= TOKEN || interval.is_zero() {
            return Duration::ZERO;
        }
        let wait = ((TOKEN - left) * u128::from(nanos(interval))).div_ceil(TOKEN);
        at.saturating_add(Duration::from_nanos(
            u64::try_from(wait).unwrap_or(u64::MAX),
        ))
    }

    /// Spends one sync for a batch sync starting at `now`.
    pub(crate) fn spend(&mut self, now: Duration, interval: Duration) {
        let left = self.left(now, interval).saturating_sub(TOKEN);
        let spent = u64::try_from(Self::full() - left).expect("the budget holds a few syncs");
        self.spent = Some((spent, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const US: u64 = 1_000;
    const FLOOR: Duration = Duration::from_millis(2);
    const CEILING: Duration = Duration::from_millis(10);

    fn pace(samples: &[u64]) -> SyncPace {
        let mut p = SyncPace::default();
        for &s in samples {
            p.sample(s);
        }
        p
    }

    fn est(p: &SyncPace) -> u64 {
        nanos(p.estimate().unwrap())
    }

    #[test]
    fn no_estimate_gives_the_ceiling_whatever_the_floor() {
        let p = SyncPace::default();
        assert_eq!(p.estimate(), None);
        assert_eq!(p.interval(FLOOR, CEILING), CEILING);
        assert_eq!(p.interval(Duration::ZERO, CEILING), CEILING);
        assert_eq!(p.interval(20 * MS, CEILING), CEILING);
    }

    #[test]
    fn the_first_sample_and_any_higher_one_are_taken_at_once() {
        let mut p = pace(&[300 * US]);
        assert_eq!(est(&p), 300 * US);
        p.sample(160 * US);
        p.sample(2_000 * US);
        assert_eq!(est(&p), 2_000 * US);
    }

    #[test]
    fn a_lower_sample_moves_the_estimate_down_by_an_eighth_of_the_difference() {
        let p = pace(&[2_000 * US, 160 * US]);
        assert_eq!(est(&p), 1_770 * US);
        let p = pace(&[9, 0]);
        assert_eq!(est(&p), 9 - 1, "the fall rounds towards the old estimate");
    }

    #[test]
    fn the_interval_is_eight_times_the_estimate_between_the_bounds() {
        assert_eq!(pace(&[500 * US]).interval(FLOOR, CEILING), 4 * MS);
        assert_eq!(pace(&[160 * US]).interval(FLOOR, CEILING), FLOOR);
        assert_eq!(pace(&[1_250 * US]).interval(FLOOR, CEILING), CEILING);
        assert_eq!(pace(&[0]).interval(Duration::ZERO, CEILING), Duration::ZERO);
        assert_eq!(pace(&[160 * US]).interval(20 * MS, CEILING), CEILING);
        assert_eq!(
            pace(&[160 * US]).interval(FLOOR, Duration::ZERO),
            Duration::ZERO
        );
    }

    #[test]
    fn the_largest_sample_and_ceiling_saturate() {
        let p = pace(&[u64::MAX]);
        assert_eq!(p.interval(FLOOR, CEILING), CEILING);
        let top = Duration::from_nanos(u64::MAX);
        assert_eq!(p.interval(FLOOR, Duration::MAX), top);
        assert_eq!(SyncPace::default().interval(FLOOR, Duration::MAX), top);
        assert_eq!(est(&pace(&[u64::MAX, 0])), u64::MAX - u64::MAX / 8);
    }

    #[test]
    fn a_full_budget_lets_the_burst_through_at_once_then_one_sync_per_interval() {
        let mut b = Budget::default();
        let t = Duration::from_secs(1);
        for k in 0..BURST {
            assert_eq!(b.ready_at(FLOOR), Duration::ZERO, "sync {k}");
            b.spend(t, FLOOR);
        }
        assert_eq!(b.ready_at(FLOOR), t + FLOOR);
        b.spend(t + FLOOR, FLOOR);
        assert_eq!(b.ready_at(FLOOR), t + 2 * FLOOR);
    }

    #[test]
    fn the_budget_refills_to_full_and_no_further() {
        let mut b = Budget::default();
        let t = Duration::from_secs(1);
        b.spend(t, FLOOR);
        assert_eq!(b.left(t + FLOOR / 2, FLOOR), Budget::full() - TOKEN / 2);
        assert_eq!(b.left(t + 100 * FLOOR, FLOOR), Budget::full());
        b.spend(t + 100 * FLOOR, FLOOR);
        assert_eq!(b.left(t + 100 * FLOOR, FLOOR), Budget::full() - TOKEN);
    }

    #[test]
    fn the_wait_for_a_partial_sync_rounds_up_so_the_refill_has_reached_it() {
        let mut b = Budget::default();
        let (i, ns) = (Duration::from_nanos(7), Duration::from_nanos(1));
        let t = Duration::from_secs(1);
        for _ in 0..BURST {
            b.spend(t, i);
        }
        b.spend(t + 10 * ns, i);
        let r = b.ready_at(i);
        assert_eq!(r, t + 15 * ns);
        assert!(b.left(r, i) >= TOKEN);
        assert!(b.left(r - ns, i) < TOKEN);
    }

    #[test]
    fn a_zero_interval_leaves_the_budget_full() {
        let mut b = Budget::default();
        for _ in 0..10 * BURST {
            b.spend(Duration::from_secs(1), Duration::ZERO);
            assert_eq!(b.ready_at(Duration::ZERO), Duration::ZERO);
        }
    }

    #[test]
    fn the_ready_time_saturates() {
        let mut b = Budget::default();
        let top = Duration::from_nanos(u64::MAX);
        for _ in 0..BURST {
            b.spend(Duration::MAX, top);
        }
        assert_eq!(b.ready_at(top), Duration::MAX);
    }
}
