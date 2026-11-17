//! Key-history lookups in local data (roadmap 11.0): every local fact of a key `(series, t)` in position order, or
//! of a series over a span of event time, read from the local log through the segment indexes (`format.md` A.8) and
//! a scan of every segment without one. The decision's slow path and the deliverer call it through [`LocalFacts`];
//! a later read path (roadmap 9.0a) builds on the same primitive.
//!
//! An invalid region the store's start already passed over is lost (spec §3 Recovery rule 2): its positions are
//! out of the log, as they are out of the state the start built. Any other damage, and a segment that cannot be read,
//! is never taken as absence: the keys it may hold are reported unreadable (Q-750).

use std::collections::{BTreeMap, HashMap, HashSet};

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

/// The invalid regions a reader passed over, by segment: their positions are lost (spec §3 Recovery rule 2). A region
/// met later is lost only if it lies inside one of them, so damage that lengthens one is never read as absence.
#[derive(Clone, Debug, Default)]
pub(crate) struct Lost(BTreeMap<u64, Vec<(u64, u64)>>);

impl Lost {
    pub(crate) fn insert(&mut self, file_id: u64, start: u64, end: u64) {
        self.0.entry(file_id).or_default().push((start, end));
    }

    /// The regions not recorded as lost by any of `recorded` (the manifest's lost ranges, by segment and bytes).
    pub(crate) fn unrecorded(&self, recorded: &[kvik_log::manifest::LostRange]) -> u64 {
        let mut n = 0;
        for (file_id, rs) in &self.0 {
            for (a, z) in rs {
                let inside = recorded.iter().any(|l| {
                    l.file_id == *file_id
                        && l.offset <= *a
                        && *z <= l.offset.saturating_add(l.length)
                });
                n += u64::from(!inside);
            }
        }
        n
    }

    pub(crate) fn covers(&self, file_id: u64, start: u64, end: u64) -> bool {
        self.0
            .get(&file_id)
            .is_some_and(|rs| rs.iter().any(|(a, z)| *a <= start && end <= *z))
    }
}

/// [`LocalFacts`] on a log reader of this process and the regions its owner passed over as lost.
#[derive(Debug)]
pub(crate) struct OnLog<'a> {
    pub(crate) reader: &'a mut LogReader,
    pub(crate) lost: &'a Lost,
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
        let lost = self.lost;
        let mut out = Found::default();
        match self.reader.lookup(&wanted, &|f, a, z| lost.covers(f, a, z)) {
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
        let lost = self.lost;
        let l = self
            .reader
            .lookup(&wanted, &|f, a, z| lost.covers(f, a, z))
            .ok()?;
        let mut facts = l.facts;
        facts.sort_by_key(|f| f.position);
        l.complete[0].then_some(facts)
    }
}

/// [`LocalFacts`] on a log reader it owns.
#[derive(Debug)]
pub(crate) struct LogFacts {
    reader: LogReader,
    /// Invalid regions the start passed over, by segment and offset.
    lost: Lost,
}

impl LogFacts {
    pub(crate) fn new(reader: LogReader, lost: Lost) -> Self {
        LogFacts { reader, lost }
    }

    fn on(&mut self) -> OnLog<'_> {
        OnLog {
            reader: &mut self.reader,
            lost: &self.lost,
        }
    }
}

impl LocalFacts for LogFacts {
    fn at(&mut self, keys: &[Key]) -> Found {
        self.on().at(keys)
    }

    fn between(&mut self, series: u32, from: i64, to: i64) -> Option<Vec<FoundFact>> {
        self.on().between(series, from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_is_lost_only_inside_one_passed_over() {
        let mut lost = Lost::default();
        lost.insert(3, 64, 400);
        assert!(lost.covers(3, 64, 400));
        assert!(lost.covers(3, 128, 256));
        assert!(!lost.covers(3, 64, 401), "lengthened by damage found later");
        assert!(!lost.covers(3, 32, 400));
        assert!(!lost.covers(4, 64, 400));
    }

    #[test]
    fn a_region_is_recorded_only_inside_a_lost_range_of_its_segment() {
        let range = |file_id, offset, length| kvik_log::manifest::LostRange {
            identity: (0, 1),
            first: (0, 1),
            last: (0, 2),
            file_id,
            offset,
            length,
            cause: kvik_log::manifest::LossCause::Corruption,
            boot_id: [0; 16],
            id_floor: 0,
            time_before: None,
            time_after: None,
            extends: false,
            stamp: None,
        };
        let mut lost = Lost::default();
        lost.insert(3, 64, 400);
        assert_eq!(lost.unrecorded(&[]), 1);
        assert_eq!(lost.unrecorded(&[range(3, 64, 336)]), 0);
        assert_eq!(
            lost.unrecorded(&[range(3, 64, 335)]),
            1,
            "ends short of the region"
        );
        assert_eq!(lost.unrecorded(&[range(3, 72, 400)]), 1, "starts after it");
        assert_eq!(lost.unrecorded(&[range(4, 64, 336)]), 1, "another segment");
    }

    #[test]
    fn a_lookup_that_fails_reports_every_key_unreadable() {
        use std::path::Path;
        use std::sync::Arc;
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
        let mut facts = LogFacts::new(reader, Lost::default());
        fs.remove_file(&Path::new("s").join("log").join(MARK))
            .unwrap();
        let found = facts.at(&[(1, 10), (2, 20)]);
        assert!(found.facts.is_empty());
        assert_eq!(found.unreadable.len(), 2);
        assert!(facts.between(1, 0, 10).is_none());
    }
}
