use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;
use crate::entries::{Entry, MarkState, Recovery};
use crate::facts::{FactFlags, FactRow, Quality, Value};
use crate::name::SegmentName;
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

fn recovery(after: u64) -> Item {
    Item::from(Entry::Recovery(Recovery {
        after,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    }))
}

/// What the damage step did to a sealed segment.
enum Damage {
    None,
    /// A byte of a batch without a recovery entry.
    Other,
    /// A byte of a recovery entry's batch, with every fact a refresh took before it.
    Entry(BTreeSet<Seen>),
    /// A header field that refuses the segment, with every fact a refresh took before it outside the segment.
    Refused(BTreeSet<Seen>),
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

    fn path(&self, name: SegmentName) -> std::path::PathBuf {
        Path::new("s")
            .join(crate::store::LOG_DIR)
            .join(name.to_string())
    }

    /// The sealed segments, by file id.
    fn sealed(&self) -> Vec<SegmentName> {
        let dir = Path::new("s").join(crate::store::LOG_DIR);
        let mut names: Vec<_> = self
            .fs
            .read_dir(&dir)
            .unwrap()
            .into_iter()
            .filter_map(|e| SegmentName::parse(&e.name))
            .filter(|n| n.sealed)
            .collect();
        names.sort_by_key(|n| n.file_id);
        names
    }

    /// The first and last sequence of the segment's batches.
    fn sequences(&self, name: SegmentName) -> (u64, u64) {
        let bytes = self.fs.read(&self.path(name)).unwrap();
        let id = SegmentIdentity {
            store_id: [1; 16],
            file_id: name.file_id,
            generation: name.generation,
        };
        let batches: Vec<_> = Scanner::new(&bytes, &id, ScanLimit::End)
            .unwrap()
            .filter_map(|f| match f {
                Ok(Found::Batch(b)) => Some((b.commit.first_sequence, b.commit.last_sequence)),
                _ => None,
            })
            .collect();
        (batches[0].0, batches[batches.len() - 1].1)
    }

    /// The start and end offsets of the segment's batches that hold a recovery entry.
    fn entry_batches(&self, name: SegmentName) -> Vec<(usize, usize)> {
        let bytes = self.fs.read(&self.path(name)).unwrap();
        let id = SegmentIdentity {
            store_id: [1; 16],
            file_id: name.file_id,
            generation: name.generation,
        };
        let mut scan = Scanner::new(&bytes, &id, ScanLimit::End)
            .unwrap()
            .keeping_content();
        let mut out = Vec::new();
        while let Some(found) = scan.next() {
            if let Ok(Found::Batch(b)) = found
                && scan.take_content().is_some_and(|c| {
                    c.items
                        .iter()
                        .any(|i| matches!(i, Item::Entry(e) if matches!(**e, Entry::Recovery(_))))
                })
            {
                out.push((b.offset as usize, b.end() as usize));
            }
        }
        out
    }

    /// Every fact a fresh refresh takes, voids applied.
    fn refreshed(&mut self) -> BTreeSet<Seen> {
        self.try_refreshed().unwrap()
    }

