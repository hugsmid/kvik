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

/// The one invalid region a fresh reader passes over in segment `file_id`, as `(start, end)`.
fn passed_over(fs: &Arc<MemFs>, file_id: u64) -> (u64, u64) {
    let regions: Vec<(u64, u64)> = reader(fs, 91)
        .refresh()
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e {
            ReadEvent::Corruption { file_id: f, region } if f == file_id => {
                Some((region.start, region.end))
            }
            _ => None,
        })
        .collect();
    assert_eq!(regions.len(), 1, "{regions:?}");
    regions[0]
}

#[test]
fn damage_that_lengthens_a_lost_region_is_not_read_as_absence() {
    let (fs, log) = store(300);
    log.shutdown().unwrap();
    let first = names(&fs).into_iter().find(|n| n.sealed).unwrap();
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(first.to_string());
    let bytes = fs.read(&path).unwrap();
    // The first batch is damaged before the start: the region it leaves is lost.
    fs.poke(&path, HEADER_LEN + 40, &[bytes[HEADER_LEN + 40] ^ 0xFF]);
    let region = passed_over(&fs, first.file_id);
    let lost = |id: u64, a: u64, z: u64| id == first.file_id && region.0 <= a && z <= region.1;
    let everything = Wanted {
        series_id: 5,
        from: i64::MIN,
        to: i64::MAX,
    };
    let mut r = reader(&fs, 21);
    let before = r.lookup(&[everything], &lost).unwrap();
    assert_eq!(before.complete, [true]);
    // More damage during the run, further on, until it reaches the batch after the lost region.
    let mut at = HEADER_LEN + 40;
    loop {
        at += 97;
        assert!(at < bytes.len() / 2, "no second batch met");
        fs.poke(&path, at, &[bytes[at] ^ 0xFF]);
        let got = reader(&fs, 22).lookup(&[everything], &lost).unwrap();
        let kept = r.lookup(&[everything], &lost).unwrap();
        if got.facts.len() < before.facts.len() {
            assert_eq!(
                got.complete,
                [false],
                "fresh reader: damage at {at} read as absence"
            );
            assert_eq!(
                kept.complete,
                [false],
                "mapped reader: damage at {at} read as absence"
            );
            break;
        }
    }
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
            let last = u.events.iter().rev().find_map(|e| match e {
                ReadEvent::Batch(b) => Some(b.last),
                _ => None,
            });
            assert_eq!(
                Some(u.state.durable),
                last,
                "a part reports the last position it returned"
            );
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
    let got = r.lookup(&wanted, &|_, _, _| false).unwrap();
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
            &|_, _, _| false,
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
        r.lookup(&[everything], &|_, _, _| false).unwrap().complete,
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
        .lookup(&[key(damaged_t), key(later)], &|_, _, _| false)
        .unwrap();
    assert_eq!(got.complete, vec![false, true]);
    assert_eq!(got.facts.len(), 1);
    // The region is now in the segment's map: a later lookup still reports it.
    let again = r
        .lookup(&[key(damaged_t), key(later)], &|_, _, _| false)
        .unwrap();
    assert_eq!(again.complete, vec![false, true]);
    let region = passed_over(&fs, first.file_id);
    let lost = |id: u64, a: u64, z: u64| id == first.file_id && region.0 <= a && z <= region.1;
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
        let again = found(r.lookup(&wanted, &|_, _, _| false).unwrap());
        let fresh = found(reader(&fs, 15).lookup(&wanted, &|_, _, _| false).unwrap());
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

#[test]
fn a_failed_lease_renewal_leaves_a_lookup_complete() {
    let (fs, log) = store(60);
    log.shutdown().unwrap();
    let clock = Arc::new(SystemClock::default());
    let mut cfg = ReaderConfig::new("s", fs.clone(), clock, Some([2; 16]), [23; 16]);
    cfg.lease_refresh = Duration::from_millis(2);
    let mut r = LogReader::open(cfg).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    fs.fail_nth(crate::fs::FaultOp::Write, "leases", 0, io::ErrorKind::Other);
    let got = r
        .lookup(
            &[Wanted {
                series_id: 3,
                from: 0,
                to: 0,
            }],
            &|_, _, _| false,
        )
        .unwrap();
    assert!(!got.lease_kept);
    assert_eq!(got.complete, [true]);
    assert!(!got.facts.is_empty());
}

#[test]
fn a_lookup_in_small_parts_and_small_caches_finds_what_one_in_large_ones_finds() {
    let (fs, log) = store(300);
    let wanted = [
        Wanted {
            series_id: 3,
            from: 0,
            to: 0,
        },
        Wanted {
            series_id: 9,
            from: 20,
            to: 260,
        },
    ];
    let found = |l: Lookup| -> Vec<(Position, FactRow)> {
        assert_eq!(l.complete, [true, true]);
        l.facts.into_iter().map(|f| (f.position, f.row)).collect()
    };
    let want = found(reader(&fs, 30).lookup(&wanted, &|_, _, _| false).unwrap());
    let mut r = reader(&fs, 31);
    r.caches.limit(600, 256);
    for round in 0..3 {
        let got = found(r.lookup(&wanted, &|_, _, _| false).unwrap());
        assert_eq!(got, want, "round {round}");
        assert!(r.caches.bytes() <= 600, "{}", r.caches.bytes());
        // Trimmed after each segment: never more than the bound and the entry just added.
        let (peak, entry) = r.caches.peak();
        assert!(peak <= 600 + entry, "{peak} with entries up to {entry}");
    }
    // Damage in a small part is still never read as absence.
    log.shutdown().unwrap();
    let first = names(&fs).into_iter().find(|n| n.sealed).unwrap();
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(first.to_string());
    let bytes = fs.read(&path).unwrap();
    fs.poke(&path, HEADER_LEN + 40, &[bytes[HEADER_LEN + 40] ^ 0xFF]);
    let everything = Wanted {
        series_id: 3,
        from: 0,
        to: 0,
    };
    let mut r = reader(&fs, 32);
    r.caches.limit(600, 256);
    assert_eq!(
        r.lookup(&[everything], &|_, _, _| false).unwrap().complete,
        [false]
    );
}

#[test]
fn every_part_size_returns_what_one_refresh_does() {
    let (fs, log) = store(120);
    let whole = reader(&fs, 40).refresh().unwrap().events;
    for budget in 1..=12 {
        let mut r = reader(&fs, 41);
        let mut events = Vec::new();
        loop {
            let (u, more) = r.refresh_some(budget).unwrap();
            events.extend(u.events);
            if !more {
                break;
            }
        }
        assert_eq!(events, whole, "parts of {budget}");
    }
    log.shutdown().unwrap();
}

#[test]
fn a_lookup_takes_nothing_above_the_marks_last_position() {
    let (fs, log) = store(60);
    log.shutdown().unwrap();
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(crate::store::MARK);
    let (_, m) = crate::mark::read_mark(&fs.read(&path).unwrap(), &[1; 16])
        .unwrap()
        .unwrap();
    let behind = crate::mark::MarkSlot {
        counter: m.counter + 1,
        last_sequence: 200,
        ..m
    };
    fs.poke(
        &path,
        0,
        &crate::mark::encode_mark_file([Some(behind), None]),
    );
    let got = reader(&fs, 42)
        .lookup(
            &[Wanted {
                series_id: 3,
                from: i64::MIN,
                to: i64::MAX,
            }],
            &|_, _, _| false,
        )
        .unwrap();
    assert!(!got.facts.is_empty());
    assert!(got.facts.iter().all(|f| f.position.sequence <= 200));
}

#[test]
fn a_missing_mark_segment_leaves_every_range_incomplete() {
    let (fs, log) = store(60);
    log.shutdown().unwrap();
    let open = names(&fs).into_iter().rfind(|n| !n.sealed).unwrap();
    let mut r = reader(&fs, 43);
    r.refresh().unwrap();
    let mark = r.read_mark().unwrap();
    let mark_name = names(&fs)
        .into_iter()
        .find(|n| n.file_id == mark.file_id)
        .unwrap_or(open);
    fs.remove_file(
        &Path::new("s")
            .join(crate::store::LOG_DIR)
            .join(mark_name.to_string()),
    )
    .unwrap();
    let got = r
        .lookup(
            &[Wanted {
                series_id: 3,
                from: 0,
                to: 0,
            }],
            &|_, _, _| false,
        )
        .unwrap();
    assert_eq!(got.complete, [false]);
}

#[test]
fn a_recovery_entry_voids_what_precedes_it_at_every_lookup() {
    let (fs, log) = store(10);
    let before = log.stats().durable.sequence;
    log.submit(vec![fact(4, 1_000, 1), fact(4, 1_001, 1)])
        .unwrap()
        .wait()
        .unwrap();
    // An entry naming `before` as the last kept position voids the two facts just written.
    let r = crate::entries::Recovery {
        after: before,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: crate::entries::MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    };
    log.submit(vec![Item::from(crate::entries::Entry::Recovery(r))])
        .unwrap()
        .wait()
        .unwrap();
    log.shutdown().unwrap();
    let wanted = [Wanted {
        series_id: 4,
        from: 1_000,
        to: 1_001,
    }];
    let mut r = reader(&fs, 44);
    for round in 0..2 {
        let got = r.lookup(&wanted, &|_, _, _| false).unwrap();
        assert_eq!(got.complete, [true]);
        assert!(got.facts.is_empty(), "round {round}: {:?}", got.facts);
    }
}

#[test]
fn a_refresh_and_a_lookup_decode_each_facts_block_once() {
    let (fs, log) = store(60);
    log.shutdown().unwrap();
    let mut r = reader(&fs, 45);
    crate::facts::FACTS_DECODES.with(|c| c.set(0));
    let events = r.refresh().unwrap().events;
    let batches = events
        .iter()
        .filter(|e| matches!(e, ReadEvent::Batch(b) if b.recovery().is_none()))
        .count() as u64;
    assert!(batches >= 60);
    assert_eq!(crate::facts::FACTS_DECODES.with(|c| c.get()), batches);
    let wanted = [Wanted {
        series_id: 3,
        from: 0,
        to: 0,
    }];
    let mut l = reader(&fs, 46);
    crate::facts::FACTS_DECODES.with(|c| c.set(0));
    let got = l.lookup(&wanted, &|_, _, _| false).unwrap();
    assert_eq!(got.complete, [true]);
    let first = crate::facts::FACTS_DECODES.with(|c| c.get());
    assert!(first <= batches, "{first} decodes for {batches} batches");
}

#[test]
fn a_refresh_checksums_whole_batches_only_for_the_last_ones_it_remembers() {
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
        segment_size: 1 << 20,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    };
    let w = LogWriter::open(store, cfg, Some([2; 16]), Duration::ZERO, 0).unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    for k in 0..400 {
        let items: Vec<Item> = (0..20).map(|i| fact(i, k, k as u64)).collect();
        log.submit(items).unwrap().wait().unwrap();
    }
    log.shutdown().unwrap();
    let mut r = reader(&fs, 47);
    crate::crc::BYTES_HASHED.with(|c| c.set(0));
    r.refresh().unwrap();
    let hashed = crate::crc::BYTES_HASHED.with(|c| c.get());
    assert_eq!(r.view.recent.len(), RECENT);
    assert!(r.view.recent.iter().all(|f| f.crc.is_some()));
    let log_bytes: u64 = names(&fs)
        .iter()
        .map(|n| {
            let p = Path::new("s")
                .join(crate::store::LOG_DIR)
                .join(n.to_string());
            crate::scan::nonzero_end(&fs.read(&p).unwrap())
        })
        .sum();
    let remembered: u64 = r.view.recent.iter().map(|f| f.end - f.offset).sum();
    // Every byte once for its block's checksum, each batch's 12-byte block headers a few times, and the
    // remembered batches once more.
    assert!(
        hashed <= log_bytes + 128 * 400 + remembered,
        "{hashed} bytes hashed for {log_bytes} bytes of log and {remembered} remembered"
    );
}

