//! The second-process reader and leases (roadmap 4.6): a reader on the same files as a writer, driven
//! deterministically on the in-memory file system and on threads, on both file systems; leases with an injectable
//! clock; and a property test interleaving writer steps, crashes, restarts and reader refreshes.
//!
//! A reader handle survives every crash of the writer process here. It is also kept across a simulated power loss,
//! which a real reader on the same machine would not survive: that only makes the checks stronger.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::Entry;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{Fs, MemFs, OpenMode, StdFs};
use kvik_log::lease::{LEASE_EXPIRY, LeaseSlot, LeaseStatus, lease_name};
use kvik_log::manifest::{LossCause, read_manifest};
use kvik_log::mark::{MarkSlot, encode_mark_file, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig, ReaderError, Update, VoidCause};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, Clock, IngestQueue, Log, LogWriter, MarkSync, StartRule, WriterConfig,
    create_segment, seal_segment,
};
use proptest::prelude::*;

const STORE_ID: [u8; 16] = [6; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const MS: Duration = Duration::from_millis(1);

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn log_path(name: &str) -> PathBuf {
    dir().join("log").join(name)
}

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
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        mark_sync: MarkSync::Interval(Duration::from_millis(30)),
        ..WriterConfig::default()
    }
}

fn fact(series: u32, t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t: T0 + t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

fn alias(id: u32) -> Item {
    Item::Entry(Entry::Alias {
        id,
        name: format!("a{id}"),
    })
}

/// A clock the test moves by hand.
#[derive(Debug, Default)]
struct ManualClock(Mutex<Duration>);

impl ManualClock {
    fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn wall(&self) -> i64 {
        T0 + self.monotonic().as_nanos() as i64
    }
}

/// What a reader holds: its items by position, after applying every void.
#[derive(Debug, Default)]
struct Held {
    items: BTreeMap<u64, Item>,
    voids: Vec<VoidCause>,
    corruption: usize,
}

impl Held {
    /// Applies an update, checking that nothing returned is above the mark it was read under and that every batch
    /// lies above everything still held.
    fn apply(&mut self, u: &Update) {
        for e in &u.events {
            match e {
                ReadEvent::Batch(b) => {
                    assert!(
                        b.last <= u.state.durable,
                        "batch {:?} above the mark {:?}",
                        b.last,
                        u.state.durable
                    );
                    assert!(
                        self.items
                            .keys()
                            .last()
                            .is_none_or(|&k| k < b.first.sequence),
                        "batch {:?} not above what is held",
                        b.first
                    );
                    for (k, item) in b.content.items.iter().enumerate() {
                        self.items.insert(b.first.sequence + k as u64, item.clone());
                    }
                }
                ReadEvent::Void { above, cause } => {
                    self.items.retain(|&k, _| k <= above.sequence);
                    self.voids.push(*cause);
                }
                ReadEvent::Corruption { .. } => self.corruption += 1,
            }
        }
    }
}

/// A writer driven step by step, a reader on the same file system, and every acknowledged entry by position.
struct Rig {
    fs: MemFs,
    clock: Arc<ManualClock>,
    boot: u8,
    cfg: WriterConfig,
    w: Option<LogWriter>,
    q: IngestQueue,
    calls: BTreeMap<u64, Vec<Item>>,
    acked: BTreeMap<u64, Item>,
    reader: Option<LogReader>,
    held: Held,
    readers: u8,
}

impl Rig {
    fn new(cfg: WriterConfig) -> Self {
        let mut r = Self {
            fs: MemFs::new(),
            clock: Arc::new(ManualClock::default()),
            boot: 1,
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            w: None,
            calls: BTreeMap::new(),
            acked: BTreeMap::new(),
            reader: None,
            held: Held::default(),
            readers: 0,
        };
        assert!(r.open(), "the first start");
        r.open_reader().expect("a reader");
        r
    }

    fn boot_id(&self) -> [u8; 16] {
        [self.boot; 16]
    }

    fn now(&self) -> Duration {
        self.clock.monotonic()
    }

    fn open(&mut self) -> bool {
        self.w = None;
        let Ok(store) = StoreDir::open(Arc::new(self.fs.clone()), &dir(), Some(&new_store()))
        else {
            return false;
        };
        let Ok(w) = LogWriter::open(
            store,
            self.cfg.clone(),
            Some(self.boot_id()),
            self.now(),
            self.clock.wall(),
        ) else {
            return false;
        };
        if w.fault().is_some() {
            return false;
        }
        self.w = Some(w);
        self.q = IngestQueue::new(self.cfg.ingest_queue);
        self.calls.clear();
        self.maint();
        true
    }

    fn reader_cfg(&mut self) -> ReaderConfig {
        self.readers += 1;
        ReaderConfig::new(
            dir(),
            Arc::new(self.fs.clone()),
            self.clock.clone(),
            Some(self.boot_id()),
            [self.readers; 16],
        )
    }

    fn open_reader(&mut self) -> Result<(), ReaderError> {
        self.reader = None;
        self.held = Held::default();
        let c = self.reader_cfg();
        self.reader = Some(LogReader::open(c)?);
        Ok(())
    }

    fn refresh(&mut self) -> Result<Update, ReaderError> {
        let u = self.reader.as_mut().expect("a reader").refresh()?;
        self.held.apply(&u);
        Ok(u)
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items.clone(), self.cfg.call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.calls.insert(id, items);
    }

    fn maint(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let fs = self.fs.clone();
        let (now, wall) = (self.clock.monotonic(), self.clock.wall());
        for _ in 0..2 {
            if let Some(req) = w.segment_request(wall) {
                let r = create_segment(&fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(now) {
                let r = seal_segment(&fs, &job, &w.halt());
                w.seal_done(job, r, now);
            }
        }
    }

    fn step(&mut self) {
        let (now, wall) = (self.now(), self.clock.wall());
        let Some(w) = self.w.as_mut() else { return };
        let s = w.step(now, wall, &mut self.q);
        for (id, r) in s.done {
            if let Ok(a) = r {
                for (k, item) in self.calls[&id].iter().enumerate() {
                    self.acked.insert(a.first.sequence + k as u64, item.clone());
                }
            }
        }
        self.maint();
    }

    fn drain(&mut self) {
        for _ in 0..1000 {
            if self.q.is_empty() || self.w.as_ref().is_none_or(|w| w.fault().is_some()) {
                return;
            }
            self.step();
            self.clock.advance(10 * MS);
        }
    }

    fn kill(&mut self) {
        self.w = None;
        self.fs.restart();
    }

    fn mark(&self) -> Option<MarkSlot> {
        let b = self.fs.read(&log_path("MARK"))?;
        read_mark(&b, &STORE_ID).ok()?.map(|m| m.1)
    }

    /// Every complete batch of every segment as the files now hold them.
    fn batches(&self) -> Vec<BatchContent> {
        let mut out = Vec::new();
        let mut segs: Vec<(SegmentName, PathBuf)> = self
            .fs
            .paths()
            .into_iter()
            .filter_map(|p| Some((SegmentName::parse(p.file_name()?.to_str()?)?, p.clone())))
            .collect();
        segs.sort_by_key(|s| s.0.file_id);
        for (n, p) in segs {
            let bytes = self.fs.read(&p).unwrap();
            let id = SegmentIdentity {
                store_id: STORE_ID,
                file_id: n.file_id,
                generation: n.generation,
            };
            for b in &scan(&bytes, &id, ScanLimit::End).unwrap().batches {
                out.push(BatchContent::decode(b).unwrap());
            }
        }
        out
    }

    fn positions(&self) -> BTreeMap<u64, Item> {
        let mut m = BTreeMap::new();
        for c in self.batches() {
            for (k, item) in c.items.into_iter().enumerate() {
                m.insert(c.first_sequence + k as u64, item);
            }
        }
        m
    }

    /// What the reader holds is what the files hold at those positions.
    fn check_held(&self) {
        let have = self.positions();
        for (k, item) in &self.held.items {
            assert_eq!(have.get(k), Some(item), "held position {k}");
        }
    }

    /// After a refresh with the writer drained: the reader holds exactly the files' positions up to the mark.
    fn check_caught_up(&self) {
        let m = self.mark().expect("a readable mark");
        let have: BTreeMap<u64, Item> = self
            .positions()
            .into_iter()
            .filter(|(k, _)| *k <= m.last_sequence)
            .collect();
        assert_eq!(self.held.items, have);
    }
}

fn items(n: usize, base: i64) -> Vec<Item> {
    (0..n).map(|k| fact(k as u32, base + k as i64)).collect()
}

#[test]
fn a_reader_on_a_new_store_holds_nothing_and_writes_its_lease() {
    let mut r = Rig::new(cfg());
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty());
    assert_eq!(u.state.durable.sequence, 0);
    let path = dir().join("leases").join(lease_name(&[r.readers; 16]));
    let lease = LeaseSlot::decode_file(&r.fs.read(&path).unwrap(), &STORE_ID).unwrap();
    assert_eq!(lease.boot_id, r.boot_id());
    assert_eq!(lease.epoch, u.state.epoch);
    // One at open, one at the refresh.
    assert_eq!(lease.refresh_counter, 2);
}

#[test]
fn a_reader_takes_every_acknowledged_batch_and_nothing_above_the_mark() {
    let mut r = Rig::new(cfg());
    for k in 0..30 {
        r.submit(items(1 + k % 5, k as i64 * 10));
        r.step();
        r.clock.advance(7 * MS);
        if k % 3 == 0 {
            r.refresh().unwrap();
            r.check_held();
        }
    }
    r.drain();
    let u = r.refresh().unwrap();
    assert_eq!(u.state.durable.sequence, *r.acked.keys().last().unwrap());
    assert_eq!(r.held.items, r.acked);
    r.check_caught_up();

    // A complete batch written beyond the mark, as one whose sync has not returned, is not taken.
    let m = r.mark().unwrap();
    let name = SegmentName {
        generation: 0,
        file_id: m.file_id,
        sealed: false,
    };
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: m.file_id,
        generation: 0,
    };
    let content = BatchContent {
        first_sequence: m.last_sequence + 1,
        received_at: T0,
        items: items(2, 999),
    };
    let bytes = content.encode(&id, m.offset, T0).unwrap();
    r.fs.poke(&log_path(&name.to_string()), m.offset as usize, &bytes);
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty(), "{:?}", u.events);
    assert_eq!(u.state.durable.sequence, m.last_sequence);
}

