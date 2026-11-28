//! A start in a claimed generation (roadmap 8.3c, `spec.md` §8 Generations, Restore step 1): a directory holding the
//! sealed segments of generation 0, as a restore leaves it, started in generation 1.

use std::sync::Arc;

use kvik_log::batch::SegmentIdentity;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::manifest::{LossCause, LostRange, RECORD_LOST_RANGE, encode_record};
use kvik_log::mark::ResetStamp;
use kvik_log::name::SegmentName;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::setaside::SetAside;
use kvik_log::store::{StoreDir, StoreError};
use kvik_log::writer::{
    GenerationStart, LossKind, OpenError, Position, StartRule, SystemClock, WriterConfig,
    create_segment, init_generation,
};

mod first_start;
mod rig;

use rig::*;

#[test]
fn init_generation_writes_the_first_mark_once_and_refuses_another_generations() {
    let (r, end) = restored(cfg());
    let m = r.mark();
    assert_eq!(
        (m.generation, m.stamp, m.file_id, m.offset, m.last_sequence),
        (1, stamp(), 0, 0, 0)
    );
    let store = StoreDir::open(Arc::new(r.fs.clone()), &dir(), Some(&new_store())).unwrap();
    let before = r.fs.read(&log_path("MARK"));
    init_generation(&store, &generation_start(end), T0).unwrap();
    assert_eq!(r.fs.read(&log_path("MARK")), before);
    let restamped = GenerationStart {
        stamp: ResetStamp {
            sequence: 1,
            nonce: 78,
        },
        ..generation_start(end)
    };
    init_generation(&store, &restamped, T0).unwrap();
    assert_eq!(r.fs.read(&log_path("MARK")), before);
    let other = GenerationStart {
        generation: 2,
        ..generation_start(end)
    };
    assert!(matches!(
        init_generation(&store, &other, T0),
        Err(StoreError::Generation { found: 1, start: 2 })
    ));
}

#[test]
fn a_restored_generation_takes_ingest_at_its_first_position_with_the_restores_stamp() {
    let (mut r, _) = restored(cfg());
    let rep = r.open();
    assert_eq!(rep.rule, StartRule::FirstStart);
    assert!(rep.entry.is_none());
    assert!(rep.losses.is_empty());
    r.submit(vec![fact(5, 0)]);
    r.drain();
    assert_eq!(
        r.acked,
        vec![Position {
            generation: 1,
            sequence: 1
        }]
    );
    let m = r.mark();
    assert_eq!((m.generation, m.stamp, m.boot_id), (1, stamp(), [1; 16]));
    assert!(r.lost_ranges().is_empty());
}

#[test]
fn earlier_generations_sealed_segments_take_no_part_in_the_chain() {
    let (mut r, _) = restored(cfg());
    r.open();
    r.submit(vec![fact(5, 0)]);
    r.drain();
    r.kill();
    let rep = r.open();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.losses.is_empty() && rep.entry.is_none());
    r.submit(vec![fact(5, 1)]);
    r.drain();
    assert_eq!(r.acked.last().unwrap().sequence, 2);
    r.kill();
    r.boot += 1;
    let rep = r.open();
    let e = rep.entry.expect("rule 3 writes an entry");
    assert_eq!((e.entry.after, e.sequence), (2, 3));
    assert!(
        rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
        "{:?}",
        rep.losses
    );
    let m = r.mark();
    assert_eq!((m.generation, m.last_sequence), (1, 3));
}

#[test]
fn set_aside_batches_reserve_their_ids_but_name_no_consumer_and_give_no_interval() {
    let (mut r, _) = restored(cfg());
    r.open();
    assert!(r.w.as_ref().unwrap().stats().max_series_id >= 9);
    r.submit(vec![fact(5, 0), fact(9, 0)]);
    r.drain();
    r.timed_seal();
    assert_eq!(r.reach(1, 5), Some(HALF_HOUR));
    assert_eq!(r.reach(1, 9), Some(0));
    r.kill();
    r.boot += 1;
    let rep = r.open();
    let e = rep.entry.unwrap();
    assert_eq!(e.entry.consumers, vec!["kept".to_string()]);
    assert!(e.entry.id_floor >= 9);
}

