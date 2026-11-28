//! Gates: in-store conditions on one stream (spec §6 Gates, `docs/api.md` Gates).
//!
//! A consumer watches a gate under a name with [`Consumer::watch`](crate::Consumer::watch); kvik judges it on each
//! reading of its series and delivers a [`Transition`] when its state changes.

use std::time::Duration;

use crate::error::RouteError;
use crate::read::Aggregate;
use crate::series::SeriesKey;
use crate::time::Timestamp;
use crate::types::{Position, SeriesId, Value};

/// Most gates of one consumer.
pub const MAX_GATES_PER_CONSUMER: usize = 4_096;
/// Most gates of one store, over all consumers.
pub const MAX_GATES_PER_STORE: usize = 65_536;
/// Most boundaries of one levels gate.
pub const MAX_GATE_LEVELS: usize = 16;
/// Most members of one set gate.
pub const MAX_GATE_MEMBERS: usize = 64;
/// Most instances of one gate under [`Gate::each`] at a time (Q-1143).
pub const MAX_GATE_INSTANCES: usize = 256;
/// Longest gate name, in bytes.
pub const MAX_GATE_NAME: usize = 64;
/// Longest hold, staleness limit, window width or cadence: ten years of 365 days.
pub const MAX_GATE_DURATION: Duration = Duration::from_secs(10 * 365 * 86_400);

/// Gate identifier, unique within its consumer and never reused by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GateId(pub u32);

/// A rolling window a gate compares, `[now − width, now)` (spec §6 Aggregates).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct InputWindow {
    /// Window width.
    pub width: Duration,
    /// Aggregate.
    pub aggregate: Aggregate,
    /// Least coverage in thousandths of `width` below which the input has no value (default 500).
    pub min_coverage: u16,
    /// Tick interval on the commit clock (default 1 s).
    pub cadence: Duration,
}

/// What a gate compares: a series' live value or a window over it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Input {
    /// The series; `None` under [`Gate::each`].
    pub series: Option<SeriesKey>,
    /// A window; `None` for the live value.
    pub window: Option<InputWindow>,
}

impl Input {
    /// The live value of one series, by id or by name (resolved when the watch commits).
    pub fn value(series: impl Into<SeriesKey>) -> Self {
        Input {
            series: Some(series.into()),
            window: None,
        }
    }

    /// The live value of each series of [`Gate::each`].
    pub fn value_each() -> Self {
        Input {
            series: None,
            window: None,
        }
    }

    /// A rolling window over one series.
    pub fn window(series: impl Into<SeriesKey>, width: Duration, aggregate: Aggregate) -> Self {
        Input {
            series: Some(series.into()),
            window: Some(InputWindow {
                width,
                aggregate,
                min_coverage: 500,
                cadence: Duration::from_secs(1),
            }),
        }
    }

    /// A rolling window over each series of [`Gate::each`].
    pub fn window_each(width: Duration, aggregate: Aggregate) -> Self {
        Input {
            series: None,
            ..Input::window(SeriesId(0), width, aggregate)
        }
    }

    /// The window's least coverage, in thousandths; no effect on a value input.
    pub fn min_coverage(mut self, thousandths: u16) -> Self {
        if let Some(w) = &mut self.window {
            w.min_coverage = thousandths;
        }
        self
    }

    /// The window's tick interval; no effect on a value input.
    pub fn cadence(mut self, cadence: Duration) -> Self {
        if let Some(w) = &mut self.window {
            w.cadence = cadence;
        }
        self
    }
}

/// One boundary of a levels gate.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Level {
    /// Crossed upwards once the value is above this.
    pub at: f64,
    /// Crossed back once the value is below this (`clear ≤ at`).
    pub clear: f64,
    /// How long the value stays above `at` before the upward crossing.
    pub hold_up: Duration,
    /// How long the value stays below `clear` before the downward crossing.
    pub hold_down: Duration,
}

impl Level {
    /// A boundary at `at`, with no hysteresis and no holds.
    pub fn above(at: f64) -> Self {
        Level {
            at,
            clear: at,
            hold_up: Duration::ZERO,
            hold_down: Duration::ZERO,
        }
    }

    /// The clear level.
    pub fn clear(self, clear: f64) -> Self {
        Level { clear, ..self }
    }

    /// The hold before the upward crossing.
    pub fn hold_up(self, hold_up: Duration) -> Self {
        Level { hold_up, ..self }
    }

