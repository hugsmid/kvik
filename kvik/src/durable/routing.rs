//! The routing table (spec §4 Routing): the funnels and `emits` of every consumer, built by applying the routing
//! changes of the log (`format.md` A.6 entry type 5) in position order, so the routes in force at a position are those
//! the log names up to it. A name pattern is kept as declared: a fact funnel's pattern is resolved against the live
//! series whenever a definition or alias is applied, a condition funnel's once, when routed, and carried in the entry.
//!
//! Also here: the conversion between the interface's [`Funnel`] and a routing change, and the cycle check over the
//! consumers' `emits` and selections.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kvik_log::entries::{
    Entry, RouteCondition, RouteOp, RouteOperator, Routing, Selection, StateArg,
};
use kvik_log::writer::Position as LogPos;

use super::registry::Registry;
use crate::backend::{Error, convert, matches_pattern};
use crate::consume::{Condition, Funnel, Operator, Select};
use crate::read::Aggregate;
use crate::series::{Kind, StateValues, Timing};
use crate::types::{SeriesId, Value};

/// A funnel in force.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunnelRec {
    /// The position of the routing change that added it: its incarnation (spec §6 Latch).
    pub(crate) incarnation: LogPos,
    /// The selection as declared.
    pub(crate) selection: Selection,
    /// The operator.
    pub(crate) operator: Option<RouteOperator>,
    /// The condition.
    pub(crate) condition: Option<RouteCondition>,
}

impl FunnelRec {
    /// The one series a condition funnel watches.
    pub(crate) fn condition_series(&self) -> Option<u32> {
        self.condition.as_ref()?;
        match &self.selection {
            Selection::Ids(ids) if ids.len() == 1 => Some(ids[0]),
            Selection::Pattern { resolved, .. } if resolved.len() == 1 => Some(resolved[0]),
            _ => None,
        }
    }
}

/// One consumer's routes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConsumerRoutes {
    /// Funnels in force by id.
    pub(crate) funnels: BTreeMap<u32, FunnelRec>,
    /// The series it may emit.
    pub(crate) emits: Option<Selection>,
    /// The highest funnel id any routing change named, removed ones included: the next is above it.
    pub(crate) max_funnel: u32,
}

/// The routing table with the registry it resolves patterns against, as of one position.
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub(crate) registry: Registry,
    pub(crate) consumers: BTreeMap<String, ConsumerRoutes>,
    /// The series each fact funnel with a name pattern selects now, by consumer and funnel.
    sets: BTreeMap<(String, u32), BTreeSet<u32>>,
}

/// The live series with a name or alias matching `pattern`.
fn resolve(registry: &Registry, pattern: &str) -> BTreeSet<u32> {
    registry
        .iter()
        .filter(|s| !s.retired() && s.names.iter().any(|n| matches_pattern(pattern, n)))
        .map(|s| s.def.id)
        .collect()
}

impl View {
    /// Applies an entry at its position.
    pub(crate) fn apply(&mut self, p: LogPos, e: &Entry) {
        match e {
            Entry::Routing(r) => self.route(p, r),
            Entry::Definition(_) | Entry::Alias { .. } => {
                self.registry.apply(e);
                // Patterns are re-evaluated when a definition or alias commits (spec §4 Routing).
                let registry = &self.registry;
                for ((c, f), set) in &mut self.sets {
                    if let Some(Selection::Pattern { pattern, .. }) = self
                        .consumers
                        .get(c)
                        .and_then(|r| r.funnels.get(f))
                        .map(|f| &f.selection)
                    {
                        *set = resolve(registry, pattern);
                    }
                }
            }
            _ => self.registry.apply(e),
        }
    }

    fn route(&mut self, p: LogPos, r: &Routing) {
        let c = self.consumers.entry(r.consumer.clone()).or_default();
        c.emits = Some(r.emits.clone());
        let Some(id) = r.funnel else { return };
        c.max_funnel = c.max_funnel.max(id);
        match r.op {
            RouteOp::Add => {
                let selection = r.selection.clone().unwrap_or(Selection::None);
                if let (Selection::Pattern { pattern, .. }, None) = (&selection, &r.condition) {
                    self.sets
                        .insert((r.consumer.clone(), id), resolve(&self.registry, pattern));
                }
                c.funnels.insert(
                    id,
                    FunnelRec {
                        incarnation: p,
                        selection,
                        operator: r.operator,
                        condition: r.condition,
                    },
                );
            }
            RouteOp::Remove => {
                c.funnels.remove(&id);
                self.sets.remove(&(r.consumer.clone(), id));
            }
            RouteOp::Emits => {}
        }
    }

