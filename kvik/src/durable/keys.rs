//! Key-history lookups in local data (roadmap 11.0): every local fact of a key `(series, t)` in position order, or
//! of a series over a span of event time, read from the local log through the segment indexes (`format.md` A.8) and
//! a scan of every segment without one. The decision's slow path and the deliverer call it through [`LocalFacts`];
//! a later read path (roadmap 9.0a) builds on the same primitive.
//!
//! An invalid region the store's start passed over, or one a scanning reader of this process reported lost, is lost
//! (spec §3 Recovery rule 2, Q-922): its positions are out of the log, as they are out of the decision state. Any
//! other damage, and a segment that cannot be read, is never taken as absence: the keys it may hold are reported
//! unreadable (Q-750).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kvik_log::reader::{FoundFact, LogReader, Wanted};

use super::history::Key;
use super::loss::{Latch, Regions};

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
    /// Every local fact of a series with `t` from `from` to `to`, both inclusive, that could be read, in position
    /// order, and whether every place that may hold one was read.
    fn read_between(&mut self, series: u32, from: i64, to: i64) -> (Vec<FoundFact>, bool);
    /// [`LocalFacts::read_between`], `None` when it was incomplete.
    fn between(&mut self, series: u32, from: i64, to: i64) -> Option<Vec<FoundFact>> {
        let (facts, complete) = self.read_between(series, from, to);
        complete.then_some(facts)
    }
    /// Bytes its reader keeps between refreshes.
    #[cfg(test)]
    fn kept_bytes(&self) -> usize {
        0
    }
}

/// [`LocalFacts`] on a log reader of this process, skipping the regions the start passed over and those the latch
/// holds.
#[derive(Debug)]
pub(crate) struct OnLog<'a> {
    pub(crate) reader: &'a mut LogReader,
    pub(crate) lost: &'a Regions,
    pub(crate) latch: &'a Latch,
}

impl OnLog<'_> {
    fn skip(lost: &Regions, latch: &Latch, file_id: u64, start: u64, end: u64) -> bool {
        lost.covers(file_id, start, end) || latch.covers(file_id, start, end)
    }
}

impl LocalFacts for OnLog<'_> {
    fn at(&mut self, keys: &[Key]) -> Found {
        let wanted: Vec<Wanted> = keys
            .iter()
            .map(|&(series_id, t)| Wanted {
                series_id,
                from: t,
                to: t,
            })
            .collect();
        let mut out = Found::default();
        let (lost, latch) = (self.lost, self.latch);
        let skipped = |f, a, z| OnLog::skip(lost, latch, f, a, z);
        match self.reader.lookup(&wanted, &skipped) {
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

    fn read_between(&mut self, series: u32, from: i64, to: i64) -> (Vec<FoundFact>, bool) {
        let wanted = [Wanted {
            series_id: series,
            from,
            to,
        }];
        let (lost, latch) = (self.lost, self.latch);
        let skipped = |f, a, z| OnLog::skip(lost, latch, f, a, z);
        match self.reader.lookup(&wanted, &skipped) {
            Ok(l) => {
                let mut facts = l.facts;
                facts.sort_by_key(|f| f.position);
                (facts, l.complete[0])
            }
            Err(_) => (Vec::new(), false),
        }
    }
}

/// [`LocalFacts`] on a log reader it owns.
#[derive(Debug)]
pub(crate) struct LogFacts {
    reader: LogReader,
    /// Invalid regions the start passed over, by segment and offset.
    lost: Regions,
    latch: Arc<Latch>,
}

impl LogFacts {
    pub(crate) fn new(reader: LogReader, lost: Regions, latch: Arc<Latch>) -> Self {
        LogFacts {
            reader,
            lost,
            latch,
        }
    }

    fn on(&mut self) -> OnLog<'_> {
        OnLog {
            reader: &mut self.reader,
            lost: &self.lost,
            latch: &self.latch,
        }
    }
}

impl LocalFacts for LogFacts {
    fn at(&mut self, keys: &[Key]) -> Found {
        self.on().at(keys)
    }

    fn read_between(&mut self, series: u32, from: i64, to: i64) -> (Vec<FoundFact>, bool) {
        self.on().read_between(series, from, to)
    }

    #[cfg(test)]
    fn kept_bytes(&self) -> usize {
        self.reader.kept_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lookup_that_fails_reports_every_key_unreadable() {
        use std::path::Path;
        use std::time::Duration;

        use kvik_log::fs::{Fs, MemFs};
        use kvik_log::reader::ReaderConfig;
        use kvik_log::store::{MARK, NewStore, StoreDir};
        use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};

        let fs = Arc::new(MemFs::new());
        let new = NewStore {
            store_id: [1; 16],
            created_at: 0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "test".into(),
        };
        let store = StoreDir::open(fs.clone(), Path::new("s"), Some(&new)).unwrap();
        let w = LogWriter::open(
            store,
            WriterConfig::default(),
            Some([2; 16]),
            Duration::ZERO,
            0,
        )
        .unwrap();
        let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
        log.shutdown().unwrap();
        let clock = Arc::new(SystemClock::default());
        let reader = LogReader::open(ReaderConfig::new(
            "s",
            fs.clone(),
            clock,
            Some([2; 16]),
            [9; 16],
        ))
        .unwrap();
        let mut facts = LogFacts::new(reader, Regions::default(), Arc::default());
        fs.remove_file(&Path::new("s").join("log").join(MARK))
            .unwrap();
        let found = facts.at(&[(1, 10), (2, 20)]);
        assert!(found.facts.is_empty());
        assert_eq!(found.unreadable.len(), 2);
        assert!(facts.between(1, 0, 10).is_none());
        assert_eq!(facts.read_between(1, 0, 10), (Vec::new(), false));
    }
}