    /// The hold before the downward crossing.
    pub fn hold_down(self, hold_down: Duration) -> Self {
        Level { hold_down, ..self }
    }
}

/// How a gate turns its input into a state.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GateKind {
    /// Rising boundaries; the state is how many are crossed (n + 1 states).
    Levels(Vec<Level>),
    /// Whether the value is one of a set, with holds each way.
    Set {
        /// Members, compared as the duplicate rule compares values (spec §2.4).
        members: Vec<Value>,
        /// How long the value stays in the set before the state becomes `Match(true)`.
        hold_on: Duration,
        /// How long the value stays out of the set before the state becomes `Match(false)`.
        hold_off: Duration,
    },
    /// The value itself, moving when it leaves the dead-band around the last state.
    Change {
        /// Dead-band.
        deadband: f64,
    },
}

/// A gate: one input of one stream (or of each stream a pattern selects), a kind and a staleness limit.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Gate {
    /// Input.
    pub input: Input,
    /// Kind.
    pub kind: GateKind,
    /// Silence after a series' last arrival that makes the gate `Stale`; `None` never.
    pub stale_after: Option<Duration>,
    /// A name pattern: one instance per series it selects (spec §4 Funnel grammar).
    pub each: Option<String>,
}

impl Gate {
    fn of(input: Input, kind: GateKind) -> Self {
        Gate {
            input,
            kind,
            stale_after: None,
            each: None,
        }
    }

    /// A levels gate: 1 to 16 rising boundaries.
    pub fn levels(input: Input, levels: impl IntoIterator<Item = Level>) -> Self {
        Gate::of(input, GateKind::Levels(levels.into_iter().collect()))
    }

    /// A set gate: 1 to 64 values, none `Absent`.
    pub fn set(input: Input, members: impl IntoIterator<Item = Value>) -> Self {
        Gate::of(
            input,
            GateKind::Set {
                members: members.into_iter().collect(),
                hold_on: Duration::ZERO,
                hold_off: Duration::ZERO,
            },
        )
    }

    /// A change gate, with a dead-band of 0.
    pub fn change(input: Input) -> Self {
        Gate::of(input, GateKind::Change { deadband: 0.0 })
    }

    /// The dead-band of a change gate; no effect on another kind.
    pub fn deadband(mut self, x: f64) -> Self {
        if let GateKind::Change { deadband } = &mut self.kind {
            *deadband = x;
        }
        self
    }

    /// The hold into a set gate's set; no effect on another kind.
    pub fn hold_on(mut self, d: Duration) -> Self {
        if let GateKind::Set { hold_on, .. } = &mut self.kind {
            *hold_on = d;
        }
        self
    }

    /// The hold out of a set gate's set; no effect on another kind.
    pub fn hold_off(mut self, d: Duration) -> Self {
        if let GateKind::Set { hold_off, .. } = &mut self.kind {
            *hold_off = d;
        }
        self
    }

    /// The staleness limit.
    pub fn stale_after(self, d: Duration) -> Self {
        Gate {
            stale_after: Some(d),
            ..self
        }
    }

    /// One instance per series the pattern selects; the input names no series.
    pub fn each(self, pattern: impl Into<String>) -> Self {
        Gate {
            each: Some(pattern.into()),
            ..self
        }
    }

