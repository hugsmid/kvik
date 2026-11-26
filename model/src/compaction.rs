//! Where a compaction run (level L0) places each position of its inputs (`spec.md` §7, Partitions and classes): the
//! oracle the durable run is compared with.

use std::collections::{BTreeMap, BTreeSet};

/// A day, in nanoseconds.
pub const DAY: i64 = 86_400_000_000_000;
/// A week, in nanoseconds.
pub const WEEK: i64 = 7 * DAY;

/// A position: generation, then sequence.
pub type Position = (u32, u64);

/// What a position of a run's inputs holds, as far as placement depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// A reading.
    Fact {
        /// Its series.
        series: u32,
        /// Event time.
        t: i64,
        /// `changed_at`, for a row that carries one.
        changed_at: Option<u64>,
        /// Set aside beyond a restored end (§8 Restore step 1).
        set_aside: bool,
    },
    /// Any other entry, placed by its batch's commit time.
    Entry {
        /// The commit time of its batch.
        commit_time: i64,
    },
}

/// The class of the file a position goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Placed {
    /// The current file: the highest-ranked row of its key in the run, or an entry.
    Current,
    /// The revision file: a row outranked within the run.
    Revision,
    /// A set-aside chunk of the revision file.
    SetAside,
}

/// A file of a run: its period `[start, start + len)` and class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Placement {
    /// Period start, UTC nanoseconds.
    pub start: i64,
    /// Period length: [`DAY`] or [`WEEK`].
    pub len: i64,
    /// Class.
    pub class: Placed,
}

/// The period of a time `t`: its UTC day, or its week (weeks from the epoch's day 0) when that week's merge is
/// published or the day ended more than 14 days before `last_commit_time`. `None` when the period leaves `i64`.
pub fn period(t: i64, last_commit_time: i64, weeks_merged: &BTreeSet<i64>) -> Option<(i64, i64)> {
    let day = t.div_euclid(DAY);
    let week = day.div_euclid(7);
    let day_end = day.checked_add(1)?.checked_mul(DAY)?;
    let old = (i128::from(day_end) + 14 * i128::from(DAY)) < i128::from(last_commit_time);
    let (start, len) = if old || weeks_merged.contains(&week) {
        (week.checked_mul(WEEK)?, WEEK)
    } else {
        (day.checked_mul(DAY)?, DAY)
    };
    start.checked_add(len)?;
    Some((start, len))
}

/// Places every position of a run. Within a period the rows of a key `(series, t)` are ranked by position, or by
/// `(changed_at, position)` on rows that carry `changed_at`; the highest goes to the current file. `None` when a
/// period leaves `i64`.
pub fn place_l0(
    rows: &BTreeMap<Position, Held>,
    last_commit_time: i64,
    weeks_merged: &BTreeSet<i64>,
) -> Option<BTreeMap<Position, Placement>> {
    let mut best: BTreeMap<(u32, i64), (u64, Position)> = BTreeMap::new();
    for (&p, h) in rows {
        if let Held::Fact {
            series,
            t,
            changed_at,
            set_aside: false,
        } = *h
        {
            let rank = (changed_at.unwrap_or(0), p);
            let slot = best.entry((series, t)).or_insert(rank);
            if rank > *slot {
                *slot = rank;
            }
        }
    }
    let mut out = BTreeMap::new();
    for (&p, h) in rows {
        let (when, class) = match *h {
            Held::Entry { commit_time } => (commit_time, Placed::Current),
            Held::Fact {
                t, set_aside: true, ..
            } => (t, Placed::SetAside),
            Held::Fact { series, t, .. } => {
                let top = best[&(series, t)].1 == p;
                (
                    t,
                    if top {
                        Placed::Current
                    } else {
                        Placed::Revision
                    },
                )
            }
        };
        let (start, len) = period(when, last_commit_time, weeks_merged)?;
        out.insert(p, Placement { start, len, class });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(series: u32, t: i64, changed_at: Option<u64>) -> Held {
        Held::Fact {
            series,
            t,
            changed_at,
            set_aside: false,
        }
    }

    #[test]
    fn negative_times_fall_in_the_day_before_the_epoch() {
        assert_eq!(period(-1, 0, &BTreeSet::new()), Some((-DAY, DAY)));
        assert_eq!(period(0, 0, &BTreeSet::new()), Some((0, DAY)));
    }

    #[test]
    fn a_day_ended_more_than_fourteen_days_before_the_last_commit_goes_to_its_week() {
        let t = 10 * DAY + 5;
        assert_eq!(period(t, 25 * DAY, &BTreeSet::new()), Some((t - 5, DAY)));
        assert_eq!(
            period(t, 25 * DAY + 1, &BTreeSet::new()),
            Some((7 * DAY, WEEK))
        );
        assert_eq!(period(t, 0, &BTreeSet::from([1])), Some((7 * DAY, WEEK)));
    }

    #[test]
    fn a_period_leaving_i64_places_nothing() {
        assert_eq!(period(i64::MAX, 0, &BTreeSet::new()), None);
        assert_eq!(period(i64::MIN, 0, &BTreeSet::new()), None);
    }

    #[test]
    fn the_highest_rank_of_a_key_is_current_and_changed_at_ranks_first() {
        let rows = BTreeMap::from([
            ((0, 1), fact(1, 5, Some(9))),
            ((0, 2), fact(1, 5, Some(3))),
            ((0, 3), fact(2, 5, None)),
            ((0, 4), fact(2, 5, None)),
            ((0, 5), Held::Entry { commit_time: DAY }),
        ]);
        let placed = place_l0(&rows, DAY, &BTreeSet::new()).unwrap();
        let class = |p| placed[&p].class;
        assert_eq!(class((0, 1)), Placed::Current);
        assert_eq!(class((0, 2)), Placed::Revision);
        assert_eq!(class((0, 3)), Placed::Revision);
        assert_eq!(class((0, 4)), Placed::Current);
        assert_eq!(
            placed[&(0, 5)],
            Placement {
                start: DAY,
                len: DAY,
                class: Placed::Current
            }
        );
    }

    #[test]
    fn set_aside_rows_are_not_ranked() {
        let rows = BTreeMap::from([
            ((0, 1), fact(1, 5, None)),
            (
                (0, 2),
                Held::Fact {
                    series: 1,
                    t: 5,
                    changed_at: None,
                    set_aside: true,
                },
            ),
        ]);
        let placed = place_l0(&rows, 0, &BTreeSet::new()).unwrap();
        assert_eq!(placed[&(0, 1)].class, Placed::Current);
        assert_eq!(placed[&(0, 2)].class, Placed::SetAside);
    }
}