#[test]
fn a_start_refuses_a_mark_or_segment_of_another_generation() {
    let (r, end) = restored(cfg());
    let fs = r.fs.clone();
    let mut zero = Rig::on(fs.clone(), cfg(), None);
    assert!(matches!(
        zero.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 1,
            start: 0
        }))
    ));
    let mut two = Rig::on(
        fs.clone(),
        cfg(),
        Some(GenerationStart {
            generation: 2,
            ..generation_start(end)
        }),
    );
    assert!(matches!(
        two.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 1,
            start: 2
        }))
    ));
    let later = SegmentName {
        generation: 2,
        file_id: 40,
        sealed: true,
    };
    let (n, _) = r.segments().into_iter().find(|(n, _)| n.sealed).unwrap();
    let bytes = fs.read(&log_path(&n.to_string())).unwrap();
    let f = fs
        .open(&log_path(&later.to_string()), OpenMode::CreateNew)
        .unwrap();
    f.write_at(&bytes, 0).unwrap();
    let mut one = Rig::on(fs, cfg(), Some(generation_start(end)));
    assert!(matches!(
        one.try_open(),
        Err(OpenError::Store(StoreError::Generation {
            found: 2,
            start: 1
        }))
    ));
}

/// A restored generation with unsealed segments only: the lowest has acknowledged batches; ingest stops once the
/// mark is two segments above it. Segments are created but never sealed.
fn unsealed_in_generation_1() -> Rig {
    let small = WriterConfig {
        segment_size: 4096,
        max_batch_bytes: 1024,
        ..cfg()
    };
    let (mut r, _) = restored(small);
    r.open();
    for k in 0..400 {
        r.submit(vec![fact(5, k)]);
        r.step();
        let (fs, wall) = (r.fs.clone(), r.wall);
        let w = r.w.as_mut().unwrap();
        if let Some(req) = w.segment_request(wall) {
            let res = create_segment(&fs, &req, &w.halt());
            w.segment_created(res);
        }
        r.advance(10 * MS);
        let open: Vec<_> = r
            .segments()
            .into_iter()
            .filter(|(n, _)| !n.sealed)
            .collect();
        if open.len() >= 4 && r.mark().file_id >= open[2].0.file_id {
            break;
        }
    }
    r
}

/// Damages the lowest unsealed segment of generation 1: its header and every batch (0), all its bytes zeroed (1),
/// or the file removed (2). Returns the last position it held.
fn damage_lowest(r: &mut Rig, how: u8) -> u64 {
    let (n, path) = r
        .segments()
        .into_iter()
        .find(|(n, _)| n.generation == 1 && !n.sealed)
        .unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: n.file_id,
        generation: 1,
    };
    let bytes = r.fs.read(&path).unwrap();
    let held: Vec<(u64, u64)> = scan(&bytes, &id, ScanLimit::End)
        .unwrap()
        .batches
        .iter()
        .map(|b| (b.offset, b.commit.last_sequence))
        .collect();
    r.kill();
    match how {
        0 => {
            r.fs.poke(&path, 8, &[0xff; 8]);
            for &(at, _) in &held {
                r.fs.poke(&path, at as usize + 40, &[0xff; 8]);
            }
        }
        1 => r.fs.poke(&path, 0, &vec![0; bytes.len()]),
        _ => r.fs.remove_file(&path).unwrap(),
    }
    r.fs.restart();
    held.last().unwrap().1
}

/// Q-264: with no sealed segment of its own, a restored generation's log starts at its first position, so its
/// lowest unsealed segment damaged, zeroed or missing is a recorded `data_lost` range from there (Q-321 in
/// generation 0, pinned in `recovery.rs`).
#[test]
fn a_restored_generations_lowest_segment_damaged_zeroed_or_missing_is_a_recorded_loss() {
    for reboot in [false, true] {
        for how in 0..3 {
            let mut r = unsealed_in_generation_1();
            let last = damage_lowest(&mut r, how);
            if reboot {
                r.boot += 1;
            }
            let rep = r.open();
            let lost: Vec<_> = r
                .lost_ranges()
                .into_iter()
                .map(|(g, l)| (g, l.first, l.last))
                .collect();
            assert_eq!(
                lost,
                vec![(1, (1, 1), (1, last))],
                "reboot {reboot}, damage {how}"
            );
            assert!(
                rep.losses
                    .iter()
                    .any(|l| l.kind == LossKind::DataLost
                        && (l.first, l.last) == (Some(1), Some(last))),
                "reboot {reboot}, damage {how}: {:?}",
                rep.losses
            );
        }
    }
}