#[test]
fn a_reader_follows_segment_rolls_and_seals() {
    let mut r = Rig::new(WriterConfig {
        segment_size: 8 << 10,
        max_batch_bytes: 2 << 10,
        ..cfg()
    });
    for k in 0..200 {
        r.submit(items(1 + k % 7, k as i64 * 10));
        r.step();
        r.clock.advance(3 * MS);
        if k % 11 == 0 {
            r.refresh().unwrap();
        }
    }
    r.drain();
    r.refresh().unwrap();
    let sealed =
        r.fs.paths()
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".kvl"))
            .count();
    assert!(sealed >= 3, "{sealed} sealed segments");
    assert_eq!(r.held.items, r.acked);
    // A second reader opened now reads the sealed segments whole.
    r.open_reader().unwrap();
    r.refresh().unwrap();
    assert_eq!(r.held.items, r.acked);
}

#[test]
fn a_writer_restart_and_a_reboot_are_followed_and_a_recovery_entry_voids_nothing_below_it() {
    let mut r = Rig::new(cfg());
    r.submit(items(3, 0));
    r.drain();
    r.refresh().unwrap();
    // A kill on the same boot: rule 1 keeps everything up to the mark.
    r.kill();
    assert!(r.open());
    r.submit(items(2, 100));
    r.drain();
    r.refresh().unwrap();
    assert_eq!(r.held.items, r.acked);
    // A reboot: a recovery entry follows the last batch.
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    r.boot += 1;
    assert!(r.open());
    r.submit(items(1, 200));
    r.drain();
    let u = r.refresh().unwrap();
    assert!(
        u.events.iter().any(
            |e| matches!(e, ReadEvent::Batch(b) if b.recovery().is_some_and(|e| e.after == 5))
        )
    );
    assert!(r.held.voids.is_empty());
    r.check_caught_up();
    for (k, item) in &r.acked {
        assert_eq!(r.held.items.get(k), Some(item));
    }
}

