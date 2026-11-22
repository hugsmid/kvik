//! Losses found while the store is open (spec §3 Recovery rule 2, §11 Health, Q-200), and the merge of the sources of
//! loss reports into one report per loss. [`Finder`] finds them in what a log reader returns, [`Watch`] is the reader
//! that finds them for `losses` and `health` (Q-927), and the [`Latch`] holds what every finder of the process found.

mod finder;
mod regions;
mod watch;

use std::sync::{Mutex, MutexGuard, PoisonError};

use kvik_log::writer::Position as LogPos;

pub(crate) use finder::{Find, Finder};
pub(crate) use regions::Regions;
pub(crate) use watch::Watch;

use super::index::log_pos;
use crate::health::{LossKind, LossReport};
use crate::types::Position;

#[derive(Debug, Default)]
struct State {
    /// Ordered by first position, no two overlapping.
    found: Vec<LossReport>,
    /// Lost ranges the writer's decisions have not yet taken in.
    fresh: Vec<Fresh>,
    /// Every closed range taken, in the order taken, once per region (Q-938).
    closed: Vec<(LogPos, LogPos)>,
    regions: Regions,
}

/// A lost range for the writer's decisions, and whether its find grew the regions found (Q-943): a find of damage
/// already found grows nothing, whatever its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fresh {
    pub(crate) first: LogPos,
    pub(crate) last: LogPos,
    pub(crate) grew: bool,
}

/// The losses the scanning readers of a process found, latched for the life of the process: the reports, the
/// ranges for the writer's decisions and for the reader that answers queries, and the regions (Q-926).
#[derive(Debug, Default)]
pub(crate) struct Latch(Mutex<State>);

impl Latch {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes losses found by a read, by any reader of the process, the kept log included (Q-928).
    pub(crate) fn note(&self, found: &[Find]) {
        let mut s = self.lock();
        for f in found {
            let l = &f.report;
            let grew = !s.regions.covers(f.file_id, f.span.0, f.span.1);
            s.regions.insert(f.file_id, f.span.0, f.span.1);
            if let (Some(a), Some(b)) = (l.first, l.last) {
                s.fresh.push(Fresh {
                    first: log_pos(a),
                    last: log_pos(b),
                    grew,
                });
            }
            // A region met again holds no fact a reader still took: its closed range was handed on when first met.
            if grew && f.closed.0 <= f.closed.1 {
                s.closed.push(f.closed);
            }
            hold(&mut s.found, l);
        }
    }

    /// Marks the end of the start's read; the ranges the start found are not for the decisions, which it built
    /// without them.
    pub(crate) fn started(&self) {
        self.lock().fresh.clear();
    }

    /// The lost ranges found since the last call.
    pub(crate) fn take_fresh(&self) -> Vec<Fresh> {
        std::mem::take(&mut self.lock().fresh)
    }

    /// The closed ranges taken after the first `*seen`, which then counts them all.
    pub(crate) fn closed_since(&self, seen: &mut usize) -> Vec<(LogPos, LogPos)> {
        let s = self.lock();
        let new = s.closed.get(*seen..).unwrap_or_default().to_vec();
        *seen = s.closed.len();
        new
    }

    /// How many closed ranges were taken.
    pub(crate) fn closed_count(&self) -> usize {
        self.lock().closed.len()
    }

    /// Whether the regions found hold the whole span `start..end` of segment `file_id` (Q-922).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn covers(&self, file_id: u64, start: u64, end: u64) -> bool {
        self.lock().regions.covers(file_id, start, end)
    }

    /// Every loss found.
    pub(crate) fn found(&self) -> Vec<LossReport> {
        self.lock().found.clone()
    }
}

/// Adds lost-data report `l` to `found`, ordered by first position: a report it overlaps is the same loss and grows
/// to hold it, and reports it bridges become one under the identity of the lowest (Q-940).
fn hold(found: &mut Vec<LossReport>, l: &LossReport) {
    let lo = found.partition_point(|f| f.last < l.first);
    let hi = found.partition_point(|f| f.first <= l.last);
    if lo >= hi {
        found.insert(lo, l.clone());
        return;
    }
    let mut merged = found[lo].clone();
    for f in found.drain(lo + 1..hi).collect::<Vec<_>>() {
        widen(&mut merged, &f);
    }
    widen(&mut merged, l);
    found[lo] = merged;
}

