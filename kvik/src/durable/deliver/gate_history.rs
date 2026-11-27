//! The gates core's history reads on the durable backend (Q-1880, Q-2001): the series' facts at or before a position,
//! read from the log, judged by the same rules as the tests' double ([`Replay`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use kvik_log::writer::Position as LogPos;
use kvik_model::types::{Nanos, Position, SeriesId};

use super::{Deliverer, next};
use crate::durable::gates::core_value;
use crate::durable::index::quality_out;
use crate::durable::keys::OnLog;
use crate::gates::{FactItem, History, HistoryError, Replay, SeriesState};

pub(super) fn log_pos(p: Position) -> LogPos {
    LogPos {
        generation: p.generation,
        sequence: p.sequence,
    }
}

pub(super) fn core_pos(p: LogPos) -> Position {
    Position::new(p.generation, p.sequence)
}

/// The series a definition, alias or retirement entry changes: a definition entry to the gates core (for `Each`).
pub(super) fn defined(e: &kvik_log::entries::Entry) -> Option<u32> {
    use kvik_log::entries::Entry;
    match e {
        Entry::Definition(d) => Some(d.id),
        Entry::Alias { id, .. } | Entry::Retirement { id, .. } => Some(*id),
        _ => None,
    }
}

/// The series' names in `view` (after its entry is applied), none once retired.
pub(super) fn names_of(id: u32, view: &crate::durable::routing::View) -> Vec<String> {
    view.registry
        .get(id)
        .filter(|s| !s.retired())
        .map(|s| s.names.clone())
        .unwrap_or_default()
}

/// A fact as the gates core is fed it.
pub(super) fn fact_item(row: &kvik_log::facts::FactRow, d: &Deliverer) -> FactItem {
    let spec = d.view.registry.get(row.series_id).map(|s| &s.spec);
    FactItem {
        series: SeriesId(row.series_id),
        t: row.t,
        value: core_value(&row.value, spec),
        quality: crate::backend::convert::mquality(quality_out(row.quality)),
        changed_at: row.changed_at,
    }
}

/// [`History`] over the deliverer's log: a series' facts read through the segment indexes (`format.md` A.8), once
/// per series, position and window for the core call it serves, so a reseed reads each series once for all its gates
/// (Q-2001). A read takes the facts from a window before the time asked about and answers from them only where
/// the facts before it cannot change the answer; otherwise it widens the window, at last to the whole series.
pub(super) struct LogHistory<'a> {
    d: &'a Deliverer,
    read: RefCell<HashMap<WindowKey, Option<Rc<Window>>>>,
}

/// A window: its series, the position read through and its `lo`.
type WindowKey = (SeriesId, Position, Nanos);

const DAY: Nanos = 86_400_000_000_000;

/// The windows tried, in days before the time asked about, before the whole series (Q-2295). The first covers the
/// longest look-back (24 h).
const WINDOWS: [i64; 3] = [1, 16, 256];

/// The series' facts with `t` at or after `lo`, the commit time of the first of them, and whether they are all it has.
struct Window {
    replay: Replay,
    lo: Nanos,
    first_commit: Nanos,
    whole: bool,
}

impl<'a> LogHistory<'a> {
    pub(super) fn new(d: &'a Deliverer) -> Self {
        LogHistory {
            d,
            read: RefCell::default(),
        }
    }

    /// The facts with `t` at or after `lo`, none when the window is empty but the series is not, or a fact before
    /// it may have been committed after its first (a backfill the window cannot see).
    fn window(
        &self,
        series: SeriesId,
        through: Position,
        lo: Nanos,
    ) -> Result<Option<Rc<Window>>, HistoryError> {
        if let Some(r) = self.read.borrow().get(&(series, through, lo)) {
            return Ok(r.clone());
        }
        #[cfg(test)]
        self.d.gate_reads.set(self.d.gate_reads.get() + 1);
        let unreadable = || HistoryError("the log could not be read".into());
        self.d.slow.set(self.d.slow.get() + 1);
        let upto = log_pos(through);
        let mut reader = self.d.local().ok_or_else(unreadable)?;
        let mut on = OnLog {
            reader: &mut reader,
            lost: &self.d.damaged,
            latch: &self.d.latch,
        };
        let facts = on
            .within(series.0, lo, i64::MAX, None, upto)
            .ok_or_else(unreadable)?;
        // Whether a fact before the window may be committed after its first (a backfill it cannot see), and whether
        // any fact is before it at all.
        let before = |on: &mut OnLog, above| on.any(series.0, i64::MIN, lo - 1, above, upto);
        let (backfilled, whole) = match facts.first() {
            _ if lo == i64::MIN => (false, true),
            None => (false, !before(&mut on, None)),
            Some(f) => match before(&mut on, Some(f.position)) {
                true => (true, false),
                false => (false, !before(&mut on, None)),
            },
        };
        let backfilled = backfilled || (facts.is_empty() && !whole);
        drop(reader);
        #[cfg(test)]
        self.d
            .gate_facts_read
            .set(self.d.gate_facts_read.get() + facts.len() as u64);
        let w = (!backfilled).then(|| {
            let mut replay = Replay::default();
            for f in &facts {
                replay.push(
                    f.received_at,
                    core_pos(f.position),
                    fact_item(&f.row, self.d),
                );
            }
            Rc::new(Window {
                replay,
                lo,
                first_commit: facts.first().map_or(i64::MIN, |f| f.received_at),
                whole,
            })
        });
        self.read
            .borrow_mut()
            .insert((series, through, lo), w.clone());
        Ok(w)
    }