#[test]
fn a_returned_batch_truncated_after_a_page_cache_loss_on_the_same_boot_is_voided() {
    let mut r = Rig::new(WriterConfig {
        mark_sync: MarkSync::Interval(Duration::from_secs(1)),
        ..cfg()
    });
    r.submit(vec![alias(1)]);
    r.drain();
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    assert_eq!(r.held.items.len(), 3);
    // The mark on disk names position 1 only; the page cache is lost on the same boot.
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    // Before the writer starts again, the files still hold the batch: nothing is voided, nothing new is taken.
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty());
    assert_eq!(u.state.durable.sequence, 1);
    let mut w_report = None;
    if r.open() {
        w_report = Some(r.w.as_ref().unwrap().start_report().rule);
    }
    assert_eq!(w_report, Some(StartRule::SameBoot));
    // Positions 2 and 3 are assigned again to other facts.
    r.submit(items(3, 500));
    r.drain();
    let u = r.refresh().unwrap();
    assert!(matches!(
        u.events[0],
        ReadEvent::Void {
            above,
            cause: VoidCause::Rewritten
        } if above.sequence == 1
    ));
    // The truncation was a reset point: a recovery entry at position 2 and the stamp the reader now holds.
    let stamp = r.mark().unwrap().stamp;
    assert_eq!(stamp.sequence, 2);
    assert_ne!(stamp.nonce, 0);
    assert_eq!(u.state.stamp, stamp);
    assert!(matches!(r.held.items[&2], Item::Entry(Entry::Recovery(_))));
    r.check_held();
    r.check_caught_up();
}

#[test]
fn a_damaged_tail_truncated_under_rule_3_is_voided_and_the_entry_is_taken() {
    let mut r = Rig::new(cfg());
    for k in 0..4 {
        r.submit(items(2, k * 10));
        r.drain();
    }
    r.refresh().unwrap();
    assert_eq!(r.held.items.len(), 8);
    // The writer stops; the mark is lost and the last batch is damaged: the start treats it as a torn tail.
    r.kill();
    let m = r.mark().unwrap();
    r.fs.remove_file(&log_path("MARK")).unwrap();
    let seg = log_path(
        &SegmentName {
            generation: 0,
            file_id: m.file_id,
            sealed: false,
        }
        .to_string(),
    );
    r.fs.poke(&seg, m.offset as usize - 40, &[0xee; 16]);
    assert!(r.open());
    let rep = r.w.as_ref().unwrap().start_report().clone();
    assert_eq!(rep.entry.as_ref().unwrap().entry.after, 6);
    r.submit(items(1, 900));
    r.drain();
    r.refresh().unwrap();
    // The last batch returned no longer reads the same: what was taken above the one before it is void.
    assert_eq!(r.held.voids, [VoidCause::Rewritten]);
    r.check_held();
    r.check_caught_up();
}

/// A start truncates every batch from position `from` on, more than the reader remembers; the log starts with a
/// recovery entry the reader has read, so with `from` 1 the new entry lands at that entry's offset.
fn long_truncation(from: u64) {
    let mut r = Rig::new(WriterConfig {
        segment_size: 64 << 10,
        ..cfg()
    });
    // A reboot first: its recovery entry, at position 1, is read before the batches.
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    r.boot += 1;
    assert!(r.open());
    for k in 0..89 {
        r.submit(items(1, k * 10));
        r.drain();
    }
    r.refresh().unwrap();
    assert_eq!(r.held.items.len(), 90);
    assert!(matches!(r.held.items[&1], Item::Entry(Entry::Recovery(_))));
    // Every batch from position `from` on is damaged and the mark is lost: the start truncates them all.
    r.kill();
    let m = r.mark().unwrap();
    r.fs.remove_file(&log_path("MARK")).unwrap();
    let seg = log_path(
        &SegmentName {
            generation: 0,
            file_id: m.file_id,
            sealed: false,
        }
        .to_string(),
    );
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: m.file_id,
        generation: 0,
    };
    let found = scan(&bytes, &id, ScanLimit::End).unwrap();
    assert_eq!(found.batches.len(), 90, "one segment");
    for b in found
        .batches
        .iter()
        .filter(|b| b.commit.first_sequence >= from)
    {
        r.fs.poke(&seg, b.commit_offset() as usize, &[0xee; 16]);
    }
    assert!(r.open());
    assert_eq!(
        r.w.as_ref()
            .unwrap()
            .start_report()
            .entry
            .as_ref()
            .unwrap()
            .entry
            .after,
        from - 1
    );
    r.refresh().unwrap();
    // None of the remembered batches reads the same: the first void stops at the oldest of them; the log is read
    // again from the segment's start, where the recovery entries void the rest.
    assert_eq!(r.held.voids[0], VoidCause::Rewritten);
    assert!(r.held.voids.len() >= 2);
    assert!(
        r.held.voids[1..]
            .iter()
            .all(|v| matches!(v, VoidCause::Recovery { .. }))
    );
    r.check_held();
    r.check_caught_up();
    assert_eq!(r.held.items.len() as u64, from);
}

#[test]
fn a_truncation_longer_than_what_the_reader_remembers_is_voided_by_the_recovery_entry() {
    long_truncation(11);
}

#[test]
fn a_recovery_entry_written_where_an_earlier_one_was_read_is_read_as_new() {
    long_truncation(1);
}

#[test]
fn a_missing_or_unreadable_mark_is_mark_unreadable_and_changes_nothing() {
    let fs = MemFs::new();
    let clock = Arc::new(ManualClock::default());
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    drop(store);
    // A store whose first start has not written the mark.
    let c = ReaderConfig::new(dir(), Arc::new(fs.clone()), clock, None, [9; 16]);
    assert!(matches!(
        LogReader::open(c),
        Err(ReaderError::MarkUnreadable)
    ));

    let mut r = Rig::new(cfg());
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    let before = r.reader.as_ref().unwrap().state();
    r.kill();
    r.fs.poke(&log_path("MARK"), 0, &[0xee; 8]);
    r.fs.poke(&log_path("MARK"), 4096, &[0xee; 8]);
    assert!(matches!(r.refresh(), Err(ReaderError::MarkUnreadable)));
    assert_eq!(r.reader.as_ref().unwrap().state(), before);
    // The next start writes the mark again; the reader goes on.
    assert!(r.open());
    r.submit(items(1, 50));
    r.drain();
    r.refresh().unwrap();
    r.check_caught_up();
}

