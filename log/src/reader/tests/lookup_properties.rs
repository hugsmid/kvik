use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;
use crate::entries::{Entry, MarkState, Recovery};
use crate::facts::{FactFlags, FactRow, Quality, Value};
use crate::store::{NewStore, StoreDir};
use crate::writer::{Log, LogWriter, SystemClock, WriterConfig};

#[derive(Clone, Debug)]
enum Op {
    /// Calls, each of this many facts.
    Calls(u8, u8),
    /// A recovery entry voiding up to this many positions of the segment in use, then calls past what it voided.
    Recovery(u8),
    /// The reader under test looks up.
    Lookup,
}

/// A fact as compared: position, series, `t` and the value each fact of these logs carries.
type Seen = (Position, u32, i64, u64);

fn bits(v: &Value) -> u64 {
    match v {
        Value::F64(b) => *b,
        _ => u64::MAX,
    }
}

fn seen(f: &FoundFact) -> Seen {
    (f.position, f.row.series_id, f.row.t, bits(&f.row.value))
}

struct World {
    fs: Arc<MemFs>,
    log: Log,
    next: u64,
    /// The first position of the segment in use, and that segment's file id.
    segment: (u64, Option<u64>),
    reader_id: u8,
}

impl World {
    fn new() -> World {
        let fs = Arc::new(MemFs::new());
        let new = NewStore {
            store_id: [1; 16],
            created_at: 0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "test".into(),
        };
        let store = StoreDir::open(fs.clone(), Path::new("s"), Some(&new)).unwrap();
        let cfg = WriterConfig {
            segment_size: 16 << 10,
            max_batch_bytes: 4 << 10,
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            ..WriterConfig::default()
        };
        let w = LogWriter::open(store, cfg, Some([2; 16]), Duration::ZERO, 0).unwrap();
        let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
        World {
            fs,
            log,
            next: 0,
            segment: (1, None),
            reader_id: 0,
        }
    }

    fn durable(&self) -> u64 {
        self.log.stats().durable.sequence
    }

    /// Submits `items`; returns whether they started a new segment.
    fn submit(&mut self, items: Vec<Item>) -> bool {
        let before = self.durable();
        self.log.submit(items).unwrap().wait().unwrap();
        let open = self.log.stats().open_file_id;
        let rolled = open != self.segment.1;
        if rolled {
            self.segment = (before + 1, open);
        }
        rolled
    }

    fn call(&mut self, facts: u8) {
        let items = (0..facts)
            .map(|_| {
                self.next += 1;
                let k = self.next;
                Item::Fact(FactRow {
                    series_id: (k % 4) as u32,
                    t: (k % 50) as i64,
                    quality: Quality::Good,
                    flags: FactFlags::default(),
                    value: Value::F64(k),
                    changed_at: None,
                })
            })
            .collect();
        self.submit(items);
    }

    fn reader(&mut self) -> LogReader {
        self.reader_id += 1;
        let clock = Arc::new(SystemClock::default());
        LogReader::open(ReaderConfig::new(
            "s",
            self.fs.clone(),
            clock,
            Some([2; 16]),
            [self.reader_id; 16],
        ))
        .unwrap()
    }

    /// Every fact a fresh refresh takes, voids applied.
    fn refreshed(&mut self) -> BTreeSet<Seen> {
        let mut out = BTreeSet::new();
        for e in self.reader().refresh().unwrap().events {
            match e {
                ReadEvent::Batch(b) => {
                    let mut s = b.first.sequence;
                    for item in &b.content.items {
                        if let Item::Fact(r) = item {
                            let p = Position {
                                generation: b.first.generation,
                                sequence: s,
                            };
                            out.insert((p, r.series_id, r.t, bits(&r.value)));
                        }
                        s += item.sequences();
                    }
                }
                ReadEvent::Void { above, .. } => out.retain(|f| f.0 <= above),
                ReadEvent::Corruption { .. } => {}
            }
        }
        out
    }
}

