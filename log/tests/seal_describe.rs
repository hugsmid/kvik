//! The description of sealed segments no manifest record lists, after a start (`spec.md` §7 Manifest; Q-221 (b),
//! (c)), and the order of descriptions against the start's lost-range records (Q-290).

mod seal_rig;

use std::path::PathBuf;
use std::sync::OnceLock;

use kvik_log::batch::SegmentIdentity;
use kvik_log::block::{BLOCK_HEADER_LEN, block_type, encode_block};
use kvik_log::body::{END_MARKER_LEN, TRAILER_LEN, Trailer};
use kvik_log::fs::{FaultOp, Fs, MemFs, OpenMode};
use kvik_log::manifest::{
    LossCause, LostRange, RECORD_LOST_RANGE, RECORD_SEALED, encode_record, read_manifest,
};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::writer::{DescribeError, Reason, describe_sealed};
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
    let id = r.w.take_unlisted(r.now).expect("an unlisted segment");
    assert!(!r.w.seals_running());
    assert!(r.w.take_unlisted(r.now).is_none());
    let res = describe_sealed(&r.fs, r.w.log_dir(), &id);
    r.w.unlisted_done(res, r.now);
    r.maint();
    assert_eq!(r.w.stats().sealed_segments, 0);
    let got = by_id(&r.described);
    assert_eq!(got, at_seal);
    for (id, bytes) in &files {
        assert_eq!(got[id], expected(*id, bytes));
    }
}

#[test]
fn a_start_leaves_a_segment_that_fails_its_checks_unnamed_and_degrades_health_naming_it() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    r.w.shutdown(r.now).unwrap();
    let fs = r.fs.clone();
    drop(r);
    let files = sealed_files(&fs.inner);
    let ids: Vec<u64> = files.keys().copied().collect();
    // The first cannot be read, which is retried and degrades nothing; the second has a byte flipped inside a batch,
    // which `file_crc32c` covers.
    fs.fail_reads(&seg_path(ids[0], true).to_string_lossy());
    let at = 200;
    fs.inner
        .poke(&seg_path(ids[1], true), at, &[files[&ids[1]][at] ^ 0x40]);
    let mut r = Rig::open(fs.clone());
    r.maint();
    assert!(r.w.fault().is_none());
    let got = by_id(&r.described);
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![ids[2]]);
    assert_eq!(damaged(&r), vec![ids[1]]);
    r.fill(1);
    r.maint();
    assert!(r.w.fault().is_none());
    assert_eq!(damaged(&r), vec![ids[1]]);
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
    assert_eq!(damaged(&r), vec![ids[1]]);
}

fn damaged(r: &Rig) -> Vec<u64> {
    r.w.reasons()
        .into_iter()
        .filter_map(|x| match x {
            Reason::SegmentDamaged { file_id } => Some(file_id),
            _ => None,
        })
        .collect()
}

/// A store with two sealed, unlisted segments, rebooted, whose manifest names position 100,005: the next start writes
/// its recovery entry above it and records the gap below as a lost range (step 6, Q-290).
fn store_with_a_gap_to_record() -> Watch {
    let fs = sealed_store(2);
    let path = manifest_path();
    let bytes = fs.inner.read(&path).unwrap();
    let epoch = read_manifest(&bytes, &STORE_ID).unwrap().last_epoch() + 1;
    let named = LostRange {
        identity: (0, 100_000),
        first: (0, 100_000),
        last: (0, 100_005),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: None,
    };
    fs.inner.poke(
        &path,
        bytes.len(),
        &encode_record(RECORD_LOST_RANGE, epoch, 0, T0, Some(&named)),
    );
    fs.inner.power_loss();
    fs.inner.restart();
    fs
}

#[test]
fn no_description_leaves_the_writer_before_the_gap_below_a_recovery_entry_is_recorded() {
    let fs = store_with_a_gap_to_record();
    assert_eq!(lost_ranges(&fs.inner), 1);
    let mut r = Rig::open_on_boot(fs, [2; 16]);
    assert!(r.w.fault().is_none());
    assert_eq!(r.w.start_report().entry.as_ref().unwrap().sequence, 100_006);
    assert_eq!(lost_ranges(&r.fs.inner), 2);
    r.maint();
    assert_eq!(r.described.len(), 2);
    r.fill(1);
    r.maint();
    assert_eq!(r.described.len(), 3);
}

