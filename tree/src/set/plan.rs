//! The merge planner (design 7.1b section 5 Merges): a pure function of the root, `now` and the policy, as
//! compaction's.

use std::collections::BTreeSet;

use super::{cover_levels, covered, interleaved, rollups};
use crate::level::MINUTE;
use crate::part::Class;
use crate::root::{PartRef, Root};

/// One UTC day in nanoseconds.
pub const DAY: i64 = 1_440 * MINUTE;
const WEEK: i64 = 7 * DAY;
/// 1970-01-05T00:00Z, the first Monday: weeks, and the 13- and 52-week periods, count from it (Q-1552).
const MONDAY: i64 = 4 * DAY;
/// Deltas of one day that merge whatever their age.
const DELTAS: usize = 8;
/// Two deltas merge once the oldest's last entry ends this long before `now` (Q-1551).
const DELTA_AGE: i64 = 120 * MINUTE;
/// A merged period overlaid by more parts than this is merged again.
const OVERLAID: usize = 4;
/// Weeks and longer periods merge this long after they end.
const SETTLE: i64 = 2 * WEEK;

/// The periods a merged part spans (section 5 Merges).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// A UTC day.
    Day,
    /// A week from a Monday.
    Week,
    /// 13 weeks.
    Weeks13,
    /// 52 weeks.
    Weeks52,
}

impl Tier {
    const ALL: [Tier; 4] = [Tier::Day, Tier::Week, Tier::Weeks13, Tier::Weeks52];

    fn len(self) -> i64 {
        match self {
            Tier::Day => DAY,
            Tier::Week => WEEK,
            Tier::Weeks13 => 13 * WEEK,
            Tier::Weeks52 => 52 * WEEK,
        }
    }

    /// The period of this tier holding `t`, as `(start, length)`; `None` when its start is before the least
    /// instant.
    pub fn period(self, t: i64) -> Option<(i64, u64)> {
        let (len, origin) = match self {
            Tier::Day => (DAY, 0),
            _ => (self.len(), MONDAY),
        };
        let rem = (i128::from(t) - i128::from(origin)).rem_euclid(i128::from(len)) as i64;
        Some((t.checked_sub(rem)?, len as u64))
    }

    fn of(start: i64, len: u64) -> Option<Tier> {
        Tier::ALL
            .into_iter()
            .find(|t| t.len() as u64 == len && t.period(start).is_some_and(|p| p.0 == start))
    }

    fn due(self, start: i64, policy: &Policy) -> i64 {
        let wait = match self {
            Tier::Day => policy.late_window,
            _ => SETTLE,
        };
        start.saturating_add(self.len()).saturating_add(wait)
    }
}

/// The planner's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// After a day ends, how long late data is awaited before its day part is written.
    pub late_window: i64,
    /// How long entries finer than a quarter-hour are kept (`tree.minute_keep`, Q-1420).
    pub minute_keep: i64,
}

/// One merge: the inputs, the output periods (one part per period that keeps an entry), whether roll-ups are
/// recomputed, and the end at or before which finer-than-quarter-hour entries are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Input `file_id`s, ascending.
    pub inputs: Vec<u64>,
    /// Output periods `(start, length)`, ascending and disjoint.
    pub periods: Vec<(i64, u64)>,
    /// Recompute every roll-up of the periods (T2); the inputs are then every live part of the periods.
    pub rollups: bool,
    /// Finer-than-quarter-hour entries ending at or before this are dropped.
    pub minute_from: i64,
}

fn end(r: &PartRef) -> i64 {
    r.period_start.saturating_add(r.period_len as i64)
}

fn overlaps(r: &PartRef, start: i64, len: u64) -> bool {
    r.period_start < start.saturating_add(len as i64) && start < end(r)
}

fn inside(r: &PartRef, start: i64, len: u64) -> bool {
    start <= r.period_start && end(r) <= start.saturating_add(len as i64)
}

fn tier(r: &PartRef) -> Option<Tier> {
    (r.class != Class::Delta)
        .then(|| Tier::of(r.period_start, r.period_len))
        .flatten()
}

fn day(r: &PartRef) -> Option<i64> {
    let d = Tier::Day.period(r.period_start)?;
    inside(r, d.0, d.1).then_some(d.0)
}

/// Whether day `d`'s roll-ups are written: every roll-up span inside the day that the finest cover level covers is
/// covered at its own level. A day with nothing covered counts as merged, so it never waits on a day part (Q-1555).
fn merged(root: &Root, d: i64) -> bool {
    let Some(base) = cover_levels(&root.levels).last().copied() else {
        return true;
    };
    rollups(&root.levels)
        .into_iter()
        .filter(|(p, _)| DAY % p.width == 0)
        .all(|(p, _)| {
            (0..DAY / p.width)
                .filter_map(|i| {
                    let x = d.checked_add(i * p.width)?;
                    Some((x, x.checked_add(p.width)?))
                })
                .filter(|&(x, e)| covered(root, base.id, x, e))
                .all(|(x, e)| covered(root, p.id, x, e))
        })
}