#[test]
fn a_failed_lease_write_or_a_corrupt_manifest_record_fails_the_refresh_and_changes_nothing() {
    let mut r = Rig::new(cfg());
    r.submit(items(2, 0));
    r.drain();
    let before = r.reader.as_ref().unwrap().state();
    r.fs.fail_nth(
        kvik_log::fs::FaultOp::Write,
        ".lease",
        0,
        std::io::ErrorKind::Other,
    );
    assert!(matches!(r.refresh(), Err(ReaderError::Io { .. })));
    assert_eq!(r.reader.as_ref().unwrap().state(), before);
    assert!(r.held.items.is_empty());
    r.fs.restart();
    // A record followed by a valid one with a higher epoch is corrupt: no epoch can be taken.
    let path = dir().join("MANIFEST");
    let man = r.fs.read(&path).unwrap();
    let last = read_manifest(&man, &STORE_ID).unwrap();
    let next = kvik_log::manifest::encode_record(
        kvik_log::manifest::RECORD_EMPTY,
        last.last_epoch() + 1,
        0,
        T0,
        None,
    );
    r.fs.poke(&path, man.len(), &next);
    r.fs.poke(&path, last.records[0].offset as usize + 20, &[0xee; 4]);
    assert!(matches!(r.refresh(), Err(ReaderError::Manifest(_))));
    assert_eq!(r.reader.as_ref().unwrap().state(), before);
    r.fs.poke(&path, 0, &man);
    r.refresh().unwrap();
    r.check_caught_up();
}

#[test]
fn a_mark_slot_of_a_newer_version_is_refused() {
    let mut r = Rig::new(cfg());
    r.kill();
    let mut b = r.fs.read(&log_path("MARK")).unwrap();
    b[8] = 4;
    let crc = kvik_log::crc::crc32c(&b[..124]);
    b[124..128].copy_from_slice(&crc.to_le_bytes());
    r.fs.poke(&log_path("MARK"), 0, &b[..128]);
    assert!(matches!(r.refresh(), Err(ReaderError::Unsupported { .. })));
}

#[test]
fn a_new_generation_voids_everything_and_reads_again() {
    let mut r = Rig::new(cfg());
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    r.kill();
    let mut m = r.mark().unwrap();
    m.generation = 1;
    m.counter += 1;
    r.fs.poke(&log_path("MARK"), 0, &encode_mark_file([Some(m), None]));
    let u = r.refresh().unwrap();
    assert!(matches!(
        u.events[0],
        ReadEvent::Void {
            cause: VoidCause::Generation { from: 0, to: 1 },
            ..
        }
    ));
    // The generation-0 batches are read again (how a restore sets positions aside is roadmap 8.3).
    assert_eq!(r.held.items.len(), 2);
}

#[test]
fn leases_are_renewed_expired_voided_and_epochs_continue_above_them() {
    let mut r = Rig::new(cfg());
    let leases = dir().join("leases");
    let mine = leases.join(lease_name(&[r.readers; 16]));
    // A lease of another boot, an invalid lease and a lease naming a high epoch.
    let void = LeaseSlot {
        epoch: 1,
        refresh_counter: 1,
        boot_id: [0xbb; 16],
        created_at: T0,
    };
    let write = |name: [u8; 16], bytes: &[u8]| {
        let f =
            r.fs.open(&leases.join(lease_name(&name)), OpenMode::CreateNew)
                .unwrap();
        f.write_at(bytes, 0).unwrap();
    };
    write([0xa1; 16], &void.encode_file(STORE_ID, 0));
    write([0xa2; 16], &[0x55; 128]);
    let high = LeaseSlot {
        epoch: 50,
        refresh_counter: 1,
        boot_id: r.boot_id(),
        created_at: T0,
    };
    write([0xa3; 16], &high.encode_file(STORE_ID, 0));
    // A restart that records a loss: its manifest record's epoch is above the lease's 50.
    r.submit(items(2, 0));
    r.drain();
    r.kill();
    let m = r.mark().unwrap();
    let seg = log_path(
        &SegmentName {
            generation: 0,
            file_id: m.file_id,
            sealed: false,
        }
        .to_string(),
    );
    r.fs.poke(&seg, 64 + 8, &[0xee; 8]);
    assert!(r.open());
    let man = read_manifest(&r.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID).unwrap();
    let rec = man.records.last().unwrap();
    assert_eq!(rec.lost.as_ref().unwrap().cause, LossCause::Corruption);
    assert_eq!(rec.epoch, 51);
    // At the start the void lease was deleted; the others count as first observed then.
    let w = r.w.as_ref().unwrap();
    assert!(r.fs.read(&leases.join(lease_name(&[0xa1; 16]))).is_none());
    let seen = w.leases().observed();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[&[0xa2; 16]].epoch(), 0);
    assert_eq!(w.leases().oldest_live_epoch(r.now()), Some(0));
    let started = r.now();

    // The reader renews within the hour; the invalid and the high lease do not change.
    for _ in 0..5 {
        r.clock.advance(Duration::from_secs(15 * 60));
        let renewed = r.reader.as_mut().unwrap().keep_alive().unwrap();
        assert!(renewed);
        r.step();
    }
    let w = r.w.as_ref().unwrap();
    assert!(r.now() >= started + LEASE_EXPIRY);
    assert!(r.fs.read(&leases.join(lease_name(&[0xa2; 16]))).is_none());
    assert!(r.fs.read(&leases.join(lease_name(&[0xa3; 16]))).is_none());
    assert!(r.fs.read(&mine).is_some());
    let o = w.leases().observed()[&[r.readers; 16]];
    assert_eq!(w.leases().status(&o, r.now()), LeaseStatus::Live);

    // A reader idle past the expiry: the writer deletes its lease; its next call creates it again with the
    // manifest's current epoch.
    r.clock.advance(LEASE_EXPIRY + Duration::from_secs(60));
    r.step();
    assert!(r.fs.read(&mine).is_none());
    assert!(r.reader.as_mut().unwrap().keep_alive().unwrap());
    let lease = LeaseSlot::decode_file(&r.fs.read(&mine).unwrap(), &STORE_ID).unwrap();
    assert_eq!(lease.epoch, 51);
    // Within half of the refresh interval nothing is written.
    r.clock.advance(Duration::from_secs(60));
    assert!(!r.reader.as_mut().unwrap().keep_alive().unwrap());
    // Close deletes the lease.
    r.reader.take().unwrap().close().unwrap();
    assert!(r.fs.read(&mine).is_none());
}

