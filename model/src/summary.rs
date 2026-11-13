//! Summary nodes (spec §5.2), their direct computation, and `merge` (§5.3).
//!
//! A node over `[a, b)` is either **empty** (the identity of `merge`) or
//! summarises the readings r₁ … rₙ inside its span (in `t` order): per-reading
//! fields (count, first, last, sum, min, max) and per-pair fields over each
//! consecutive pair (rᵢ, rᵢ₊₁) inside the span. Nothing is held past a node's
//! last reading. `merge(L, R)` adds the one boundary pair (L.last, R.first), so
//! the pairs of a concatenation are the same whatever the bracketing.

use std::collections::BTreeMap;

use crate::types::{Exact, Kind, Nanos, SECOND, StateKey, Value, exact, exact_ns};

/// Counter rules (§5.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CounterRules {
    /// The value at which the counter wraps to zero, if it does.
    pub wrap_at: Option<f64>,
    /// A decrease to below this fraction of the previous value is a reset;
    /// any other decrease is a regression (counted, contributes 0).
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

/// Per-series parameters that summaries depend on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Series kind.
    pub kind: Kind,
    /// Staleness limit S (§2.1, §5.4); `None` means a reading is valid until
    /// the next one.
    pub staleness: Option<Nanos>,
    /// Counter rules.
    pub counter: CounterRules,
}

impl Params {
    /// Parameters with a staleness limit and default counter rules.
    pub fn new(kind: Kind, staleness: impl Into<Option<Nanos>>) -> Self {
        let staleness = staleness.into();
        assert!(
            staleness.is_none_or(|s| s > 0),
            "staleness limit must be positive"
        );
        Params {
            kind,
            staleness,
            counter: CounterRules::default(),
        }
    }

    /// End of the validity of a reading at `t`, clipped to `limit`.
    pub fn valid_until(&self, t: Nanos, limit: Nanos) -> Nanos {
        self.staleness
            .map_or(limit, |s| limit.min(t.saturating_add(s)))
    }
}

/// Kind-specific node fields.
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
        /// Sum of steps (§5.5).
        increase: Exact,
        /// Decreases counted as resets.
        resets: u64,
        /// Decreases counted as regressions (contribute 0).
        regressions: u64,
    },
    /// State fields.
    State {
        /// Nanoseconds spent in each state.
        time: BTreeMap<StateKey, i128>,
        /// Number of changes between consecutive readings.
        transitions: u64,
    },
    /// Text has no extra fields.
    Text,
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
            },
            Kind::State => Body::State {
                time: BTreeMap::new(),
                transitions: 0,
            },
            Kind::Text => Body::Text,
        }
    }
}

/// A summary node over `[start, end)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Span start (inclusive).
    pub start: Nanos,
    /// Span end (exclusive).
    pub end: Nanos,
    /// Number of readings in the span.
    pub count: u64,
    /// First reading in the span.
    pub first: Option<(Nanos, Value)>,
    /// Last reading in the span (for a window, possibly a held value; §6).
    pub last: Option<(Nanos, Value)>,
    /// Nanoseconds with a valid value.
    pub coverage: i128,
    /// Kind-specific fields.
    pub body: Body,
}

impl Node {
    /// An empty node.
    pub fn empty(kind: Kind, start: Nanos, end: Nanos) -> Node {
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

    /// Credits `ns` nanoseconds of value `v` (coverage, integral, state time).
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
                *time.entry(v.state_key()).or_insert(0) += ns;
            }
            Body::Counter { .. } | Body::Text => {}
        }
    }

    /// Credits the event terms of a pair: counter step, state transition.
    pub(crate) fn step(&mut self, p: &Params, prev: &Value, cur: &Value) {
        match &mut self.body {
            Body::Counter {
                increase,
                resets,
                regressions,
            } => {
                let a = prev.as_f64().expect("counter value");
                let b = cur.as_f64().expect("counter value");
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
                if prev.state_key() != cur.state_key() {
                    *transitions += 1;
                }
            }
            Body::Gauge { .. } | Body::Text => {}
        }
    }

    /// All terms of a consecutive pair (rᵢ, rᵢ₊₁).
    fn pair(&mut self, p: &Params, a: &(Nanos, Value), b: &(Nanos, Value)) {
        let to = p.valid_until(a.0, b.0);
        self.hold(&a.1, i128::from(to) - i128::from(a.0));
        self.step(p, &a.1, &b.1);
    }

    /// Credits the per-reading fields of a reading inside the span.
    pub(crate) fn observe(&mut self, t: Nanos, v: &Value) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some((t, v.clone()));
        }
        self.last = Some((t, v.clone()));
        if let Body::Gauge { sum, min, max, .. } = &mut self.body {
            let x = v.as_f64().expect("gauge value");
            *sum += exact(x);
            *min = Some(min.map_or(x, |m| m.min(x)));
            *max = Some(max.map_or(x, |m| m.max(x)));
        }
    }
}

