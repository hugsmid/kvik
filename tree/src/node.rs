//! Node contents per series kind (spec §5.2) and the terms a reading or a consecutive pair credits.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::exact::{Exact, exact, exact_ns};

/// The kind of a series (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Instantaneous value.
    Gauge,
    /// Cumulative value that may reset.
    Counter,
    /// Named or boolean state.
    State,
    /// Short text.
    Text,
    /// An amount over a fixed interval `[t, t + d)`.
    Interval,
}

/// A summarised reading's value, as the log holds it (`format.md` A.5): a state is its index among the series'
/// declared names, so a node never needs the registry.
#[derive(Clone, Debug)]
pub enum Value {
    /// A finite `f64` (gauge, counter, interval).
    F64(f64),
    /// A boolean state.
    Bool(bool),
    /// A named state, by index.
    State(u16),
    /// Text, at most 255 bytes.
    Text(String),
}

impl PartialEq for Value {
    /// Numbers compare by bit pattern, as a stored node does.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::State(a), Value::State(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    /// The number of a numeric value.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(v) => Some(*v),
            _ => None,
        }
    }

    /// The time-in-state key of a state value.
    pub fn state_key(&self) -> Option<StateKey> {
        match self {
            Value::Bool(b) => Some(StateKey::Bool(*b)),
            Value::State(i) => Some(StateKey::State(*i)),
            Value::F64(_) | Value::Text(_) => None,
        }
    }
}

/// Key of a state in a time-in-state map; the map's order is the stored order (`format-tree.md` T.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKey {
    /// A boolean state.
    Bool(bool),
    /// A named state, by index.
    State(u16),
}

/// Counter rules (spec §5.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CounterRules {
    /// The value at which the counter wraps to zero, if it does.
    pub wrap_at: Option<f64>,
    /// A decrease to below this fraction of the previous value is a reset; any other is a regression.
    pub reset_ceiling: f64,
}

impl Default for CounterRules {
    fn default() -> Self {
        CounterRules {
            wrap_at: None,
            reset_ceiling: 0.1,
        }
    }
}

/// The series parameters a node depends on (spec §5.2, §5.5), fixed for the readings folded (Q-83).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Series kind.
    pub kind: Kind,
    /// Staleness limit S; `None` holds a reading until the next one.
    pub staleness: Option<i64>,
    /// Counter rules.
    pub counter: CounterRules,
    /// Interval length d of an interval series.
    pub interval: Option<i64>,
}

impl Params {
    /// Parameters of a series that is not an interval series, with default counter rules.
    ///
    /// # Panics
    /// Panics when the staleness limit is not positive.
    pub fn new(kind: Kind, staleness: Option<i64>) -> Self {
        assert!(
            staleness.is_none_or(|s| s > 0),
            "staleness limit must be positive"
        );
        Params {
            kind,
            staleness,
            counter: CounterRules::default(),
            interval: None,
        }
    }

    /// Parameters of an interval series of length `d` (spec §5.2: no hold, no S).
    ///
    /// # Panics
    /// Panics when `d` is not positive.
    pub fn interval(d: i64) -> Self {
        assert!(d > 0, "interval length must be positive");
        Params {
            interval: Some(d),
            ..Params::new(Kind::Interval, None)
        }
    }

    /// End of the validity of a reading at `t`, clipped to `limit` (spec §5.4).
    pub fn valid_until(&self, t: i64, limit: i64) -> i64 {
        self.staleness
            .map_or(limit, |s| limit.min(t.saturating_add(s)))
    }
}

/// Kind-specific node fields (spec §5.2).
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// Gauge fields.
    Gauge {
        /// Sum of reading values.
        sum: Exact,
        /// Smallest reading.
        min: Option<f64>,
        /// Largest reading.
        max: Option<f64>,
        /// Time-weighted integral in value × nanoseconds.
        integral: Exact,
    },
    /// Counter fields.
    Counter {
        /// Sum of steps (spec §5.5).
        increase: Exact,
        /// Decreases counted as resets.
        resets: u64,
        /// Decreases counted as regressions.
        regressions: u64,
        /// Steps credited; in a stored node, `count − 1`.
        steps: u64,
        /// Steps spanning more than S at their earlier reading: a rate including one is flagged (spec §6).
        long_steps: u64,
        /// Nanoseconds between the readings bounding the steps credited: the divisor of rate.
        elapsed: i128,
    },
    /// State fields.
    State {
        /// Nanoseconds spent in each state.
        time: BTreeMap<StateKey, i128>,
        /// Changes between consecutive readings.
        transitions: u64,
    },
    /// Text has no extra fields.
    Text,
    /// Interval fields; coverage is the total length of the intervals credited.
    Interval {
        /// Amount: the sum of reading values, or in a window the amount in it, apportioned at its edges.
        sum: Exact,
        /// Smallest reading.
        min: Option<f64>,
        /// Largest reading.
        max: Option<f64>,
        /// A window edge lies inside an interval (spec §6 `apportioned`); never set in a stored node.
        apportioned: bool,
    },
}

impl Body {
    fn empty(kind: Kind) -> Body {
        match kind {
            Kind::Gauge => Body::Gauge {
                sum: exact(0.0),
                min: None,
                max: None,
                integral: exact(0.0),
            },
            Kind::Counter => Body::Counter {
                increase: exact(0.0),
                resets: 0,
                regressions: 0,
                steps: 0,
                long_steps: 0,
                elapsed: 0,
            },
            Kind::State => Body::State {
                time: BTreeMap::new(),
                transitions: 0,
            },
            Kind::Text => Body::Text,
            Kind::Interval => Body::Interval {
                sum: exact(0.0),
                min: None,
                max: None,
                apportioned: false,
            },
        }
    }

