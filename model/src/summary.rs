//! Summary nodes (spec §5.2), their direct computation, and `merge` (§5.3).
//!
//! A node over `[a, b)` is either **empty** (the identity of `merge`) or
//! summarises the readings r₁ … rₙ inside its span (in `t` order): per-reading
//! fields (count, first, last, sum, min, max) and per-pair fields over each
//! consecutive pair (rᵢ, rᵢ₊₁) inside the span. Nothing is held past a node's
//! last reading. `merge(L, R)` adds the one boundary pair (L.last, R.first), so
//! the pairs of a concatenation are the same whatever the bracketing.

use std::collections::BTreeMap;

use crate::registry::Timing;
use crate::types::{
    Exact, IntervalSpec, Kind, MINUTE, Nanos, QUARTER, SECOND, StateKey, Value, exact, exact_ns,
};

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
    /// Expected interval (§2.1); it decides which levels are stored
    /// ([`Params::stores`]).
    pub expected_interval: Option<Nanos>,
    /// Timing (§2.1); it decides the credit limit ([`Params::credit_limit`]).
    pub timing: Timing,
    /// Length and phase of an interval series (§2.1).
    pub interval: Option<IntervalSpec>,
}

/// The longest expected interval for which each stored level is kept
/// (§5.1 density rule): 5 s for the 1-minute level, 60 s for the 15-minute
/// level.
pub const DENSITY: [Nanos; 2] = [5 * SECOND, 60 * SECOND];

