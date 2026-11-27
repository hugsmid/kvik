//! Gates: in-store conditions on one stream, judged on the commit clock (spec §6 Gates).
//!
//! A gate reads one series (or, under [`Selector::Each`], each series a name pattern selects) and has a
//! state at every instant. [`judge`] computes, from a history of log entries in position order, every
//! transition of every gate with an identity that depends only on the log ([`TransitionId`]), so a
//! replay after a restart finds the same transitions under the same ids. [`resume`] is the replay a
//! consumer receives from its gate cursor and checkpoint.

mod judge;
mod pattern;
mod resume;

pub use judge::{Judgement, judge};
pub use pattern::{matches as pattern_matches, valid as pattern_valid};
pub use resume::{Frontier, GatePoint, ReplayBound, Resume, resume};

use crate::summary::{Aggregate, Params};
use crate::types::{Fact, Nanos, Position, SeriesId, Value};

/// Identifier of a gate within its consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConditionId(pub u32);

/// The series a gate reads.
#[derive(Clone, Debug, PartialEq)]
pub enum Selector {
    /// One series, by id.
    Series(SeriesId),
    /// One instance per series a name pattern selects (spec §4 Funnel grammar), joining and leaving as
    /// definitions commit; at most [`MAX_INSTANCES`] at a time (Q-1143).
    Each(String),
}

/// What a gate compares.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// The series' live-state value (spec §2.3).
    Value,
    /// A rolling window `[now − width, now)` over the series (spec §6 Aggregates).
    Window(Window),
}

/// A rolling window input, sampled on each fact of its series and on ticks at whole multiples of
/// `cadence` on the commit clock.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// Window width.
    pub width: Nanos,
    /// Aggregate (spec §6).
    pub aggregate: Aggregate,
    /// Least coverage, in thousandths of `width`, below which the input has no value (default 500).
    pub min_coverage: u16,
    /// Tick interval (default 1 s).
    pub cadence: Nanos,
}

/// One boundary of a [`Kind::Levels`] gate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    /// The boundary is crossed upwards once the value is above `at`.
    pub at: f64,
    /// It is crossed back once the value is below `clear` (`clear ≤ at`).
    pub clear: f64,
    /// The value must stay above `at` this long before the upward crossing.
    pub hold_up: Nanos,
    /// The value must stay below `clear` this long before the downward crossing.
    pub hold_down: Nanos,
}

impl Level {
    /// A boundary at `at` with no hysteresis and no holds.
    pub fn at(at: f64) -> Self {
        Level {
            at,
            clear: at,
            hold_up: 0,
            hold_down: 0,
        }
    }

    /// The same boundary with a clear level.
    pub fn clear(self, clear: f64) -> Self {
        Level { clear, ..self }
    }

    /// The same boundary with holds.
    pub fn hold(self, hold_up: Nanos, hold_down: Nanos) -> Self {
        Level {
            hold_up,
            hold_down,
            ..self
        }
    }
}

/// How a gate turns its input into a state.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Ordered boundaries; the state is the number of boundaries crossed upwards, counted from the
    /// lowest while each is crossed (n + 1 states).
    Levels(Vec<Level>),
    /// Whether the value is one of a set of codes, with holds each way.
    Set {
        /// Members, compared as the duplicate rule compares values (spec §2.4).
        members: Vec<Value>,
        /// The value must stay in the set this long before the state becomes `Match(true)`.
        hold_on: Nanos,
        /// The value must stay out of the set this long before the state becomes `Match(false)`.
        hold_off: Nanos,
    },
    /// The value itself, moving when it leaves the dead-band around the last state (codes: any change).
    Change {
        /// Dead-band.
        deadband: f64,
    },
}

/// A gate: one input of one stream, a kind and a staleness limit.
#[derive(Clone, Debug, PartialEq)]
pub struct Gate {
    /// Series.
    pub selector: Selector,
    /// Input.
    pub input: Input,
    /// Kind.
    pub kind: Kind,
    /// Silence after a series' last arrival that makes the gate `Stale`; `None` never.
    pub stale_after: Option<Nanos>,
}