#[test]
fn a_reader_that_released_its_bytes_reads_on_as_one_that_kept_them() {
    let (fs, log) = store(30);
    let mut kept = reader(&fs, 48);
    let mut freed = reader(&fs, 49);
    kept.refresh().unwrap();
    freed.refresh().unwrap();
    assert!(freed.kept_bytes() > 0);
    freed.release_bytes();
    assert_eq!(freed.kept_bytes(), 0);
    for k in 30..40 {
        log.submit((0..20).map(|i| fact(i, k, k as u64)).collect())
            .unwrap()
            .wait()
            .unwrap();
    }
    log.shutdown().unwrap();
    let a = kept.refresh().unwrap();
    let b = freed.refresh().unwrap();
    assert!(!a.events.is_empty());
    assert_eq!(a.events, b.events);
    assert_eq!(a.state.durable, b.state.durable);
}

#[test]
fn a_later_lookup_allocates_only_the_batches_it_reads_and_those_appended() {
    let (fs, log) = store(30);
    // A key of the open segment's last batch.
    let wanted = [Wanted {
        series_id: 7,
        from: 29,
        to: 29,
    }];
    let mut r = reader(&fs, 50);
    let first = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert_eq!(
        (first.complete.as_slice(), first.facts.len()),
        (&[true][..], 1)
    );
    let segment = |r: &LogReader| r.read_mark().unwrap().offset;
    log.submit((0..20).map(|i| fact(i, 30, 30)).collect())
        .unwrap()
        .wait()
        .unwrap();
    lookup::BUFFER_BYTES.with(|c| c.set(0));
    let again = r.lookup(&wanted, &|_, _, _| false).unwrap();
    let allocated = lookup::BUFFER_BYTES.with(|c| c.get());
    assert_eq!(again.facts, first.facts);
    // The batch holding the key and the one appended, not the segment up to the mark.
    let offset = segment(&r);
    assert!(
        allocated < offset,
        "{allocated} bytes for a segment of {offset}"
    );
    lookup::BUFFER_BYTES.with(|c| c.set(0));
    let third = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert_eq!(third.facts, first.facts);
    assert!(lookup::BUFFER_BYTES.with(|c| c.get()) < offset / 2);
    log.shutdown().unwrap();
}

