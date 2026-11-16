//! The description of sealed segments no manifest record lists, after a start (`spec.md` §7 Manifest; Q-221 (b),
//! (c)), and the order of descriptions against the start's lost-range records (Q-290).

mod seal_rig;

use std::path::PathBuf;
use std::sync::OnceLock;

use kvik_log::batch::SegmentIdentity;
use kvik_log::block::encode_block;
use kvik_log::fs::{FaultOp, Fs, MemFs, OpenMode};
use kvik_log::manifest::{RECORD_SEALED, read_manifest};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::writer::{DescribeError, describe_sealed};
use proptest::prelude::*;
use seal_rig::{Rig, STORE_ID, T0, Watch, by_id, dir, expected, sealed_files, seg_path};

fn manifest_path() -> PathBuf {
    dir().join("MANIFEST")
}

/// A log segment's A.14 file description: kind 1, positions present, no hash needed by the start's listing.
fn description(generation: u32, file_id: u64) -> [u8; 80] {
    let mut b = [0u8; 80];
    b[0] = 1;
    b[2] = 1;
    b[4..8].copy_from_slice(&generation.to_le_bytes());
    b[8..16].copy_from_slice(&file_id.to_le_bytes());
    b[16..20].copy_from_slice(&generation.to_le_bytes());
    b[28..32].copy_from_slice(&generation.to_le_bytes());
    b
}

/// Appends a sealed-segment record (`0x22`) naming `files` (generation, file id), one epoch above the last.
fn append_sealed_record(fs: &MemFs, files: &[(u32, u64)]) {
    let path = manifest_path();
    let bytes = fs.read(&path).unwrap();
    let epoch = read_manifest(&bytes, &STORE_ID).unwrap().last_epoch() + 1;
    let mut body = Vec::new();
    body.extend_from_slice(&epoch.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&T0.to_le_bytes());
    for &(g, id) in files {
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&80u16.to_le_bytes());
        body.extend_from_slice(&description(g, id));
    }
    let mut block = Vec::new();
    encode_block(RECORD_SEALED, 0, &body, &mut block).unwrap();
    let f = fs.open(&path, OpenMode::ReadWrite).unwrap();
    f.write_at(&block, bytes.len() as u64).unwrap();
    f.sync_all().unwrap();
}

/// A store whose writer sealed `n` segments and stopped cleanly; nothing lists them.
fn sealed_store(n: u64) -> Watch {
    let mut r = Rig::open(Watch::new());
    r.fill(n);
    r.maint();
    r.w.shutdown(r.now).unwrap();
    r.fs.clone()
}

fn lost_ranges(fs: &MemFs) -> usize {
    read_manifest(&fs.read(&manifest_path()).unwrap(), &STORE_ID)
        .unwrap()
        .lost_ranges()
        .count()
}

/// Damages the second batch of the open segment, which a start records as a lost range (step 4).
fn damage_open_segment(r: &Rig) {
    let file_id = r.w.stats().open_file_id.expect("an open segment");
    let path = seg_path(file_id, false);
    let bytes = r.fs.inner.read(&path).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    };
    let second = scan(&bytes, &id, ScanLimit::End).unwrap().batches[1].offset;
    r.fs.inner.poke(&path, second as usize + 40, &[0xff; 8]);
}

/// A store with two sealed, unlisted segments and an open one whose second batch is damaged.
fn store_with_a_loss_to_record() -> Watch {
    let mut r = Rig::open(Watch::new());
    r.fill(2);
    r.maint();
    r.fill_batches(3);
    r.w.shutdown(r.now).unwrap();
    damage_open_segment(&r);
    r.fs.clone()
}

#[test]
fn a_start_describes_every_sealed_segment_no_record_names() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    let at_seal = by_id(&r.described);
    r.w.shutdown(r.now).unwrap();
    let fs = r.fs.clone();
    drop(r);
    let mut r = Rig::open(fs);
    let files = sealed_files(&r.fs.inner);
    assert_eq!(files.len(), 3);
    // One description at a time, outside the seals a shutdown waits for and the seal count.
    assert!(r.w.take_seal_jobs(r.now).is_empty());
    let id = r.w.take_unlisted().expect("an unlisted segment");
    assert!(!r.w.seals_running());
    assert!(r.w.take_unlisted().is_none());
    let res = describe_sealed(&r.fs, r.w.log_dir(), &id);
    r.w.unlisted_done(res);
    r.maint();
    assert_eq!(r.w.stats().sealed_segments, 0);
    let got = by_id(&r.described);
    assert_eq!(got, at_seal);
    for (id, bytes) in &files {
        assert_eq!(got[id], expected(*id, bytes));
    }
}

#[test]
fn a_start_leaves_a_segment_it_cannot_read_or_verify_unnamed() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    r.w.shutdown(r.now).unwrap();
    let fs = r.fs.clone();
    drop(r);
    let files = sealed_files(&fs.inner);
    let ids: Vec<u64> = files.keys().copied().collect();
    // The first cannot be read; the second has a byte flipped inside a batch, which `file_crc32c` covers.
    fs.fail_reads(&seg_path(ids[0], true).to_string_lossy());
    let at = 200;
    fs.inner
        .poke(&seg_path(ids[1], true), at, &[files[&ids[1]][at] ^ 0x40]);
    let mut r = Rig::open(fs.clone());
    r.maint();
    assert!(r.w.fault().is_none());
    let got = by_id(&r.described);
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![ids[2]]);
    r.fill(1);
    r.maint();
    assert!(r.w.fault().is_none());
    r.w.shutdown(r.now).unwrap();
    drop(r);
    // The next start tries again: the readable one is described, the damaged one never.
    fs.clear_failures();
    let mut r = Rig::open(fs);
    r.maint();
    let got = by_id(&r.described);
    assert!(got.contains_key(&ids[0]));
    assert!(!got.contains_key(&ids[1]));
    assert_eq!(got[&ids[0]], expected(ids[0], &files[&ids[0]]));
}