impl Params {
    /// Parameters with a staleness limit, default counter rules and the
    /// kind's default expected interval (§2.1: 60 s for gauges and counters,
    /// none for state and text).
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
            expected_interval: crate::registry::default_expected_interval(kind, None),
            timing: Timing::Observed,
            interval: None,
        }
    }

    /// Parameters of an observed interval series of length `length` and
    /// phase `phase`, whose expected interval is its length and S 3 × that
    /// (§2.1); S plays no part in its summaries (§5.2).
    pub fn interval(length: Nanos, phase: Nanos) -> Self {
        Params {
            interval: Some(IntervalSpec { length, phase }),
            expected_interval: Some(length),
            ..Params::new(Kind::Interval, 3 * length)
        }
    }

    /// When a reading at `t` is ready against `now` (§2.2): `t + d` for a
    /// reading of an observed interval series, otherwise `t`.
    pub fn ready_at(&self, t: Nanos) -> Nanos {
        match (self.timing, self.interval) {
            (Timing::Observed, Some(iv)) => t.saturating_add(iv.length),
            _ => t,
        }
    }

    /// The end of the time a window `[_, end)` evaluated at `now` may credit
    /// (§6 credit limit): `now` for an observed series, the window end for a
    /// scheduled or forecast series, whose values are known in advance.
    /// Readings after it are ignored.
    pub fn credit_limit(&self, end: Nanos, now: Nanos) -> Nanos {
        match self.timing {
            Timing::Observed => end.min(now),
            Timing::Scheduled | Timing::Forecast => end,
        }
    }

    /// Whether a window `[start, _)` at `now` lies wholly after `now` on an
    /// observed series, and so is empty with zero coverage (§6 Windows).
    pub fn wholly_future(&self, start: Nanos, now: Nanos) -> bool {
        self.timing == Timing::Observed && start > now
    }

    /// Whether the stored level `level` (0: 1 minute, 1: 15 minutes) is kept
    /// for the series (§5.1 density rule): only where its nodes summarise many
    /// facts, so for an expected interval of at most [`DENSITY`]`[level]`. A
    /// series with no expected interval stores no level. A level that is not
    /// stored is answered from the facts.
    pub fn stores(&self, level: usize) -> bool {
        // An interval series also needs every node edge of the level to be a
        // multiple of d from the phase, so that no interval crosses one
        // (§5.4): d divides the node length and the phase is 0.
        let width = [MINUTE, QUARTER][level];
        self.expected_interval.is_some_and(|e| e <= DENSITY[level])
            && self
                .interval
                .is_none_or(|iv| iv.phase == 0 && width % iv.length == 0)
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
        /// Decreases counted as regressions (contribute 0, §12 D2).
        regressions: u64,
        /// Steps credited.
        steps: u64,
        /// Steps spanning more than S in force at their earlier reading:
        /// a rate including one is flagged (§6).
        long_steps: u64,
        /// Nanoseconds between the readings of every step credited. Steps are
        /// consecutive, so this is the time between the readings that bound
        /// them: the divisor of rate (§6).
        elapsed: i128,
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
    /// Interval fields (§5.2, §6). Coverage is the total length of the
    /// intervals credited.
    Interval {
        /// Amount: the sum of reading values, or in a window the amount in
        /// it, apportioned at its edges.
        sum: Exact,
        /// Smallest reading.
        min: Option<f64>,
        /// Largest reading.
        max: Option<f64>,
        /// A window edge lies inside an interval, whose amount is credited
        /// pro rata (§6 `apportioned`).
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
            Body::Counter { .. } | Body::Text | Body::Interval { .. } => {}
        }
    }

    /// Credits the event terms of a pair: counter step, state transition.
    pub(crate) fn step(&mut self, p: &Params, prev: &(Nanos, Value), cur: &(Nanos, Value)) {
        match &mut self.body {
            Body::Counter {
                increase,
                resets,
                regressions,
                steps,
                long_steps,
                elapsed,
            } => {
                let span = cur.0 - prev.0;
                *steps += 1;
                *elapsed += i128::from(span);
                if p.staleness.is_some_and(|s| span > s) {
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

    /// All terms of a consecutive pair (rᵢ, rᵢ₊₁); none for an interval
    /// series (§5.2: no hold, no S).
    fn pair(&mut self, p: &Params, a: &(Nanos, Value), b: &(Nanos, Value)) {
        if p.interval.is_some() {
            return;
        }
        let to = p.valid_until(a.0, b.0);
        self.hold(&a.1, i128::from(to) - i128::from(a.0));
        self.step(p, a, b);
    }

    /// Credits the per-reading fields of a reading inside the span.
    pub(crate) fn observe(&mut self, t: Nanos, v: &Value) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some((t, v.clone()));
        }
        self.last = Some((t, v.clone()));
        if let Body::Gauge { sum, min, max, .. } | Body::Interval { sum, min, max, .. } =
            &mut self.body
        {
            let x = v.as_f64().expect("numeric value");
            *sum += exact(x);
            *min = Some(min.map_or(x, |m| m.min(x)));
            *max = Some(max.map_or(x, |m| m.max(x)));
        }
    }

    /// Whether the result is flagged `apportioned` (§6): a window edge lies
    /// inside an interval.
    pub fn apportioned(&self) -> bool {
        matches!(self.body, Body::Interval { apportioned, .. } if apportioned)
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
        if let Some(iv) = p.interval {
            node.coverage += i128::from(iv.length);
        }
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
    // A window part of an interval series can hold no reading and still an
    // apportioned amount from an interval reaching into it, so it is the
    // identity only with no coverage either.
    if l.is_empty() && l.coverage == 0 {
        return Node {
            start: l.start,
            ..r.clone()
        };
    }
    if r.is_empty() && r.coverage == 0 {
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
                steps: l_steps,
                long_steps: l_long,
                elapsed: l_el,
            },
            Body::Counter {
                increase: r_inc,
                resets: r_resets,
                regressions: r_reg,
                steps: r_steps,
                long_steps: r_long,
                elapsed: r_el,
            },
        ) => Body::Counter {
            increase: l_inc + r_inc,
            resets: l_resets + r_resets,
            regressions: l_reg + r_reg,
            steps: l_steps + r_steps,
            long_steps: l_long + r_long,
            elapsed: l_el + r_el,
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
        (
            Body::Interval {
                sum: l_sum,
                min: l_min,
                max: l_max,
                apportioned: l_ap,
            },
            Body::Interval {
                sum: r_sum,
                min: r_min,
                max: r_max,
                apportioned: r_ap,
            },
        ) => Body::Interval {
            sum: l_sum + r_sum,
            min: opt(*l_min, *r_min, f64::min),
            max: opt(*l_max, *r_max, f64::max),
            apportioned: *l_ap || *r_ap,
        },
        _ => panic!("merge of nodes of different kinds"),
    };
    let mut out = Node {
        start: l.start,
        end: r.end,
        count: l.count + r.count,
        first: l.first.clone().or_else(|| r.first.clone()),
        last: r.last.clone().or_else(|| l.last.clone()),
        coverage: l.coverage + r.coverage,
        body,
    };
    if let (Some(a), Some(b)) = (&l.last, &r.first) {
        out.pair(p, a, b);
    }
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
        if let Some(first) = core.first.clone() {
            core.step(p, pr, &first);
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

/// Credits the readings of an interval series to the window `[start, end)`
/// (§6 Aggregates, interval series): each reading's amount pro rata to the
/// part of its interval `[t, t + d)` inside the window, computed exactly
/// (the specification computes it in f64 and compares within the §5.6
/// tolerance); coverage is the length of those parts; a part shorter than d
/// flags the result `apportioned`. Count, first, last, min and max are over
/// readings whose `t` lies in the window. Readings must be ready: the caller
/// leaves out those of an observed series with `t + d > now`.
pub fn credit_intervals<'a, I>(p: &Params, node: &mut Node, readings: I, start: Nanos, end: Nanos)
where
    I: IntoIterator<Item = &'a (Nanos, Value)>,
{
    let d = p.interval.expect("interval series").length;
    for (t, v) in readings {
        let overlap = (t.saturating_add(d)).min(end) - (*t).max(start);
        if overlap <= 0 && !(start <= *t && *t < end) {
            continue;
        }
        let sum_before = match &node.body {
            Body::Interval { sum, .. } => sum.clone(),
            _ => unreachable!("interval node"),
        };
        if start <= *t && *t < end {
            // `observe` adds the whole value; the amount is credited below.
            node.observe(*t, v);
        }
        if let Body::Interval {
            sum, apportioned, ..
        } = &mut node.body
        {
            *sum = sum_before;
            if overlap > 0 {
                let x = v.as_f64().expect("interval value");
                *sum += exact(x) * exact_ns(i128::from(overlap)) / exact_ns(i128::from(d));
                *apportioned |= overlap < d;
            }
        }
        if overlap > 0 {
            node.coverage += i128::from(overlap);
        }
    }
}

/// Whether a counter step from `a` to `b` is a regression (§5.5, §12 D2): a
/// decrease without `wrap_at` that is not below `reset_ceiling` × `a`.
pub fn is_regression(p: &Params, a: f64, b: f64) -> bool {
    b < a && p.counter.wrap_at.is_none() && b >= p.counter.reset_ceiling * a
}

/// Aggregates refused for a kind (§6): avg, integral, increase and
/// time-in-state on an interval series.
pub fn refused(kind: Kind, agg: &Aggregate) -> bool {
    kind == Kind::Interval
        && matches!(
            agg,
            Aggregate::Avg | Aggregate::Integral | Aggregate::Increase | Aggregate::TimeInState(_)
        )
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
    /// Counter increase ÷ the elapsed seconds between the readings that
    /// bound the steps credited; empty when no step is credited, flagged
    /// ([`Node::rate_flagged`]) when a step spans more than S (§6).
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
    /// Whether a rate over this node is flagged: a step it includes spans
    /// more than S in force at the step's earlier reading (§6).
    pub fn rate_flagged(&self) -> bool {
        matches!(self.body, Body::Counter { long_steps, .. } if long_steps > 0)
    }

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
            (
                Aggregate::Rate,
                Body::Counter {
                    increase, elapsed, ..
                },
            ) => (*elapsed > 0).then(|| AggValue::Exact(increase * ns_per_s / exact_ns(*elapsed))),
            (Aggregate::Sum, Body::Interval { sum, .. }) => {
                covered.map(|_| AggValue::Exact(sum.clone()))
            }
            (Aggregate::Rate, Body::Interval { sum, .. }) => {
                covered.map(|c| AggValue::Exact(sum * ns_per_s / c))
            }
            (Aggregate::Min, Body::Interval { min, .. }) => min.map(|m| AggValue::Exact(exact(m))),
            (Aggregate::Max, Body::Interval { max, .. }) => max.map(|m| AggValue::Exact(exact(m))),
            (Aggregate::Last, _) => self.last.as_ref().map(|(_, v)| AggValue::Value(v.clone())),
            (Aggregate::TimeInState(k), Body::State { time, .. }) => Some(AggValue::Exact(
                exact_ns(time.get(k).copied().unwrap_or(0)) / ns_per_s,
            )),
            _ => None,
        }
    }
}
