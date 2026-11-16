use std::path::Path;

use super::*;
use crate::fs::{FsFile, MemFs, OpenMode};

fn file(len: usize) -> Box<dyn FsFile> {
    let fs = MemFs::new();
    let f = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
    f.write_at(&bytes, 0).unwrap();
    f
}

#[test]
fn a_read_up_to_the_mark_writes_only_the_header_and_the_bytes_from_the_resume_point() {
    let f = file(4_096);
    let mut b = vec![0xAA; 1_024];
    read_part(&mut b, &*f, 2_048, 3_072, false).unwrap();
    let mut want = vec![0u8; 4_096];
    f.read_at(&mut want, 0).unwrap();
    assert_eq!(b.len(), 3_072);
    assert_eq!(b[..HEADER_LEN], want[..HEADER_LEN]);
    assert!(b[HEADER_LEN..1_024].iter().all(|&x| x == 0xAA));
    assert!(b[1_024..2_048].iter().all(|&x| x == 0));
    assert_eq!(b[2_048..], want[2_048..3_072]);
}

#[test]
fn a_read_to_the_end_zeroes_the_gap_and_bytes_past_the_end_of_the_file() {
    let f = file(3_000);
    let mut b = vec![0xAA; 4_096];
    read_part(&mut b, &*f, 2_048, 3_072, true).unwrap();
    let mut want = vec![0u8; 3_000];
    f.read_at(&mut want, 0).unwrap();
    assert_eq!(b[..HEADER_LEN], want[..HEADER_LEN]);
    assert!(b[HEADER_LEN..2_048].iter().all(|&x| x == 0));
    assert_eq!(b[2_048..3_000], want[2_048..]);
    assert!(b[3_000..].iter().all(|&x| x == 0));
    let short = file(40);
    let mut b = vec![0xAA; 128];
    read_part(&mut b, &*short, HEADER_LEN as u64, 128, false).unwrap();
    assert!(b[40..].iter().all(|&x| x == 0));
}

#[test]
fn the_kept_bytes_are_those_of_the_followed_segment_and_a_whole_file_read_is_freed() {
    use crate::facts::{FactFlags, FactRow, Quality, Value};
    use crate::store::{NewStore, StoreDir};
    use crate::writer::{Log, LogWriter, SystemClock, WriterConfig};

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
    let clock = Arc::new(SystemClock::default());
    let mut r = LogReader::open(ReaderConfig::new(
        "s",
        fs.clone(),
        clock,
        Some([2; 16]),
        [7; 16],
    ))
    .unwrap();
    let facts = |k: i64| -> Vec<Item> {
        (0..20)
            .map(|i| {
                Item::Fact(FactRow {
                    series_id: i,
                    t: k * 100 + i64::from(i),
                    quality: Quality::Good,
                    flags: FactFlags::default(),
                    value: Value::F64(k as u64),
                    changed_at: None,
                })
            })
            .collect()
    };
    let mut k = 0;
    let mut followed = false;
    while log.stats().open_file_id != Some(2) {
        log.submit(facts(k)).unwrap().wait().unwrap();
        k += 1;
        if log.stats().open_file_id == Some(1) && k == 5 {
            r.refresh().unwrap();
            assert_eq!(r.scratch.file_id, Some(1));
            assert_eq!(r.scratch.bytes.len() as u64, r.read_mark().unwrap().offset);
            followed = true;
        }
    }
    assert!(followed);
    log.submit(facts(k)).unwrap().wait().unwrap();
    r.refresh().unwrap();
    assert_eq!(r.scratch.file_id, Some(2));
    assert!(
        r.scratch.bytes.capacity() < 4 << 10,
        "the first segment's bytes are freed: {} kept",
        r.scratch.bytes.capacity()
    );
    log.shutdown().unwrap();
    // A mark behind the first segment's last batch: the pass ends inside that whole-file read, and still frees it.
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(crate::store::MARK);
    let (_, m) = crate::mark::read_mark(&fs.read(&path).unwrap(), &[1; 16])
        .unwrap()
        .unwrap();
    let behind = crate::mark::MarkSlot {
        counter: m.counter + 1,
        last_sequence: 20,
        ..m
    };
    fs.poke(
        &path,
        0,
        &crate::mark::encode_mark_file([Some(behind), None]),
    );
    let clock = Arc::new(SystemClock::default());
    let mut r = LogReader::open(ReaderConfig::new("s", fs, clock, Some([2; 16]), [8; 16])).unwrap();
    let u = r.refresh().unwrap();
    assert_eq!(u.state.durable.sequence, 20);
    assert_eq!((r.scratch.file_id, r.scratch.bytes.capacity()), (None, 0));
}

