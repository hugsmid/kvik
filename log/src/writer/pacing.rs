//! Batch-sync pacing (spec §3 Group commit, Q-980 to Q-984): the spacing that holds between batch syncs under load,
//! and the smoothed batch-sync duration reported in `LogStats::sync_time`.

use std::time::Duration;

/// A shorter sample moves the estimate down by this fraction of the difference.
pub(crate) const FALL: u64 = 8;

pub(crate) fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The least time between the starts of two batch syncs when calls were waiting as the first returned:
/// `min(sync_interval_floor, min_sync_interval)`, in `u64` nanoseconds (Q-981).
pub(crate) fn spacing(floor: Duration, ceiling: Duration) -> Duration {
    Duration::from_nanos(nanos(floor).min(nanos(ceiling)))
}

/// When the next batch sync may start: at once unless calls were waiting as the previous batch sync returned, then
/// `spacing` after that sync started (Q-980, Q-982).
pub(crate) fn ready_at(previous: Option<Duration>, waited: bool, spacing: Duration) -> Duration {
    match previous {
        Some(start) if waited => start.saturating_add(spacing),
        _ => Duration::ZERO,
    }
}

/// The smoothed duration of the batch syncs that succeeded, in nanoseconds: it rises at once and falls by an eighth
/// of the difference per sample. A statistic only; it sets no timing (Q-984).
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
    fn the_spacing_is_the_floor_capped_by_the_ceiling() {
        assert_eq!(spacing(FLOOR, CEILING), FLOOR);
        assert_eq!(spacing(20 * MS, CEILING), CEILING);
        assert_eq!(spacing(Duration::ZERO, CEILING), Duration::ZERO);
        assert_eq!(spacing(FLOOR, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn the_spacing_stays_within_u64_nanoseconds() {
        let top = Duration::from_nanos(u64::MAX);
        assert_eq!(spacing(Duration::MAX, Duration::MAX), top);
        assert_eq!(spacing(FLOOR, Duration::MAX), FLOOR);
    }

    #[test]
    fn a_batch_sync_with_nothing_waiting_as_it_returned_leaves_the_next_free_to_start_at_once() {
        assert_eq!(ready_at(None, true, FLOOR), Duration::ZERO);
        assert_eq!(ready_at(Some(5 * MS), false, FLOOR), Duration::ZERO);
        assert_eq!(ready_at(Some(5 * MS), true, FLOOR), 7 * MS);
        assert_eq!(ready_at(Some(5 * MS), true, Duration::ZERO), 5 * MS);
    }

    #[test]
    fn the_ready_time_saturates() {
        let top = Duration::from_nanos(u64::MAX);
        assert_eq!(ready_at(Some(Duration::MAX), true, top), Duration::MAX);
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
    fn no_sample_gives_no_estimate_and_the_largest_saturates() {
        assert_eq!(SyncPace::default().estimate(), None);
        let p = pace(&[u64::MAX]);
        assert_eq!(est(&p), u64::MAX);
        let p = pace(&[u64::MAX, 0]);
        assert_eq!(est(&p), u64::MAX - u64::MAX / 8);
    }
}