#[test]
fn a_segment_holding_a_recovery_entry_keeps_its_map_and_lookups_stay_exact() {
    let (fs, log) = store(10);
    let before = log.stats().durable.sequence;
    log.submit(vec![
        fact(4, 1_000, 1),
        fact(4, 1_001, 1),
        fact(5, 1_000, 1),
    ])
    .unwrap()
    .wait()
    .unwrap();
    let r = crate::entries::Recovery {
        after: before,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: crate::entries::MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    };
    log.submit(vec![Item::from(crate::entries::Entry::Recovery(r))])
        .unwrap()
        .wait()
        .unwrap();
    // The voided positions taken again, by other values of the same keys.
    log.submit(vec![fact(4, 1_000, 2), fact(4, 1_002, 2)])
        .unwrap()
        .wait()
        .unwrap();
    for k in 11..20 {
        log.submit((0..20).map(|i| fact(i, k, k as u64)).collect())
            .unwrap()
            .wait()
            .unwrap();
    }
    log.shutdown().unwrap();
    let wanted = [
        Wanted {
            series_id: 4,
            from: 1_000,
            to: 1_002,
        },
        Wanted {
            series_id: 5,
            from: 1_000,
            to: 1_000,
        },
    ];
    let values = |l: &Lookup| -> Vec<(u32, i64, Value)> {
        l.facts
            .iter()
            .map(|f| (f.row.series_id, f.row.t, f.row.value.clone()))
            .collect()
    };
    let mut r = reader(&fs, 51);
    crate::facts::FACTS_DECODES.with(|c| c.set(0));
    let first = r.lookup(&wanted, &|_, _, _| false).unwrap();
    let whole = crate::facts::FACTS_DECODES.with(|c| c.get());
    assert_eq!(first.complete, [true, true]);
    assert_eq!(
        values(&first),
        vec![(4, 1_000, Value::F64(2)), (4, 1_002, Value::F64(2))]
    );
    for round in 0..2 {
        crate::facts::FACTS_DECODES.with(|c| c.set(0));
        let again = r.lookup(&wanted, &|_, _, _| false).unwrap();
        let decoded = crate::facts::FACTS_DECODES.with(|c| c.get());
        assert_eq!(again.complete, first.complete);
        assert_eq!(again.facts, first.facts, "round {round}");
        assert!(
            decoded < whole,
            "round {round}: {decoded} decodes, {whole} for a whole read"
        );
    }
    let fresh = reader(&fs, 52).lookup(&wanted, &|_, _, _| false).unwrap();
    assert_eq!(fresh.facts, first.facts);
}