mod parts_and_lookup {
    use std::collections::BTreeMap;

    use super::*;
    use crate::facts::{FactFlags, FactRow, Quality, Value};
    use crate::store::{NewStore, StoreDir};
    use crate::writer::{Log, LogWriter, SystemClock, WriterConfig};

    /// A store of small segments holding `calls` calls of 20 facts, series 0..20 at `t = call`, and every fifth
    /// call a revision of series 3 at `t = 0`, once every segment before the open one is sealed.
    fn store(calls: i64) -> (Arc<MemFs>, Log) {
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
        for k in 0..calls {
            let mut items: Vec<Item> = (0..20).map(|i| fact(i, k, k as u64)).collect();
            if k % 5 == 4 {
                items.push(fact(3, 0, 1_000 + k as u64));
            }
            log.submit(items).unwrap().wait().unwrap();
        }
        let open = log.stats().open_file_id.unwrap();
        for _ in 0..2_000 {
            if names(&fs).iter().filter(|n| n.sealed).count() as u64 >= open - 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            names(&fs).iter().filter(|n| n.sealed).count() as u64,
            open - 1
        );
        (fs, log)
    }

    fn fact(series: u32, t: i64, v: u64) -> Item {
        Item::Fact(FactRow {
            series_id: series,
            t,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: Value::F64(v),
            changed_at: None,
        })
    }

    fn names(fs: &MemFs) -> Vec<crate::name::SegmentName> {
        let dir = Path::new("s").join(crate::store::LOG_DIR);
        let mut v: Vec<_> = fs
            .read_dir(&dir)
            .unwrap()
            .into_iter()
            .filter_map(|e| crate::name::SegmentName::parse(&e.name))
            .collect();
        v.sort_by_key(|n| n.file_id);
        v
    }

    fn reader(fs: &Arc<MemFs>, id: u8) -> LogReader {
        let clock = Arc::new(SystemClock::default());
        LogReader::open(ReaderConfig::new(
            "s",
            fs.clone(),
            clock,
            Some([2; 16]),
            [id; 16],
        ))
        .unwrap()
    }

    /// Every fact of the log by position, from one plain refresh.
    fn all_facts(fs: &Arc<MemFs>) -> BTreeMap<Position, FactRow> {
        let mut out = BTreeMap::new();
        for e in reader(fs, 90).refresh().unwrap().events {
            if let ReadEvent::Batch(b) = e {
                let mut s = b.first.sequence;
                for item in b.content.items {
                    if let Item::Fact(r) = &item {
                        out.insert(
                            Position {
                                generation: b.first.generation,
                                sequence: s,
                            },
                            r.clone(),
                        );
                    }
                    s += item.sequences();
                }
            }
        }
        out
    }

    #[test]
    fn a_bounded_refresh_returns_what_one_refresh_does_in_parts_and_frees_the_kept_file() {
        let (fs, log) = store(300);
        let whole = reader(&fs, 10).refresh().unwrap();
        let mut r = reader(&fs, 11);
        let mut events = Vec::new();
        let mut parts = 0;
        let mut kept = false;
        loop {
            let (u, more) = r.refresh_some(7).unwrap();
            parts += 1;
            kept |= r.scratch.whole.is_some();
            let items: usize = u
                .events
                .iter()
                .map(|e| match e {
                    ReadEvent::Batch(b) => b.content.items.len(),
                    _ => 0,
                })
                .sum();
            if more {
                assert!(items >= 7);
                assert!(u.state.durable <= whole.state.durable);
            }
            events.extend(u.events);
            if !more {
                assert_eq!(u.state.durable, whole.state.durable);
                break;
            }
        }
        assert!(parts > 10, "{parts} parts");
        assert!(kept, "an earlier segment's bytes were kept between parts");
        assert_eq!(events, whole.events);
        assert_eq!(r.scratch.whole, None);
        assert!(r.refresh().unwrap().events.is_empty());
        log.shutdown().unwrap();
    }