#[test]
fn an_invalid_lease_and_a_lease_without_boot_id_live_until_expiry() {
    let mut r = Rig::new(cfg());
    let leases = dir().join("leases");
    let zero = LeaseSlot {
        epoch: 4,
        refresh_counter: 9,
        boot_id: [0; 16],
        created_at: T0,
    };
    let f =
        r.fs.open(&leases.join(lease_name(&[0xc1; 16])), OpenMode::CreateNew)
            .unwrap();
    f.write_at(&zero.encode_file(STORE_ID, 0), 0).unwrap();
    // A reader between creating its lease and writing it.
    r.fs.open(&leases.join(lease_name(&[0xc2; 16])), OpenMode::CreateNew)
        .unwrap();
    r.clock.advance(Duration::from_secs(120));
    r.step();
    let seen = r.w.as_ref().unwrap().leases().observed().clone();
    assert!(seen.contains_key(&[0xc1; 16]) && seen.contains_key(&[0xc2; 16]));
    r.clock.advance(LEASE_EXPIRY);
    r.step();
    let seen = r.w.as_ref().unwrap().leases().observed();
    assert!(!seen.contains_key(&[0xc1; 16]) && !seen.contains_key(&[0xc2; 16]));
}

/// Runs `f` on its own thread and fails with a message if it does not finish within `limit`, so that a hang is a
/// failure, never a stalled run.
fn bounded<T: Send + 'static>(
    what: &str,
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(limit) {
        Ok(v) => {
            h.join().unwrap();
            v
        }
        // The thread panicked: report its panic.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match h.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => unreachable!("sent nothing and did not panic"),
        },
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} did not finish within {} s", limit.as_secs())
        }
    }
}

fn start_log(fs: &Arc<dyn Fs>, root: &Path) -> Log {
    let store = StoreDir::open(fs.clone(), root, Some(&new_store())).unwrap();
    let writer = LogWriter::open(
        store,
        WriterConfig {
            segment_size: 64 << 10,
            ..cfg()
        },
        Some([1; 16]),
        Duration::ZERO,
        T0,
    )
    .unwrap();
    Log::start(writer, Arc::new(kvik_log::writer::SystemClock::default())).unwrap()
}