    /// The funnels of a consumer in force.
    pub(crate) fn funnels(&self, consumer: &str) -> impl Iterator<Item = (u32, &FunnelRec)> {
        self.consumers
            .get(consumer)
            .into_iter()
            .flat_map(|c| c.funnels.iter().map(|(id, f)| (*id, f)))
    }

    /// Whether a fact funnel of the consumer selects the series.
    pub(crate) fn routed(&self, consumer: &str, series: u32) -> bool {
        self.funnels(consumer)
            .filter(|(_, f)| f.condition.is_none())
            .any(|(id, f)| self.selects(consumer, id, f, series))
    }

    fn selects(&self, consumer: &str, id: u32, f: &FunnelRec, series: u32) -> bool {
        match &f.selection {
            Selection::None => false,
            Selection::All => true,
            Selection::Ids(ids) => ids.contains(&series),
            Selection::Pattern { resolved, .. } if f.condition.is_some() => {
                resolved.contains(&series)
            }
            Selection::Pattern { .. } => self
                .sets
                .get(&(consumer.to_string(), id))
                .is_some_and(|s| s.contains(&series)),
        }
    }

    /// The series a funnel selects now, for a resync's funnel states.
    pub(crate) fn selected(&self, consumer: &str, id: u32, f: &FunnelRec) -> Vec<u32> {
        match &f.selection {
            Selection::None => Vec::new(),
            Selection::All => self.registry.iter().map(|s| s.def.id).collect(),
            Selection::Ids(ids) => ids
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            Selection::Pattern { resolved, .. } if f.condition.is_some() => resolved.clone(),
            Selection::Pattern { .. } => self
                .sets
                .get(&(consumer.to_string(), id))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default(),
        }
    }

    /// The consumer's `emits` in force: none before any routing change.
    pub(crate) fn emits(&self, consumer: &str) -> Selection {
        self.consumers
            .get(consumer)
            .and_then(|c| c.emits.clone())
            .unwrap_or(Selection::None)
    }

    /// Whether the consumer may emit a reading on the series.
    pub(crate) fn may_emit(&self, consumer: &str, series: u32) -> bool {
        match self.emits(consumer) {
            Selection::None => false,
            Selection::All => true,
            Selection::Ids(ids) => ids.contains(&series),
            Selection::Pattern { pattern, .. } => self
                .registry
                .get(series)
                .is_some_and(|s| s.names.iter().any(|n| matches_pattern(&pattern, n))),
        }
    }

    /// A cycle the routes would have after `change` (spec §4 Routing), with `extra_name` given to a series (an alias
    /// being decided): the consumers on it, the first again at the end. `None` when there is none.
    pub(crate) fn cycle_with(
        &self,
        change: Option<&Routing>,
        extra_name: Option<(u32, &str)>,
    ) -> Option<Vec<String>> {
        let mut consumers = self.consumers.clone();
        if let Some(r) = change {
            let mut v = View {
                registry: Registry::default(),
                consumers: std::mem::take(&mut consumers),
                sets: BTreeMap::new(),
            };
            v.route(LogPos::default(), r);
            consumers = v.consumers;
        }
        let names = |id: u32| -> Vec<String> {
            let mut n: Vec<String> = self
                .registry
                .get(id)
                .map(|s| s.names.clone())
                .unwrap_or_default();
            if let Some((x, name)) = extra_name
                && x == id
            {
                n.push(name.to_string());
            }
            n
        };
        let list: Vec<&String> = consumers.keys().collect();
        let edge = |a: &str, b: &str| -> bool {
            let emits = consumers[a].emits.clone().unwrap_or(Selection::None);
            consumers[b]
                .funnels
                .values()
                .any(|f| overlap(&emits, &f.selection, &names))
        };
        // Depth-first search from each consumer.
        fn visit(
            at: usize,
            list: &[&String],
            edge: &dyn Fn(&str, &str) -> bool,
            path: &mut Vec<usize>,
            done: &mut BTreeSet<usize>,
        ) -> Option<Vec<usize>> {
            if let Some(i) = path.iter().position(|x| *x == at) {
                let mut cycle = path[i..].to_vec();
                cycle.push(at);
                return Some(cycle);
            }
            if done.contains(&at) {
                return None;
            }
            path.push(at);
            for next in 0..list.len() {
                if edge(list[at], list[next])
                    && let Some(c) = visit(next, list, edge, path, done)
                {
                    return Some(c);
                }
            }
            path.pop();
            done.insert(at);
            None
        }
        let mut done = BTreeSet::new();
        for start in 0..list.len() {
            if let Some(c) = visit(start, &list, &edge, &mut Vec::new(), &mut done) {
                return Some(c.into_iter().map(|i| list[i].clone()).collect());
            }
        }
        None
    }
}

