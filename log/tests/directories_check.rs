//! The derived files (`format.md` A.18) checked against the catalogue: a replaced segment, a restore, a reset, a lost
//! range and a manifest rebuild each invalidate what they touch; the log directory is contiguous.

mod dirs_rig;

use dirs_rig::*;

#[test]
fn a_replaced_segment_invalidates_its_group_alone() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 7_000, 0);
    r.settle();
    let live = r.load().unwrap();
    assert!(live.groups.len() >= 2);
    let victim = live.groups[0].as_ref().unwrap().segments[3].clone();
    let mut cat = r.catalogue();
    let l = cat.segments.remove(&victim.file_id).unwrap();
    cat.segments.insert(
        10_000,
        Listed {
            seal: Some((l.seal.unwrap().0, 1)),
            ..l
        },
    );
    let d = logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
        .unwrap()
        .unwrap();
    assert!(
        d.groups[0].is_none(),
        "the group listing the replaced segment"
    );
    assert!(
        d.groups[1..].iter().all(Option::is_some),
        "every other group"
    );
    // A segment whose hash differs, as the manifest describes it, is replaced too.
    let mut cat = r.catalogue();
    cat.segments.get_mut(&victim.file_id).unwrap().sha256 = Some([0; 32]);
    let d = logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
        .unwrap()
        .unwrap();
    assert!(d.groups[0].is_none() && d.groups[1].is_some());
    cat.segments.get_mut(&victim.file_id).unwrap().sha256 = Some(victim.sha256);
    assert!(
        logdir::load(&r.fs, &dir(), &STORE_ID, &cat)
            .unwrap()
            .unwrap()
            .groups[0]
            .is_some()
    );
}

#[test]
fn a_restore_invalidates_every_file_of_the_older_generation() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 100, 0);
    r.settle();
    let mut cat = r.catalogue();
    cat.generation = 1;
    assert_eq!(logdir::load(&r.fs, &dir(), &STORE_ID, &cat).unwrap(), None);
    let (frame, s) = r.series().unwrap();
    let mut start = r.w.as_ref().unwrap().directory_start();
    assert!(start.keeps(frame.generation, s.stamp, s.through));
    start.generation = 1;
    assert!(!start.keeps(frame.generation, s.stamp, s.through));
}

#[test]
fn a_reset_below_the_series_directory_invalidates_it_and_one_above_keeps_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 100, 0);
    r.settle();
    let (frame, s) = r.series().unwrap();
    let mut start = r.w.as_ref().unwrap().directory_start();
    start.stamp = Some(kvik_log::mark::ResetStamp {
        sequence: s.through.sequence + 5,
        nonce: 3,
    });
    start.reset_after = Some(s.through);
    assert!(start.keeps(frame.generation, s.stamp, s.through));
    start.reset_after = Some(Position {
        generation: 0,
        sequence: s.through.sequence - 1,
    });
    assert!(!start.keeps(frame.generation, s.stamp, s.through));
}

#[test]
fn a_manifest_rebuild_invalidates_the_root_alone() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 400, 0);
    r.settle();
    let live = r.load().unwrap();
    // A rebuilt manifest renumbers its records: the record the root names is at the same offset with another epoch.
    let path = dir().join("MANIFEST");
    let old = read_manifest(&r.fs.read(&path).unwrap(), &STORE_ID).unwrap();
    let mut bytes = encode_initial(STORE_ID, T0);
    bytes.truncate(old.records[0].offset as usize);
    bytes.extend_from_slice(&encode_record(RECORD_EMPTY, 7, 0, T0, None));
    r.fs.remove_file(&path).unwrap();
    let f = r.fs.open(&path, kvik_log::fs::OpenMode::CreateNew).unwrap();
    f.write_at(&bytes, 0).unwrap();
    drop(f);
    assert_eq!(r.load(), None, "the root no longer names a record");
    r.open();
    r.settle();
    assert_eq!(
        r.dirs.as_ref().unwrap().stats().described,
        0,
        "every group reused"
    );
    let d = r.load().unwrap();
    assert_eq!(d.root.groups, live.root.groups);
    assert_eq!(d.root.epoch, 7);
}

#[test]
fn a_segment_whose_trailer_changed_under_its_record_is_described_again_by_the_next_start() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 600, 0);
    r.settle();
    let victim = r.load().unwrap().groups[0].as_ref().unwrap().segments[2].clone();
    // The trailer's `file_crc32c` changed (its block checksum made right): the record no longer matches.
    let p = sealed_path(victim.file_id);
    let mut bytes = r.fs.read(&p).unwrap();
    let n = bytes.len();
    let at = u64::from_le_bytes(bytes[n - 16..n - 8].try_into().unwrap()) as usize;
    bytes[at + 16 + 56] ^= 0xff;
    let body = bytes[at + 16..at + 16 + 64].to_vec();
    bytes[at + 8..at + 12].copy_from_slice(&kvik_log::crc::crc32c(&body).to_le_bytes());
    let head = bytes[at..at + 12].to_vec();
    bytes[at + 12..at + 16].copy_from_slice(&kvik_log::crc::crc32c(&head).to_le_bytes());
    rewrite(&r, &p, &bytes);
    assert!(
        r.load().unwrap().groups[0].is_none(),
        "the group listing it"
    );
    r.open();
    r.settle();
    assert_eq!(r.dirs.as_ref().unwrap().stats().described, 1);
    let d = r.load().unwrap();
    let rec = d.segments().find(|s| s.file_id == victim.file_id).unwrap();
    assert_ne!(rec.file_crc32c, victim.file_crc32c);
}

