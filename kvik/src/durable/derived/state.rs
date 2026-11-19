//! The bounded per-series state of the series directory's fold (`format.md` A.18, Q-777): what the writer's
//! [`super::SeriesFold`] keeps per series and a reader continues from a file it finds (roadmap 9.0c).

use std::collections::BTreeMap;

use kvik_log::directory::series::{Candidate, CandidateValue, Pending, SeriesEntry};
use kvik_log::facts::{FactRow, Quality, Value};
use kvik_log::writer::Position as LogPos;

/// A fact as the fold keeps it: enough to rank it and to lay out a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Kept {
    pub(crate) t: i64,
    pub(crate) position: LogPos,
    pub(crate) changed_at: Option<u64>,
    pub(crate) quality: Quality,
    pub(crate) value: CandidateValue,
}

impl Kept {
    fn rank(&self, versioned: bool) -> (Option<u64>, LogPos) {
        (
            if versioned { self.changed_at } else { None },
            self.position,
        )
    }
}

/// One series' bounded state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    latest_t: Option<i64>,
    reference: Option<i64>,
    facts: u64,
    candidate: Option<Kept>,
    /// The candidate was corrected to bad: unsure until one with a later `t` than this replaces it.
    unsure_until: Option<i64>,
    /// Of the good facts ahead of the horizon, the earliest and the latest; `pending_more`: others lie between.
    pending: Option<Kept>,
    last_ahead: Option<Kept>,
    pending_more: bool,
    /// A kept fact ahead was corrected to bad: facts ahead may exist that the state does not bound.
    ahead_lost: bool,
    /// Versioned series: bad facts above the candidate's `t`, the highest rank at each, so that a good fact a bad one
    /// outranks is never taken; `bad_unknown`: bad facts not kept (past [`BAD_KEPT`], or before a continued file)
    /// may lie at or below this `t`.
    bad: BTreeMap<i64, (Option<u64>, LogPos)>,
    bad_unknown: Option<i64>,
}

/// The most bad facts above the candidate a versioned series' state keeps.
const BAD_KEPT: usize = 16;

/// A fact's value as a candidate's, or `None` for a fact recorded without one (bad).
fn value_of(v: &Value) -> Option<CandidateValue> {
    Some(match v {
        Value::F64(b) => CandidateValue::F64(*b),
        Value::Decimal { units, scale } => CandidateValue::Decimal {
            units: *units,
            scale: *scale,
        },
        Value::State(s) => CandidateValue::State(*s),
        Value::Bool(b) => CandidateValue::Bool(*b),
        Value::Text(_) => CandidateValue::Text,
        Value::Absent { .. } => return None,
    })
}

/// The later of two facts by `t`, then rank.
fn later(a: Option<Kept>, b: Kept, versioned: bool) -> Kept {
    match a {
        Some(a) if (a.t, a.rank(versioned)) > (b.t, b.rank(versioned)) => a,
        _ => b,
    }
}

impl State {
    /// The live candidate: the latest current fact that is not bad and not ahead of the horizon.
    pub(crate) fn candidate(&self) -> Option<Kept> {
        self.candidate
    }

    /// The earliest and the latest good fact ahead of the horizon that the state keeps.
    pub(crate) fn ahead(&self) -> [Option<Kept>; 2] {
        [self.pending, self.last_ahead]
    }

    /// The facts folded, a fact a state continued from a file counted.
    pub(crate) fn count(&self) -> u64 {
        self.facts
    }

    /// The latest recorded `t` and the lateness reference (spec §2.2).
    pub(crate) fn latest(&self) -> (Option<i64>, Option<i64>) {
        (self.latest_t, self.reference)
    }

    /// Whether the candidate may not be the live fact (`format.md` A.18 bit 3).
    pub(crate) fn unsure(&self) -> bool {
        self.unsure_until.is_some() || self.ahead_unsure()
    }

    /// Facts ahead that fell due by `horizon` reach the candidate: once the latest has, every one has.
    pub(crate) fn promote(&mut self, horizon: i64, versioned: bool) {
        if let Some(l) = self.last_ahead.filter(|l| l.t <= horizon) {
            self.candidate = Some(later(self.candidate, l, versioned));
            self.pending = None;
            self.last_ahead = None;
            self.pending_more = false;
            return;
        }
        if let Some(p) = self.pending.filter(|p| p.t <= horizon) {
            self.candidate = Some(later(self.candidate, p, versioned));
            self.pending = None;
            if !self.pending_more {
                self.pending = self.last_ahead;
            }
        }
    }

    /// Whether a fact ahead that the state does not name may have fallen due.
    pub(crate) fn ahead_unsure(&self) -> bool {
        self.ahead_lost || (self.pending.is_none() && self.pending_more)
    }