/// Whether one name could be matched by both selections (spec §4 Routing): two patterns that one name could match,
/// an id against a pattern through any name of its series, or the same id.
pub(crate) fn overlap(a: &Selection, b: &Selection, names: &dyn Fn(u32) -> Vec<String>) -> bool {
    use Selection as S;
    let empty = |s: &Selection| matches!(s, S::None) || matches!(s, S::Ids(ids) if ids.is_empty());
    if empty(a) || empty(b) {
        return false;
    }
    match (a, b) {
        (S::All, _) | (_, S::All) => true,
        (S::Ids(x), S::Ids(y)) => x.iter().any(|i| y.contains(i)),
        (S::Ids(ids), S::Pattern { pattern, .. }) | (S::Pattern { pattern, .. }, S::Ids(ids)) => {
            ids.iter()
                .any(|i| names(*i).iter().any(|n| matches_pattern(pattern, n)))
        }
        (S::Pattern { pattern: p, .. }, S::Pattern { pattern: q, .. }) => patterns_overlap(p, q),
        _ => false,
    }
}

/// Whether some name matches both patterns: the same number of `/`-separated segments, and in each segment some
/// string that both globs match (`*` matching any run of characters within a segment, Q-88).
pub(crate) fn patterns_overlap(p: &str, q: &str) -> bool {
    let (a, b): (Vec<&str>, Vec<&str>) = (p.split('/').collect(), q.split('/').collect());
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| globs_meet(x.as_bytes(), y.as_bytes()))
}

fn globs_meet(p: &[u8], q: &[u8]) -> bool {
    // seen[i][j]: the suffixes p[i..] and q[j..] were found to share no string.
    let mut seen = vec![vec![false; q.len() + 1]; p.len() + 1];
    fn go(p: &[u8], q: &[u8], i: usize, j: usize, seen: &mut Vec<Vec<bool>>) -> bool {
        if seen[i][j] {
            return false;
        }
        let r = match (p.get(i), q.get(j)) {
            (None, None) => true,
            (Some(b'*'), _) => {
                go(p, q, i + 1, j, seen) || (j < q.len() && go(p, q, i, j + 1, seen))
            }
            (_, Some(b'*')) => {
                go(p, q, i, j + 1, seen) || (i < p.len() && go(p, q, i + 1, j, seen))
            }
            (Some(x), Some(y)) => x == y && go(p, q, i + 1, j + 1, seen),
            _ => false,
        };
        if !r {
            seen[i][j] = true;
        }
        r
    }
    go(p, q, 0, 0, &mut seen)
}

/// A selection as the log carries it, for `emits` (no pattern is resolved).
pub(crate) fn emits_in(s: &Select) -> Selection {
    match s {
        Select::None => Selection::None,
        Select::All => Selection::All,
        Select::Ids(ids) => Selection::Ids(ids.iter().map(|i| i.0).collect()),
        Select::Names(p) => Selection::Pattern {
            pattern: p.clone(),
            resolved: Vec::new(),
        },
    }
}

