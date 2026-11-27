//! Builders shared by the gate tests.
#![allow(dead_code)]

use kvik_model::gate::{
    ConditionId, Entry, Gate, GateState, Input, Kind, Level, Selector, Transition,
};
use kvik_model::summary::Params;
use kvik_model::types::{
    Fact, Flags, Kind as SeriesKind, Nanos, Position, Quality, SECOND, SeriesId, Value,
};

pub const S: SeriesId = SeriesId(1);
pub const C: ConditionId = ConditionId(7);

/// A history built in position order.
#[derive(Clone, Debug, Default)]
pub struct H {
    pub entries: Vec<Entry>,
    seq: u64,
}

impl H {
    pub fn new() -> Self {
        H::default()
    }

    fn next(&mut self) -> Position {
        self.seq += 1;
        Position::new(0, self.seq)
    }

    pub fn last(&self) -> Position {
        Position::new(0, self.seq)
    }

    pub fn watch(mut self, secs: i64, id: ConditionId, gate: Gate) -> Self {
        let position = self.next();
        self.entries.push(Entry::Watch {
            position,
            commit_time: secs * SECOND,
            id,
            gate,
        });
        self
    }

    pub fn unwatch(mut self, secs: i64, id: ConditionId) -> Self {
        let position = self.next();
        self.entries.push(Entry::Unwatch {
            position,
            commit_time: secs * SECOND,
            id,
        });
        self
    }

    pub fn reset(mut self, secs: i64) -> Self {
        let position = self.next();
        self.entries.push(Entry::Reset {
            position,
            commit_time: secs * SECOND,
        });
        self
    }

    pub fn define(mut self, secs: i64, series: SeriesId, names: &[&str]) -> Self {
        let position = self.next();
        self.entries.push(Entry::Define {
            position,
            commit_time: secs * SECOND,
            series,
            names: names.iter().map(|n| n.to_string()).collect(),
            params: Params::new(SeriesKind::Gauge, None),
        });
        self
    }

    /// A good numeric fact committed at `secs` with `t` the same instant.
    pub fn num(self, secs: i64, v: f64) -> Self {
        self.fact_on(
            S,
            secs * SECOND,
            secs * SECOND,
            Value::Num(v),
            Quality::Good,
        )
    }

    pub fn num_on(self, series: SeriesId, secs: i64, v: f64) -> Self {
        self.fact_on(
            series,
            secs * SECOND,
            secs * SECOND,
            Value::Num(v),
            Quality::Good,
        )
    }

    pub fn fact_on(
        mut self,
        series: SeriesId,
        commit: Nanos,
        t: Nanos,
        value: Value,
        quality: Quality,
    ) -> Self {
        let position = self.next();
        self.entries.push(Entry::Fact {
            commit_time: commit,
            fact: fact(series, position, commit, t, value, quality),
        });
        self
    }

    pub fn push_fact(mut self, commit: Nanos, mut f: Fact) -> Self {
        f.position = self.next();
        self.entries.push(Entry::Fact {
            commit_time: commit,
            fact: f,
        });
        self
    }
}

pub fn fact(
    series: SeriesId,
    position: Position,
    commit: Nanos,
    t: Nanos,
    value: Value,
    quality: Quality,
) -> Fact {
    Fact {
        series,
        t,
        position,
        received_at: commit,
        value,
        quality,
        supplied_quality: quality,
        changed_at: None,
        flags: Flags::default(),
    }
}

pub fn gate(kind: Kind) -> Gate {
    Gate {
        selector: Selector::Series(S),
        input: Input::Value,
        kind,
        stale_after: None,
    }
}

pub fn levels(ls: &[Level]) -> Gate {
    gate(Kind::Levels(ls.to_vec()))
}

pub fn lv(n: u8) -> Option<GateState> {
    Some(GateState::Level(n))
}

/// `(from, to)` of each transition, in order.
pub fn moves(ts: &[Transition]) -> Vec<(Option<GateState>, Option<GateState>)> {
    ts.iter().map(|t| (t.from.clone(), t.to.clone())).collect()
}

/// The commit-clock instant of each transition, in seconds.
pub fn instants(ts: &[Transition]) -> Vec<i64> {
    ts.iter().map(|t| t.now / SECOND).collect()
}