    /// Checks the gate's parameters (`docs/api.md` Gates, Kinds and Inputs).
    pub(crate) fn check(&self) -> Result<(), GateError> {
        match (&self.each, &self.input.series) {
            (Some(p), None) => crate::pattern::check(p).map_err(GateError::Pattern)?,
            (None, Some(_)) => {}
            _ => return Err(GateError::Selector),
        }
        let span = |d: Duration| !d.is_zero() && d <= MAX_GATE_DURATION;
        let hold = |d: Duration| d <= MAX_GATE_DURATION;
        if self.stale_after.is_some_and(|d| !span(d)) {
            return Err(GateError::Duration);
        }
        if let Some(w) = &self.input.window {
            if !span(w.width) || !span(w.cadence) {
                return Err(GateError::Duration);
            }
            if w.min_coverage > 1_000 {
                return Err(GateError::Coverage);
            }
        }
        match &self.kind {
            GateKind::Levels(ls) => {
                let finite = ls.iter().all(|l| l.at.is_finite() && l.clear.is_finite());
                let each = ls.iter().all(|l| l.clear <= l.at);
                let rising = ls
                    .windows(2)
                    .all(|w| w[0].at < w[1].at && w[0].clear <= w[1].clear);
                if ls.is_empty() || ls.len() > MAX_GATE_LEVELS || !finite || !each || !rising {
                    return Err(GateError::Levels);
                }
                if ls.iter().any(|l| !hold(l.hold_up) || !hold(l.hold_down)) {
                    return Err(GateError::Duration);
                }
            }
            GateKind::Set {
                members,
                hold_on,
                hold_off,
            } => {
                let bad = |v: &Value| match v {
                    Value::Absent => true,
                    Value::F64(x) => !x.is_finite(),
                    Value::Decimal { scale, .. } => *scale > kvik_log::facts::MAX_SCALE,
                    Value::Text(t) => t.is_empty() || t.len() > 255,
                    _ => false,
                };
                if members.is_empty() || members.len() > MAX_GATE_MEMBERS || members.iter().any(bad)
                {
                    return Err(GateError::Set);
                }
                if !hold(*hold_on) || !hold(*hold_off) {
                    return Err(GateError::Duration);
                }
            }
            GateKind::Change { deadband } => {
                if !deadband.is_finite() || *deadband < 0.0 {
                    return Err(GateError::Deadband);
                }
            }
        }
        Ok(())
    }
}

/// Checks a gate name: 1 to 64 bytes, one segment of the series-name grammar (no `/`).
pub(crate) fn check_name(name: &str) -> Result<(), GateError> {
    if name.is_empty() || name.len() > MAX_GATE_NAME || name.contains('/') {
        return Err(GateError::Name);
    }
    Ok(())
}

/// A gate refused (`docs/api.md` Gates, Limits and errors).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GateError {
    /// Levels: none, more than 16, not finite, not rising, or a clear level above its boundary.
    #[error("levels must be 1 to 16 finite boundaries, rising, each clear at or below its level")]
    Levels,
    /// Set: empty, more than 64 values, or holding `Absent`, a non-finite number, a decimal of scale above 18 or text
    /// not of 1 to 255 bytes.
    #[error("a set holds 1 to 64 values, none absent")]
    Set,
    /// A dead-band negative or not finite.
    #[error("dead-band must be finite and not negative")]
    Deadband,
    /// A duration zero where one is needed, or beyond ten years.
    #[error("durations must be positive (holds: may be zero) and at most ten years")]
    Duration,
    /// A window's least coverage above 1,000 thousandths.
    #[error("min_coverage is in thousandths, at most 1000")]
    Coverage,
    /// A series and `each` together, or neither.
    #[error("a gate reads one named series or, under each, the series of a pattern")]
    Selector,
    /// The pattern of `each` is refused (spec §4 Funnel).
    #[error("pattern refused: {0}")]
    Pattern(RouteError),
    /// A name not of 1 to 64 bytes or holding `/`.
    #[error("a gate name is 1 to 64 bytes without '/'")]
    Name,
    /// A different gate is in force under the name.
    #[error("another gate is in force under this name")]
    NameInUse,
    /// No gate is in force under the name.
    #[error("no gate is in force under this name")]
    NoSuchGate,
    /// More than 4,096 gates on the consumer or 65,536 in the store.
    #[error("too many gates")]
    TooMany,
}

/// A gate's state.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum GateState {
    /// No value to judge: no live-state fact yet, or a window below its coverage.
    Unknown,
    /// No arrival for `stale_after`.
    Stale,
    /// Levels: how many boundaries are crossed, 0 to n.
    Level(u8),
    /// Set: whether the value is in the set.
    Match(bool),
    /// Change: the value delivered last.
    Value(Value),
}

/// A transition's identity, stable across restarts (spec §6 Identity). Ordered as delivered: by `at`, then
/// entry-caused before timer-caused, then `due`, gate and series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransitionId {
    /// The cause entry's position; for a timer, the last position before `due`.
    pub at: Position,
    /// A timer's instant on the commit clock; `None` for a transition caused by an entry.
    pub due: Option<Timestamp>,
    /// The gate.
    pub condition: GateId,
    /// The instance's series.
    pub series: SeriesId,
}

/// Which deadline a timer transition met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimerKind {
    /// A hold completed.
    Hold,
    /// The series fell stale.
    Stale,
    /// A future fact took effect.
    Mature,
    /// A window tick.
    Window,
}