/// A selection as the interface shows it.
pub(crate) fn select_out(s: &Selection) -> Select {
    match s {
        Selection::None => Select::None,
        Selection::All => Select::All,
        Selection::Ids(ids) => Select::Ids(ids.iter().map(|i| SeriesId(*i)).collect()),
        Selection::Pattern { pattern, .. } => Select::Names(pattern.clone()),
    }
}

fn nanos(d: Duration) -> Result<i64, Error> {
    convert::dur_nanos(d)
}

fn route_err(s: &str) -> Error {
    Error::Route(s.into())
}

/// The routing change that adds `funnel` as funnel `id` of `consumer`, with `emits` in force, checked against the
/// registry as the model backend checks it (`model/OPEN-QUESTIONS.md` Q-88 for patterns). Operators are refused:
/// they need windows over recent facts (roadmap 5.4).
pub(crate) fn add_change(
    registry: &Registry,
    consumer: &str,
    id: u32,
    funnel: &Funnel,
    emits: Selection,
) -> Result<Routing, Error> {
    if funnel.operator.is_some() {
        return Err(Error::NotSupportedYet(
            "a funnel with an operator on the durable backend (windowed operators come with roadmap 5.4)",
        ));
    }
    let selection = match &funnel.series {
        Select::None => Selection::None,
        Select::All => Selection::All,
        Select::Ids(ids) => {
            for i in ids {
                if registry.get(i.0).is_none() {
                    return Err(Error::UnknownSeries(i.to_string()));
                }
            }
            Selection::Ids(ids.iter().map(|i| i.0).collect())
        }
        Select::Names(p) => {
            if p.is_empty() || p.len() > 255 {
                return Err(route_err("a name pattern is 1 to 255 bytes"));
            }
            let resolved = if funnel.condition.is_some() {
                resolve(registry, p).into_iter().collect()
            } else {
                Vec::new()
            };
            Selection::Pattern {
                pattern: p.clone(),
                resolved,
            }
        }
    };
    let condition = match &funnel.condition {
        None => None,
        Some(c) => {
            let series = match &selection {
                Selection::Ids(ids) if ids.len() == 1 => ids[0],
                Selection::Pattern { resolved, .. } if resolved.len() == 1 => resolved[0],
                _ => return Err(route_err("a condition funnel selects exactly one series")),
            };
            let s = registry
                .get(series)
                .ok_or_else(|| Error::UnknownSeries(series.to_string()))?;
            let finite = |x: f64, what: &str| {
                if x.is_finite() {
                    Ok(x)
                } else {
                    Err(Error::Route(format!("the {what} is not finite")))
                }
            };
            let rc = match c {
                Condition::Threshold { on, clear } => {
                    if clear > on {
                        return Err(route_err("the clear level is above the activation level"));
                    }
                    RouteCondition::Threshold {
                        on: finite(*on, "activation level")?,
                        clear: finite(*clear, "clear level")?,
                    }
                }
                Condition::OnChange { deadband } => {
                    let d = finite(*deadband, "dead-band")?;
                    if d < 0.0 {
                        return Err(route_err("the dead-band is negative"));
                    }
                    RouteCondition::OnChange { deadband: d }
                }
                Condition::HeldFor { above, n } => RouteCondition::HeldFor {
                    above: finite(*above, "level")?,
                    n: nanos(*n)?,
                },
                Condition::StaleFor { n } => {
                    if s.def.staleness.is_none() {
                        return Err(route_err("stale-for needs a staleness limit"));
                    }
                    RouteCondition::StaleFor { n: nanos(*n)? }
                }
            };
            // §4 Future facts, §6 Cadence: a condition over a forecast series needs a windowed operator.
            if s.spec.timing == Timing::Forecast {
                return Err(route_err(match rc {
                    RouteCondition::StaleFor { .. } => {
                        "a stale-for condition on a forecast series is rejected"
                    }
                    _ => "a condition over a forecast series needs a windowed operator",
                }));
            }
            Some(rc)
        }
    };
    Ok(Routing {
        consumer: consumer.to_string(),
        op: RouteOp::Add,
        funnel: Some(id),
        selection: Some(selection),
        operator: None,
        emits,
        condition,
    })
}

