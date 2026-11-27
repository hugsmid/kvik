//! The gates core's history reads on the durable backend (Q-1880): the series' facts at or before a position,
//! read from the log, judged by the same rules as the tests' double ([`Replay`]).

use kvik_log::writer::Position as LogPos;
use kvik_model::types::{Nanos, Position, SeriesId};

use super::{Ctx, Deliverer, next};
use crate::durable::gates::core_value;
use crate::durable::index::quality_out;
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

/// [`History`] over the deliverer's log. Each read scans the log from its start to `through` (Q-2001).
pub(super) struct LogHistory<'a> {
    pub(super) d: &'a Deliverer,
    pub(super) ctx: &'a Ctx<'a>,
}

impl LogHistory<'_> {
    fn replay(&self, series: SeriesId, through: Position) -> Result<Replay, HistoryError> {
        let facts = self
            .d
            .facts_between(LogPos::default(), next(log_pos(through)), self.ctx)
            .ok_or_else(|| HistoryError("the log could not be read".into()))?;
        let mut r = Replay::default();
        for (stored, _, _) in facts.iter().filter(|(s, _, _)| s.row.series_id == series.0) {
            r.push(
                stored.received_at,
                core_pos(stored.position),
                fact_item(&stored.row, self.d),
            );
        }
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
