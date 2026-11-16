//! The adaptive batch-sync interval (spec §3 Group commit, Q-310, Q-712): a smoothed batch-sync duration sets the
//! interval between the floor and `min_sync_interval`.

use std::time::Duration;

/// The interval is this many times the smoothed batch-sync duration (Q-712).
pub(crate) const MULTIPLIER: u64 = 8;
/// A shorter sample moves the estimate down by this fraction of the difference (Q-712).
pub(crate) const FALL: u64 = 8;

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
    /// `[min(floor, ceiling), ceiling]`.
    pub(crate) fn interval(&self, floor: Duration, ceiling: Duration) -> Duration {
        let Some(d) = self.estimate else {
            return ceiling;
        };
        let c = nanos(ceiling);
        let f = nanos(floor).min(c);
        Duration::from_nanos(d.saturating_mul(MULTIPLIER).clamp(f, c))
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
        assert_eq!(pace(&[1_000 * US]).interval(FLOOR, CEILING), 8 * MS);
    }

    #[test]
    fn a_fast_device_runs_at_the_floor_and_a_zero_sample_is_valid() {
        assert_eq!(pace(&[160 * US]).interval(FLOOR, CEILING), FLOOR);
        assert_eq!(pace(&[0]).interval(FLOOR, CEILING), FLOOR);
        assert_eq!(pace(&[0]).interval(Duration::ZERO, CEILING), Duration::ZERO);
    }

    #[test]
    fn a_sync_of_an_eighth_of_the_ceiling_or_more_runs_at_the_ceiling() {
        assert_eq!(pace(&[1_250 * US]).interval(FLOOR, CEILING), CEILING);
        assert_eq!(
            pace(&[1_249 * US]).interval(FLOOR, CEILING),
            Duration::from_micros(9_992)
        );
        assert_eq!(pace(&[20_000 * US]).interval(FLOOR, CEILING), CEILING);
    }

    #[test]
    fn a_floor_at_or_above_the_ceiling_fixes_the_interval_at_the_ceiling() {
        for samples in [&[0][..], &[160 * US], &[30_000 * US]] {
            assert_eq!(pace(samples).interval(CEILING, CEILING), CEILING);
            assert_eq!(pace(samples).interval(20 * MS, CEILING), CEILING);
        }
    }

    #[test]
    fn a_zero_ceiling_disables_the_bound_whatever_the_floor() {
        for samples in [&[][..], &[0], &[160 * US], &[30_000 * US]] {
            assert_eq!(
                pace(samples).interval(FLOOR, Duration::ZERO),
                Duration::ZERO
            );
        }
    }

    #[test]
    fn the_largest_sample_saturates_the_multiplication() {
        let p = pace(&[u64::MAX]);
        assert_eq!(est(&p), u64::MAX);
        assert_eq!(p.interval(FLOOR, CEILING), CEILING);
        let max = Duration::from_nanos(u64::MAX);
        assert_eq!(p.interval(FLOOR, max), max);
        let p = pace(&[u64::MAX, 0]);
        assert_eq!(est(&p), u64::MAX - u64::MAX / 8);
    }

    #[test]
    fn a_single_slow_sync_holds_the_ceiling_for_four_intervals_and_the_floor_returns_after_23() {
        let mut p = pace(&[160 * US; 30]);
        assert_eq!(p.interval(FLOOR, CEILING), FLOOR);
        p.sample(2_000 * US);
        let mut at_ceiling = 0;
        while p.interval(FLOOR, CEILING) == CEILING {
            at_ceiling += 1;
            p.sample(160 * US);
        }
        assert_eq!(at_ceiling, 4);
        let mut above_floor = at_ceiling;
        while p.interval(FLOOR, CEILING) > FLOOR {
            above_floor += 1;
            p.sample(160 * US);
        }
        assert_eq!(above_floor, 23);
    }
}