/// A rule 2 region found in a restored generation is recorded in that generation, also beside a loss of generation 0
/// at the same sequences, which a restore records (Q-261).
#[test]
fn a_loss_in_a_restored_generation_is_recorded_in_it_beside_one_of_generation_0() {
    let (mut r, _) = restored(cfg());
    r.open();
    for k in 0..3 {
        r.submit(vec![fact(5, k)]);
        r.drain();
    }
    let (n, path) = r
        .segments()
        .into_iter()
        .find(|(n, _)| n.generation == 1 && !n.sealed)
        .unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: n.file_id,
        generation: 1,
    };
    let at = scan(&r.fs.read(&path).unwrap(), &id, ScanLimit::End)
        .unwrap()
        .batches[1]
        .offset;
    r.kill();
    let earlier = LostRange {
        identity: (0, 2),
        first: (0, 2),
        last: (0, 2),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::LostInBucket,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: Some(stamp()),
    };
    let manifest = dir().join("MANIFEST");
    let f = r.fs.open(&manifest, OpenMode::ReadWrite).unwrap();
    f.write_at(
        &encode_record(RECORD_LOST_RANGE, 2, 1, T0, Some(&earlier)),
        f.len().unwrap(),
    )
    .unwrap();
    r.fs.poke(&path, at as usize + 40, &[0xff; 8]);
    r.fs.restart();
    r.open();
    let lost: Vec<_> = r
        .lost_ranges()
        .into_iter()
        .map(|(g, l)| (g, l.first, l.last))
        .collect();
    assert_eq!(lost, vec![(1, (0, 2), (0, 2)), (1, (1, 2), (1, 2))]);
}

fn read_all(r: &Rig, set_aside: SetAside) -> Vec<(Position, Position)> {
    let mut cfg = ReaderConfig::new(
        dir(),
        Arc::new(r.fs.clone()),
        Arc::new(SystemClock::default()),
        Some([r.boot; 16]),
        [8; 16],
    );
    cfg.set_aside = set_aside;
    let mut reader = LogReader::open(cfg).unwrap();
    let update = reader.refresh().unwrap();
    reader.close().unwrap();
    update
        .events
        .into_iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some((b.first, b.last)),
            _ => None,
        })
        .collect()
}

fn at(generation: u32, sequence: u64) -> Position {
    Position {
        generation,
        sequence,
    }
}

/// A reader on a restored directory reads the earlier generations' segments whole, whatever segment the mark
/// names, and skips what is set aside; with only the first mark, as a drill leaves it, it reads the restored log.
#[test]
fn a_reader_reads_earlier_generations_past_what_is_set_aside() {
    let (mut r, end) = restored(cfg());
    let aside = generation_start(end).set_aside;
    assert_eq!(read_all(&r, aside.clone()), vec![(at(0, 1), at(0, end))]);
    r.open();
    r.submit(vec![fact(5, 0)]);
    r.drain();
    assert_eq!(
        read_all(&r, aside),
        vec![(at(0, 1), at(0, end)), (at(1, 1), at(1, 1))]
    );
    assert_eq!(read_all(&r, SetAside::default()).len(), 3);
}

