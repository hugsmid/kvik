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