#[test]
fn a_start_that_cannot_record_the_gap_below_a_recovery_entry_describes_nothing() {
    let fs = store_with_a_gap_to_record();
    // The first write repairs the last record (step 4); the second is the gap's record (step 6).
    fs.inner
        .fail_nth(FaultOp::Write, "MANIFEST", 1, std::io::ErrorKind::Other);
    let mut r = Rig::open_on_boot(fs, [2; 16]);
    assert!(r.w.fault().is_some());
    assert_eq!(lost_ranges(&r.fs.inner), 1);
    assert!(r.w.take_unlisted(r.now).is_none());
    r.maint();
    assert!(r.described.is_empty());
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
    // The first write repairs the last record; the second is the loss's record (step 4).
    fs.inner
        .fail_nth(FaultOp::Write, "MANIFEST", 1, std::io::ErrorKind::Other);
    let mut r = Rig::open(fs);
    assert!(r.w.fault().is_some());
    assert_eq!(lost_ranges(&r.fs.inner), 0);
    assert!(r.w.take_unlisted(r.now).is_none());
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
    // A trailer with a valid checksum that names no batches.
    let at = n - END_MARKER_LEN - TRAILER_LEN - BLOCK_HEADER_LEN;
    let mut again = Vec::new();
    let mut t = Trailer::decode(&bytes[at + BLOCK_HEADER_LEN..n - END_MARKER_LEN]).unwrap();
    encode_block(block_type::TRAILER, 0, &t.encode(), &mut again).unwrap();
    assert_eq!(again, bytes[at..n - END_MARKER_LEN]);
    t.batch_count = 0;
    again.clear();
    encode_block(block_type::TRAILER, 0, &t.encode(), &mut again).unwrap();
    let mut b = bytes.clone();
    b[at..n - END_MARKER_LEN].copy_from_slice(&again);
    assert!(matches!(
        describe(&put(&b), id),
        Err(DescribeError::Invalid(_))
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

#[test]
fn no_segment_is_described_while_a_seal_is_ready_or_running() {
    let mut r = Rig::open(sealed_store(2));
    r.maint();
    r.fill(1);
    // Killed with a full segment not yet sealed: the next start seals it before it describes anything.
    let fs = r.fs.clone();
    drop(r);
    fs.inner.restart();
    let mut r = Rig::open(fs);
    let jobs = r.w.take_seal_jobs(r.now);
    assert_eq!(jobs.len(), 1);
    assert!(r.w.take_unlisted(r.now).is_none());
    for job in jobs {
        let res = kvik_log::writer::seal_segment_described(&r.fs, &job, &r.w.halt());
        r.w.seal_done(job, res, r.now);
    }
    assert!(r.w.take_unlisted(r.now).is_some());
}

#[test]
fn no_segment_is_described_after_a_fault() {
    let mut r = Rig::open(sealed_store(2));
    r.fill(1);
    r.fs.fail_reads(".open");
    r.maint();
    assert!(r.w.fault().is_some());
    assert!(r.w.take_unlisted(r.now).is_none());
    assert!(r.described.is_empty());
}

#[test]
fn a_running_log_describes_the_unlisted_segments_on_its_maintenance_thread() {
    use std::sync::Arc;
    use std::time::Instant;

    use kvik_log::store::StoreDir;
    use kvik_log::writer::{Log, LogWriter, SystemClock};

    let fs = sealed_store(2);
    let want = sealed_files(&fs.inner);
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), None).unwrap();
    let w = LogWriter::open(
        store,
        seal_rig::cfg(),
        Some([1; 16]),
        std::time::Duration::ZERO,
        T0,
    )
    .unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let deadline = Instant::now() + seal_rig::WAIT;
    let mut got = Vec::new();
    while got.len() < want.len() {
        assert!(Instant::now() < deadline, "described {got:?}");
        got.extend(log.take_sealed());
        std::thread::sleep(seal_rig::MS);
    }
    log.shutdown().unwrap();
    let got = by_id(&got);
    assert_eq!(got.len(), want.len());
    for (id, bytes) in &want {
        assert_eq!(got[id], expected(*id, bytes));
    }
}

fn start_log(fs: &Watch) -> kvik_log::writer::Log {
    use std::sync::Arc;

    use kvik_log::store::StoreDir;
    use kvik_log::writer::{Log, LogWriter, SystemClock};

    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), None).unwrap();
    let w = LogWriter::open(
        store,
        seal_rig::cfg(),
        Some([1; 16]),
        std::time::Duration::ZERO,
        T0,
    )
    .unwrap();
    Log::start(w, Arc::new(SystemClock::default())).unwrap()
}

