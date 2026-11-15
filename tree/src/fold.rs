//! The fold of a node from its readings (spec §5.6 canonical order) and `merge` (spec §5.3).

use crate::node::{Body, Node, Params, higher, lower};

/// The canonical computation of a node over `[start, end)`: the left-to-right fold over its readings, which must
/// lie in the span in strictly increasing `t` (spec §5.6). Every stored 1-minute node is this fold.
pub fn fold(p: &Params, start: i64, end: i64, readings: &[(i64, crate::Value)]) -> Node {
    let mut node = Node::empty(p.kind, start, end);
    for (i, r) in readings.iter().enumerate() {
        debug_assert!(start <= r.0 && r.0 < end, "reading outside the span");
        node.observe(r.0, &r.1);
        if let Some(d) = p.interval {
            node.coverage += i128::from(d);
        }
        if i > 0 {
            node.pair(p, &readings[i - 1], r);
        }
    }
    node
}

/// Merges adjacent nodes (`l.end == r.start`): the empty node is the identity; otherwise fields combine and the
/// boundary pair (`l.last`, `r.first`) contributes once (spec §5.3), so merge is associative.
///
/// # Panics
/// Panics when the nodes are not adjacent or are of different kinds.
pub fn merge(p: &Params, l: &Node, r: &Node) -> Node {
    assert_eq!(l.end, r.start, "merge needs adjacent nodes");
    if l.is_identity() {
        return Node {
            start: l.start,
            ..r.clone()
        };
    }
    if r.is_identity() {
        return Node {
            end: r.end,
            ..l.clone()
        };
    }
    let body = match (&l.body, &r.body) {
        (
            Body::Gauge {
                sum: ls,
                min: lmin,
                max: lmax,
                integral: li,
            },
            Body::Gauge {
                sum: rs,
                min: rmin,
                max: rmax,
                integral: ri,
            },
        ) => Body::Gauge {
            sum: ls + rs,
            min: either(*lmin, *rmin, lower),
            max: either(*lmax, *rmax, higher),
            integral: li + ri,
        },
        (
            Body::Counter {
                increase: li,
                resets: lr,
                regressions: lg,
                steps: ls,
                long_steps: ll,
                elapsed: le,
            },
            Body::Counter {
                increase: ri,
                resets: rr,
                regressions: rg,
                steps: rs,
                long_steps: rl,
                elapsed: re,
            },
        ) => Body::Counter {
            increase: li + ri,
            resets: lr + rr,
            regressions: lg + rg,
            steps: ls + rs,
            long_steps: ll + rl,
            elapsed: le + re,
        },
        (
            Body::State {
                time: lt,
                transitions: ltr,
            },
            Body::State {
                time: rt,
                transitions: rtr,
            },
        ) => {
            let mut time = lt.clone();
            for (k, v) in rt {
                *time.entry(*k).or_insert(0) += v;
            }
            Body::State {
                time,
                transitions: ltr + rtr,
            }
        }
        (Body::Text, Body::Text) => Body::Text,
        (
            Body::Interval {
                sum: ls,
                min: lmin,
                max: lmax,
                apportioned: la,
            },
            Body::Interval {
                sum: rs,
                min: rmin,
                max: rmax,
                apportioned: ra,
            },
        ) => Body::Interval {
            sum: ls + rs,
            min: either(*lmin, *rmin, lower),
            max: either(*lmax, *rmax, higher),
            apportioned: *la || *ra,
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

/// Merges adjacent nodes left to right (spec §5.6: a higher node is the left-to-right merge of its children); no
/// nodes give the empty node over `[start, start)`.
pub fn merge_all<'a, I>(p: &Params, start: i64, nodes: I) -> Node
where
    I: IntoIterator<Item = &'a Node>,
{
    nodes
        .into_iter()
        .fold(None, |acc: Option<Node>, n| {
            Some(match acc {
                None => n.clone(),
                Some(a) => merge(p, &a, n),
            })
        })
        .unwrap_or_else(|| Node::empty(p.kind, start, start))
}

fn either(a: Option<f64>, b: Option<f64>, f: fn(f64, f64) -> f64) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(f(x, y)),
        (x, None) => x,
        (None, y) => y,
    }
}
