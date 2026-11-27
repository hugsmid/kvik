//! The entries a checkpoint writes for one dirty quarter-hour (design 7.1b section 5 step 3), folded from the span's
//! current facts at the checkpoint's cut.

use kvik_log::writer::Position;

use crate::fold::fold;
use crate::level::Level;
use crate::node::{Params, Value};
use crate::part::Entry;
use crate::set::QUARTER;

/// A current fact of a span: its event time, its summarised reading (`None` for one summaries ignore, such as bad
/// quality), and its position, which a node's `as_of` is the highest of (Q-1422).
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    /// Event time.
    pub t: i64,
    /// The summarised reading, if the fact gives one.
    pub reading: Option<Value>,
    /// The fact's position.
    pub at: Position,
}

/// The levels of `levels` a checkpoint folds from facts: those no wider than a quarter-hour that tile it.
pub fn folded_levels(levels: &[Level]) -> Vec<Level> {
    levels
        .iter()
        .copied()
        .filter(|l| l.width <= QUARTER && QUARTER % l.width == 0)
        .collect()
}

/// One entry per span of each level of `levels` inside `[q, q + QUARTER)`: the node of its readings, or an empty
/// marker that hides an older entry. `facts` are the quarter's current facts in ascending `t`, at most one per `t`.
pub fn quarter_entries(p: &Params, levels: &[Level], q: i64, facts: &[Fact]) -> Vec<(u8, Entry)> {
    debug_assert_eq!(q.rem_euclid(QUARTER), 0);
    let mut out = Vec::new();
    for l in levels {
        let mut lo = 0;
        for i in 0..QUARTER / l.width {
            let start = q + i * l.width;
            let end = start + l.width;
            let hi = lo + facts[lo..].partition_point(|f| f.t < end);
            let span = &facts[lo..hi];
            lo = hi;
            let readings: Vec<(i64, Value)> = span
                .iter()
                .filter_map(|f| f.reading.clone().map(|v| (f.t, v)))
                .collect();
            let node = fold(p, start, end, &readings);
            let as_of = span.iter().map(|f| f.at).max().unwrap_or_default();
            out.push((
                l.id,
                Entry {
                    start,
                    stale: false,
                    as_of,
                    node: (!node.is_empty()).then_some(node),
                },
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::{LEVELS, MINUTE};
    use crate::node::Kind;

    fn pos(s: u64) -> Position {
        Position {
            generation: 1,
            sequence: s,
        }
    }

    #[test]
    fn folded_levels_are_the_minute_and_the_quarter() {
        let ids: Vec<u8> = folded_levels(&LEVELS).iter().map(|l| l.id).collect();
        assert_eq!(ids, [1, 2]);
    }

    #[test]
    fn a_quarter_gives_its_node_fifteen_minute_entries_and_empty_markers_where_no_reading_is() {
        let p = Params::new(Kind::Gauge, None);
        let q = 4 * QUARTER;
        let facts = vec![
            Fact {
                t: q + 30_000_000_000,
                reading: Some(Value::F64(1.0)),
                at: pos(5),
            },
            Fact {
                t: q + 3 * MINUTE,
                reading: None,
                at: pos(9),
            },
            Fact {
                t: q + 14 * MINUTE,
                reading: Some(Value::F64(2.0)),
                at: pos(7),
            },
        ];
        let e = quarter_entries(&p, &folded_levels(&LEVELS), q, &facts);
        assert_eq!(e.len(), 16);
        let quarter = e.iter().find(|(l, _)| *l == 2).unwrap();
        assert_eq!(quarter.1.as_of, pos(9));
        assert_eq!(quarter.1.node.as_ref().unwrap().count, 2);
        let minutes: Vec<&Entry> = e.iter().filter(|(l, _)| *l == 1).map(|(_, e)| e).collect();
        assert!(minutes[0].node.is_some());
        assert_eq!(
            minutes[3].node, None,
            "a bad-quality fact alone leaves its minute empty"
        );
        assert_eq!(minutes[3].as_of, pos(9), "and still versions it");
        assert_eq!(minutes[5].as_of, Position::default());
        assert!(minutes[14].node.is_some());
    }
}