    pub(crate) fn apply(&mut self, f: &FactRow, position: LogPos, versioned: bool, horizon: i64) {
        self.promote(horizon, versioned);
        self.facts += 1;
        self.latest_t = Some(self.latest_t.map_or(f.t, |t| t.max(f.t)));
        if !f.flags.future && !f.flags.unsynced_clock {
            self.reference = Some(self.reference.map_or(f.t, |t| t.max(f.t)));
        }
        let value = value_of(&f.value).filter(|_| f.quality != Quality::Bad);
        let k = Kept {
            t: f.t,
            position,
            changed_at: f.changed_at,
            quality: f.quality,
            value: value.unwrap_or(CandidateValue::Text),
        };
        let good = value.is_some();
        if versioned && self.outranked(&k, good) {
            return;
        }
        if f.t > horizon {
            self.keep_ahead(k, good, versioned);
            return;
        }
        let unsure = |u: &mut Option<i64>| *u = Some(u.map_or(k.t, |x| x.max(k.t)));
        match self.candidate {
            // A candidate continued from a file has no `changed_at` to rank a revision of it by.
            Some(c) if c.t == k.t && versioned && c.changed_at.is_none() => {
                unsure(&mut self.unsure_until);
            }
            Some(c) if c.t == k.t => {
                if k.rank(versioned) > c.rank(versioned) {
                    if good {
                        self.candidate = Some(k);
                    } else {
                        // The candidate was corrected to bad: the fact in force before it is not kept.
                        unsure(&mut self.unsure_until);
                    }
                }
            }
            Some(c) if c.t > k.t => {}
            _ if !good => {}
            _ => {
                if versioned && self.bad_unknown.is_some_and(|u| u >= k.t) {
                    unsure(&mut self.unsure_until);
                }
                self.candidate = Some(k);
                self.bad = self.bad.split_off(&k.t);
                if self.unsure_until.is_some_and(|u| u < k.t) {
                    self.unsure_until = None;
                }
            }
        }
    }

    /// Versioned series: records a bad fact above the candidate, and tells whether a good one is outranked by a bad
    /// one kept at its `t` (and so not current).
    fn outranked(&mut self, k: &Kept, good: bool) -> bool {
        let rank = k.rank(true);
        if !good {
            if self.candidate.is_none_or(|c| c.t < k.t) {
                if self.bad.len() >= BAD_KEPT && !self.bad.contains_key(&k.t) {
                    self.bad_unknown = Some(self.bad_unknown.map_or(k.t, |u| u.max(k.t)));
                } else {
                    let e = self.bad.entry(k.t).or_insert(rank);
                    *e = (*e).max(rank);
                }
            }
            return false;
        }
        match self.bad.get(&k.t) {
            Some(b) if *b > rank => true,
            Some(_) => {
                self.bad.remove(&k.t);
                false
            }
            None => {
                if self.bad_unknown.is_some_and(|u| u >= k.t)
                    && self.candidate.is_none_or(|c| c.t < k.t)
                {
                    self.ahead_lost = true;
                    self.pending_more = true;
                }
                false
            }
        }
    }

    /// A fact ahead of the horizon: the earliest and latest good ones are kept, any between only counted.
    fn keep_ahead(&mut self, k: Kept, good: bool, versioned: bool) {
        let outranks = |o: Option<Kept>| {
            o.is_some_and(|o| o.t == k.t && k.rank(versioned) > o.rank(versioned))
        };
        let same = |o: Option<Kept>| o.is_some_and(|o| o.t == k.t);
        if !good {
            // A kept fact corrected to bad: what lies ahead is no longer bounded.
            if outranks(self.pending) || outranks(self.last_ahead) {
                self.ahead_lost = true;
                self.pending_more = true;
                if same(self.pending) {
                    self.pending = None;
                }
                if same(self.last_ahead) {
                    self.last_ahead = None;
                }
            }
            return;
        }
        let (Some(p), Some(l)) = (self.pending, self.last_ahead) else {
            if self.pending.is_none() && self.last_ahead.is_none() && !self.ahead_lost {
                self.pending = Some(k);
                self.last_ahead = Some(k);
            } else {
                self.pending_more = true;
                self.ahead_lost = true;
            }
            return;
        };
        if same(Some(p)) || same(Some(l)) {
            if outranks(Some(p)) {
                self.pending = Some(k);
            }
            if outranks(Some(l)) {
                self.last_ahead = Some(k);
            }
        } else if k.t < p.t {
            self.pending_more |= p.t < l.t;
            self.pending = Some(k);
        } else if k.t > l.t {
            self.pending_more |= p.t < l.t;
            self.last_ahead = Some(k);
        } else {
            self.pending_more = true;
        }
    }

    pub(crate) fn entry(&self, id: u32, defined: bool) -> SeriesEntry {
        SeriesEntry {
            series_id: id,
            latest_t: self.latest_t,
            lateness_reference: self.reference,
            fact_count: self.facts,
            orphan_count: if defined { 0 } else { self.facts },
            candidate: self.candidate.map(|c| Candidate {
                t: c.t,
                position: c.position,
                quality: c.quality,
                value: c.value,
            }),
            candidate_unsure: self.unsure_until.is_some() || self.ahead_unsure(),
            pending: self.pending.map(|p| Pending {
                t: p.t,
                position: p.position,
            }),
            // The entry names the earliest fact ahead only: with a later one kept, others are not named.
            pending_more: self.pending_more
                || self.ahead_lost
                || self
                    .pending
                    .zip(self.last_ahead)
                    .is_some_and(|(p, l)| p.t != l.t),
        }
    }

    /// The state an entry describes, written at `written_at`, to continue the fold from (A.18 Crash and rebuild).
    pub(crate) fn of_entry(e: &SeriesEntry, written_at: i64) -> Self {
        let kept = |t, position: LogPos, quality, value| Kept {
            t,
            position,
            changed_at: None,
            quality,
            value,
        };
        let pending = e
            .pending
            .map(|p| kept(p.t, p.position, Quality::Good, CandidateValue::AtPosition));
        State {
            latest_t: e.latest_t,
            reference: e.lateness_reference,
            facts: e.fact_count,
            candidate: e
                .candidate
                .map(|c| kept(c.t, c.position, c.quality, c.value)),
            unsure_until: e.candidate_unsure.then_some(written_at),
            pending,
            // The latest fact ahead is not in the entry: with others ahead, they are unbounded.
            last_ahead: pending.filter(|_| !e.pending_more),
            pending_more: e.pending_more,
            ahead_lost: e.pending_more,
            bad: BTreeMap::new(),
            bad_unknown: e.latest_t,
        }
    }
}
