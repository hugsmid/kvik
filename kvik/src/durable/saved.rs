//! The saved-point check (`spec.md` §7 Readers, Reset stamp and saved points): the resets the files hold, in the
//! order they were written, and the answer for a point. `judge` and `resets` are the model's `check_saved_point` and
//! `Model::resets` statement for statement; a difference between the bodies is a defect.

use kvik_log::entries::Recovery;
use kvik_log::manifest::{LossCause, LostRange};
use kvik_log::writer::Position as LogPos;

use super::index::{log_pos, pos};
use crate::types::{PointCheck, ResetStamp, SavedPoint};

/// A reset as the files hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reset {
    /// A kept recovery entry; no stamp counts as zero.
    Recovery {
        stamp: Option<ResetStamp>,
        at: LogPos,
        after: LogPos,
    },
    /// Never built until the log exposes restore records with their stamps (Q-1400).
    #[cfg_attr(not(test), allow(dead_code))]
    Restore {
        stamp: ResetStamp,
        generation: u32,
        restored_end: LogPos,
    },
    /// A lost range no kept recovery entry describes.
    Lost { from: LogPos, to: LogPos },
}

impl Reset {
    fn stamp(&self) -> Option<ResetStamp> {
        match self {
            Reset::Recovery { stamp, .. } => Some(stamp.unwrap_or_default()),
            Reset::Restore { stamp, .. } => Some(*stamp),
            Reset::Lost { .. } => None,
        }
    }

    fn generation(&self) -> u32 {
        match self {
            Reset::Recovery { at, .. } => at.generation,
            Reset::Restore { generation, .. } => *generation,
            Reset::Lost { from, .. } => from.generation,
        }
    }
}

fn stamp_of(s: kvik_log::mark::ResetStamp) -> ResetStamp {
    ResetStamp {
        sequence: s.sequence,
        nonce: s.nonce,
    }
}

fn lp((generation, sequence): (u32, u64)) -> LogPos {
    LogPos {
        generation,
        sequence,
    }
}

fn before(p: LogPos) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence.saturating_sub(1),
    }
}

/// Whether the recovery entry at `at` keeping up to `after` describes `l`: `l` is exactly its gap, a truncated tail
/// (`spec.md` §4 Resync; Q-124 (a)).
pub(crate) fn describes(at: LogPos, after: LogPos, l: &LostRange) -> bool {
    let (first, last) = (lp(l.first), lp(l.last));
    l.cause == LossCause::TruncatedTail
        && last.generation == at.generation
        && last.sequence.checked_add(1) == Some(at.sequence)
        && after == before(first)
}

/// The resets in the order they were written: the kept recovery entries in log order, each lost range after the reset
/// whose stamp its record carries (zero: first; none, or one no reset carries: last, Q-124 (c)). Restore records
/// are not read yet (Q-1400).
pub(crate) fn resets(recoveries: &[(LogPos, &Recovery)], lost: &[LostRange]) -> Vec<Reset> {
    let after = |at: LogPos, r: &Recovery| LogPos {
        generation: at.generation,
        sequence: r.after,
    };
    let mut out: Vec<Reset> = recoveries
        .iter()
        .map(|(at, r)| Reset::Recovery {
            stamp: r.stamp.map(stamp_of),
            at: *at,
            after: after(*at, r),
        })
        .collect();
    let mut placed: Vec<(usize, Reset)> = Vec::new();
    for l in lost {
        if recoveries
            .iter()
            .any(|(at, r)| describes(*at, after(*at, r), l))
        {
            continue;
        }
        let i = match l.stamp.map(stamp_of) {
            Some(s) if s == ResetStamp::default() => 0,
            Some(s) => out
                .iter()
                .position(|x| x.stamp() == Some(s))
                .map_or(out.len(), |i| i + 1),
            None => out.len(),
        };
        placed.push((
            i,
            Reset::Lost {
                from: lp(l.first),
                to: lp(l.last),
            },
        ));
    }
    // Stable, and inserted from the back: ranges placed at one index keep their recorded order.
    placed.sort_by_key(|x| x.0);
    for (i, r) in placed.into_iter().rev() {
        out.insert(i, r);
    }
    out
}

/// The answer for `point` against the current mark, of `generation` and stamp `current`, and `resets`.
pub(crate) fn judge(
    point: SavedPoint,
    generation: u32,
    current: ResetStamp,
    resets: &[Reset],
) -> PointCheck {
    let zero = ResetStamp::default();
    let p = log_pos(point.position);
    if p.generation == generation && current == point.stamp {
        return PointCheck::Kept;
    }
    let carries = |s: ResetStamp| resets.iter().position(|r| r.stamp() == Some(s));
    if current != zero && carries(current).is_none() {
        return PointCheck::Unknown;
    }
    let later: &[Reset] = match carries(point.stamp).filter(|_| point.stamp != zero) {
        Some(i) => &resets[i + 1..],
        None => resets,
    };
    let mut void = p;
    for r in later {
        if r.generation() < p.generation {
            continue;
        }
        let v = match *r {
            Reset::Recovery { after, .. } => after,
            Reset::Restore { restored_end, .. } if restored_end.generation == p.generation => {
                restored_end
            }
            Reset::Restore { .. } => continue,
            Reset::Lost { from, .. } if from.generation == p.generation => before(from),
            Reset::Lost { .. } => continue,
        };
        void = void.min(v);
    }
    PointCheck::VoidAbove(pos(void))
}

/// `judge` over the recovery entries among `entries` and the manifest's `lost` ranges.
pub(crate) fn check<'a>(
    point: SavedPoint,
    generation: u32,
    current: ResetStamp,
    entries: impl Iterator<Item = (LogPos, &'a kvik_log::entries::Entry)>,
    lost: &[LostRange],
) -> PointCheck {
    let recoveries: Vec<(LogPos, &Recovery)> = entries
        .filter_map(|(p, e)| match e {
            kvik_log::entries::Entry::Recovery(r) => Some((p, r)),
            _ => None,
        })
        .collect();
    judge(point, generation, current, &resets(&recoveries, lost))
}

#[cfg(test)]
mod tests;
