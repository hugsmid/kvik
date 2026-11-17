//! The plaintext SHA-256 of a sealed log segment (`format.md` A.9, A.14; Q-220, Q-221 (a)): computed at seal on
//! `kvik-maint`, once per seal, and handed over once.

mod seal_rig;

use std::io;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::fs::FaultOp;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::StoreDir;
use kvik_log::writer::{Log, LogWriter, SEAL_RETRY_INTERVAL, SystemClock};
use seal_rig::{
    MS, Rig, STORE_ID, T0, WAIT, Watch, by_id, cfg, dir, expected, fact, new_store, sealed_files,
    seg_path,
};

#[test]
fn the_hash_is_sha256_of_the_sealed_files_bytes() {
    let mut r = Rig::open(Watch::new());
    r.fill(3);
    r.maint();
    let files = sealed_files(&r.fs.inner);
    assert_eq!(files.len(), 3);
    let got = by_id(&r.described);
    assert_eq!(r.w.stats().sealed_segments, 3);
    for (id, bytes) in &files {
        assert_eq!(got[id], expected(*id, bytes));
    }
    assert_eq!(got.len(), files.len());
    assert!(
        r.w.take_sealed().is_empty(),
        "each description is handed over once"
    );
}

#[test]
fn a_blocked_seal_reads_and_hashes_its_segment_once() {
    let mut r = Rig::open(Watch::new());
    r.fill(1);
    r.fs.reset_reads();
    r.fs.inner
        .fail_nth(FaultOp::Rename, ".kvl", 0, io::ErrorKind::StorageFull);
    r.maint();
    assert!(r.described.is_empty());
    assert!(sealed_files(&r.fs.inner).is_empty());
    r.now += SEAL_RETRY_INTERVAL;
    r.maint();
    let files = sealed_files(&r.fs.inner);
    let (id, bytes) = files.iter().next().unwrap();
    let id = *id;
    let seal = scan(
        bytes,
        &SegmentIdentity {
            store_id: STORE_ID,
            file_id: id,
            generation: 0,
        },
        ScanLimit::End,
    )
    .unwrap()
    .seal
    .unwrap();
    assert_eq!(r.fs.read_of(&seg_path(id, false)), seal.start);
    assert_eq!(r.described, vec![expected(id, bytes)]);
}

#[test]
fn a_read_error_while_sealing_is_a_write_failure_on_the_log() {
    let mut r = Rig::open(Watch::new());
    r.fill(1);
    r.fs.fail_reads(".open");
    r.maint();
    let f = r.w.fault().expect("read-only").clone();
    assert_eq!(f.op, kvik_log::writer::FaultOp::Seal);
    assert!(r.described.is_empty());
    assert!(sealed_files(&r.fs.inner).is_empty());
}

/// Waits for an acknowledgement on another thread, so a writer that never answers fails the test instead of
/// hanging it.
fn ack_within(p: kvik_log::writer::Pending, d: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.wait().is_ok());
    });
    rx.recv_timeout(d).unwrap_or(false)
}

#[test]
fn the_commit_thread_acknowledges_while_a_seal_is_hashing() {
    let fs = Watch::new();
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    let w = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let (entered, release) = fs.hold_next_maint_read();
    let mut t = 0;
    let mut submit = |log: &Log| {
        let items: Vec<Item> = (0..40).map(|i| fact(1, t + i)).collect();
        t += 40;
        log.submit(items).unwrap()
    };
    // Fill the first segment until its seal starts reading it, on `kvik-maint`.
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(ack_within(submit(&log), WAIT));
        if entered.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "no seal started");
    }
    // The seal is held mid-read: acknowledgements continue and nothing is described yet.
    for _ in 0..20 {
        assert!(ack_within(submit(&log), WAIT));
    }
    assert!(log.take_sealed().is_empty());
    release.send(()).unwrap();
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.is_empty() {
        assert!(Instant::now() < deadline, "the seal did not finish");
        got.extend(log.take_sealed());
        std::thread::sleep(MS);
    }
    log.shutdown().unwrap();
    let files = sealed_files(&fs.inner);
    for d in &got {
        assert_eq!(*d, expected(d.id.file_id, &files[&d.id.file_id]));
    }
}
