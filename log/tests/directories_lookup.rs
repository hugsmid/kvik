//! The derived files (`format.md` A.18) used: a lookup located through them equals a full scan, and the positions
//! they name are sources of F.

mod dirs_rig;

use dirs_rig::*;

#[test]
fn a_lookup_located_through_the_directories_equals_a_full_scan() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 500, 0);
    r.settle();
    // Seals not yet listed and an open segment: the reader covers both from the log.
    let d = r.dirs.take();
    ingest(&mut r, 40, 90_000_000);
    r.dirs = d;
    let mark = read_mark(
        &r.fs.read(&dir().join("log").join("MARK")).unwrap(),
        &STORE_ID,
    )
    .unwrap()
    .unwrap()
    .1;
    let all = batches(&r.fs);
    let full = |s: u32| -> Vec<(u64, i64)> {
        all.iter()
            .filter(|b| b.1 <= mark.last_sequence)
            .flat_map(|b| {
                facts_of(&b.2)
                    .filter(|f| f.1.series_id == s)
                    .map(|(p, f)| (p, f.t))
            })
            .collect()
    };
    let dirs = r.load().unwrap();
    let series = r.series().unwrap().1;
    let listed: BTreeMap<u64, &SegmentRecord> = dirs.segments().map(|s| (s.file_id, s)).collect();
    let cat = r.catalogue();
    let uncovered: Vec<u64> = cat
        .segments
        .keys()
        .filter(|id| !listed.contains_key(id))
        .copied()
        .collect();
    let uncovered_bytes: u64 = uncovered
        .iter()
        .map(|id| cat.segments[id].seal.unwrap().0)
        .sum();
    assert!(!uncovered.is_empty() && uncovered_bytes <= READER_ALLOWANCE);
    let read = |file_id: u64, sealed: bool| -> (Option<Vec<u32>>, Batches) {
        let name = SegmentName {
            generation: 0,
            file_id,
            sealed,
        };
        let bytes =
            r.fs.read(&dir().join("log").join(name.to_string()))
                .unwrap();
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id,
            generation: 0,
        };
        let limit = if sealed {
            ScanLimit::End
        } else {
            ScanLimit::Mark(mark.offset)
        };
        let rep = scan(&bytes, &id, limit).unwrap();
        let index = rep
            .seal
            .and_then(|s| s.index.flatten())
            .map(|i| i.records.iter().map(|x| x.series_id).collect());
        let b = rep
            .batches
            .iter()
            .map(|b| {
                (
                    b.commit.first_sequence,
                    b.commit.last_sequence,
                    BatchContent::decode(b).unwrap(),
                )
            })
            .collect();
        (index, b)
    };
    for s in 1..=6 {
        let mut found = Vec::new();
        let mut take = |b: Batches| {
            for (_, _, c) in &b {
                found.extend(
                    facts_of(c)
                        .filter(|f| f.1.series_id == s)
                        .map(|(p, f)| (p, f.t)),
                );
            }
        };
        for rec in listed.values() {
            let (index, b) = read(rec.file_id, true);
            // A segment whose index does not list the series is passed over (A.8).
            if index.is_some_and(|i| i.contains(&s)) {
                take(b);
            }
        }
        for id in &uncovered {
            take(read(*id, true).1);
        }
        take(read(mark.file_id, false).1);
        found.sort();
        assert_eq!(found, full(s), "series {s}");
        // The series directory's count and latest `t`, with the facts above its `through`, are the full scan's.
        let e = series.entries.iter().find(|e| e.series_id == s);
        let above: Vec<_> = full(s)
            .into_iter()
            .filter(|(p, _)| *p > series.through.sequence)
            .collect();
        let count = e.map_or(0, |e| e.fact_count) + above.len() as u64;
        assert_eq!(count, full(s).len() as u64, "series {s}");
        let latest = e
            .and_then(|e| e.latest_t)
            .into_iter()
            .chain(above.iter().map(|f| f.1))
            .max();
        assert_eq!(latest, full(s).iter().map(|f| f.1).max(), "series {s}");
    }
}

#[test]
fn a_position_a_derived_file_names_raises_f() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 20, 0);
    r.settle();
    let mark = r.durable_mark();
    let d = SeriesDirectory {
        through: Position {
            generation: 0,
            sequence: mark + 40,
        },
        lost: 0,
        stamp: None,
        judged_at: T0,
        entries: Vec::new(),
    };
    let frame = Frame {
        generation: 0,
        file_id: 99,
        written_at: T0,
    };
    directory::write_file(
        &r.fs,
        &dir(),
        Family::Series,
        99,
        &d.encode(frame, STORE_ID).unwrap(),
    )
    .unwrap();
    r.fs.remove_file(&dir().join("log").join("MARK")).unwrap();
    r.dirs = None;
    r.w = None;
    r.open();
    let e = r.w.as_ref().unwrap().start_report().entry.clone().unwrap();
    assert_eq!(e.sequence, mark + 41, "F above the derived file's position");
}