#[test]
fn a_shutdown_drops_a_description_in_flight_before_its_next_read() {
    use std::time::Instant;

    use kvik_log::header::HEADER_LEN;

    let fs = sealed_store(2);
    let files = sealed_files(&fs.inner);
    let (entered, release) = fs.hold_next_maint_read();
    let log = start_log(&fs);
    entered.recv_timeout(seal_rig::WAIT).unwrap();
    fs.reset_reads();
    let watch = log.watch();
    let stopping = std::thread::spawn(move || log.shutdown());
    let deadline = Instant::now() + seal_rig::WAIT;
    while !watch.stopped() {
        assert!(Instant::now() < deadline, "the writer did not stop");
        std::thread::sleep(seal_rig::MS);
    }
    release.send(()).unwrap();
    stopping.join().unwrap().unwrap();
    let read: Vec<u64> = files
        .keys()
        .map(|&id| fs.read_of(&seg_path(id, true)))
        .collect();
    assert_eq!(read, vec![HEADER_LEN as u64, 0]);
    // Found again at the next start.
    let mut r = Rig::open(fs);
    r.maint();
    assert_eq!(by_id(&r.described).len(), 2);
}

fn unlisted(file_id: u64) -> SegmentIdentity {
    SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    }
}

#[test]
fn a_description_that_cannot_read_is_retried_after_the_interval_and_succeeds_once_the_fault_clears()
{
    use kvik_log::writer::DESCRIBE_RETRY_INTERVAL;

    let fs = sealed_store(2);
    let files = sealed_files(&fs.inner);
    let ids: Vec<u64> = files.keys().copied().collect();
    fs.fail_reads(&seg_path(ids[0], true).to_string_lossy());
    let mut r = Rig::open(fs);
    let failed_at = r.now;
    r.maint();
    assert_eq!(
        by_id(&r.described).keys().copied().collect::<Vec<_>>(),
        vec![ids[1]]
    );
    assert!(r.w.fault().is_none());
    assert_eq!(r.w.reasons(), vec![]);
    let due = failed_at + DESCRIBE_RETRY_INTERVAL;
    let next = r.w.step(r.now, r.wall, &mut r.q).next;
    assert!(next.is_some_and(|t| t <= due), "{next:?}");
    r.now = due - seal_rig::MS;
    assert!(r.w.take_unlisted(r.now).is_none());
    r.fs.clear_failures();
    r.now = due;
    r.maint();
    let got = by_id(&r.described);
    assert_eq!(got[&ids[0]], expected(ids[0], &files[&ids[0]]));
    assert_eq!(r.w.reasons(), vec![]);
}

fn retry_wait(k: u32) -> std::time::Duration {
    use kvik_log::writer::{DESCRIBE_RETRY_INTERVAL, DESCRIBE_RETRY_MAX};

    (DESCRIBE_RETRY_INTERVAL * 2u32.pow(k)).min(DESCRIBE_RETRY_MAX)
}

/// Runs seal jobs that came due, then hands out `id` at `r.now` and reports `res` for it.
fn try_describe(r: &mut Rig, id: u64, k: u32) -> Result<(), DescribeError> {
    for job in r.w.take_seal_jobs(r.now) {
        let res = kvik_log::writer::seal_segment_described(&r.fs, &job, &r.w.halt());
        r.w.seal_done(job, res, r.now);
        r.create();
    }
    assert_eq!(r.w.take_unlisted(r.now), Some(unlisted(id)), "try {k}");
    let res = describe_sealed(&r.fs, r.w.log_dir(), &unlisted(id));
    let out = res.as_ref().map(drop).map_err(|e| match e {
        DescribeError::Io(e) => DescribeError::Io(e.kind().into()),
        DescribeError::NotSealed => DescribeError::NotSealed,
        DescribeError::Invalid(m) => DescribeError::Invalid(m),
    });
    r.w.unlisted_done(res, r.now);
    out
}