/// A writer on its threads and a reader on another thread, on the given file system: the reader never holds
/// anything the files do not, and catches up with every acknowledged entry. With `restart`, the writer is shut
/// down half way and opened again while the reader goes on.
fn concurrent(fs: Arc<dyn Fs>, root: PathBuf, restart: bool) {
    let mut log = start_log(&fs, &root);
    let clock = Arc::new(kvik_log::writer::SystemClock::default());
    let rc = ReaderConfig::new(&root, fs.clone(), clock, Some([1; 16]), [3; 16]);
    let done = Arc::new(Mutex::new(None::<u64>));
    let d = done.clone();
    let reader = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut reader = LogReader::open(rc).unwrap();
        let mut held = Held::default();
        loop {
            let u = reader.refresh().unwrap();
            held.apply(&u);
            assert!(held.voids.is_empty() && held.corruption == 0);
            if let Some(last) = *d.lock().unwrap()
                && u.state.durable.sequence >= last
            {
                reader.close().unwrap();
                return held.items;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reader did not catch up within 60 s: durable {:?}, waiting for {:?}",
                u.state.durable,
                *d.lock().unwrap()
            );
            // Leave the file system to the writer between refreshes.
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut acked = BTreeMap::new();
    let mut pending = Vec::new();
    let settle = |pending: &mut Vec<(kvik_log::writer::Pending, Vec<Item>)>,
                  acked: &mut BTreeMap<u64, Item>| {
        for (p, it) in pending.drain(..) {
            let a = p.wait().unwrap();
            for (j, item) in it.into_iter().enumerate() {
                acked.insert(a.first.sequence + j as u64, item);
            }
        }
    };
    for k in 0..400u32 {
        if restart && k == 200 {
            settle(&mut pending, &mut acked);
            log.shutdown().unwrap();
            log = start_log(&fs, &root);
        }
        let mut it = items(1 + (k % 9) as usize, i64::from(k) * 100);
        if k % 50 == 0 {
            it.push(alias(k + 1));
        }
        pending.push((log.submit(it.clone()).unwrap(), it));
        if pending.len() >= 8 {
            settle(&mut pending, &mut acked);
        }
    }
    settle(&mut pending, &mut acked);
    *done.lock().unwrap() = acked.keys().last().copied();
    let held = reader.join().unwrap();
    log.shutdown().unwrap();
    assert_eq!(held, acked);
}

#[test]
fn a_writer_and_a_reader_on_threads_on_the_in_memory_file_system() {
    bounded("the threaded run", Duration::from_secs(120), || {
        concurrent(Arc::new(MemFs::new()), dir(), false)
    });
}

#[test]
fn a_writer_restarted_mid_run_and_a_reader_on_threads() {
    bounded("the threaded run", Duration::from_secs(120), || {
        concurrent(Arc::new(MemFs::new()), dir(), true)
    });
}

#[test]
fn a_writer_and_a_reader_on_threads_on_the_real_file_system() {
    let root = std::env::temp_dir().join(format!("kvik-log-reader-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let r = root.clone();
    bounded("the threaded run", Duration::from_secs(120), move || {
        concurrent(Arc::new(StdFs), r, false)
    });
    // The reader deleted its lease on close.
    assert_eq!(std::fs::read_dir(root.join("leases")).unwrap().count(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[derive(Clone, Debug)]
enum Op {
    /// A call of n facts, with an alias entry if set.
    Submit(usize, bool),
    /// Writer turns until the queue is empty.
    Drain,
    /// The reader refreshes.
    Refresh,
    /// The writer process is killed, and started again.
    Kill,
    /// A power loss (partial, by seed), on the same boot (a remount) or with a reboot; the writer starts again.
    Power(u64, bool),
    /// The writer is killed, its mark is deleted or damaged, and it starts again.
    LoseMark(bool),
    /// A new reader handle replaces the old one.
    Reopen,
    /// Time passes.
    Advance(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1..10usize, prop::bool::weighted(0.15)).prop_map(|(n, e)| Op::Submit(n, e)),
        4 => Just(Op::Drain),
        6 => Just(Op::Refresh),
        1 => Just(Op::Kill),
        1 => (any::<u64>(), any::<bool>()).prop_map(|(s, b)| Op::Power(s, b)),
        1 => any::<bool>().prop_map(Op::LoseMark),
        1 => Just(Op::Reopen),
        1 => (1..2_000u64).prop_map(Op::Advance),
    ]
}

/// Runs the operations, refreshing as told and checking after every refresh; at the end the writer is drained and
/// the reader must hold exactly the files' positions up to the mark, and every acknowledged entry except those above
/// the mark on disk at a remount (the accepted limit of `spec.md` §3 Recovery rule 1).
fn interleave(ops: &[Op], every: bool) {
    let mut r = Rig::new(WriterConfig {
        segment_size: 8 << 10,
        max_batch_bytes: 2 << 10,
        mark_sync: if every {
            MarkSync::EveryCommit
        } else {
            MarkSync::Interval(Duration::from_millis(30))
        },
        ..cfg()
    });
    let mut series = 0u32;
    let mut t = 0i64;
    for o in ops {
        match o {
            Op::Submit(n, entry) => {
                let mut it = items(*n, t);
                t += 100;
                if *entry {
                    series += 1;
                    it.push(alias(series));
                }
                if r.w.is_some() {
                    r.submit(it);
                    r.step();
                    r.clock.advance(7 * MS);
                }
            }
            Op::Drain => r.drain(),
            Op::Refresh => match r.refresh() {
                Ok(_) => r.check_held(),
                Err(ReaderError::MarkUnreadable) => {}
                Err(e) => panic!("refresh: {e}"),
            },
            Op::Kill => {
                r.kill();
                assert!(r.open());
            }
            Op::Power(seed, same_boot) => {
                r.w = None;
                r.fs.power_loss_partial(*seed);
                r.fs.restart();
                if *same_boot {
                    // The accepted limit of rule 1: what is above the mark on disk may be truncated. Only those
                    // positions are exempt from the final check; they count again once acknowledged again.
                    if let Some(m) = r.mark() {
                        r.acked.retain(|&k, _| k <= m.last_sequence);
                    }
                } else {
                    r.boot = r.boot.wrapping_add(1).max(1);
                }
                assert!(r.open());
            }
            Op::LoseMark(delete) => {
                r.kill();
                let m = log_path("MARK");
                if *delete {
                    let _ = r.fs.remove_file(&m);
                } else {
                    r.fs.poke(&m, 0, &[0xee; 8]);
                    r.fs.poke(&m, 4096, &[0xee; 8]);
                }
                // Before the writer starts again the reader may find no mark.
                match r.refresh() {
                    Ok(_) => r.check_held(),
                    Err(ReaderError::MarkUnreadable) => {}
                    Err(e) => panic!("refresh: {e}"),
                }
                assert!(r.open());
            }
            Op::Reopen => {
                let boot = r.boot_id();
                r.reader = None;
                r.held = Held::default();
                let mut c = r.reader_cfg();
                c.boot_id = Some(boot);
                r.reader = Some(LogReader::open(c).unwrap());
            }
            Op::Advance(ms) => {
                r.clock.advance(Duration::from_millis(*ms));
                r.step();
            }
        }
    }
    r.drain();
    r.refresh().unwrap();
    r.check_held();
    r.check_caught_up();
    for (k, item) in &r.acked {
        assert_eq!(r.held.items.get(k), Some(item), "acknowledged position {k}");
    }
}

proptest! {
    /// Writer steps, crashes, restarts and reader refreshes in any order: the reader never takes a batch above the
    /// mark, never keeps one the files no longer hold without voiding it, and catches up after a refresh.
    #[test]
    fn a_reader_interleaved_with_writer_steps_crashes_and_restarts(
        ops in prop::collection::vec(op(), 1..60),
        every in any::<bool>(),
    ) {
        interleave(&ops, every);
    }
}

/// Found by the property test: after a remount, positions 4 to 10 are assigned again to other facts in a batch of the
/// same size at the same offset with the same commit time; before Q-119 its commit block was byte for byte the old one.
#[test]
fn a_batch_assigned_again_with_an_identical_commit_block_is_still_voided() {
    use Op::*;
    interleave(
        &[
            Submit(1, false),
            Submit(1, false),
            Submit(1, false),
            Submit(7, false),
            Advance(2),
            Refresh,
            Power(6_786_131_029_947_831_036, true),
            Submit(7, false),
        ],
        false,
    );
    interleave(
        &[
            Submit(1, false),
            Refresh,
            Power(11_450_480_047_445_383_222, true),
        ],
        false,
    );
}

/// Called before each open; `false` reports the file missing for that open.
type MarkHook = Box<dyn FnMut(&MemFs, &Path) -> bool + Send>;

/// The in-memory file system with a hook on every open.
struct HookFs {
    inner: MemFs,
    hook: Mutex<Option<MarkHook>>,
}

impl std::fmt::Debug for HookFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookFs").finish_non_exhaustive()
    }
}

impl Fs for HookFs {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn kvik_log::fs::FsFile>> {
        if let Some(h) = self.hook.lock().unwrap().as_mut()
            && !h(&self.inner, path)
        {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "hidden"));
        }
        self.inner.open(path, mode)
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<kvik_log::fs::DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.free_space(path)
    }
}

/// A rig whose reader goes through a [`HookFs`].
fn hooked_rig() -> (Rig, Arc<HookFs>) {
    let mut r = Rig::new(cfg());
    let fs = Arc::new(HookFs {
        inner: r.fs.clone(),
        hook: Mutex::new(None),
    });
    let mut c = r.reader_cfg();
    c.fs = fs.clone();
    r.reader = Some(LogReader::open(c).unwrap());
    r.held = Held::default();
    (r, fs)
}

/// The in-memory file system counting the bytes read from each file, by name.
#[derive(Debug)]
struct CountFs {
    inner: MemFs,
    read: Arc<Mutex<BTreeMap<String, u64>>>,
}

#[derive(Debug)]
struct CountFile {
    inner: Box<dyn kvik_log::fs::FsFile>,
    name: String,
    read: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl kvik_log::fs::FsFile for CountFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let n = self.inner.read_at(buf, offset)?;
        *self
            .read
            .lock()
            .unwrap()
            .entry(self.name.clone())
            .or_insert(0) += n as u64;
        Ok(n)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> std::io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
}

impl Fs for CountFs {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn kvik_log::fs::FsFile>> {
        Ok(Box::new(CountFile {
            inner: self.inner.open(path, mode)?,
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            read: self.read.clone(),
        }))
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<kvik_log::fs::DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.free_space(path)
    }
}

/// The bytes read from log segments since the last call, and the number of segments read.
fn segment_reads(read: &Mutex<BTreeMap<String, u64>>) -> (u64, usize) {
    let taken = std::mem::take(&mut *read.lock().unwrap());
    let segs: Vec<u64> = taken
        .into_iter()
        .filter(|(n, _)| SegmentName::parse(n).is_some())
        .map(|(_, b)| b)
        .collect();
    (segs.iter().sum(), segs.len())
}

#[test]
fn a_refresh_reads_of_its_segment_only_the_header_the_new_bytes_and_the_last_batch_again() {
    let mut r = Rig::new(WriterConfig {
        segment_size: 1 << 20,
        ..cfg()
    });
    let read = Arc::new(Mutex::new(BTreeMap::new()));
    let mut c = r.reader_cfg();
    c.fs = Arc::new(CountFs {
        inner: r.fs.clone(),
        read: read.clone(),
    });
    r.reader = Some(LogReader::open(c).unwrap());
    r.held = Held::default();
    let mut ends = Vec::new();
    for k in 0..150 {
        r.submit(items(20, k * 100));
        r.drain();
        ends.push(r.mark().unwrap().offset);
    }
    r.refresh().unwrap();
    let (m0, m1) = (ends[ends.len() - 2], ends[ends.len() - 1]);
    assert!(m1 > 64 << 10, "a high offset in the segment: {m1}");
    segment_reads(&read);
    r.submit(items(20, 9_000));
    r.drain();
    let m2 = r.mark().unwrap().offset;
    r.refresh().unwrap();
    let header = kvik_log::header::HEADER_LEN as u64;
    assert_eq!(segment_reads(&read), (header + (m2 - m1) + (m1 - m0), 1));
    // Nothing new: only the check of the last batch reads the segment.
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty());
    assert_eq!(segment_reads(&read), (m2 - m1, 1));
    r.check_caught_up();
}