/// The first position comes from the stamp, not a literal 1.
#[test]
fn a_generation_starting_above_1_takes_ingest_at_its_stamps_sequence() {
    let (files, end) = source();
    let fs = MemFs::new();
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    for (n, bytes) in &files {
        let f = fs
            .open(&log_path(&n.to_string()), OpenMode::CreateNew)
            .unwrap();
        f.write_at(bytes, 0).unwrap();
    }
    let start = GenerationStart {
        stamp: ResetStamp {
            sequence: 5,
            nonce: 3,
        },
        ..generation_start(end)
    };
    init_generation(&store, &start, T0).unwrap();
    drop(store);
    let mut r = Rig::on(fs, cfg(), Some(start));
    assert_eq!(r.mark().last_sequence, 4);
    assert_eq!(r.open().rule, StartRule::FirstStart);
    r.submit(vec![fact(5, 0)]);
    r.drain();
    assert_eq!(r.acked, vec![at(1, 5)]);
    r.kill();
    let rep = r.open();
    assert!(
        rep.losses.is_empty() && r.lost_ranges().is_empty(),
        "{:?}",
        rep.losses
    );
}

/// F after a reboot in a restored generation counts its own cursor files, lost ranges and registry file only, and
/// a registry file of this generation covers the sealed log below its `through` as a position (Q-262).
#[test]
fn f_and_the_id_floor_in_a_restored_generation_come_from_its_own_sources() {
    use kvik_log::cursor::{CursorState, append};
    use kvik_log::registry::{RegistryFile, write};
    let (mut r, _) = restored(cfg());
    r.open();
    r.submit(vec![fact(5, 0)]);
    r.drain();
    r.kill();
    let state = |consumer: &str, generation, sequence| CursorState {
        consumer: consumer.into(),
        position: at(generation, sequence),
        latches: Vec::new(),
        reset: None,
        gate: None,
    };
    append(&r.fs, &dir(), STORE_ID, 1, T0, &state("other", 1, 12), None).unwrap();
    append(&r.fs, &dir(), STORE_ID, 0, T0, &state("old", 0, 90), None).unwrap();
    r.boot += 1;
    let e = r.open().entry.unwrap();
    assert_eq!(e.sequence, 13, "F above the cursor of this generation only");
    assert_eq!(e.entry.consumers, vec!["kept".to_string()]);
    let gap: Vec<_> = r
        .lost_ranges()
        .into_iter()
        .map(|(g, l)| (g, l.first, l.last))
        .collect();
    assert_eq!(
        gap,
        vec![(1, (1, 2), (1, 12))],
        "the entry's gap is in generation 1"
    );
    r.kill();
    let lost = LostRange {
        identity: (1, 20),
        first: (1, 20),
        last: (1, 30),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: Some(stamp()),
    };
    let manifest = dir().join("MANIFEST");
    let f = r.fs.open(&manifest, OpenMode::ReadWrite).unwrap();
    f.write_at(
        &encode_record(RECORD_LOST_RANGE, 9, 1, T0, Some(&lost)),
        f.len().unwrap(),
    )
    .unwrap();
    r.boot += 1;
    let e = r.open().entry.unwrap();
    assert_eq!(e.sequence, 31, "F above this generation's lost range");
    r.kill();
    for (generation, floor) in [(0, 90), (1, 50)] {
        let file = RegistryFile {
            generation,
            through: 0,
            max_series_id: floor,
            entries: Vec::new(),
        };
        write(&r.fs, &dir(), STORE_ID, T0, &file).unwrap();
    }
    r.boot += 1;
    let e = r.open().entry.unwrap();
    assert_eq!(e.entry.id_floor, 50);
    assert!(e.entry.consumers.is_empty(), "{:?}", e.entry.consumers);
}

/// A recovery entry that needs a new segment takes it in the start's generation.
#[test]
fn a_recovery_entry_in_a_new_segment_is_in_the_restored_generation() {
    let (mut r, _) = restored(cfg());
    r.open();
    r.submit(vec![fact(5, 0)]);
    r.drain();
    r.kill();
    for (n, p) in r.segments() {
        if n.generation == 1 && !n.sealed {
            r.fs.remove_file(&p).unwrap();
        }
    }
    r.fs.restart();
    r.boot += 1;
    let rep = r.open();
    assert!(rep.entry.is_some());
    let open: Vec<SegmentName> = r
        .segments()
        .into_iter()
        .map(|(n, _)| n)
        .filter(|n| !n.sealed)
        .collect();
    assert!(
        !open.is_empty() && open.iter().all(|n| n.generation == 1),
        "{open:?}"
    );
    r.kill();
    r.open();
}