/// What caused a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransitionCause {
    /// A fact of the series.
    Fact {
        /// Its position.
        position: Position,
        /// Its event time.
        t: Timestamp,
    },
    /// A timer on the commit clock.
    Timer {
        /// Its instant.
        due: Timestamp,
        /// Which deadline.
        kind: TimerKind,
    },
    /// A watch, or a definition that makes a series join or leave an `each` gate.
    Entry {
        /// Its position.
        position: Position,
    },
}

/// One change of a gate instance's state.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Transition {
    /// Identity: de-duplicate by it.
    pub id: TransitionId,
    /// The gate's name.
    pub name: String,
    /// The instance's series.
    pub series: SeriesId,
    /// State before; `None` when the instance starts.
    pub from: Option<GateState>,
    /// State after; `None` when the instance leaves.
    pub to: Option<GateState>,
    /// Cause.
    pub cause: TransitionCause,
    /// The input's value at evaluation.
    pub value: Option<Value>,
    /// The event time of the live-state fact the value came from.
    pub event_time: Option<Timestamp>,
    /// The commit-clock instant of the evaluation.
    pub now: Timestamp,
}

/// One gate instance's state in a resync.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GateInstanceState {
    /// The gate.
    pub condition: GateId,
    /// The instance's series.
    pub series: SeriesId,
    /// State now.
    pub state: GateState,
    /// The transition that set it; `None` after a reseed (Q-1392).
    pub set_by: Option<TransitionId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: SeriesId = SeriesId(1);

    fn levels(ls: Vec<Level>) -> Result<(), GateError> {
        Gate::levels(Input::value(S), ls).check()
    }

    fn set(ms: Vec<Value>) -> Result<(), GateError> {
        Gate::set(Input::value(S), ms).check()
    }

    #[test]
    fn levels_are_one_to_sixteen_finite_rising_boundaries_each_clear_at_or_below() {
        let n = |k: usize| (0..k).map(|i| Level::above(i as f64)).collect::<Vec<_>>();
        assert_eq!(levels(n(1)), Ok(()));
        assert_eq!(levels(n(16)), Ok(()));
        assert_eq!(levels(n(0)), Err(GateError::Levels));
        assert_eq!(levels(n(17)), Err(GateError::Levels));
        assert_eq!(levels(vec![Level::above(f64::NAN)]), Err(GateError::Levels));
        assert_eq!(
            levels(vec![Level::above(1.0).clear(f64::NEG_INFINITY)]),
            Err(GateError::Levels)
        );
        assert_eq!(
            levels(vec![Level::above(1.0).clear(1.5)]),
            Err(GateError::Levels)
        );
        assert_eq!(levels(vec![Level::above(1.0).clear(1.0)]), Ok(()));
        assert_eq!(
            levels(vec![Level::above(2.0), Level::above(2.0)]),
            Err(GateError::Levels)
        );
        assert_eq!(
            levels(vec![
                Level::above(2.0).clear(1.5),
                Level::above(3.0).clear(1.0)
            ]),
            Err(GateError::Levels)
        );
        assert_eq!(
            levels(vec![
                Level::above(2.0).clear(1.5),
                Level::above(3.0).clear(1.5)
            ]),
            Ok(())
        );
    }

    #[test]
    fn a_set_holds_one_to_sixty_four_values_none_absent() {
        let n = |k: usize| (0..k).map(|i| Value::F64(i as f64)).collect::<Vec<_>>();
        assert_eq!(set(n(1)), Ok(()));
        assert_eq!(set(n(64)), Ok(()));
        assert_eq!(set(n(0)), Err(GateError::Set));
        assert_eq!(set(n(65)), Err(GateError::Set));
        assert_eq!(set(vec![Value::Absent]), Err(GateError::Set));
        assert_eq!(set(vec![Value::F64(f64::NAN)]), Err(GateError::Set));
        assert_eq!(set(vec![Value::Text(String::new())]), Err(GateError::Set));
        let dec = |scale| vec![Value::Decimal { units: 1, scale }];
        assert_eq!(set(dec(18)), Ok(()));
        assert_eq!(set(dec(19)), Err(GateError::Set));
        assert_eq!(set(vec![Value::Text("a".repeat(256))]), Err(GateError::Set));
        assert_eq!(set(vec![Value::Text("a".repeat(255))]), Ok(()));
        assert_eq!(
            set(vec![
                Value::State(3),
                Value::Bool(true),
                Value::Decimal { units: 1, scale: 2 }
            ]),
            Ok(())
        );
    }

    #[test]
    fn a_deadband_is_finite_and_not_negative() {
        let c = |x: f64| Gate::change(Input::value(S)).deadband(x).check();
        assert_eq!(c(0.0), Ok(()));
        assert_eq!(c(-0.001), Err(GateError::Deadband));
        assert_eq!(c(f64::NAN), Err(GateError::Deadband));
        assert_eq!(c(f64::INFINITY), Err(GateError::Deadband));
    }

    #[test]
    fn durations_are_at_most_ten_years_and_spans_not_zero() {
        let ten = MAX_GATE_DURATION;
        let over = ten + Duration::from_nanos(1);
        let base = || Gate::change(Input::value(S));
        assert_eq!(base().stale_after(ten).check(), Ok(()));
        assert_eq!(base().stale_after(over).check(), Err(GateError::Duration));
        assert_eq!(
            base().stale_after(Duration::ZERO).check(),
            Err(GateError::Duration)
        );
        let hold = |up: Duration, down: Duration| {
            levels(vec![Level::above(1.0).hold_up(up).hold_down(down)])
        };
        assert_eq!(hold(Duration::ZERO, ten), Ok(()));
        assert_eq!(hold(over, Duration::ZERO), Err(GateError::Duration));
        assert_eq!(hold(Duration::ZERO, over), Err(GateError::Duration));
        let s = |on: Duration, off: Duration| {
            Gate::set(Input::value(S), [Value::Bool(true)])
                .hold_on(on)
                .hold_off(off)
                .check()
        };
        assert_eq!(s(ten, Duration::ZERO), Ok(()));
        assert_eq!(s(over, Duration::ZERO), Err(GateError::Duration));
        assert_eq!(s(Duration::ZERO, over), Err(GateError::Duration));
        let w = |width: Duration, cadence: Duration| {
            Gate::change(Input::window(S, width, Aggregate::Avg).cadence(cadence)).check()
        };
        assert_eq!(w(ten, ten), Ok(()));
        assert_eq!(w(Duration::ZERO, ten), Err(GateError::Duration));
        assert_eq!(w(ten, Duration::ZERO), Err(GateError::Duration));
        assert_eq!(w(over, ten), Err(GateError::Duration));
    }

    #[test]
    fn coverage_is_at_most_a_thousand_thousandths() {
        let c = |t: u16| {
            Gate::change(Input::window(S, Duration::from_secs(60), Aggregate::Avg).min_coverage(t))
                .check()
        };
        assert_eq!(c(1_000), Ok(()));
        assert_eq!(c(1_001), Err(GateError::Coverage));
    }

    #[test]
    fn a_gate_reads_one_series_or_each_series_of_a_valid_pattern() {
        let each = Gate::change(Input::value_each()).each("plant/*/code");
        assert_eq!(each.check(), Ok(()));
        assert_eq!(
            Gate::change(Input::value(S)).each("plant/*/code").check(),
            Err(GateError::Selector)
        );
        assert_eq!(
            Gate::change(Input::value_each()).check(),
            Err(GateError::Selector)
        );
        assert_eq!(
            Gate::change(Input::value_each())
                .each("plant/x*/code")
                .check(),
            Err(GateError::Pattern(RouteError::PartialWildcard))
        );
        let w =
            Gate::change(Input::window_each(Duration::from_secs(60), Aggregate::Max)).each("a/*/b");
        assert_eq!(w.check(), Ok(()));
        assert_eq!(w.input.series, None);
    }

    #[test]
    fn a_gate_name_is_one_to_sixty_four_bytes_without_a_slash() {
        assert_eq!(check_name("humidity"), Ok(()));
        assert_eq!(check_name(&"a".repeat(64)), Ok(()));
        assert_eq!(check_name(""), Err(GateError::Name));
        assert_eq!(check_name(&"a".repeat(65)), Err(GateError::Name));
        assert_eq!(check_name("a/b"), Err(GateError::Name));
    }

    #[test]
    fn transition_ids_order_entry_causes_before_timers_at_a_position() {
        let p = Position {
            generation: 1,
            sequence: 5,
        };
        let id = |due: Option<i64>, g: u32| TransitionId {
            at: p,
            due: due.map(Timestamp),
            condition: GateId(g),
            series: S,
        };
        assert!(id(None, 9) < id(Some(0), 1));
        assert!(id(Some(1), 1) < id(Some(2), 0));
    }
}