#[test]
fn a_start_skips_every_segment_a_record_lists_by_generation_and_id() {
    let fs = sealed_store(3);
    let ids: Vec<u64> = sealed_files(&fs.inner).keys().copied().collect();
    append_sealed_record(&fs.inner, &[(0, ids[0]), (0, ids[1])]);
    // The same file id in another generation is another file.
    append_sealed_record(&fs.inner, &[(1, ids[2])]);
    let mut r = Rig::open(fs);
    r.maint();
    assert_eq!(
        by_id(&r.described).keys().copied().collect::<Vec<_>>(),
        vec![ids[2]]
    );
}

#[test]
fn no_description_leaves_the_writer_before_the_starts_lost_range_records() {
    let fs = store_with_a_loss_to_record();
    assert_eq!(lost_ranges(&fs.inner), 0);
    let mut r = Rig::open(fs);
    assert!(r.w.fault().is_none());
    assert_eq!(r.w.start_report().losses.len(), 1);
    // Every description handed over from here, of an unlisted segment or of one sealed in this run, follows the
    // start's synced lost-range record, so a sealed-segment record appended from it has a higher epoch.
    assert_eq!(lost_ranges(&r.fs.inner), 1);
    let damaged = r.w.stats().open_file_id.unwrap();
    r.maint();
    let unlisted = by_id(&r.described);
    assert_eq!(unlisted.len(), 2);
    r.fill(1);
    r.maint();
    let all = by_id(&r.described);
    assert!(
        all.contains_key(&damaged),
        "the segment holding the loss is sealed and described"
    );
    assert_eq!(lost_ranges(&r.fs.inner), 1);
}

#[test]
fn a_start_that_cannot_record_a_loss_is_read_only_and_describes_nothing() {
    let fs = store_with_a_loss_to_record();
    fs.inner
        .fail_nth(FaultOp::Write, "MANIFEST", 0, std::io::ErrorKind::Other);
    let mut r = Rig::open(fs);
    assert!(r.w.fault().is_some());
    assert_eq!(lost_ranges(&r.fs.inner), 0);
    assert!(r.w.take_unlisted().is_none());
    r.maint();
    assert!(r.described.is_empty());
}

fn describe(fs: &MemFs, file_id: u64) -> Result<kvik_log::segment::SealedFile, DescribeError> {
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    };
    describe_sealed(fs, &dir().join("log"), &id)
}

#[test]
fn describe_sealed_refuses_each_check_it_makes() {
    let fs = sealed_store(1);
    let (&id, bytes) = sealed_files(&fs.inner)
        .iter()
        .next()
        .map(|(k, v)| (k, v.clone()))
        .unwrap();
    let path = seg_path(id, true);
    assert_eq!(describe(&fs.inner, id).unwrap(), expected(id, &bytes));
    assert!(matches!(
        describe(&fs.inner, id + 7),
        Err(DescribeError::Io(_))
    ));
    let put = |b: &[u8]| {
        let mem = MemFs::new();
        mem.create_dir_all(&dir().join("log")).unwrap();
        let f = mem.open(&path, OpenMode::CreateNew).unwrap();
        f.write_at(b, 0).unwrap();
        mem
    };
    // A header naming another file.
    let other = put(&bytes);
    other.rename(&path, &seg_path(id + 1, true)).unwrap();
    assert!(matches!(
        describe(&other, id + 1),
        Err(DescribeError::Invalid(_))
    ));
    // A damaged end marker, a damaged trailer, a file cut short.
    let n = bytes.len();
    for at in [n - 1, n - 40] {
        let mut b = bytes.clone();
        b[at] ^= 0x10;
        assert!(
            matches!(describe(&put(&b), id), Err(DescribeError::NotSealed)),
            "{at}"
        );
    }
    assert!(matches!(
        describe(&put(&bytes[..n - 8]), id),
        Err(DescribeError::NotSealed)
    ));
    // A batch byte, which `file_crc32c` covers.
    let mut b = bytes.clone();
    b[200] ^= 0x10;
    assert!(matches!(
        describe(&put(&b), id),
        Err(DescribeError::Invalid(_))
    ));
}

fn one_sealed_file() -> &'static (u64, Vec<u8>) {
    static FILE: OnceLock<(u64, Vec<u8>)> = OnceLock::new();
    FILE.get_or_init(|| {
        let fs = sealed_store(1);
        sealed_files(&fs.inner).into_iter().next().unwrap()
    })
}

proptest! {
    #[test]
    fn a_sealed_file_with_any_byte_changed_is_never_described(
        at in any::<prop::sample::Index>(),
        flip in 1u8..=255,
    ) {
        let (id, bytes) = one_sealed_file();
        let mut b = bytes.clone();
        let at = at.index(b.len());
        b[at] ^= flip;
        let mem = MemFs::new();
        mem.create_dir_all(&dir().join("log")).unwrap();
        mem.open(&seg_path(*id, true), OpenMode::CreateNew).unwrap().write_at(&b, 0).unwrap();
        prop_assert!(describe(&mem, *id).is_err(), "byte {} of {}", at, b.len());
    }
}