/// The merges due under `root` at `now`, in order, each input in at most one (section 5 Merges): a part whose
/// period crosses a day boundary and is no merged period is split by day (Q-1553); a merged period past its late
/// window or settling time and overlaid by more than four parts is merged again; a day past its late window and
/// not merged gets its day part, whatever its parts' class (Q-1820); eight
/// deltas of a day, or two once the oldest is two hours old, merge into one; weeks, 13 and 52 weeks merge two
/// weeks after they end.
pub fn plan(root: &Root, now: i64, policy: &Policy) -> Vec<Plan> {
    let minute_from = now.saturating_sub(policy.minute_keep);
    let mut used: BTreeSet<u64> = BTreeSet::new();
    let mut plans: Vec<Plan> = Vec::new();
    let take = |plans: &mut Vec<Plan>,
                used: &mut BTreeSet<u64>,
                mut inputs: Vec<u64>,
                periods: Vec<(i64, u64)>,
                rollups: bool| {
        inputs.sort_unstable();
        used.extend(&inputs);
        plans.push(Plan {
            inputs,
            periods,
            rollups,
            minute_from,
        });
    };

    for r in &root.parts {
        if tier(r).is_some() || day(r).is_some() {
            continue;
        }
        let periods = if r.least_start < r.greatest_end {
            let Some((mut d, _)) = Tier::Day.period(r.least_start) else {
                continue;
            };
            let mut v = Vec::new();
            while d < r.greatest_end {
                v.push((d, DAY as u64));
                let Some(next) = d.checked_add(DAY) else {
                    break;
                };
                d = next;
            }
            v
        } else {
            Vec::new()
        };
        if interleaved(root, &[r.file_id], &periods).is_some() {
            continue;
        }
        take(&mut plans, &mut used, vec![r.file_id], periods, false);
    }

    for m in &root.parts {
        let Some(t) = tier(m) else { continue };
        if used.contains(&m.file_id) || now < t.due(m.period_start, policy) {
            continue;
        }
        let over = root
            .parts
            .iter()
            .filter(|r| r.file_id != m.file_id && inside(r, m.period_start, m.period_len))
            .count();
        if over > OVERLAID
            && let Some(inputs) = whole(root, &used, m.period_start, m.period_len)
        {
            take(
                &mut plans,
                &mut used,
                inputs,
                vec![(m.period_start, m.period_len)],
                true,
            );
        }
    }

    let days: BTreeSet<i64> = root
        .parts
        .iter()
        .filter(|r| !used.contains(&r.file_id))
        .filter_map(day)
        .collect();
    for d in days {
        let span = (d, DAY as u64);
        if !merged(root, d) && now >= Tier::Day.due(d, policy) {
            if let Some(inputs) = whole(root, &used, d, DAY as u64) {
                take(&mut plans, &mut used, inputs, vec![span], true);
            }
            continue;
        }
        if let Some(inputs) = deltas(root, &used, d, now) {
            take(&mut plans, &mut used, inputs, vec![span], false);
        }
    }

    for t in [Tier::Week, Tier::Weeks13, Tier::Weeks52] {
        let periods: BTreeSet<i64> = root
            .parts
            .iter()
            .filter(|r| !used.contains(&r.file_id) && tier(r).is_none_or(|x| x < t))
            .filter_map(|r| t.period(r.period_start).map(|p| p.0))
            .collect();
        for p in periods {
            let len = t.len() as u64;
            if now < t.due(p, policy) {
                continue;
            }
            let Some(inputs) = whole(root, &used, p, len) else {
                continue;
            };
            let parts: Vec<&PartRef> = root
                .parts
                .iter()
                .filter(|r| inputs.contains(&r.file_id))
                .collect();
            if parts.len() >= 2 && parts.iter().all(|r| tier(r) != Some(t)) {
                take(&mut plans, &mut used, inputs, vec![(p, len)], true);
            }
        }
    }
    plans
}

/// Every live part overlapping the period, when all are unused and inside it.
fn whole(root: &Root, used: &BTreeSet<u64>, start: i64, len: u64) -> Option<Vec<u64>> {
    let near: Vec<&PartRef> = root
        .parts
        .iter()
        .filter(|r| overlaps(r, start, len))
        .collect();
    let ok = !near.is_empty()
        && near
            .iter()
            .all(|r| !used.contains(&r.file_id) && inside(r, start, len));
    ok.then(|| near.iter().map(|r| r.file_id).collect())
}

/// The largest run of the day's unused deltas, consecutive in `seq` among the parts overlapping the day, when it is
/// due: eight, or two with the oldest's last entry two hours before `now`.
fn deltas(root: &Root, used: &BTreeSet<u64>, d: i64, now: i64) -> Option<Vec<u64>> {
    let mut near: Vec<&PartRef> = root
        .parts
        .iter()
        .filter(|r| overlaps(r, d, DAY as u64))
        .collect();
    near.sort_by_key(|r| (r.seq, r.file_id));
    let ok = |r: &PartRef| {
        r.class == Class::Delta && !used.contains(&r.file_id) && inside(r, d, DAY as u64)
    };
    let mut best: Vec<&PartRef> = Vec::new();
    let mut run: Vec<&PartRef> = Vec::new();
    for (i, r) in near.iter().enumerate() {
        let tied_with_blocker = near
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && o.seq == r.seq && !ok(o));
        if ok(r) && !tied_with_blocker {
            run.push(r);
        } else {
            run.clear();
        }
        if run.len() > best.len() {
            best = run.clone();
        }
    }
    let due = best.len() >= DELTAS
        || (best.len() >= 2 && best[0].greatest_end <= now.saturating_sub(DELTA_AGE));
    due.then(|| best.iter().map(|r| r.file_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::QUARTER;

    #[test]
    fn weeks_start_on_monday_and_each_period_lies_inside_the_next() {
        let monday_2026_01_05 = 1_767_571_200_000_000_000;
        assert_eq!(
            Tier::Week.period(monday_2026_01_05 + DAY),
            Some((monday_2026_01_05, WEEK as u64))
        );
        let (s52, l52) = Tier::Weeks52.period(monday_2026_01_05).unwrap();
        let (s13, _) = Tier::Weeks13.period(monday_2026_01_05).unwrap();
        assert!(s52 <= s13 && s13 < s52 + l52 as i64);
        assert_eq!(QUARTER * 96, DAY);
    }
}
