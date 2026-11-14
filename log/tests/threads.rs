//! The writer on its threads (`kvik-writer`, `kvik-maint`): concurrent callers, back-pressure, a failed sync, and a
//! smoke test on the real file system. Timing is tested on the synchronous core (`tests/writer.rs`); nothing here
//! depends on how long anything takes.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::Entry;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{FaultOp, Fs, MemFs, StdFs};
use kvik_log::mark::read_mark;
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, SealState, scan};
use kvik_log::store::{NewStore, StoreDir, StoreError};
use kvik_log::writer::{
    Ack, CommitError, Log, LogWriter, SubmitError, SystemClock, Wait, WriterConfig,
};

const STORE_ID: [u8; 16] = [3; 16];
const T0: i64 = 1_767_225_600_000_000_000;

fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 64 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    }
}

fn fact(series: u32, i: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + i,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(i as u64),
        changed_at: None,
    })
}

fn start(fs: Arc<dyn Fs>, dir: &Path, cfg: WriterConfig) -> Log {
    let store = StoreDir::open(fs, dir, Some(&new_store())).unwrap();
    let w = LogWriter::open(store, cfg, Some([5; 16]), Duration::ZERO).unwrap();
    Log::start(w, Arc::new(SystemClock::default())).unwrap()
}

/// Acknowledgements hold dense positions from 1, each call's as long as the call.
fn check_dense(mut acks: Vec<(Ack, usize)>) -> u64 {
    acks.sort_by_key(|(a, _)| a.first);
    let mut next = 1;
    for (a, n) in acks {
        assert_eq!(a.first.sequence, next);
        assert_eq!(a.last.sequence, next + n as u64 - 1);
        next += n as u64;
    }
    next - 1
}