/// Most boundaries in one [`Kind::Levels`] gate.
pub const MAX_LEVELS: usize = 16;
/// Most members of one [`Kind::Set`] gate.
pub const MAX_MEMBERS: usize = 64;
/// Most instances of one [`Selector::Each`] gate at a time.
pub const MAX_INSTANCES: usize = 256;
/// Longest hold, staleness limit, window width or cadence: ten years of 365 days.
pub const MAX_DURATION: Nanos = 10 * 365 * crate::types::DAY;

/// A gate the model refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    /// Levels: none, more than [`MAX_LEVELS`], a level not finite, `at` not strictly increasing, `clear`
    /// above `at` or decreasing.
    #[error("levels must be 1 to 16 finite boundaries, rising, each clear at or below its level")]
    Levels,
    /// Set: empty, more than [`MAX_MEMBERS`] or holding `Absent`.
    #[error("a set holds 1 to 64 values")]
    Set,
    /// A dead-band negative or not finite.
    #[error("dead-band must be finite and not negative")]
    Deadband,
    /// A duration not positive where one is needed, negative, or beyond [`MAX_DURATION`].
    #[error("durations must be positive (holds: not negative) and at most ten years")]
    Duration,
    /// `min_coverage` above 1,000.
    #[error("min_coverage is in thousandths, at most 1000")]
    Coverage,
    /// A pattern the grammar refuses (spec §4 Funnel).
    #[error("pattern refused")]
    Pattern,
    /// History entries not in strictly increasing position order or with commit times decreasing.
    #[error("history out of order at {0}")]
    Order(Position),
    /// A watch under an id in force, or an unwatch of an id not in force.
    #[error("condition id {0:?} watched twice or not in force")]
    Id(ConditionId),
}

impl Gate {
    /// Checks the gate's parameters.
    pub fn validate(&self) -> Result<(), GateError> {
        if let Selector::Each(p) = &self.selector
            && !pattern_valid(p)
        {
            return Err(GateError::Pattern);
        }
        if self
            .stale_after
            .is_some_and(|s| !(1..=MAX_DURATION).contains(&s))
        {
            return Err(GateError::Duration);
        }
        if let Input::Window(w) = &self.input {
            let span = 1..=MAX_DURATION;
            if !span.contains(&w.width) || !span.contains(&w.cadence) {
                return Err(GateError::Duration);
            }
            if w.min_coverage > 1_000 {
                return Err(GateError::Coverage);
            }
        }
        match &self.kind {
            Kind::Levels(ls) => {
                let finite = ls.iter().all(|l| l.at.is_finite() && l.clear.is_finite());
                let each = ls.iter().all(|l| l.clear <= l.at);
                let rising = ls
                    .windows(2)
                    .all(|w| w[0].at < w[1].at && w[0].clear <= w[1].clear);
                if ls.is_empty() || ls.len() > MAX_LEVELS || !finite || !each || !rising {
                    return Err(GateError::Levels);
                }
                if ls
                    .iter()
                    .any(|l| !hold_ok(l.hold_up) || !hold_ok(l.hold_down))
                {
                    return Err(GateError::Duration);
                }
            }
            Kind::Set {
                members,
                hold_on,
                hold_off,
            } => {
                if members.is_empty()
                    || members.len() > MAX_MEMBERS
                    || members.iter().any(|m| matches!(m, Value::Absent))
                {
                    return Err(GateError::Set);
                }
                if !hold_ok(*hold_on) || !hold_ok(*hold_off) {
                    return Err(GateError::Duration);
                }
            }
            Kind::Change { deadband } => {
                if !deadband.is_finite() || *deadband < 0.0 {
                    return Err(GateError::Deadband);
                }
            }
        }
        Ok(())
    }
}

fn hold_ok(d: Nanos) -> bool {
    (0..=MAX_DURATION).contains(&d)
}

/// A gate's state.
#[derive(Clone, Debug, PartialEq)]
pub enum GateState {
    /// No value to judge: no live-state fact yet, or a window below its coverage (Q-1141).
    Unknown,
    /// No arrival for `stale_after`; folds over every other state.
    Stale,
    /// [`Kind::Levels`]: the number of boundaries crossed, 0 to n.
    Level(u8),
    /// [`Kind::Set`]: whether the value is in the set.
    Match(bool),
    /// [`Kind::Change`]: the value delivered last.
    Value(Value),
}