/// Direct computation of a node: the left-to-right fold over its readings
/// (`start ≤ t < end`, strictly increasing `t`). This is the canonical
/// computation for a 1-minute node (§5.6).
pub fn direct(p: &Params, start: Nanos, end: Nanos, readings: &[(Nanos, Value)]) -> Node {
    let mut node = Node::empty(p.kind, start, end);
    for (i, r) in readings.iter().enumerate() {
        debug_assert!(start <= r.0 && r.0 < end, "reading outside span");
        node.observe(r.0, &r.1);
        if i > 0 {
            node.pair(p, &readings[i - 1], r);
        }
    }
    node
}

/// Merges two adjacent nodes (`l.end == r.start`). The empty node is the
/// identity; otherwise fields combine and the boundary pair contributes once.
///
/// # Panics
/// Panics if the nodes are not adjacent or are of different kinds.
pub fn merge(p: &Params, l: &Node, r: &Node) -> Node {
    assert_eq!(l.end, r.start, "merge needs adjacent nodes");
    if l.is_empty() {
        return Node {
            start: l.start,
            ..r.clone()
        };
    }
    if r.is_empty() {
        return Node {
            end: r.end,
            ..l.clone()
        };
    }
    let body = match (&l.body, &r.body) {
        (
            Body::Gauge {
                sum: l_sum,
                min: l_min,
                max: l_max,
                integral: l_int,
            },
            Body::Gauge {
                sum: r_sum,
                min: r_min,
                max: r_max,
                integral: r_int,
            },
        ) => Body::Gauge {
            sum: l_sum + r_sum,
            min: opt(*l_min, *r_min, f64::min),
            max: opt(*l_max, *r_max, f64::max),
            integral: l_int + r_int,
        },
        (
            Body::Counter {
                increase: l_inc,
                resets: l_resets,
                regressions: l_reg,
            },
            Body::Counter {
                increase: r_inc,
                resets: r_resets,
                regressions: r_reg,
            },
        ) => Body::Counter {
            increase: l_inc + r_inc,
            resets: l_resets + r_resets,
            regressions: l_reg + r_reg,
        },
        (
            Body::State {
                time: l_time,
                transitions: l_tr,
            },
            Body::State {
                time: r_time,
                transitions: r_tr,
            },
        ) => {
            let mut time = l_time.clone();
            for (k, v) in r_time {
                *time.entry(k.clone()).or_insert(0) += v;
            }
            Body::State {
                time,
                transitions: l_tr + r_tr,
            }
        }
        (Body::Text, Body::Text) => Body::Text,
        _ => panic!("merge of nodes of different kinds"),
    };
    let mut out = Node {
        start: l.start,
        end: r.end,
        count: l.count + r.count,
        first: l.first.clone(),
        last: r.last.clone(),
        coverage: l.coverage + r.coverage,
        body,
    };
    let a = l.last.as_ref().expect("non-empty");
    let b = r.first.as_ref().expect("non-empty");
    out.pair(p, a, b);
    out
}

/// Merges adjacent nodes left to right; an empty sequence gives an empty node
/// over `[start, start)`.
pub fn merge_all<'a, I>(p: &Params, start: Nanos, nodes: I) -> Node
where
    I: IntoIterator<Item = &'a Node>,
{
    let mut acc: Option<Node> = None;
    for n in nodes {
        acc = Some(match acc {
            None => n.clone(),
            Some(a) => merge(p, &a, n),
        });
    }
    acc.unwrap_or_else(|| Node::empty(p.kind, start, start))
}