#[test]
fn concurrent_callers_get_dense_positions_and_every_batch_is_on_disk() {
    let fs = MemFs::new();
    let dir = PathBuf::from("store");
    let log = Arc::new(start(Arc::new(fs.clone()), &dir, cfg()));
    let acks = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..4u32)
        .map(|t| {
            let log = log.clone();
            let acks = acks.clone();
            std::thread::spawn(move || {
                let mut pending = Vec::new();
                for i in 0..150i64 {
                    let mut items: Vec<Item> = (0..1 + i % 3).map(|k| fact(t, i * 4 + k)).collect();
                    if i % 37 == 0 {
                        items.push(Item::Entry(Entry::Alias {
                            id: t,
                            name: format!("t{t}-{i}"),
                        }));
                    }
                    let n = items.len();
                    pending.push((log.submit(items).unwrap(), n));
                }
                for (p, n) in pending {
                    acks.lock().unwrap().push((p.wait().unwrap(), n));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let last = check_dense(acks.lock().unwrap().clone());
    let log = Arc::into_inner(log).unwrap();
    assert_eq!(log.stats().durable.sequence, last);
    log.shutdown().unwrap();
    // Every segment reads back completely; sealed ones carry their seal.
    let mut entries = 0;
    for p in fs.paths() {
        let Some(name) = p.file_name().and_then(|n| SegmentName::parse(n.to_str()?)) else {
            continue;
        };
        let bytes = fs.read_durable(&p).unwrap();
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: name.file_id,
            generation: name.generation,
        };
        let r = scan(&bytes, &id, ScanLimit::End).unwrap();
        if name.sealed {
            assert_eq!(r.seal.as_ref().unwrap().state, SealState::Sealed);
        }
        entries += r.batches.iter().map(|b| b.entry_count()).sum::<u64>();
    }
    assert_eq!(entries, last);
    let mark = read_mark(&fs.read_durable(&dir.join("log/MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(mark.last_sequence, last, "clean shutdown syncs the mark");
}

#[test]
fn a_full_queue_is_back_pressure_and_try_submit_names_it() {
    let fs = MemFs::new();
    let held = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
    let log = start(
        Arc::new(fs.clone()),
        Path::new("store"),
        WriterConfig {
            ingest_queue: 4,
            ..cfg()
        },
    );
    let a0 = log.submit(vec![fact(1, 0)]).unwrap().wait().unwrap();
    // Hold the writer inside its next batch sync.
    let h = held.clone();
    fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        let writer = std::thread::current().name() == Some("kvik-writer");
        if writer && p.to_string_lossy().ends_with(".open") && !h.swap(true, Ordering::SeqCst) {
            let (entered, release) = gate.lock().unwrap().take().unwrap();
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
    })));
    let first = log.submit(vec![fact(1, 1)]).unwrap();
    entered_rx.recv().unwrap();
    let queued: Vec<_> = (0..4)
        .map(|i| log.try_submit(vec![fact(2, i)]).unwrap())
        .collect();
    assert_eq!(
        log.try_submit(vec![fact(2, 9)]).unwrap_err(),
        SubmitError::Busy(Wait::QueueFull)
    );
    assert_eq!(log.queue_depth(), 4);
    release_tx.send(()).unwrap();
    let mut acks = vec![(a0, 1), (first.wait().unwrap(), 1)];
    acks.extend(queued.into_iter().map(|p| (p.wait().unwrap(), 1)));
    assert_eq!(check_dense(acks), 6);
    fs.set_sync_hook(None);
    log.shutdown().unwrap();
}

struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

#[test]
fn a_failed_sync_fails_the_calls_it_covered_and_the_store_turns_read_only() {
    let fs = MemFs::new();
    let log = start(Arc::new(fs.clone()), Path::new("store"), cfg());
    // The acknowledgement is a future that any executor can await.
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut p = pin!(log.submit(vec![fact(1, 0)]).unwrap());
    let ack = loop {
        if let Poll::Ready(r) = p.as_mut().poll(&mut Context::from_waker(&waker)) {
            break r.unwrap();
        }
        std::thread::park_timeout(Duration::from_millis(100));
    };
    assert_eq!(ack.first.sequence, 1);
    // Wait until the next segment is created ahead (its sync done), so the next `.open` sync is the batch's.
    let next = PathBuf::from("store/log/00000000-0000000000000002.open");
    while fs.read_durable(&next).map(|b| b.len()) != Some(64 << 10) {
        std::thread::sleep(Duration::from_millis(1));
    }
    fs.fail_nth(FaultOp::Sync, ".open", 0, std::io::ErrorKind::Other);
    let r = log.submit(vec![fact(1, 1)]).unwrap().wait();
    assert_eq!(r, Err(CommitError::OutcomeUnknown));
    match log.submit(vec![fact(1, 2)]) {
        Err(SubmitError::ReadOnly) => {}
        Ok(p) => assert_eq!(p.wait(), Err(CommitError::ReadOnly)),
        Err(e) => panic!("{e}"),
    }
    assert!(log.fault().is_some());
    assert!(log.shutdown().is_err());
}

#[test]
fn smoke_on_the_real_file_system() {
    let dir = std::env::temp_dir().join(format!("kvik-log-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fs: Arc<dyn Fs> = Arc::new(StdFs);
    let log = start(fs.clone(), &dir, cfg());
    assert!(matches!(
        StoreDir::open(fs.clone(), &dir, None),
        Err(StoreError::Locked)
    ));
    let mut pending = Vec::new();
    for i in 0..400i64 {
        let mut items = vec![fact(1, i), fact(2, i)];
        if i % 100 == 0 {
            items.push(Item::Entry(Entry::Alias {
                id: 1,
                name: format!("n{i}"),
            }));
        }
        let n = items.len();
        pending.push((log.submit(items).unwrap(), n));
    }
    let acks: Vec<_> = pending
        .into_iter()
        .map(|(p, n)| (p.wait().unwrap(), n))
        .collect();
    let last = check_dense(acks);
    log.shutdown().unwrap();

    let mut seen = Vec::new();
    for e in std::fs::read_dir(dir.join("log")).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        let Some(n) = SegmentName::parse(&name) else {
            continue;
        };
        let bytes = std::fs::read(e.path()).unwrap();
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: n.file_id,
            generation: n.generation,
        };
        let r = scan(&bytes, &id, ScanLimit::End).unwrap();
        if n.sealed {
            let seal = r.seal.as_ref().unwrap();
            assert!(seal.file_crc_matches(&bytes));
        }
        for b in &r.batches {
            seen.push(BatchContent::decode(b).unwrap());
        }
    }
    seen.sort_by_key(|c| c.first_sequence);
    let mut next = 1;
    for c in &seen {
        assert_eq!(c.first_sequence, next);
        next = c.last_sequence() + 1;
    }
    assert_eq!(next - 1, last);
    let mark = read_mark(&std::fs::read(dir.join("log/MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(mark.last_sequence, last);
    // The store is closed: its lock is free again, and its log needs recovery to be opened (roadmap 4.5).
    let store = StoreDir::open(fs, &dir, None).unwrap();
    assert!(LogWriter::open(store, cfg(), Some([5; 16]), Duration::ZERO).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
