//! `dirty_from_log` (design 7.1b sections 5 Start and 6 The stretch): the quarter-hours touched by facts in `(K, P]`
//! and the series named by node-changing entries there, from one pass over that stretch of the log.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::reader::{LogReader, ReadEvent, ReaderError};
use kvik_log::writer::Position as LogPos;

use super::dirty::{Key, quarter_of};
use crate::env::Env;

/// What a stretch of the log dirties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stretch {
    /// Each pair touched, at the highest position that touched it.
    pub(crate) keys: BTreeMap<Key, LogPos>,
    /// Series a node-changing entry names. A definition counts whatever was defined before, as the pass does not know
    /// the registry at K (Q-1832).
    pub(crate) series: BTreeSet<u32>,
}

impl Stretch {
    /// Takes in one batch's items from position `first`, those at or below `after` and above `upto` left out.
    pub(crate) fn take(&mut self, first: LogPos, items: &[Item], after: LogPos, upto: LogPos) {
        let mut seq = first.sequence;
        for item in items {
            let at = LogPos {
                generation: first.generation,
                sequence: seq,
            };
            seq = seq.saturating_add(item.sequences());
            if at <= after || at > upto {
                continue;
            }
            match item {
                Item::Fact(r) => {
                    if let Some(q) = quarter_of(r.t) {
                        let e = self.keys.entry((r.series_id, q)).or_default();
                        *e = (*e).max(at);
                    }
                }
                Item::Entry(e) => {
                    if let Some(id) = changes_nodes(e) {
                        self.series.insert(id);
                    }
                }
            }
        }
    }
}

/// [`super::dirty::node_changing`] without the registry: a definition always counts.
fn changes_nodes(e: &Entry) -> Option<u32> {
    match e {
        Entry::Definition(d) => Some(d.id),
        other => super::dirty::node_changing(&super::super::registry::Registry::default(), other),
    }
}

/// The stretch `(after, upto]` of the log in `dir`, read in parts.
pub(crate) fn dirty_from_log(
    dir: &Path,
    env: &Env,
    after: LogPos,
    upto: LogPos,
) -> Result<Stretch, ReaderError> {
    let mut out = Stretch::default();
    if upto <= after {
        return Ok(out);
    }
    let mut reader = crate::durable::start::log_reader(dir, env)?;
    if after.sequence > 0 {
        let _ = reader.start_after(after)?;
    }
    let r = read(&mut reader, &mut out, after, upto);
    let _ = reader.close();
    r.map(|()| out)
}

fn read(
    reader: &mut LogReader,
    out: &mut Stretch,
    after: LogPos,
    upto: LogPos,
) -> Result<(), ReaderError> {
    loop {
        let (u, more) = reader.refresh_upto(PART, upto)?;
        for e in &u.events {
            if let ReadEvent::Batch(b) = e {
                out.take(b.first, &b.content.items, after, upto);
            }
        }
        reader.release_bytes();
        if !more {
            return Ok(());
        }
    }
}

/// Items read per part of the pass.
const PART: usize = 1 << 14;

#[cfg(test)]
mod tests {
    use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
    use kvik_tree::set::QUARTER;

    use super::*;

    fn fact(t: i64) -> Item {
        Item::Fact(FactRow {
            series_id: 1,
            t,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: Value::F64(0),
            changed_at: None,
        })
    }

    fn p(s: u64) -> LogPos {
        LogPos {
            generation: 1,
            sequence: s,
        }
    }

    #[test]
    fn take_keeps_items_above_after_and_at_or_below_upto_only() {
        let mut s = Stretch::default();
        let items: Vec<Item> = (0..5).map(|i| fact(i * QUARTER)).collect();
        s.take(p(10), &items, p(11), p(13));
        let got: Vec<(Key, LogPos)> = s.keys.into_iter().collect();
        assert_eq!(got, [((1, 2 * QUARTER), p(12)), ((1, 3 * QUARTER), p(13))]);
    }

    #[test]
    fn take_leaves_out_facts_whose_quarter_hour_does_not_fit() {
        let mut s = Stretch::default();
        s.take(
            p(1),
            &[fact(i64::MIN + 1), fact(i64::MAX), fact(0)],
            p(0),
            p(9),
        );
        assert_eq!(s.keys.into_keys().collect::<Vec<_>>(), [(1, 0)]);
    }
}