#[test]
fn a_mark_at_the_header_end_of_a_segment_not_read_before_still_has_its_header_checked() {
    let mut r = Rig::new(cfg());
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    let m = r.mark().unwrap();
    assert_eq!(m.file_id, 1);
    let first =
        r.fs.read(&log_path("00000000-0000000000000001.open"))
            .unwrap();
    let f =
        r.fs.open(
            &log_path("00000000-0000000000000002.open"),
            OpenMode::ReadWrite,
        )
        .unwrap();
    // The segment created ahead, with the first segment's header: the wrong file id.
    f.write_at(&first[..kvik_log::header::HEADER_LEN], 0)
        .unwrap();
    let at_header_end = MarkSlot {
        counter: m.counter + 1,
        file_id: 2,
        offset: kvik_log::header::HEADER_LEN as u64,
        ..m
    };
    r.fs.poke(
        &log_path("MARK"),
        0,
        &encode_mark_file([Some(at_header_end), None]),
    );
    assert!(matches!(r.refresh(), Err(ReaderError::Unsupported { .. })));
}

#[test]
fn the_mark_is_read_at_most_nine_times_per_call() {
    for (hidden, ok) in [(8, true), (9, false)] {
        let (mut r, fs) = hooked_rig();
        r.submit(items(2, 0));
        r.drain();
        let mut left = hidden;
        let reads = Arc::new(Mutex::new(0));
        let n = reads.clone();
        *fs.hook.lock().unwrap() = Some(Box::new(move |_, p: &Path| {
            if !p.ends_with("MARK") {
                return true;
            }
            *n.lock().unwrap() += 1;
            if left > 0 {
                left -= 1;
                false
            } else {
                true
            }
        }));
        let got = r.refresh();
        if ok {
            got.unwrap();
            assert_eq!(r.held.items.len(), 2);
        } else {
            assert!(matches!(got, Err(ReaderError::MarkUnreadable)));
            assert!(r.held.items.is_empty());
            assert_eq!(*reads.lock().unwrap(), 9);
        }
    }
}

#[test]
fn a_mark_that_moves_backwards_during_a_refresh_discards_the_pass() {
    let (mut r, fs) = hooked_rig();
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    let older = r.mark().unwrap();
    r.submit(items(3, 100));
    r.drain();
    let newer = r.mark().unwrap();
    let before = r.reader.as_ref().unwrap().state();
    // The second read of the mark in the refresh finds an older position, as after a start's truncation.
    let mut opens = 0;
    *fs.hook.lock().unwrap() = Some(Box::new(move |m: &MemFs, p: &Path| {
        if !p.ends_with("MARK") {
            return true;
        }
        opens += 1;
        if opens == 2 {
            let back = MarkSlot {
                counter: newer.counter + 1,
                ..older
            };
            m.poke(&log_path("MARK"), 0, &encode_mark_file([Some(back), None]));
        }
        true
    }));
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty());
    assert_eq!(u.state.durable, before.durable);
    assert_eq!(r.held.items.len(), 2);
    // The mark moves on again: the batches are read then.
    *fs.hook.lock().unwrap() = None;
    let ahead = MarkSlot {
        counter: newer.counter + 2,
        ..newer
    };
    r.fs.poke(&log_path("MARK"), 0, &encode_mark_file([Some(ahead), None]));
    r.refresh().unwrap();
    assert_eq!(r.held.items.len(), 5);
}

/// The race a backward check alone leaves: a start truncates and writes on, so the mark is past its old place before
/// the second read; its new reset stamp still discards the pass.
#[test]
fn a_mark_with_a_new_reset_stamp_during_a_refresh_discards_the_pass() {
    let (mut r, fs) = hooked_rig();
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    r.submit(items(3, 100));
    r.drain();
    let newer = r.mark().unwrap();
    let before = r.reader.as_ref().unwrap().state();
    let mut opens = 0;
    *fs.hook.lock().unwrap() = Some(Box::new(move |m: &MemFs, p: &Path| {
        if !p.ends_with("MARK") {
            return true;
        }
        opens += 1;
        if opens == 2 {
            let reset = MarkSlot {
                counter: newer.counter + 1,
                offset: newer.offset + 64,
                last_sequence: newer.last_sequence + 1,
                stamp: kvik_log::mark::ResetStamp::new_at(newer.last_sequence + 1),
                ..newer
            };
            m.poke(&log_path("MARK"), 0, &encode_mark_file([Some(reset), None]));
        }
        true
    }));
    let u = r.refresh().unwrap();
    assert!(u.events.is_empty());
    assert_eq!(u.state.durable, before.durable);
    assert_eq!(u.state.stamp, before.stamp);
    assert_eq!(r.held.items.len(), 2);
}

