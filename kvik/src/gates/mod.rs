//! The gates engine core (spec §6 Gates): one pure state machine per consumer that judges its gates
//! incrementally, batch by batch on the commit clock, giving what `kvik_model::gate::judge` gives over
//! the same log. No clock and no files: facts it cannot hold are read through [`History`].

mod engine;
mod replay;
mod rules;
mod seed;
mod snapshot;

#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

pub(crate) use engine::Core;
pub(crate) use replay::Replay;

use kvik_model::gate::ConditionId;
use kvik_model::types::{Nanos, Position, Quality, SeriesId, Value};

/// One log entry of a batch, as the core is fed it: every entry of the log, so the frontier and timer
/// ids follow the whole log.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    /// Position.
    pub position: Position,
    /// What the entry is to the core.
    pub body: Body,
}

/// The part of an entry the core reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Body {
    /// A fact, with whether a fact already recorded at its `(series, t)` outranks it (spec §2.2).
    Fact { fact: FactItem, outranked: bool },
    /// A watch of this consumer.
    Watch {
        id: ConditionId,
        gate: kvik_model::gate::Gate,
    },
    /// An unwatch of this consumer.
    Unwatch { id: ConditionId },
    /// Any other entry: it moves the frontier only.
    Other,
}

/// A fact's fields the core reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FactItem {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Nanos,
    /// Value.
    pub value: Value,
    /// Quality after stamping.
    pub quality: Quality,
    /// Revision of a versioned series (spec §2.2).
    pub changed_at: Option<u64>,
}

impl FactItem {
    /// Neither bad quality nor without a value: it can be live and it counts as an arrival.
    pub fn usable(&self) -> bool {
        self.quality != Quality::Bad && !matches!(self.value, Value::Absent)
    }
}

/// A fact as one series' state keeps it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Held {
    /// Event time.
    pub t: Nanos,
    /// Position.
    pub position: Position,
    /// Value.
    pub value: Value,
    /// Whether it is usable.
    pub usable: bool,
    /// Revision of a versioned series.
    pub changed_at: Option<u64>,
}

/// What the core keeps of one series an instance reads, at its frontier.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SeriesState {
    /// The live-state fact (spec §2.3) as of the last instant the series was brought up to.
    pub live: Option<Held>,
    /// Latest arrival at or before that instant.
    pub last_arrival: Option<Nanos>,
    /// Facts in effect later (`t` beyond that instant), the one not outranked at each `t`.
    pub ahead: BTreeMap<Nanos, Held>,
}

/// The log reads the core needs and cannot answer from what it holds.
pub(crate) trait History {
    /// The state of `series` at `now` over the facts at or before `through`.
    fn series(&self, series: SeriesId, now: Nanos, through: Position) -> SeriesState;
    /// Facts of `series` committed at or after `from`, at or before `through`, in position order, each
    /// with its batch's commit time.
    fn facts_since(
        &self,
        series: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Vec<(Nanos, Position, FactItem)>;
}

/// A batch or call the core refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CoreError {
    /// A watch under an id in force, or an unwatch of an id not in force.
    #[error("gate {0:?} watched twice or not in force")]
    Id(ConditionId),
    /// A gate the core does not judge yet: `Each` or a window input.
    #[error("gate {0:?} not supported yet")]
    NotSupported(ConditionId),
    /// A gate whose parameters the model refuses.
    #[error("gate {0:?} refused: {1}")]
    Invalid(ConditionId, kvik_model::gate::GateError),
}
