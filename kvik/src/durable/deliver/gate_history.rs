//! The gates core's history reads on the durable backend (Q-1880, Q-2001): the series' facts at or before a position,
//! read from the log, judged by the same rules as the tests' double ([`Replay`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use kvik_log::writer::Position as LogPos;
use kvik_model::types::{Nanos, Position, SeriesId};

use super::Deliverer;
use crate::durable::gates::core_value;
use crate::durable::index::quality_out;
use crate::durable::keys::{LocalFacts, OnLog};
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
/// per series and position for the core call it serves, so a reseed reads each series once for all its gates
/// (Q-2001).
pub(super) struct LogHistory<'a> {
    d: &'a Deliverer,
    read: RefCell<HashMap<(SeriesId, Position), Rc<Replay>>>,
}

impl<'a> LogHistory<'a> {
    pub(super) fn new(d: &'a Deliverer) -> Self {
        LogHistory {
            d,
            read: RefCell::default(),
        }
    }

    fn replay(&self, series: SeriesId, through: Position) -> Result<Rc<Replay>, HistoryError> {
        if let Some(r) = self.read.borrow().get(&(series, through)) {
            return Ok(r.clone());
        }
        #[cfg(test)]
        self.d.gate_reads.set(self.d.gate_reads.get() + 1);
        let unreadable = || HistoryError("the log could not be read".into());
        self.d.slow.set(self.d.slow.get() + 1);
        let mut reader = self.d.local().ok_or_else(unreadable)?;
        let facts = OnLog {
            reader: &mut reader,
            lost: &self.d.damaged,
            latch: &self.d.latch,
        }
        .between(series.0, i64::MIN, i64::MAX)
        .ok_or_else(unreadable)?;
        drop(reader);
        let through = log_pos(through);
        let mut r = Replay::default();
        for f in facts.iter().filter(|f| f.position <= through) {
            r.push(
                f.received_at,
                core_pos(f.position),
                fact_item(&f.row, self.d),
            );
        }
        let r = Rc::new(r);
        self.read
            .borrow_mut()
            .insert((series, core_pos(through)), r.clone());
        Ok(r)
    }
}

impl History for LogHistory<'_> {
    fn series(
        &self,
        series: SeriesId,
        now: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.replay(series, through)?.series(series, now, through)
    }

    fn at_look_back(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.replay(series, through)?
            .at_look_back(series, from, through)
    }

    fn facts_since(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<Vec<(Nanos, Position, FactItem, bool)>, HistoryError> {
        self.replay(series, through)?
            .facts_since(series, from, through)
    }
}