    /// The kind whose node form this is.
    pub fn kind(&self) -> Kind {
        match self {
            Body::Gauge { .. } => Kind::Gauge,
            Body::Counter { .. } => Kind::Counter,
            Body::State { .. } => Kind::State,
            Body::Text => Kind::Text,
            Body::Interval { .. } => Kind::Interval,
        }
    }
}

/// A summary node over `[start, end)` (spec §5.2), or a window evaluation (spec §6), which may also hold edge
/// stretches, a held `last` and an apportioned amount.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Span start, inclusive.
    pub start: i64,
    /// Span end, exclusive.
    pub end: i64,
    /// Readings in the span.
    pub count: u64,
    /// First reading in the span.
    pub first: Option<(i64, Value)>,
    /// Last reading in the span; in a window with none inside, a held value.
    pub last: Option<(i64, Value)>,
    /// Nanoseconds with a valid value.
    pub coverage: i128,
    /// Kind-specific fields.
    pub body: Body,
}

impl Node {
    /// The empty node over `[start, end)`: the identity of [`crate::merge`].
    ///
    /// # Panics
    /// Panics when `start > end`.
    pub fn empty(kind: Kind, start: i64, end: i64) -> Node {
        assert!(start <= end, "span must not be negative");
        Node {
            start,
            end,
            count: 0,
            first: None,
            last: None,
            coverage: 0,
            body: Body::empty(kind),
        }
    }

    /// Whether the node holds no readings.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the node is the identity of merge: no readings and, for a window part of an interval series, no
    /// apportioned amount either.
    pub(crate) fn is_identity(&self) -> bool {
        self.count == 0 && self.coverage == 0
    }

    /// Whether a window result from this node is flagged `apportioned` (spec §6).
    pub fn apportioned(&self) -> bool {
        matches!(self.body, Body::Interval { apportioned, .. } if apportioned)
    }

    /// Credits `ns` nanoseconds of value `v`: coverage, integral, state time.
    pub(crate) fn hold(&mut self, v: &Value, ns: i128) {
        if ns <= 0 {
            return;
        }
        self.coverage += ns;
        match &mut self.body {
            Body::Gauge { integral, .. } => {
                *integral += exact(v.as_f64().expect("gauge value")) * exact_ns(ns);
            }
            Body::State { time, .. } => {
                *time.entry(v.state_key().expect("state value")).or_insert(0) += ns;
            }
            Body::Counter { .. } | Body::Text | Body::Interval { .. } => {}
        }
    }

    /// Credits the event terms of a pair: counter step (spec §5.5), state transition.
    pub(crate) fn step(&mut self, p: &Params, prev: &(i64, Value), cur: &(i64, Value)) {
        match &mut self.body {
            Body::Counter {
                increase,
                resets,
                regressions,
                steps,
                long_steps,
                elapsed,
            } => {
                let span = i128::from(cur.0) - i128::from(prev.0);
                *steps += 1;
                *elapsed += span;
                if p.staleness.is_some_and(|s| span > i128::from(s)) {
                    *long_steps += 1;
                }
                let a = prev.1.as_f64().expect("counter value");
                let b = cur.1.as_f64().expect("counter value");
                if b >= a {
                    *increase += exact(b) - exact(a);
                } else if let Some(w) = p.counter.wrap_at {
                    *increase += exact(w) - exact(a) + exact(b);
                } else if b < p.counter.reset_ceiling * a {
                    *resets += 1;
                    *increase += exact(b);
                } else {
                    *regressions += 1;
                }
            }
            Body::State { transitions, .. } => {
                if prev.1.state_key() != cur.1.state_key() {
                    *transitions += 1;
                }
            }
            Body::Gauge { .. } | Body::Text | Body::Interval { .. } => {}
        }
    }

    /// All terms of a consecutive pair; none for an interval series (spec §5.2).
    pub(crate) fn pair(&mut self, p: &Params, a: &(i64, Value), b: &(i64, Value)) {
        if p.interval.is_some() {
            return;
        }
        let to = p.valid_until(a.0, b.0);
        self.hold(&a.1, i128::from(to) - i128::from(a.0));
        self.step(p, a, b);
    }

    /// Credits count, first, last, min and max of a reading inside the span.
    pub(crate) fn note(&mut self, t: i64, v: &Value) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some((t, v.clone()));
        }
        self.last = Some((t, v.clone()));
        if let Body::Gauge { min, max, .. } | Body::Interval { min, max, .. } = &mut self.body {
            let x = v.as_f64().expect("numeric value");
            *min = Some(min.map_or(x, |m| lower(m, x)));
            *max = Some(max.map_or(x, |m| higher(m, x)));
        }
    }

    /// Credits every per-reading field of a reading inside the span: [`Node::note`] and the sum.
    pub(crate) fn observe(&mut self, t: i64, v: &Value) {
        self.note(t, v);
        if let Body::Gauge { sum, .. } | Body::Interval { sum, .. } = &mut self.body {
            *sum += exact(v.as_f64().expect("numeric value"));
        }
    }
}

/// The smaller of two readings in IEEE total order, so −0.0 is below +0.0 whatever the order of folding (Q-181).
pub(crate) fn lower(a: f64, b: f64) -> f64 {
    if b.total_cmp(&a) == Ordering::Less {
        b
    } else {
        a
    }
}

/// The larger of two readings in IEEE total order (Q-181).
pub(crate) fn higher(a: f64, b: f64) -> f64 {
    if b.total_cmp(&a) == Ordering::Greater {
        b
    } else {
        a
    }
}
