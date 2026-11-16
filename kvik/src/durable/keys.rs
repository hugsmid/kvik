//! Key-history lookups in local data (roadmap 11.0): every local fact of a key `(series, t)` in position order, or
//! of a series over a span of event time, read from the local log through the segment indexes (`format.md` A.8) and
//! a scan of every segment without one. The decision's slow path and the deliverer call it through [`LocalFacts`];
//! a later read path (roadmap 9.0a) builds on the same primitive.
//!
//! An invalid region the store's start already passed over is lost (spec §3 Recovery rule 2): its positions are
//! out of the log, as they are out of the state the start built. Any other damage, and a segment that cannot be read,
//! is never taken as absence: the keys it may hold are reported unreadable (Q-750).

use std::collections::{BTreeSet, HashMap, HashSet};

use kvik_log::reader::{FoundFact, LogReader, Wanted};

use super::history::Key;

/// What a lookup found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The facts of each key asked for, in position order; absent for a key without one.
    pub(crate) facts: HashMap<Key, Vec<FoundFact>>,
    /// Keys some place that may hold them could not be read.
    pub(crate) unreadable: HashSet<Key>,
}

/// Facts in local data by key or span.
pub(crate) trait LocalFacts: Send + std::fmt::Debug {
    /// Every local fact of each key, in position order; a key some place may hold that could not be read is
    /// unreadable, never absent.
    fn at(&mut self, keys: &[Key]) -> Found;
    /// Every local fact of a series with `t` from `from` to `to`, both inclusive, in position order; `None` when
    /// some place that may hold one could not be read.
    fn between(&mut self, series: u32, from: i64, to: i64) -> Option<Vec<FoundFact>>;
}

/// [`LocalFacts`] on the local log.
#[derive(Debug)]
pub(crate) struct LogFacts {
    reader: LogReader,
    /// Invalid regions the start passed over, by segment and offset.
    lost: BTreeSet<(u64, u64)>,
}

impl LogFacts {
    pub(crate) fn new(reader: LogReader, lost: BTreeSet<(u64, u64)>) -> Self {
        LogFacts { reader, lost }
    }
}

impl LocalFacts for LogFacts {
    fn at(&mut self, keys: &[Key]) -> Found {
        let wanted: Vec<Wanted> = keys
            .iter()
            .map(|&(series_id, t)| Wanted {
                series_id,
                from: t,
                to: t,
            })
            .collect();
        let lost = &self.lost;
        let mut out = Found::default();
        match self.reader.lookup(&wanted, &|f, o| lost.contains(&(f, o))) {
            Ok(l) => {
                for (k, complete) in keys.iter().zip(&l.complete) {
                    if !complete {
                        out.unreadable.insert(*k);
                    }
                }
                for f in l.facts {
                    out.facts
                        .entry((f.row.series_id, f.row.t))
                        .or_default()
                        .push(f);
                }
                for fs in out.facts.values_mut() {
                    fs.sort_by_key(|f| f.position);
                }
            }
            Err(_) => out.unreadable.extend(keys.iter().copied()),
        }
        out
    }

    fn between(&mut self, series: u32, from: i64, to: i64) -> Option<Vec<FoundFact>> {
        let wanted = [Wanted {
            series_id: series,
            from,
            to,
        }];
        let lost = &self.lost;
        let l = self
            .reader
            .lookup(&wanted, &|f, o| lost.contains(&(f, o)))
            .ok()?;
        let mut facts = l.facts;
        facts.sort_by_key(|f| f.position);
        l.complete[0].then_some(facts)
    }
}