    /// Whether a fact with `t` before `lo` may have been committed at or after `from`.
    fn before_committed_from(
        &self,
        series: SeriesId,
        through: Position,
        lo: Nanos,
        from: Nanos,
    ) -> bool {
        let Some(mut reader) = self.d.local() else {
            return true;
        };
        let above = self.d.index.commit_at(from.saturating_sub(1));
        OnLog {
            reader: &mut reader,
            lost: &self.d.damaged,
            latch: &self.d.latch,
        }
        .any(series.0, i64::MIN, lo - 1, above, log_pos(through))
    }

    /// The first window before `x` whose facts answer exactly (`exact`), else the whole series.
    fn pick<T>(
        &self,
        series: SeriesId,
        through: Position,
        x: Nanos,
        answer: impl Fn(&Replay) -> Result<T, HistoryError>,
        exact: impl Fn(&T, &Window) -> bool,
    ) -> Result<T, HistoryError> {
        for days in WINDOWS {
            // Whole days, so the calls of one core call share their reads.
            let lo = x
                .saturating_sub(days * DAY)
                .div_euclid(DAY)
                .saturating_mul(DAY);
            if let Some(w) = self.window(series, through, lo)? {
                let a = answer(&w.replay)?;
                if w.whole || exact(&a, &w) {
                    return Ok(a);
                }
            }
        }
        match self.window(series, through, i64::MIN)? {
            Some(w) => answer(&w.replay),
            None => Err(HistoryError("the log could not be read".into())),
        }
    }
}

impl Deliverer {
    /// The bounded history's answers at the head against the whole series' (Q-2001): the first that differs.
    #[cfg(test)]
    pub(crate) fn gate_history_differs(
        &self,
        series: SeriesId,
        now: Nanos,
        from: Nanos,
    ) -> Option<String> {
        let through = core_pos(self.head);
        let h = LogHistory::new(self);
        let whole = h.window(series, through, i64::MIN).ok()??;
        let w = &whole.replay;
        let pairs = [
            (
                format!("{:?}", h.series(series, now, through)),
                format!("{:?}", w.series(series, now, through)),
            ),
            (
                format!("{:?}", h.at_look_back(series, from, through)),
                format!("{:?}", w.at_look_back(series, from, through)),
            ),
            (
                format!("{:?}", h.facts_since(series, from, through)),
                format!("{:?}", w.facts_since(series, from, through)),
            ),
        ];
        pairs
            .into_iter()
            .find(|(a, b)| a != b)
            .map(|(a, b)| format!("bounded {a}\nwhole {b}"))
    }
}

// What a window omits is facts with `t` before `lo`, each committed at or before its first fact (else no window).
// They change no answer about a time at or after `lo` once the window holds a usable fact in effect then; nor the
// last arrival once the window's is at or after both `lo` and its first commit; nor the facts committed from a
// time after its first commit, or after which none of them was committed.
impl History for LogHistory<'_> {
    fn series(
        &self,
        series: SeriesId,
        now: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.pick(
            series,
            through,
            now,
            |r| r.series(series, now, through),
            |st, w| {
                st.live.is_some()
                    && st
                        .last_arrival
                        .is_some_and(|a| a >= w.lo && a >= w.first_commit)
            },
        )
    }

    fn at_look_back(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.pick(
            series,
            through,
            from,
            |r| r.at_look_back(series, from, through),
            |st, _| st.live.is_some(),
        )
    }

    fn facts_since(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<Vec<(Nanos, Position, FactItem, bool)>, HistoryError> {
        self.pick(
            series,
            through,
            from,
            |r| r.facts_since(series, from, through),
            |_, w| {
                w.first_commit < from || !self.before_committed_from(series, through, w.lo, from)
            },
        )
    }

    fn matching(&self, pattern: &str, through: Position) -> Result<Vec<SeriesId>, HistoryError> {
        let view = if log_pos(through) >= self.d.head {
            None
        } else {
            Some(self.d.view_before(next(log_pos(through))))
        };
        let v = view.as_ref().unwrap_or(&self.d.view);
        Ok(v.matching(pattern).into_iter().map(SeriesId).collect())
    }
}