#[test]
fn a_refused_segment_stops_the_reader_before_it_and_changes_nothing() {
    let mut r = Rig::new(cfg());
    r.submit(items(2, 0));
    r.drain();
    r.refresh().unwrap();
    for k in 1..4 {
        r.submit(items(2, k * 100));
        r.drain();
    }
    let before = r.reader.as_ref().unwrap().state();
    // The middle one of the three new batches gets a data block of an unknown critical type, its checksums intact.
    let m = r.mark().unwrap();
    let seg = log_path(
        &SegmentName {
            generation: 0,
            file_id: m.file_id,
            sealed: false,
        }
        .to_string(),
    );
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: m.file_id,
        generation: 0,
    };
    let at = scan(&bytes, &id, ScanLimit::End).unwrap().batches[2].offset as usize;
    let mut h = bytes[at..at + 16].to_vec();
    h[0] = 0x70;
    let crc = kvik_log::crc::crc32c(&h[..12]);
    h[12..16].copy_from_slice(&crc.to_le_bytes());
    r.fs.poke(&seg, at, &h);
    assert!(matches!(r.refresh(), Err(ReaderError::Unsupported { .. })));
    assert_eq!(r.reader.as_ref().unwrap().state().durable, before.durable);
    assert_eq!(r.held.items.len(), 2);
    // Repaired, the rest is read.
    r.fs.poke(&seg, at, &bytes[at..at + 16]);
    r.refresh().unwrap();
    assert_eq!(r.held.items, r.acked);
}

#[test]
fn an_earlier_segment_listed_but_gone_when_opened_is_an_error() {
    let (mut r, fs) = hooked_rig();
    r.cfg = WriterConfig {
        segment_size: 8 << 10,
        max_batch_bytes: 2 << 10,
        ..cfg()
    };
    r.kill();
    assert!(r.open());
    for k in 0..60 {
        r.submit(items(7, k * 10));
        r.step();
        r.clock.advance(3 * MS);
    }
    r.drain();
    let m = r.mark().unwrap();
    assert!(m.file_id > 2);
    // Segment 2 is listed but cannot be opened under either name, as if removed between the two.
    *fs.hook.lock().unwrap() = Some(Box::new(|_, p: &Path| {
        !p.to_string_lossy().contains("-0000000000000002.")
    }));
    assert!(matches!(
        r.refresh(),
        Err(ReaderError::SegmentMissing { file_id: 2 })
    ));
    assert!(r.held.items.is_empty());
    *fs.hook.lock().unwrap() = None;
    r.refresh().unwrap();
    assert_eq!(r.held.items, r.acked);
}

#[test]
fn a_lease_whose_header_is_damaged_is_written_whole_and_the_refresh_interval_is_capped() {
    let mut r = Rig::new(cfg());
    let mine = dir().join("leases").join(lease_name(&[r.readers; 16]));
    r.fs.poke(&mine, 0, &[0; 64]);
    r.refresh().unwrap();
    let lease = LeaseSlot::decode_file(&r.fs.read(&mine).unwrap(), &STORE_ID).unwrap();
    assert_eq!(lease.refresh_counter, 2);
    // A refresh interval above 30 minutes is taken as 30: renewal is due after 15.
    let mut c = r.reader_cfg();
    c.lease_refresh = Duration::from_secs(4 * 3_600);
    let mut reader = LogReader::open(c).unwrap();
    r.clock.advance(Duration::from_secs(15 * 60));
    assert!(reader.keep_alive().unwrap());
}

#[test]
fn a_lease_naming_the_last_epoch_leaves_a_start_that_must_record_a_loss_read_only() {
    let mut r = Rig::new(cfg());
    let max = LeaseSlot {
        epoch: u64::MAX,
        refresh_counter: 1,
        boot_id: r.boot_id(),
        created_at: T0,
    };
    let f =
        r.fs.open(
            &dir().join("leases").join(lease_name(&[0xd1; 16])),
            OpenMode::CreateNew,
        )
        .unwrap();
    f.write_at(&max.encode_file(STORE_ID, 0), 0).unwrap();
    r.submit(items(2, 0));
    r.drain();
    r.kill();
    let m = r.mark().unwrap();
    let seg = log_path(
        &SegmentName {
            generation: 0,
            file_id: m.file_id,
            sealed: false,
        }
        .to_string(),
    );
    r.fs.poke(&seg, 64 + 8, &[0xee; 8]);
    let store = StoreDir::open(Arc::new(r.fs.clone()), &dir(), Some(&new_store())).unwrap();
    let w = LogWriter::open(
        store,
        r.cfg.clone(),
        Some(r.boot_id()),
        r.now(),
        r.clock.wall(),
    )
    .unwrap();
    assert!(w.fault().is_some(), "read-only, not a wrapped epoch");
    let man = read_manifest(&r.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID).unwrap();
    assert!(man.records.iter().all(|x| x.epoch < u64::MAX));
}

#[test]
fn an_unreadable_lease_counts_as_invalid_and_does_not_stop_a_start() {
    let root = std::env::temp_dir().join(format!("kvik-log-lease-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fs: Arc<dyn Fs> = Arc::new(StdFs);
    let store = StoreDir::open(fs.clone(), &root, Some(&new_store())).unwrap();
    drop(LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap());
    // A directory named as a lease.
    std::fs::create_dir(root.join("leases").join(lease_name(&[0xe1; 16]))).unwrap();
    let store = StoreDir::open(fs.clone(), &root, Some(&new_store())).unwrap();
    let w = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap();
    assert!(w.fault().is_none());
    let seen = w.leases().observed();
    assert_eq!(seen[&[0xe1; 16]].slot, None);
    drop(w);
    std::fs::remove_dir_all(&root).unwrap();
}