    #[test]
    fn a_lookup_finds_every_fact_of_a_key_and_reads_only_the_segments_whose_index_may_hold_it() {
        let (fs, log) = store(300);
        let all = all_facts(&fs);
        let mut r = reader(&fs, 12);
        let wanted = [
            Wanted {
                series_id: 3,
                from: 0,
                to: 0,
            },
            Wanted {
                series_id: 7,
                from: 10,
                to: 12,
            },
            Wanted {
                series_id: 99,
                from: 0,
                to: 100,
            },
        ];
        let got = r.lookup(&wanted, &|_, _| false).unwrap();
        assert_eq!(got.complete, vec![true; 3]);
        let want: Vec<(Position, FactRow)> = all
            .iter()
            .filter(|(_, f)| {
                (f.series_id == 3 && f.t == 0) || (f.series_id == 7 && (10..=12).contains(&f.t))
            })
            .map(|(p, f)| (*p, f.clone()))
            .collect();
        let found: Vec<(Position, FactRow)> = got
            .facts
            .iter()
            .map(|f| (f.position, f.row.clone()))
            .collect();
        assert_eq!(found, want);
        assert_eq!(found.iter().filter(|(_, f)| f.series_id == 3).count(), 61);
        let segments = names(&fs).len() as u64;
        let narrow = r
            .lookup(
                &[Wanted {
                    series_id: 7,
                    from: 11,
                    to: 11,
                }],
                &|_, _| false,
            )
            .unwrap();
        assert_eq!(narrow.facts.len(), 1);
        assert!(
            narrow.segments_read < segments / 2,
            "{} of {segments}",
            narrow.segments_read
        );
        log.shutdown().unwrap();
    }

    #[test]
    fn a_lookup_never_reads_damage_as_absence_unless_the_region_is_lost() {
        let (fs, log) = store(300);
        log.shutdown().unwrap();
        let all = all_facts(&fs);
        let first = names(&fs).into_iter().find(|n| n.sealed).unwrap();
        let path = Path::new("s")
            .join(crate::store::LOG_DIR)
            .join(first.to_string());
        let mut r = reader(&fs, 13);
        let everything = Wanted {
            series_id: 5,
            from: i64::MIN,
            to: i64::MAX,
        };
        // A first lookup maps every segment; the damage comes after, in a batch the map holds.
        assert_eq!(
            r.lookup(&[everything], &|_, _| false).unwrap().complete,
            [true]
        );
        let bytes = fs.read(&path).unwrap();
        // A byte inside the first batch's data: the region starts at the header's end.
        fs.poke(&path, HEADER_LEN + 40, &[bytes[HEADER_LEN + 40] ^ 0xFF]);
        let damaged_t = all.values().find(|f| f.series_id == 5).unwrap().t;
        let later = all
            .values()
            .filter(|f| f.series_id == 5)
            .map(|f| f.t)
            .max()
            .unwrap();
        let key = |t| Wanted {
            series_id: 5,
            from: t,
            to: t,
        };
        let got = r
            .lookup(&[key(damaged_t), key(later)], &|_, _| false)
            .unwrap();
        assert_eq!(got.complete, vec![false, true]);
        assert_eq!(got.facts.len(), 1);
        let lost = |id: u64, start: u64| id == first.file_id && start == HEADER_LEN as u64;
        let got = r.lookup(&[key(damaged_t), key(later)], &lost).unwrap();
        assert_eq!(got.complete, vec![true, true]);
        assert_eq!(got.facts.len(), 1);
    }

    #[test]
    fn a_lookup_after_appends_finds_what_a_first_lookup_finds() {
        let (fs, log) = store(150);
        let mut r = reader(&fs, 14);
        let wanted = [
            Wanted {
                series_id: 3,
                from: 0,
                to: 0,
            },
            Wanted {
                series_id: 8,
                from: 100,
                to: i64::MAX,
            },
        ];
        let found = |l: Lookup| -> Vec<(Position, FactRow)> {
            assert_eq!(l.complete, [true, true]);
            l.facts.into_iter().map(|f| (f.position, f.row)).collect()
        };
        let mut seen = 0;
        for round in 0..6 {
            let again = found(r.lookup(&wanted, &|_, _| false).unwrap());
            let fresh = found(reader(&fs, 15).lookup(&wanted, &|_, _| false).unwrap());
            assert_eq!(again, fresh, "round {round}");
            assert!(again.len() > seen);
            seen = again.len();
            // Small appends, which stay in the segment the first lookup mapped.
            let items: Vec<Item> = (0..20).map(|i| fact(i, 150 + round, 1)).collect();
            log.submit(items).unwrap().wait().unwrap();
            log.submit(vec![fact(3, 0, 7_000 + round as u64)])
                .unwrap()
                .wait()
                .unwrap();
        }
        log.shutdown().unwrap();
    }
}