/// Adds the window edge stretches (§6) to `core`, the merge of the nodes
/// covering `[core.start, until)`:
/// - leading: from the start to the first reading inside, credited with
///   `prior` (the last reading before the start) while it is valid; with a
///   reading inside, the counter step and state transition from `prior` to it
///   are credited to this window;
/// - trailing: from the last reading inside to `until` (the window end,
///   already clipped to now), while it is valid.
///
/// With no reading inside but a valid held value, `last` is that value.
pub fn with_stretches(
    p: &Params,
    mut core: Node,
    prior: Option<&(Nanos, Value)>,
    until: Nanos,
) -> Node {
    let start = core.start;
    if let Some(pr) = prior {
        let lead_to = core.first.as_ref().map_or(until, |(t, _)| *t);
        let to = p.valid_until(pr.0, lead_to);
        core.hold(&pr.1, i128::from(to) - i128::from(start));
        if let Some((_, fv)) = core.first.clone() {
            core.step(p, &pr.1, &fv);
        } else if p.valid_until(pr.0, Nanos::MAX) > start {
            core.last = Some(pr.clone());
        }
    }
    if core.count > 0
        && let Some(last) = core.last.clone()
    {
        let to = p.valid_until(last.0, until);
        core.hold(&last.1, i128::from(to) - i128::from(last.0));
    }
    core
}

fn opt(a: Option<f64>, b: Option<f64>, f: fn(f64, f64) -> f64) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(f(x, y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Windowed aggregate (§6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Aggregate {
    /// Gauge: integral ÷ covered seconds.
    Avg,
    /// Sum of reading values.
    Sum,
    /// Integral in value × seconds.
    Integral,
    /// Smallest reading.
    Min,
    /// Largest reading.
    Max,
    /// Number of readings.
    Count,
    /// Counter increase.
    Increase,
    /// Counter increase ÷ covered seconds (OPEN-QUESTIONS Q-05).
    Rate,
    /// Last value (a held value if there is no reading inside).
    Last,
    /// Seconds spent in the given state.
    TimeInState(StateKey),
}

/// Result of an aggregate.
#[derive(Clone, Debug, PartialEq)]
pub enum AggValue {
    /// An exact number.
    Exact(Exact),
    /// A reading value.
    Value(Value),
}

impl AggValue {
    /// Approximate float for display and thresholds.
    pub fn to_f64(&self) -> Option<f64> {
        use num_traits::ToPrimitive;
        match self {
            AggValue::Exact(x) => x.to_f64(),
            AggValue::Value(v) => v.as_f64(),
        }
    }
}

impl Node {
    /// Evaluates an aggregate. `None` when empty or not applicable to the kind.
    pub fn aggregate(&self, agg: &Aggregate) -> Option<AggValue> {
        let ns_per_s = exact_ns(i128::from(SECOND));
        let covered = (self.coverage > 0).then(|| exact_ns(self.coverage));
        match (agg, &self.body) {
            (Aggregate::Avg, Body::Gauge { integral, .. }) => {
                covered.map(|c| AggValue::Exact(integral / c))
            }
            (Aggregate::Sum, Body::Gauge { sum, .. }) => {
                (self.count > 0).then(|| AggValue::Exact(sum.clone()))
            }
            (Aggregate::Integral, Body::Gauge { integral, .. }) => {
                Some(AggValue::Exact(integral / ns_per_s))
            }
            (Aggregate::Min, Body::Gauge { min, .. }) => min.map(|m| AggValue::Exact(exact(m))),
            (Aggregate::Max, Body::Gauge { max, .. }) => max.map(|m| AggValue::Exact(exact(m))),
            (Aggregate::Count, _) => Some(AggValue::Exact(exact_ns(i128::from(self.count)))),
            (Aggregate::Increase, Body::Counter { increase, .. }) => {
                Some(AggValue::Exact(increase.clone()))
            }
            (Aggregate::Rate, Body::Counter { increase, .. }) => {
                covered.map(|c| AggValue::Exact(increase * ns_per_s / c))
            }
            (Aggregate::Last, _) => self.last.as_ref().map(|(_, v)| AggValue::Value(v.clone())),
            (Aggregate::TimeInState(k), Body::State { time, .. }) => Some(AggValue::Exact(
                exact_ns(time.get(k).copied().unwrap_or(0)) / ns_per_s,
            )),
            _ => None,
        }
    }
}