/// Tries `id` `tries` times, each failing to open, at the retry times; the store stays writable throughout.
fn fail_to_open(r: &mut Rig, id: u64, tries: u32) {
    for k in 0..tries {
        if k > 0 {
            r.now += retry_wait(k - 1) - seal_rig::MS;
            assert!(r.w.take_unlisted(r.now).is_none(), "try {k}");
            r.now += seal_rig::MS;
        }
        assert!(matches!(try_describe(r, id, k), Err(DescribeError::Io(_))));
        assert!(r.w.fault().is_none());
        let unreadable = retry_wait(k) == kvik_log::writer::DESCRIBE_RETRY_MAX;
        let want = if unreadable {
            vec![Reason::SegmentUnreadable { file_id: id }]
        } else {
            vec![]
        };
        assert_eq!(r.w.reasons(), want, "try {k}");
        let due = r.now + retry_wait(k);
        let next = r.w.step(r.now, r.wall, &mut r.q).next;
        assert!(next.is_some_and(|t| t <= due), "try {k}: {next:?}");
    }
}

#[test]
fn a_description_that_never_opens_is_retried_with_doubling_waits_and_degrades_health_once_the_wait_reaches_the_cap()
 {
    let fs = sealed_store(1);
    let id = *sealed_files(&fs.inner).keys().next().unwrap();
    let mut r = Rig::open(fs);
    r.fs.fail_opens(&seg_path(id, true).to_string_lossy());
    fail_to_open(&mut r, id, 9);
    assert!(retry_wait(5) < kvik_log::writer::DESCRIBE_RETRY_MAX);
    assert_eq!(retry_wait(6), kvik_log::writer::DESCRIBE_RETRY_MAX);
    assert!(r.w.take_sealed().iter().all(|f| f.id.file_id != id));
}

#[test]
fn segment_unreadable_clears_when_a_read_succeeds() {
    let fs = sealed_store(1);
    let (id, bytes) = sealed_files(&fs.inner).into_iter().next().unwrap();
    let mut r = Rig::open(fs);
    r.fs.fail_opens(&seg_path(id, true).to_string_lossy());
    fail_to_open(&mut r, id, 7);
    assert_eq!(
        r.w.reasons(),
        vec![Reason::SegmentUnreadable { file_id: id }]
    );
    r.fs.clear_failures();
    r.now += retry_wait(6);
    assert!(try_describe(&mut r, id, 7).is_ok());
    assert_eq!(r.w.reasons(), vec![]);
    let got = by_id(&r.w.take_sealed());
    assert_eq!(got[&id], expected(id, &bytes));
}

#[test]
fn a_shutdown_that_drops_a_queued_description_still_runs_the_seals_that_follow() {
    use std::sync::mpsc;

    let fs = sealed_store(2);
    // Without the segment created ahead, the start asks for one and hands out a description in the same turn; the
    // description waits behind the held creation.
    fs.inner.remove_file(&seg_path(4, false)).unwrap();
    let (entered, release) = fs.hold_next_maint_sync_dir();
    let log = start_log(&fs);
    entered
        .recv_timeout(seal_rig::WAIT)
        .expect("a creation ahead at the start");
    // More than the open segment holds, so a seal follows the creation.
    let pending: Vec<_> = (0..20)
        .map(|c| {
            let items = (0..40).map(|i| seal_rig::fact(1 + i % 3, c * 40 + i64::from(i)));
            log.submit(items.collect()).unwrap()
        })
        .collect();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || done_tx.send(log.shutdown()).unwrap());
    std::thread::sleep(200 * seal_rig::MS);
    release.send(()).unwrap();
    done_rx
        .recv_timeout(seal_rig::WAIT)
        .expect("the shutdown completes")
        .unwrap();
    for p in pending {
        p.wait().unwrap();
    }
    assert!(sealed_files(&fs.inner).contains_key(&3));
}