#[test]
fn a_group_record_with_another_last_sequence_or_generation_does_not_match() {
    let r = two_groups();
    let d = r.load().unwrap();
    let g = d.groups[0].clone().unwrap();
    let cat = r.catalogue();
    let mut other = g.clone();
    other.segments[0].last_sequence += 1;
    assert!(!cat.group_matches(0, &other));
    let mut other = g.clone();
    other.segments[0].generation = 1;
    assert!(!cat.group_matches(0, &other));
    assert!(
        !cat.group_matches(1, &g),
        "a group written in another store generation"
    );
    assert!(cat.group_matches(0, &g));
}

#[test]
fn a_group_file_under_another_number_or_id_is_not_used() {
    let r = two_groups();
    let gens = directory::generations(&r.fs, &dir(), Family::Group(1));
    let bytes = r.fs.read(&gens[0].1).unwrap();
    // Group 1's bytes under group 0's newest name: the header's `file_id` is not the name's.
    let g0 = directory::generations(&r.fs, &dir(), Family::Group(0));
    for (_, p) in &g0 {
        r.fs.remove_file(p).unwrap();
    }
    let name = dir()
        .join(DERIVED_DIR)
        .join(Family::Group(0).file_name(gens[0].0));
    let f = r.fs.open(&name, kvik_log::fs::OpenMode::CreateNew).unwrap();
    f.write_at(&bytes, 0).unwrap();
    drop(f);
    let d = r.load().unwrap();
    assert!(d.groups[0].is_none(), "group 1 found under group 0's name");
    assert!(d.groups[1].is_some());
    // The same bytes under a name whose `file_id` differs from the header's.
    r.fs.remove_file(&name).unwrap();
    let wrong = dir()
        .join(DERIVED_DIR)
        .join(Family::Group(1).file_name(gens[0].0 + 7));
    for (_, p) in &gens {
        r.fs.remove_file(p).unwrap();
    }
    let f =
        r.fs.open(&wrong, kvik_log::fs::OpenMode::CreateNew)
            .unwrap();
    f.write_at(&bytes, 0).unwrap();
    drop(f);
    assert!(r.load().unwrap().groups[1].is_none());
}

#[test]
fn a_group_with_a_hole_or_groups_that_do_not_join_are_not_used() {
    let r = two_groups();
    let d = r.load().unwrap();
    let mut g = d.groups[0].clone().unwrap();
    // A record taken out of the middle: the group no longer joins up (A.18 Contiguity).
    g.segments.remove(5);
    assert!(!logdir::contiguous(&g, &[]));
    let lost = [kvik_log::directory::logdir::LostEntry {
        first: Position {
            generation: 0,
            sequence: g.segments[4].last_sequence + 1,
        },
        last: Position {
            generation: 0,
            sequence: g.segments[5].first_sequence - 1,
        },
        file_id: 0,
        cause: 1,
    }];
    assert!(logdir::contiguous(&g, &lost), "a lost range covers the gap");
    // A root whose groups do not join is invalid.
    let mut root = d.root.clone();
    root.groups[1].first.sequence += 1;
    let frame = Frame {
        generation: 0,
        file_id: 1,
        written_at: T0,
    };
    let bytes = root.encode(frame, STORE_ID).unwrap();
    assert!(Root::decode(&bytes, &STORE_ID).is_err());
}

#[test]
fn a_sealed_segment_whose_trailer_is_damaged_ends_through_below_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 600, 0);
    r.settle();
    let all: Vec<_> = r.load().unwrap().segments().cloned().collect();
    let victim = all[2].clone();
    let p = sealed_path(victim.file_id);
    let mut bytes = r.fs.read(&p).unwrap();
    let n = bytes.len();
    bytes[n - 3] ^= 0x40;
    rewrite(&r, &p, &bytes);
    r.open();
    r.settle();
    let d = r.load().expect("root");
    assert!(
        d.root.through < victim.first(),
        "through stays below the unreadable segment"
    );
    assert_eq!(r.dirs.as_ref().unwrap().stats().unlisted, 1);
    assert!(d.segments().all(|s| s.last() < victim.first()));
}

#[test]
fn a_lost_range_recorded_below_the_series_directory_drops_it() {
    let mut r = Rig::new(cfg());
    ingest(&mut r, 300, 0);
    r.settle();
    let (_, s) = r.series().unwrap();
    let start = r.w.as_ref().unwrap().directory_start();
    assert!(kept_series(&r.fs, &dir(), &STORE_ID, &start).is_some());
    // A start (or the scrub) records a lost range at or below its `through`.
    let path = dir().join("MANIFEST");
    let mut bytes = r.fs.read(&path).unwrap();
    let m = read_manifest(&bytes, &STORE_ID).unwrap();
    let at = s.through.sequence - 3;
    let lost = kvik_log::manifest::LostRange {
        identity: (0, at),
        first: (0, at),
        last: (0, at),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: kvik_log::manifest::LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: None,
    };
    bytes.truncate(m.valid_end as usize);
    bytes.extend_from_slice(&encode_record(
        kvik_log::manifest::RECORD_LOST_RANGE,
        m.last_epoch() + 1,
        0,
        T0,
        Some(&lost),
    ));
    rewrite(&r, &path, &bytes);
    let m = read_manifest(&r.fs.read(&path).unwrap(), &STORE_ID).unwrap();
    assert_eq!(m.lost_ranges().count(), 1);
    // Only a generation written before the loss's position, if any, is still kept.
    let kept = kept_series(&r.fs, &dir(), &STORE_ID, &start);
    assert!(
        kept.as_ref().is_none_or(|(_, k)| k.through.sequence < at),
        "{kept:?}"
    );
    assert!(r.series().unwrap().1.through.sequence >= at);
}