/// The funnel a routing change added, as the interface shows it.
pub(crate) fn funnel_out(f: &FunnelRec, registry: &Registry) -> Funnel {
    let operator = f.operator.map(|o| {
        let aggregate = match o.aggregate {
            1 => Aggregate::Avg,
            2 => Aggregate::Sum,
            3 => Aggregate::Integral,
            4 => Aggregate::Min,
            5 => Aggregate::Max,
            6 => Aggregate::Count,
            7 => Aggregate::Increase,
            8 => Aggregate::Rate,
            9 => Aggregate::Last,
            _ => Aggregate::TimeInState(match o.state {
                Some(StateArg::Bool(b)) => Value::Bool(b),
                Some(StateArg::State(i)) => Value::State(u16::try_from(i).unwrap_or(u16::MAX)),
                None => Value::Absent,
            }),
        };
        Operator::rolling(aggregate, convert::dur(i128::from(o.width)))
    });
    let condition = f.condition.map(|c| match c {
        RouteCondition::Threshold { on, clear } => Condition::Threshold { on, clear },
        RouteCondition::OnChange { deadband } => Condition::OnChange { deadband },
        RouteCondition::HeldFor { above, n } => Condition::HeldFor {
            above,
            n: convert::dur(i128::from(n)),
        },
        RouteCondition::StaleFor { n } => Condition::StaleFor {
            n: convert::dur(i128::from(n)),
        },
    });
    let _ = registry;
    Funnel {
        series: select_out(&f.selection),
        operator,
        condition,
    }
}

/// The model's condition for a funnel's condition, with the staleness limit of its series.
pub(crate) fn model_condition(
    c: &RouteCondition,
    registry: &Registry,
    series: u32,
) -> kvik_model::condition::Condition {
    use kvik_model::condition::Condition as C;
    match *c {
        RouteCondition::Threshold { on, clear } => C::Threshold { on, clear },
        RouteCondition::OnChange { deadband } => C::OnChange { deadband },
        RouteCondition::HeldFor { above, n } => C::HeldFor { above, n },
        RouteCondition::StaleFor { n } => C::StaleFor {
            staleness: registry
                .get(series)
                .and_then(|s| s.def.staleness)
                .unwrap_or(i64::MAX / 4),
            n,
        },
    }
}

/// Whether a series' values are numbers a condition can read.
pub(crate) fn numeric(registry: &Registry, series: u32) -> bool {
    registry.get(series).is_some_and(|s| {
        !matches!(
            s.spec.kind,
            Kind::State {
                values: StateValues::Bool | StateValues::Named(_)
            } | Kind::Text
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_overlap_within_segments() {
        assert!(patterns_overlap("plant/*/power", "plant/line-*/power"));
        assert!(patterns_overlap("plant/*/power", "*/a/*"));
        assert!(patterns_overlap("a*b/x/y", "*c*/x/y"));
        assert!(!patterns_overlap("plant/*/power", "plant/*/energy"));
        assert!(!patterns_overlap("plant/*/power", "plant/*"));
        assert!(!patterns_overlap("a*b/x/y", "c*/x/y"));
        // "abab" matches both.
        assert!(patterns_overlap("ab*/x/y", "*ba*b/x/y"));
        assert!(!patterns_overlap("ab/x/y", "*ba*b/x/y"));
    }

    #[test]
    fn selections_overlap() {
        let names = |id: u32| match id {
            1 => vec!["site/a/power".to_string()],
            _ => vec!["site/b/energy".to_string()],
        };
        let pat = |p: &str| Selection::Pattern {
            pattern: p.into(),
            resolved: Vec::new(),
        };
        assert!(overlap(
            &Selection::Ids(vec![1]),
            &pat("site/*/power"),
            &names
        ));
        assert!(!overlap(
            &Selection::Ids(vec![2]),
            &pat("site/*/power"),
            &names
        ));
        assert!(overlap(&Selection::All, &Selection::Ids(vec![2]), &names));
        assert!(!overlap(&Selection::All, &Selection::Ids(vec![]), &names));
        assert!(!overlap(&Selection::None, &Selection::All, &names));
        assert!(overlap(
            &Selection::Ids(vec![1, 2]),
            &Selection::Ids(vec![2]),
            &names
        ));
    }
}