/// Checks a lookup of the reader under test against a fresh reader's first lookup and, where complete, a refresh.
/// With `damaged`, the reader under test may know a range complete that a fresh reader does not: a damaged batch its
/// map shows cannot hold a wanted key is not read again (only complete ranges are compared).
fn check(
    w: &mut World,
    a: &mut LogReader,
    wanted: &[Wanted],
    damaged: bool,
) -> Result<(), TestCaseError> {
    let got = a.lookup(wanted, &|_, _, _| false).unwrap();
    let fresh = w.reader().lookup(wanted, &|_, _, _| false).unwrap();
    for (mine, theirs) in got.complete.iter().zip(&fresh.complete) {
        if damaged {
            prop_assert!(*mine || !*theirs);
        } else {
            prop_assert_eq!(mine, theirs);
        }
    }
    let in_range = |f: &Seen, keep: &[bool]| {
        wanted
            .iter()
            .zip(keep)
            .any(|(r, k)| *k && r.series_id == f.1 && r.from <= f.2 && f.2 <= r.to)
    };
    let both: Vec<bool> = got
        .complete
        .iter()
        .zip(&fresh.complete)
        .map(|(a, b)| *a && *b)
        .collect();
    let mine: BTreeSet<Seen> = got
        .facts
        .iter()
        .map(seen)
        .filter(|f| in_range(f, &both))
        .collect();
    let theirs: BTreeSet<Seen> = fresh
        .facts
        .iter()
        .map(seen)
        .filter(|f| in_range(f, &both))
        .collect();
    prop_assert_eq!(&mine, &theirs);
    let mine: BTreeSet<Seen> = got
        .facts
        .iter()
        .map(seen)
        .filter(|f| in_range(f, &got.complete))
        .collect();
    let want: BTreeSet<Seen> = w
        .refreshed()
        .into_iter()
        .filter(|f| in_range(f, &got.complete))
        .collect();
    prop_assert_eq!(mine, want);
    Ok(())
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            4 => (1u8..8, 1u8..40).prop_map(|(c, f)| Op::Calls(c, f)),
            1 => (0u8..60).prop_map(Op::Recovery),
            2 => Just(Op::Lookup),
        ],
        1..16,
    )
}

proptest! {
    #[test]
    fn a_reader_looking_up_again_finds_what_a_fresh_reader_and_a_refresh_find(
        ops in ops(),
        wanted in prop::collection::vec((0u32..4, 0i64..50, 0i64..10), 1..4),
        damage in prop::option::of((any::<prop::sample::Index>(), any::<prop::sample::Index>())),
    ) {
        let wanted: Vec<Wanted> = wanted
            .into_iter()
            .map(|(series_id, from, len)| Wanted { series_id, from, to: from + len })
            .collect();
        let mut w = World::new();
        let mut a = w.reader();
        for op in ops {
            match op {
                Op::Calls(n, f) => (0..n).for_each(|_| w.call(f)),
                Op::Recovery(back) => {
                    let durable = w.durable();
                    let after = durable.saturating_sub(u64::from(back)).max(w.segment.0 - 1);
                    let entry = Recovery {
                        after,
                        stamp: None,
                        boot_id: [0; 16],
                        tail_bytes: 0,
                        readable_commits: 0,
                        mark_state: MarkState::Readable,
                        id_floor: 0,
                        consumers: Vec::new(),
                    };
                    // A recovery entry voids only what precedes it in its own segment (format A.4).
                    if w.submit(vec![Item::from(Entry::Recovery(entry))]) && after < durable {
                        return Ok(());
                    }
                    // Past the voided positions, so that no batch the entry voids lies above the mark.
                    while w.durable() <= durable + 1 {
                        w.call(20);
                    }
                }
                Op::Lookup => check(&mut w, &mut a, &wanted, false)?,
            }
        }
        check(&mut w, &mut a, &wanted, false)?;
        if let Some((seg, at)) = damage {
            let dir = Path::new("s").join(crate::store::LOG_DIR);
            let mut names: Vec<_> = w
                .fs
                .read_dir(&dir)
                .unwrap()
                .into_iter()
                .filter_map(|e| crate::name::SegmentName::parse(&e.name))
                .filter(|n| n.sealed)
                .collect();
            names.sort_by_key(|n| n.file_id);
            if !names.is_empty() {
                let path = dir.join(seg.get(&names).to_string());
                let len = w.fs.read(&path).unwrap().len();
                if len > 2 * HEADER_LEN {
                    let off = HEADER_LEN + at.index(len / 2 - HEADER_LEN);
                    let b = w.fs.read(&path).unwrap()[off];
                    w.fs.poke(&path, off, &[b ^ 0x5A]);
                    check(&mut w, &mut a, &wanted, true)?;
                }
            }
        }
        w.log.shutdown().unwrap();
    }
}