/// Whether two reports stand for one loss: of one kind, with the same identity or, for lost data, overlapping ranges
/// (a record that extends a range is the same loss, spec §3 Order of a start step 4). A recovery entry's
/// `possible_loss` and the lost range of its gap share an identity and are two losses.
fn same_loss(a: &LossReport, b: &LossReport) -> bool {
    if a.kind != b.kind {
        return false;
    }
    if a.identity == b.identity {
        return true;
    }
    match (a.kind, a.first, a.last, b.first, b.last) {
        (LossKind::DataLost, Some(a0), Some(a1), Some(b0), Some(b1)) => a0 <= b1 && b0 <= a1,
        _ => false,
    }
}

fn covers(a: &LossReport, b: &LossReport) -> bool {
    a.first <= b.first && b.last <= a.last
}

/// Grows lost range `into` to hold `by`, a report of the same loss, keeping its identity (spec §3 Order of a start
/// step 4). The grown loss is recorded only if a recorded report covers it (Q-200). Returns whether it grew.
pub(crate) fn widen(into: &mut LossReport, by: &LossReport) -> bool {
    if into.kind != LossKind::DataLost || covers(into, by) {
        return false;
    }
    into.recorded = by.recorded && covers(by, into);
    if by.first < into.first {
        into.first = by.first;
        into.commit_time_before = by.commit_time_before;
    }
    if by.last > into.last {
        into.last = by.last;
        into.commit_time_after = by.commit_time_after;
    }
    into.bytes = into.bytes.max(by.bytes);
    true
}

/// Whether ranges acknowledged under the identities folded into `l`, its own included, cover every position of
/// it (Q-1108); positions follow one another within a generation.
pub(crate) fn acknowledged(
    l: &LossReport,
    range: impl Fn(Position) -> Option<(Position, Position)>,
) -> bool {
    let (Some(first), Some(last)) = (l.first, l.last) else {
        return false;
    };
    let mut ranges: Vec<_> = std::iter::once(l.identity)
        .chain(l.absorbed.iter().copied())
        .filter_map(range)
        .collect();
    ranges.sort();
    let mut need = first;
    for (a, b) in ranges {
        if a > need {
            return false;
        }
        if b >= last {
            return true;
        }
        if b >= need {
            let Some(s) = b.sequence.checked_add(1) else {
                return false;
            };
            need = Position::new(b.generation, s);
        }
    }
    false
}

/// Notes in `into` that `by`, a recorded loss or one that absorbed some, is folded into it (Q-1108).
fn absorb(into: &mut LossReport, by: &LossReport) {
    let own = by.recorded && by.identity != into.identity;
    into.absorbed.extend(own.then_some(by.identity));
    into.absorbed.extend(by.absorbed.iter().copied());
    into.absorbed.retain(|i| *i != into.identity);
    into.absorbed.sort();
    into.absorbed.dedup();
}

/// Recorded lost data of identities whose ranges overlap, as one loss under the lowest identity (Q-1102); the rest
/// unchanged, in no particular order.
pub(crate) fn fold_recorded(reports: Vec<LossReport>) -> Vec<LossReport> {
    let (mut data, mut out): (Vec<_>, Vec<_>) = reports
        .into_iter()
        .partition(|l| l.kind == LossKind::DataLost && l.recorded);
    data.sort_by_key(|l| l.first);
    let mut folded: Vec<LossReport> = Vec::with_capacity(data.len());
    for l in data {
        match folded.last_mut() {
            Some(f) if l.first <= f.last => {
                let by = if l.identity < f.identity {
                    std::mem::replace(f, l)
                } else {
                    l
                };
                if by.first < f.first {
                    f.first = by.first;
                    f.commit_time_before = by.commit_time_before;
                }
                if by.last > f.last {
                    f.last = by.last;
                    f.commit_time_after = by.commit_time_after;
                }
                f.bytes = f.bytes.max(by.bytes);
                absorb(f, &by);
            }
            _ => folded.push(l),
        }
    }
    out.extend(folded);
    out
}

/// Adds to `into` each report of `extra` that stands for no loss already there, grows the loss it stands for
/// otherwise, then orders them by identity. A report that stands for several makes them one under the lowest
/// identity (Q-940, Q-1102).
pub(crate) fn merge(into: &mut Vec<LossReport>, extra: impl IntoIterator<Item = LossReport>) {
    for l in extra {
        let (mut same, rest): (Vec<_>, Vec<_>) = into.drain(..).partition(|x| same_loss(x, &l));
        *into = rest;
        same.sort_by_key(|x| x.identity);
        let mut it = same.into_iter();
        let Some(mut kept) = it.next() else {
            into.push(l);
            continue;
        };
        for by in it.chain(std::iter::once(l)) {
            widen(&mut kept, &by);
            absorb(&mut kept, &by);
        }
        into.push(kept);
    }
    into.sort_by_key(|l| (l.identity, l.kind == LossKind::PossibleLoss));
}

#[cfg(test)]
mod tests;
