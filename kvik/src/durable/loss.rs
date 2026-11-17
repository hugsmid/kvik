//! Losses found while the store is open (spec §3 Recovery rule 2, §11 Health, Q-200), and the merge of the sources of
//! loss reports into one report per loss.

use std::sync::{Mutex, MutexGuard, PoisonError};

use kvik_log::writer::Position as LogPos;

use super::index::log_pos;
use crate::health::{LossKind, LossReport};

#[derive(Debug, Default)]
struct State {
    found: Vec<LossReport>,
    /// Lost ranges the writer's decisions have not yet taken in.
    fresh: Vec<(LogPos, LogPos)>,
}

/// The losses a process's own reader found, latched for the life of the process and handed to the writer's
/// decisions.
#[derive(Debug, Default)]
pub(crate) struct Latch(Mutex<State>);

impl Latch {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes losses found by a read.
    pub(crate) fn note(&self, found: &[LossReport]) {
        let mut s = self.lock();
        for l in found {
            if let (Some(a), Some(b)) = (l.first, l.last) {
                s.fresh.push((log_pos(a), log_pos(b)));
            }
            match s.found.iter_mut().find(|f| same_loss(f, l)) {
                Some(f) => {
                    widen(f, l);
                }
                None => s.found.push(l.clone()),
            }
        }
    }

    /// The lost ranges found since the last call.
    pub(crate) fn take_fresh(&self) -> Vec<(LogPos, LogPos)> {
        std::mem::take(&mut self.lock().fresh)
    }

    /// Every loss found.
    pub(crate) fn found(&self) -> Vec<LossReport> {
        self.lock().found.clone()
    }
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

/// Adds to `into` each report of `extra` that stands for no loss already there, grows the loss it stands for
/// otherwise, then orders them by identity.
pub(crate) fn merge(into: &mut Vec<LossReport>, extra: impl IntoIterator<Item = LossReport>) {
    for l in extra {
        match into.iter_mut().find(|x| same_loss(x, &l)) {
            Some(x) => {
                widen(x, &l);
            }
            None => into.push(l),
        }
    }
    into.sort_by_key(|l| (l.identity, l.kind == LossKind::PossibleLoss));
}