    /// [`World::refreshed`]; `None` if a segment is refused.
    fn try_refreshed(&mut self) -> Option<BTreeSet<Seen>> {
        let mut out = BTreeSet::new();
        for e in self.reader().refresh().ok()?.events {
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
        Some(out)
    }
}

/// Checks a lookup of the reader under test against a fresh reader's first lookup and, where complete, a refresh.
/// After damage the reader under test may know a range complete that a fresh reader does not: a damaged batch its
/// map shows cannot hold a wanted key is not read again (only complete ranges are compared). A recovery entry it
/// mapped before its batch was damaged keeps voiding for it, so it answers what a refresh gave before (Q-831).
fn check(
    w: &mut World,
    a: &mut LogReader,
    wanted: &[Wanted],
    damage: &Damage,
) -> Result<(), TestCaseError> {
    let got = a.lookup(wanted, &|_, _, _| false).unwrap();
    let fresh = w.reader().lookup(wanted, &|_, _, _| false).unwrap();
    for (mine, theirs) in got.complete.iter().zip(&fresh.complete) {
        if matches!(damage, Damage::None) {
            prop_assert_eq!(mine, theirs);
        } else {
            prop_assert!(*mine || !*theirs);
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
    let refreshed = match damage {
        Damage::Entry(before) | Damage::Refused(before) => before.clone(),
        _ => w.refreshed(),
    };
    // A range left incomplete may lack facts, but holds none a refresh does not take.
    prop_assert!(got.facts.iter().all(|f| refreshed.contains(&seen(f))));
    let want: BTreeSet<Seen> = refreshed
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
        damage in prop::option::of((any::<bool>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())),
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
                    // A recovery entry voids only what precedes it in its own segment (format A.4).
                    if w.submit(vec![recovery(after)]) && after < durable {
                        return Ok(());
                    }
                    // Past the voided positions, so that no batch the entry voids lies above the mark.
                    while w.durable() <= durable + 1 {
                        w.call(20);
                    }
                }
                Op::Lookup => check(&mut w, &mut a, &wanted, &Damage::None)?,
            }
        }
        check(&mut w, &mut a, &wanted, &Damage::None)?;
        if let Some((header, seg, at)) = damage {
            let names = w.sealed();
            if !names.is_empty() {
                let name = *seg.get(&names);
                let path = w.path(name);
                let len = w.fs.read(&path).unwrap().len();
                if len > 2 * HEADER_LEN && header {
                    // Under a valid checksum, one of the fields a scan checks against the segment's name and store
                    // (kind, generation, file id, store id): the segment is refused. A header that fails its
                    // checksum is not.
                    let fields: Vec<usize> = std::iter::once(4).chain(12..24).chain(40..56).collect();
                    let mut h = w.fs.read(&path).unwrap()[..HEADER_LEN].to_vec();
                    h[*at.get(&fields)] ^= 0x5A;
                    let crc = crc32c(&h[..HEADER_LEN - 4]);
                    h[HEADER_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
                    let before = w.refreshed();
                    let (first, last) = w.sequences(name);
                    w.fs.poke(&path, 0, &h);
                    prop_assert!(w.try_refreshed().is_none());
                    let outside = before
                        .into_iter()
                        .filter(|f| !(first..=last).contains(&f.0.sequence))
                        .collect();
                    check(&mut w, &mut a, &wanted, &Damage::Refused(outside))?;
                } else if len > 2 * HEADER_LEN {
                    let off = HEADER_LEN + at.index(len / 2 - HEADER_LEN);
                    let in_entry = w
                        .entry_batches(name)
                        .iter()
                        .any(|&(start, end)| start <= off && off < end);
                    let damage = if in_entry {
                        Damage::Entry(w.refreshed())
                    } else {
                        Damage::Other
                    };
                    let b = w.fs.read(&path).unwrap()[off];
                    w.fs.poke(&path, off, &[b ^ 0x5A]);
                    check(&mut w, &mut a, &wanted, &damage)?;
                }
            }
        }
        w.log.shutdown().unwrap();
    }
}

#[test]
fn a_recovery_entry_mapped_before_its_batch_was_damaged_keeps_voiding_for_that_reader() {
    let mut w = World::new();
    (0..3).for_each(|_| w.call(10));
    let durable = w.durable();
    let after = durable - 5;
    w.submit(vec![recovery(after)]);
    let file_id = w.log.stats().open_file_id.unwrap();
    while !w.sealed().iter().any(|n| n.file_id == file_id) {
        w.call(30);
    }
    let name = *w.sealed().iter().find(|n| n.file_id == file_id).unwrap();
    let voided = |f: &Seen| after < f.0.sequence && f.0.sequence <= durable;
    let wanted: Vec<Wanted> = (0..4)
        .map(|series_id| Wanted {
            series_id,
            from: 0,
            to: 49,
        })
        .collect();
    let mut a = w.reader();
    let first = a.lookup(&wanted, &|_, _, _| false).unwrap();
    let taken: BTreeSet<Seen> = first.facts.iter().map(seen).collect();
    assert!(first.complete.iter().all(|c| *c));
    assert!(!taken.is_empty() && !taken.iter().any(voided));
    assert_eq!(taken, w.refreshed());

    let [(start, end)] = w.entry_batches(name)[..] else {
        panic!("one recovery entry");
    };
    let path = w.path(name);
    let off = (start + end) / 2;
    let b = w.fs.read(&path).unwrap()[off];
    w.fs.poke(&path, off, &[b ^ 0x5A]);

    let again = a.lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(again.complete.iter().all(|c| *c));
    assert_eq!(again.facts.iter().map(seen).collect::<BTreeSet<_>>(), taken);
    let fresh = w.reader().lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(fresh.complete.iter().all(|c| !*c));
    // Q-836: a refresh reports the entry's batch as a region, applies no void and takes what it voided as live.
    let events = w.reader().refresh().unwrap().events;
    assert!(events.iter().any(|e| matches!(
        e,
        ReadEvent::Corruption { file_id: f, region } if *f == file_id
            && region.start as usize <= start && end <= region.end as usize
    )));
    assert!(!events.iter().any(|e| matches!(e, ReadEvent::Void { .. })));
    assert!(w.refreshed().iter().any(voided));
}