/// A transition's identity, stable across restarts: a function of the log and the commit clock only.
/// Ordered as transitions are delivered: by `at`, then fact-caused before timer-caused, then `due`, gate
/// and series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransitionId {
    /// The cause entry's position; for a timer, the last position before `due`.
    pub at: Position,
    /// The timer's instant on the commit clock; `None` for a transition caused by an entry.
    pub due: Option<Nanos>,
    /// The gate.
    pub condition: ConditionId,
    /// The instance's series (Q-1142).
    pub series: SeriesId,
}

/// Which deadline a timer transition met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    /// A hold completed.
    Hold,
    /// The series fell stale.
    Stale,
    /// A future fact took effect (`t ≤ now`).
    Mature,
    /// A window tick.
    Window,
}

/// What caused a transition.
#[derive(Clone, Debug, PartialEq)]
pub enum Cause {
    /// A fact of the series.
    Fact {
        /// Its position.
        position: Position,
        /// Its event time.
        t: Nanos,
    },
    /// A timer on the commit clock.
    Timer {
        /// Its instant.
        due: Nanos,
        /// Which deadline.
        kind: TimerKind,
    },
    /// A watch or a definition entry: an instance joining or leaving.
    Entry {
        /// Its position.
        position: Position,
    },
}

/// One change of a gate's state.
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    /// Identity.
    pub id: TransitionId,
    /// State before; `None` when the instance starts (a watch, or a series joining an `Each`).
    pub from: Option<GateState>,
    /// State after; `None` when a series leaves an `Each`.
    pub to: Option<GateState>,
    /// Cause.
    pub cause: Cause,
    /// The input's value at evaluation.
    pub value: Option<Value>,
    /// The event time of the live-state fact the value came from (window inputs: `None`).
    pub event_time: Option<Nanos>,
    /// The commit-clock instant of the evaluation.
    pub now: Nanos,
}

/// One log entry as a gate sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// A fact, committed in a batch at `commit_time`.
    Fact {
        /// The batch's commit time.
        commit_time: Nanos,
        /// The fact.
        fact: Fact,
    },
    /// A series definition, or a later change of its names; no names: retired.
    Define {
        /// Position.
        position: Position,
        /// Commit time.
        commit_time: Nanos,
        /// Series.
        series: SeriesId,
        /// Names, aliases included.
        names: Vec<String>,
        /// Summary parameters (window inputs).
        params: Params,
    },
    /// A gate registered on the consumer (a routing entry).
    Watch {
        /// Position.
        position: Position,
        /// Commit time.
        commit_time: Nanos,
        /// Id.
        id: ConditionId,
        /// The gate.
        gate: Gate,
    },
    /// A gate removed.
    Unwatch {
        /// Position.
        position: Position,
        /// Commit time.
        commit_time: Nanos,
        /// Id.
        id: ConditionId,
    },
    /// A reset point (spec §4 Resync), or the start of a new generation: every instance in force is seeded again at
    /// it by the seed rule, with no transition, as if starting there (cond2 design §4.4 step 5; Q-2170).
    Reset {
        /// Position.
        position: Position,
        /// Commit time.
        commit_time: Nanos,
    },
    /// Any other log entry: it moves the frontier only, so a timer's `at` is the last position of the whole log
    /// before its instant (Q-1582).
    Other {
        /// Position.
        position: Position,
        /// Commit time.
        commit_time: Nanos,
    },
}

impl Entry {
    /// The entry's position.
    pub fn position(&self) -> Position {
        match self {
            Entry::Fact { fact, .. } => fact.position,
            Entry::Define { position, .. }
            | Entry::Watch { position, .. }
            | Entry::Unwatch { position, .. }
            | Entry::Reset { position, .. }
            | Entry::Other { position, .. } => *position,
        }
    }

    /// The commit time of the entry's batch.
    pub fn commit_time(&self) -> Nanos {
        match self {
            Entry::Fact { commit_time, .. }
            | Entry::Define { commit_time, .. }
            | Entry::Watch { commit_time, .. }
            | Entry::Unwatch { commit_time, .. }
            | Entry::Reset { commit_time, .. }
            | Entry::Other { commit_time, .. } => *commit_time,
        }
    }
}
