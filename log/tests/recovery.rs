//! Recovery at a start (roadmap 4.5): the rules of `spec.md` §3 Recovery and the order of a start, driven
//! deterministically on the in-memory file system, and a kill loop that crashes the writer at arbitrary points.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{Entry, MarkState};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::manifest::{LossCause, RECORD_LOST_RANGE, encode_record, read_manifest};
use kvik_log::mark::{MarkSlot, SLOT_LEN, SLOT_OFFSETS, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, Tail, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, IngestQueue, LogWriter, LossKind, MarkSync, StartReport, StartRule,
    WriterConfig, create_segment, seal_segment,
};
use proptest::prelude::*;

const STORE_ID: [u8; 16] = [4; 16];
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

/// A writer, its file system and boot, and every acknowledged entry by position.
struct Rig {
    fs: MemFs,
    boot: u8,
    cfg: WriterConfig,
    w: Option<LogWriter>,
    q: IngestQueue,
    calls: BTreeMap<u64, Vec<Item>>,
    now: Duration,
    wall: i64,
    acked: BTreeMap<u64, Item>,
}

impl Rig {
    fn new(cfg: WriterConfig) -> Self {
        let mut r = Self {
            fs: MemFs::new(),
            boot: 1,
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            w: None,
            calls: BTreeMap::new(),
            now: Duration::ZERO,
            wall: T0,
            acked: BTreeMap::new(),
        };
        r.open().expect("the first start");
        r
    }

    fn boot_id(&self) -> [u8; 16] {
        [self.boot; 16]
    }

    /// Opens the store and the writer; `None` if the start could not finish (the store is read-only, or a crash
    /// stopped it).
    fn open(&mut self) -> Option<StartReport> {
        self.w = None;
        let store = StoreDir::open(Arc::new(self.fs.clone()), &dir(), Some(&new_store())).ok()?;
        let w = LogWriter::open(
            store,
            self.cfg.clone(),
            Some(self.boot_id()),
            self.now,
            self.wall,
        )
        .ok()?;
        let report = w.start_report().clone();
        if w.fault().is_some() {
            return None;
        }
        self.w = Some(w);
        self.q = IngestQueue::new(self.cfg.ingest_queue);
        self.calls.clear();
        self.maint();
        Some(report)
    }

    fn w(&mut self) -> &mut LogWriter {
        self.w.as_mut().expect("an open writer")
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items.clone(), self.cfg.call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.calls.insert(id, items);
    }

    fn maint(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let fs = self.fs.clone();
        for _ in 0..2 {
            if let Some(req) = w.segment_request(self.wall) {
                let r = create_segment(&fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(self.now) {
                let r = seal_segment(&fs, &job, &w.halt());
                w.seal_done(job, r, self.now);
            }
        }
    }

    fn step(&mut self) {
        let Some(w) = self.w.as_mut() else { return };
        let s = w.step(self.now, self.wall, &mut self.q);
        for (id, r) in s.done {
            if let Ok(a) = r {
                for (k, item) in self.calls[&id].iter().enumerate() {
                    let seq = a.first.sequence + k as u64;
                    assert!(
                        self.acked.insert(seq, item.clone()).is_none(),
                        "position {seq} acknowledged twice"
                    );
                }
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), kvik_log::writer::Fault> {
        let now = self.now;
        self.w().shutdown(now)
    }

    fn advance(&mut self, d: Duration) {
        self.now += d;
        self.wall += d.as_nanos() as i64;
    }

    /// Runs until the queue is empty or the writer stops.
    fn drain(&mut self) {
        for _ in 0..1000 {
            if self.q.is_empty() || self.w.as_ref().is_none_or(|w| w.fault().is_some()) {
                return;
            }
            self.step();
            self.maint();
            self.advance(10 * MS);
        }
    }

    /// The process is killed: what it wrote stays, as in the page cache; same boot.
    fn kill(&mut self) {
        self.w = None;
        self.fs.restart();
    }

    /// A power loss and a reboot.
    fn power_loss(&mut self, seed: Option<u64>) {
        self.w = None;
        match seed {
            Some(s) => self.fs.power_loss_partial(s),
            None => self.fs.power_loss(),
        }
        self.fs.restart();
        self.boot = self.boot.wrapping_add(1).max(1);
    }

    fn mark(&self) -> Option<MarkSlot> {
        let b = self.fs.read(&log_path("MARK"))?;
        read_mark(&b, &STORE_ID).ok()?.map(|m| m.1)
    }

    /// Every complete batch of every segment, current contents: (first, last, items, recovery entry?).
    fn batches(&self) -> Vec<BatchContent> {
        self.batches_where(|_| true)
    }

    /// The complete batches of the sealed segments.
    fn sealed_batches(&self) -> Vec<BatchContent> {
        self.batches_where(|n| n.sealed)
    }

    fn batches_where(&self, keep: impl Fn(&SegmentName) -> bool) -> Vec<BatchContent> {
        let mut out = Vec::new();
        let mut segs: Vec<(SegmentName, PathBuf)> = self
            .fs
            .paths()
            .into_iter()
            .filter_map(|p| Some((SegmentName::parse(p.file_name()?.to_str()?)?, p.clone())))
            .collect();
        segs.sort_by_key(|s| s.0.file_id);
        for (n, p) in segs.into_iter().filter(|s| keep(&s.0)) {
            let bytes = self.fs.read(&p).unwrap();
            let id = SegmentIdentity {
                store_id: STORE_ID,
                file_id: n.file_id,
                generation: n.generation,
            };
            let r = scan(&bytes, &id, ScanLimit::End).unwrap();
            for b in &r.batches {
                out.push(BatchContent::decode(b).unwrap());
            }
        }
        out
    }

    /// Entries by position.
    fn positions(&self) -> BTreeMap<u64, Item> {
        let mut m = BTreeMap::new();
        for c in self.batches() {
            for (k, item) in c.items.into_iter().enumerate() {
                m.insert(c.first_sequence + k as u64, item);
            }
        }
        m
    }

    /// Every acknowledged entry is at its position.
    fn check_acked(&self) {
        let have = self.positions();
        for (seq, item) in &self.acked {
            assert_eq!(have.get(seq), Some(item), "acknowledged position {seq}");
        }
    }

    /// After a start: the mark is readable, the segment in use has a clean tail, positions are dense apart from
    /// recovery-entry gaps, and every acknowledged entry is at its position.
    fn check_started(&self) {
        let m = self.mark().expect("a readable mark after a start");
        assert_eq!(m.boot_id, self.boot_id());
        let mut next = 1;
        for c in self.batches() {
            if let [Item::Entry(Entry::Recovery(r))] = c.items.as_slice() {
                // `after` is the last kept position: the previous batch's, or the end of a lost range.
                assert!(r.after + 1 >= next);
                assert!(c.first_sequence >= next);
            } else {
                assert_eq!(c.first_sequence, next, "dense positions");
            }
            next = c.last_sequence() + 1;
        }
        assert_eq!(m.last_sequence, next - 1, "the mark names the last batch");
        if m.file_id > 0 {
            let p = log_path(
                &SegmentName {
                    generation: 0,
                    file_id: m.file_id,
                    sealed: false,
                }
                .to_string(),
            );
            if let Some(bytes) = self.fs.read(&p) {
                let id = SegmentIdentity {
                    store_id: STORE_ID,
                    file_id: m.file_id,
                    generation: 0,
                };
                let r = scan(&bytes, &id, ScanLimit::End).unwrap();
                // A segment the mark still names may be awaiting its seal, or sealed but not yet renamed.
                assert!(
                    r.seal.is_some() || matches!(r.tail, Some(Tail::Clean { .. })),
                    "a clean tail after a start: {:?}",
                    r.tail
                );
            }
        }
        self.check_acked();
    }
}

#[test]
fn a_kill_on_the_same_boot_truncates_the_batch_beyond_the_mark() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    // The next batch is synced, but its mark write fails: never acknowledged.
    r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
    r.submit(vec![fact(1, 1)]);
    r.step();
    assert!(r.w().fault().is_some());
    assert_eq!(r.positions().len(), 2);
    let mark = r.mark().unwrap();
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.truncated_bytes > 0);
    // The truncated batch was complete, so it may have been acknowledged on a mark lost with the page cache: the
    // truncation is a reset point, a recovery entry after the mark's position with a new stamp (Q-118 (e)).
    let e = rep.entry.as_ref().expect("a recovery entry");
    assert!(!e.adopted);
    assert_eq!(e.entry.after, mark.last_sequence);
    assert_eq!(e.sequence, mark.last_sequence + 1);
    assert_eq!(e.entry.readable_commits, 1);
    assert_eq!(e.entry.mark_state, MarkState::Readable);
    let stamp = e.entry.stamp.expect("a stamp");
    assert_eq!(stamp.sequence, e.sequence);
    assert_ne!(stamp.nonce, 0);
    assert_eq!(r.mark().unwrap().stamp, stamp);
    assert!(rep.losses.iter().any(|l| l.kind == LossKind::PossibleLoss));
    assert_eq!(r.w().stats().durable.sequence, mark.last_sequence + 1);
    assert_eq!(r.positions().len(), 2);
    r.check_started();
    // The writer resumes at the next position.
    r.submit(vec![fact(2, 0)]);
    r.drain();
    r.check_acked();
    assert_eq!(r.positions().len(), 3);
    // A kill with nothing complete beyond the mark: no entry, and the stamp stays.
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.entry.is_none());
    assert_eq!(r.mark().unwrap().stamp, stamp);
}

#[test]
fn a_reboot_writes_a_recovery_entry_and_a_clean_restart_writes_none() {
    let mut r = Rig::new(cfg());
    for i in 0..5 {
        r.submit(vec![fact(1, i)]);
        r.drain();
    }
    r.shutdown().unwrap();
    r.power_loss(None);
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::Reboot(MarkState::Readable));
    let e = rep.entry.unwrap();
    assert_eq!((e.sequence, e.entry.after, e.adopted), (6, 5, false));
    assert!(!rep.rewrote_last_batch, "a readable mark of another boot");
    r.check_started();
    r.submit(vec![fact(1, 9)]);
    r.drain();
    r.check_acked();
    assert_eq!(*r.positions().keys().last().unwrap(), 7);
    // A clean shutdown, then a start on the same boot: no entry.
    r.shutdown().unwrap();
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.entry.is_none() && rep.truncated_bytes == 0);
    r.check_started();
}

#[test]
fn a_stale_mark_on_the_same_boot_is_handled_as_another_boot() {
    // The mark is synced only on its timer: after a power loss on the same boot (a remount) it lags by several
    // batches, which rule 1 calls stale.
    let mut r = Rig::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    for i in 0..4 {
        r.submit(vec![fact(1, i)]);
        r.step();
    }
    let acked = r.acked.len();
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::Reboot(MarkState::Stale));
    assert!(rep.rewrote_last_batch);
    assert_eq!(rep.entry.as_ref().unwrap().entry.after, acked as u64);
    r.check_started();
}

#[test]
fn a_remount_within_the_boot_may_lose_one_batch_unless_the_mark_is_synced_at_every_commit() {
    for every in [false, true] {
        let mut r = Rig::new(WriterConfig {
            mark_sync: if every {
                MarkSync::EveryCommit
            } else {
                MarkSync::Interval(Duration::from_secs(1))
            },
            ..cfg()
        });
        r.submit(vec![fact(1, 0)]);
        r.drain();
        r.w = None;
        r.fs.power_loss();
        r.fs.restart();
        let rep = r.open().unwrap();
        if every {
            r.check_started();
        } else {
            // Rule 1 truncates the one batch since the mark's last sync; it may have been acknowledged, so the
            // truncation is reported and is a reset point: a recovery entry after the mark's position (Q-118 (e)).
            assert_eq!(rep.rule, StartRule::SameBoot);
            let e = rep.entry.as_ref().expect("a recovery entry");
            assert_eq!(e.entry.after, 0);
            assert!(rep.losses.iter().any(|l| l.kind == LossKind::PossibleLoss));
            let p = r.positions();
            assert_eq!(p.len(), 1);
            assert!(matches!(p.get(&1), Some(Item::Entry(Entry::Recovery(e))) if e.after == 0));
        }
    }
}

#[test]
fn corruption_below_the_mark_is_a_recorded_loss_and_later_batches_are_kept() {
    let mut r = Rig::new(WriterConfig {
        mark_sync: MarkSync::EveryCommit,
        ..cfg()
    });
    for i in 0..4 {
        r.submit(vec![fact(1, i), fact(2, i)]);
        r.drain();
    }
    r.shutdown().unwrap();
    r.w = None;
    // Damage the second batch's body.
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let second = scan(&bytes, &id, ScanLimit::End).unwrap().batches[1].offset;
    r.fs.poke(&seg, second as usize + 40, &[0xff; 8]);
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert_eq!(rep.losses.len(), 1);
    let l = &rep.losses[0];
    assert_eq!(
        (l.first, l.last, l.cause),
        (Some(3), Some(4), LossCause::Corruption)
    );
    assert!(r.w().fault().is_none());
    let m = read_manifest(&r.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID).unwrap();
    assert_eq!(m.lost_ranges().count(), 1);
    // Later batches are kept; positions are not reused.
    assert_eq!(
        r.positions().keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 5, 6, 7, 8]
    );
    r.submit(vec![fact(3, 0)]);
    r.drain();
    assert_eq!(*r.positions().keys().last().unwrap(), 9);
    // A second start records nothing again.
    r.shutdown().unwrap();
    r.kill();
    let rep = r.open().unwrap();
    assert!(rep.losses.iter().all(|l| l.recorded));
    let m = read_manifest(&r.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID).unwrap();
    assert_eq!(m.lost_ranges().count(), 1);
}

#[test]
fn a_position_named_by_the_manifest_raises_the_recovery_entry_and_its_gap_is_recorded() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.shutdown().unwrap();
    r.w = None;
    // A lost-range record naming position 10 (as a cursor or derived file would name a position).
    let path = dir().join("MANIFEST");
    let m = read_manifest(&r.fs.read(&path).unwrap(), &STORE_ID).unwrap();
    let rec = kvik_log::manifest::LostRange {
        identity: (0, 8),
        first: (0, 8),
        last: (0, 10),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
    };
    let bytes = encode_record(RECORD_LOST_RANGE, m.last_epoch() + 1, 0, T0, Some(&rec));
    let len = r.fs.read(&path).unwrap().len();
    r.fs.poke(&path, len, &bytes);
    r.power_loss(None);
    let rep = r.open().unwrap();
    let e = rep.entry.unwrap();
    assert_eq!((e.entry.after, e.sequence), (1, 11));
    assert!(
        rep.losses
            .iter()
            .any(|l| l.first == Some(2) && l.last == Some(10))
    );
    r.check_started();
}

#[test]
fn a_registry_file_raises_f_and_the_id_floor_and_gives_the_segment_index_its_reach() {
    use kvik_log::entries::{Definition, IntervalLength, Kind, Retention, Timing, Versioning};
    use kvik_log::registry::{RegistryFile, write};
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.shutdown().unwrap();
    r.w = None;
    // A registry file (A.16) naming position 10, an id floor of 40 and a half-hourly interval series 7 whose
    // definition is in no segment the start reads.
    let half_hour = 1_800_000_000_000;
    let def = Definition {
        id: 7,
        name: "a/b/energy".into(),
        kind: Kind::Interval,
        value_type: kvik_log::facts::ValueType::F64,
        scale: None,
        unit: "kWh".into(),
        expected_interval: Some(half_hour),
        staleness: Some(3 * half_hour),
        record_on_change: false,
        counter: None,
        lateness_bound: 300_000_000_000,
        state_names: Vec::new(),
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: Some(IntervalLength::Fixed {
            length: half_hour,
            phase: 0,
        }),
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    };
    let file = RegistryFile {
        generation: 0,
        through: 10,
        max_series_id: 40,
        entries: vec![(9, T0, Entry::Definition(Box::new(def)))],
    };
    write(&r.fs, &dir(), STORE_ID, T0, &file).unwrap();
    r.power_loss(None);
    let rep = r.open().unwrap();
    let e = rep.entry.unwrap();
    assert_eq!(
        (e.entry.after, e.sequence),
        (1, 11),
        "F above the file's through"
    );
    assert_eq!(e.entry.id_floor, 40);
    assert_eq!(r.mark().unwrap().max_series_id, 40);
    // A fact of series 7, then a timed seal: the index reaches the end of its interval (Q-115 (f)).
    r.submit(vec![fact(7, 0)]);
    r.drain();
    r.advance(Duration::from_secs(301));
    for _ in 0..10 {
        r.step();
        r.maint();
        r.advance(10 * MS);
    }
    let mut reach = None;
    for p in r.fs.paths() {
        let Some(n) = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(SegmentName::parse)
        else {
            continue;
        };
        if !n.sealed {
            continue;
        }
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: n.file_id,
            generation: n.generation,
        };
        let bytes = r.fs.read(&p).unwrap();
        let rep = scan(&bytes, &id, ScanLimit::End).unwrap();
        if let Some(index) = rep.seal.and_then(|s| s.index.flatten()) {
            for rec in index.records.iter().filter(|r| r.series_id == 7) {
                reach = Some(rec.max_t - rec.min_t);
            }
        }
    }
    assert_eq!(reach, Some(half_hour));
}

#[test]
fn a_crash_between_the_entry_and_the_mark_adopts_the_entry() {
    let mut adopted = 0;
    for n in 0..40 {
        let mut r = Rig::new(cfg());
        r.submit(vec![fact(1, 0)]);
        r.drain();
        r.w = None;
        // A torn tail of 40 bytes after the last batch.
        let seg = log_path("00000000-0000000000000001.open");
        let end = r.batches().len();
        assert_eq!(end, 1);
        let bytes = r.fs.read(&seg).unwrap();
        let tail_at = bytes.iter().rposition(|&b| b != 0).unwrap() + 1;
        let tail_at = tail_at.div_ceil(8) * 8;
        r.fs.poke(&seg, tail_at, &[0x5a; 40]);
        r.power_loss(None);
        // The start itself is cut short after n operations.
        r.fs.crash_after(n);
        if r.open().is_some() {
            continue;
        }
        r.kill();
        let rep = r.open().expect("the next start finishes");
        // Whatever point the first start reached, the log ends with one recovery entry, which reports the tail it
        // found: written by the first start (and adopted, or covered by its mark), or by the second.
        let entries: Vec<_> = r
            .batches()
            .into_iter()
            .filter_map(|c| match c.items.as_slice() {
                [Item::Entry(Entry::Recovery(e))] => Some(e.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(entries.len(), 1, "crash after {n} operations");
        assert_eq!(entries[0].tail_bytes, 40, "crash after {n} operations");
        if rep.entry.as_ref().is_some_and(|e| e.adopted) {
            adopted += 1;
        }
        r.check_started();
    }
    assert!(
        adopted > 0,
        "some crash fell between the entry and the mark"
    );
}

#[test]
fn the_last_batch_is_written_again_when_its_sync_may_have_failed() {
    // A failed sync leaves the batch in the page cache but not on disk (Linux), and a later sync of the file does not
    // write it. With the mark gone the start cannot tell; it writes the batch again before its sync (Q-87).
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.fs.fail_nth(FaultOp::Sync, ".open", 0, io::ErrorKind::Other);
    r.submit(vec![fact(1, 1)]);
    r.step();
    assert!(r.w().fault().is_some());
    r.kill();
    r.fs.remove_file(&log_path("MARK")).unwrap();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::Reboot(MarkState::Missing));
    assert!(rep.rewrote_last_batch);
    let kept = r.positions();
    r.check_started();
    // What the start kept is on stable storage.
    r.power_loss(None);
    r.open().unwrap();
    for (seq, item) in kept {
        assert_eq!(r.positions().get(&seq), Some(&item));
    }
}

#[test]
fn a_failed_sync_at_a_start_leaves_it_read_only_and_writes_nothing_more() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.power_loss(None);
    r.fs.fail_nth(FaultOp::Sync, ".open", 0, io::ErrorKind::Other);
    r.fs.clear_ops();
    assert!(r.open().is_none());
    let ops = r.fs.ops();
    let failed = ops.iter().position(|o| !o.ok).unwrap();
    assert_eq!(failed, ops.len() - 1, "{:?}", &ops[failed..]);
    // The next start finishes.
    r.fs.restart();
    r.open().unwrap();
    r.check_started();
}

#[test]
fn segments_awaiting_a_seal_are_sealed_again_and_an_unused_one_is_kept() {
    let mut r = Rig::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    // Fill a few segments without running the seals.
    for i in 0..60 {
        r.submit((0..20).map(|k| fact(k, i)).collect());
        r.step();
        let fs = r.fs.clone();
        let w = r.w();
        if let Some(req) = w.segment_request(T0) {
            let res = create_segment(&fs, &req, &w.halt());
            w.segment_created(res);
        }
        drop(w.take_seal_jobs(Duration::ZERO));
    }
    let opens =
        r.fs.paths()
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".open"))
            .count();
    assert!(opens >= 3);
    r.kill();
    let rep = r.open().unwrap();
    assert!(!rep.to_seal.is_empty());
    r.maint();
    let sealed =
        r.fs.paths()
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(".kvl"))
            .count();
    assert_eq!(sealed, rep.to_seal.len());
    r.check_started();
}

#[test]
fn a_forged_seal_in_the_last_segment_is_a_torn_tail() {
    // Q-109: a text value can hold the bytes of a seal; in the last segment in use they are tail bytes, because a
    // segment is sealed only after the next one is in use (A.4).
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.shutdown().unwrap();
    r.w = None;
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let rep0 = scan(&bytes, &id, ScanLimit::End).unwrap();
    let end = rep0.batches[0].end() as usize;
    let mut img =
        kvik_log::segment::SegmentImage::create_ahead(id, T0, bytes.len() as u64).unwrap();
    img.bytes_mut().copy_from_slice(&bytes);
    let summary = kvik_log::segment::SealSummary {
        entry_count: 1,
        first_sequence: 1,
        last_sequence: 1,
        first_commit_time: 0,
        last_commit_time: 0,
        batch_count: 1,
    };
    let seal = kvik_log::segment::encode_seal(
        &bytes[..end],
        &summary,
        &kvik_log::body::SegmentIndex {
            flags: 0,
            records: vec![],
        },
    )
    .unwrap();
    r.fs.poke(&seg, end, &seal);
    r.power_loss(None);
    let rep = r.open().unwrap();
    // The forged seal is reported as the torn tail it is (up to its last non-zero byte), and overwritten.
    assert_eq!(
        rep.entry.as_ref().unwrap().entry.tail_bytes,
        seal.len() as u64 - 1
    );
    assert!(rep.to_seal.is_empty());
    r.check_started();
}

/// Bytes as a torn tail may hold them after the last complete batch at `end` of segment `id`: an entries block
/// defining series 77, a commit block that is checksum-valid but closes no complete batch, and then, as the last
/// bytes of a value, the 96 bytes of a seal whose trailer counts one batch ending at position 1, as the scan finds.
/// Returns them and the length of the forged seal.
fn forged_tail(bytes: &[u8], id: &SegmentIdentity, end: usize) -> (Vec<u8>, usize) {
    use kvik_log::block::{block_type, encode_block};
    use kvik_log::entries::{Definition, EntriesBlock, Kind, Retention, Timing, Versioning};
    let def = Definition {
        id: 77,
        name: "s77".into(),
        kind: Kind::Gauge,
        value_type: kvik_log::facts::ValueType::F64,
        scale: None,
        unit: "u".into(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: 1_000_000_000,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: None,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    };
    let body = EntriesBlock {
        first_sequence: 2,
        received_at: T0,
        entries: vec![Entry::Definition(Box::new(def))],
    }
    .encode()
    .unwrap();
    let mut tail = Vec::new();
    encode_block(block_type::ENTRIES, 0, &body, &mut tail).unwrap();
    let commit = kvik_log::body::Commit {
        generation: 0,
        block_count: 1,
        first_sequence: 2,
        last_sequence: 2,
        commit_time: T0,
        batch_bytes: tail.len() as u64,
        batch_crc32c: 0,
    };
    encode_block(block_type::COMMIT, 0, &commit.encode(), &mut tail).unwrap();
    let mut before = bytes[..end].to_vec();
    before.extend_from_slice(&tail);
    let summary = kvik_log::segment::SealSummary {
        entry_count: 1,
        first_sequence: 1,
        last_sequence: 1,
        first_commit_time: 0,
        last_commit_time: 0,
        batch_count: 1,
    };
    let seal = kvik_log::segment::encode_seal(
        &before,
        &summary,
        &kvik_log::body::SegmentIndex {
            flags: 0,
            records: vec![],
        },
    )
    .unwrap();
    // The forgery is what the old classing took for a seal.
    let mut forged = before.clone();
    forged.extend_from_slice(&seal);
    assert!(scan(&forged, id, ScanLimit::End).unwrap().seal.is_some());
    tail.extend_from_slice(&seal);
    (tail, seal.len())
}

/// Q-109: a forged seal as the last bytes of a torn batch in the last segment in use fails the structural rule (its
/// blocks do not start at the end of the last complete batch), so the torn tail is seen whole: the recovery entry
/// counts its readable commit block and the id floor covers the definition in it.
#[test]
fn a_forged_seal_after_a_torn_batch_in_the_last_segment_hides_nothing() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.w = None;
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let end = scan(&bytes, &id, ScanLimit::End).unwrap().batches[0].end() as usize;
    let (tail, _) = forged_tail(&bytes, &id, end);
    r.fs.poke(&seg, end, &tail);
    r.power_loss(None);
    let rep = r.open().unwrap();
    let e = rep.entry.as_ref().expect("a recovery entry").entry.clone();
    assert_eq!(e.readable_commits, 1, "the tail's commit block is counted");
    assert_eq!(
        e.id_floor, 77,
        "the definition in the tail raises the id floor"
    );
    assert_eq!(
        e.tail_bytes,
        tail.len() as u64 - 1,
        "up to the last non-zero byte"
    );
    assert_eq!(r.w().stats().max_series_id, 77);
    assert!(rep.to_seal.is_empty());
    r.check_started();
}

/// Q-109: a segment that is not the last in use, left unsealed with bytes after its last complete batch that end in
/// a forged seal: the forgery fails the structural rule, the bytes are seal blocks or damage (A.4), and the segment
/// is sealed again at the end of its last complete batch, the forged bytes dropped. (This passed before the rule
/// too: sealing again from step 1 already wrote the seal at the real end.)
#[test]
fn a_forged_seal_in_a_segment_that_is_not_the_last_in_use_is_ignored() {
    let mut r = Rig::new(WriterConfig {
        segment_size: 16 << 10,
        ..cfg()
    });
    // Write until the writer rolls to segment 2; segment 1 is killed before its seal runs.
    let mut t = 0;
    while r.w().stats().open_file_id != Some(2) {
        r.submit(vec![fact(1, t)]);
        r.step();
        if r.w().stats().open_file_id != Some(2) {
            r.maint();
        }
        r.advance(10 * MS);
        t += 1;
        assert!(t < 10_000, "no roll");
    }
    assert!(r.w().fault().is_none());
    r.kill();
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).expect("left unsealed");
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let found = scan(&bytes, &id, ScanLimit::End).unwrap();
    assert!(found.seal.is_none());
    let n = found.batches.len();
    let end = found.batches[n - 1].end() as usize;
    let (tail, _) = forged_tail(&bytes, &id, end);
    r.fs.poke(&seg, end, &tail);
    let rep = r.open().unwrap();
    assert_eq!(rep.to_seal, vec![1]);
    assert!(rep.losses.is_empty(), "{rep:?}");
    r.maint();
    let kvl =
        r.fs.read(&log_path("00000000-0000000000000001.kvl"))
            .expect("sealed");
    let sealed = scan(&kvl, &id, ScanLimit::End).unwrap();
    let real = sealed.seal.expect("a seal");
    assert_eq!(real.start, end as u64, "sealed at the real end");
    assert_eq!(real.end, kvl.len() as u64);
    assert_eq!(real.trailer.batch_count, n as u64);
    assert_eq!(sealed.batches.len(), n);
    assert!(sealed.regions.is_empty());
    assert!(real.file_crc_matches(&kvl));
    r.check_started();
}

#[test]
fn a_definition_still_readable_in_the_torn_tail_raises_the_id_floor() {
    use kvik_log::entries::{Definition, EntriesBlock, Kind, Retention, Timing, Versioning};
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.w = None;
    let def = Definition {
        id: 77,
        name: "s77".into(),
        kind: Kind::Gauge,
        value_type: kvik_log::facts::ValueType::F64,
        scale: None,
        unit: "u".into(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: 1_000_000_000,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: None,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    };
    let body = EntriesBlock {
        first_sequence: 2,
        received_at: T0,
        entries: vec![Entry::Definition(Box::new(def))],
    }
    .encode()
    .unwrap();
    let mut block = Vec::new();
    kvik_log::block::encode_block(kvik_log::block::block_type::ENTRIES, 0, &body, &mut block)
        .unwrap();
    // The block reached the disk, its commit block did not.
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let end = scan(&bytes, &id, ScanLimit::End).unwrap().batches[0].end();
    r.fs.poke(&seg, end as usize, &block);
    r.power_loss(None);
    let rep = r.open().unwrap();
    assert_eq!(rep.entry.unwrap().entry.id_floor, 77);
    assert_eq!(r.w().stats().max_series_id, 77);
    r.check_started();
}

#[test]
fn a_segment_whose_creation_stopped_before_its_directory_sync_is_made_durable_by_the_start() {
    // The segment created ahead was written and synced, but the directory sync that makes its name durable failed.
    // The next start keeps it as created ahead; step 1's directory sync makes its name durable before batches are
    // acknowledged in it.
    for seed in 0..16 {
        let mut r = Rig::new(cfg());
        r.submit(vec![fact(1, 0)]);
        r.drain();
        r.fs.fail_nth(FaultOp::SyncDir, "log", 0, io::ErrorKind::Other);
        // Write until the writer asks for its next segment; that creation fails at its directory sync.
        let fs = r.fs.clone();
        for i in 0..500 {
            r.submit(vec![fact(1, i)]);
            r.step();
            r.advance(20 * MS);
            let w = r.w();
            if let Some(req) = w.segment_request(T0) {
                let res = create_segment(&fs, &req, &w.halt());
                w.segment_created(res);
                break;
            }
        }
        assert!(r.w().fault().is_some());
        r.kill();
        let created: Vec<PathBuf> = r.fs.paths();
        r.open().unwrap();
        // Batches fill the segment in use and roll into the kept one; no further creation runs.
        for i in 0..200 {
            r.submit(vec![fact(2, i)]);
            r.step();
            r.advance(20 * MS);
        }
        r.power_loss(Some(seed));
        let ids = |v: &[PathBuf]| -> Vec<u64> {
            v.iter()
                .filter_map(|p| SegmentName::parse(p.file_name()?.to_str()?))
                .map(|n| n.file_id)
                .collect()
        };
        assert_eq!(
            ids(&r.fs.paths()),
            ids(&created),
            "no segment's name is undone"
        );
        r.open().unwrap();
        r.check_started();
    }
}

#[derive(Clone, Debug)]
enum Crash {
    /// The process is killed (same boot).
    Kill,
    /// A power loss, part of the unsynced work reaching the disk, and a reboot.
    Power(u64),
    /// A power loss without a reboot (the file system remounted within the boot); only with the mark synced at
    /// every commit, which closes the accepted limit of rule 1.
    Remount(u64),
    /// The process is killed and the mark file deleted, or damaged in both slots (same boot).
    LoseMark { delete: bool },
    /// A power loss while the segment created ahead was being taken into use: its header reached the disk, its
    /// first batch only as a torn tail.
    TornRoll(u64),
}

#[derive(Clone, Debug)]
enum Fault {
    None,
    /// Every operation fails after n more.
    Stop(usize),
    /// One sync of a segment or of the mark fails.
    Sync(&'static str, usize),
    /// The disk fills up.
    Full(u64),
}

#[derive(Clone, Debug)]
struct Cycle {
    calls: Vec<(usize, bool)>,
    fault: Fault,
    crash: Crash,
    crash_start: Option<usize>,
    /// A fault inside the start that follows the crash, and how the process then stops: a kill on the same boot,
    /// or a power loss.
    start_fault: Fault,
    after_fault: Option<u64>,
    clean: bool,
}

fn cycle() -> impl Strategy<Value = Cycle> {
    let fault = prop_oneof![
        3 => Just(Fault::None),
        3 => (0..60usize).prop_map(Fault::Stop),
        1 => (0..6usize).prop_map(|k| Fault::Sync(".open", k)),
        1 => (0..6usize).prop_map(|k| Fault::Sync("MARK", k)),
        1 => (0..40_000u64).prop_map(Fault::Full),
    ];
    let crash = prop_oneof![
        3 => Just(Crash::Kill),
        3 => any::<u64>().prop_map(Crash::Power),
        1 => any::<u64>().prop_map(Crash::Remount),
        1 => any::<bool>().prop_map(|delete| Crash::LoseMark { delete }),
        1 => any::<u64>().prop_map(Crash::TornRoll),
    ];
    (
        prop::collection::vec((1..12usize, prop::bool::weighted(0.15)), 0..12),
        fault,
        crash,
        prop::option::weighted(0.3, 0..40usize),
        prop_oneof![
            3 => Just(Fault::None),
            2 => (0..4usize).prop_map(|k| Fault::Sync(".open", k)),
            1 => (0..2usize).prop_map(|k| Fault::Sync("MARK", k)),
            1 => (0..3usize).prop_map(|k| Fault::Sync("MANIFEST", k)),
            1 => (0..20_000u64).prop_map(Fault::Full),
        ],
        prop::option::of(any::<u64>()),
        prop::bool::weighted(0.2),
    )
        .prop_map(
            |(calls, fault, crash, crash_start, start_fault, after_fault, clean)| Cycle {
                calls,
                fault,
                crash,
                crash_start,
                start_fault,
                after_fault,
                clean,
            },
        )
}

/// Runs a kill loop: every cycle writes, fails or crashes somewhere, and starts again.
fn kill_loop(cycles: &[Cycle], every: bool, small: bool) {
    // Small segments make a recovery entry miss the room after the mark more often, so that it goes to a new segment.
    let mut r = Rig::new(WriterConfig {
        mark_sync: if every {
            MarkSync::EveryCommit
        } else {
            MarkSync::Interval(Duration::from_millis(30))
        },
        segment_size: if small { 4096 } else { cfg().segment_size },
        max_batch_bytes: if small { 1024 } else { cfg().max_batch_bytes },
        ..cfg()
    });
    let mut series = 0u32;
    for c in cycles {
        match c.fault {
            Fault::None => {}
            Fault::Stop(n) => r.fs.crash_after(n),
            Fault::Sync(p, k) => r.fs.fail_nth(FaultOp::Sync, p, k, io::ErrorKind::Other),
            Fault::Full(b) => r.fs.set_free_space(b),
        }
        for &(n, entry) in &c.calls {
            let mut items: Vec<Item> = (0..n).map(|k| fact(k as u32, k as i64)).collect();
            if entry {
                series += 1;
                items.push(alias(series));
            }
            r.submit(items);
            r.step();
            r.maint();
            r.advance(7 * MS);
        }
        r.drain();
        if c.clean && r.w.as_ref().is_some_and(|w| w.fault().is_none()) {
            let _ = r.shutdown();
        }
        r.fs.set_free_space(1 << 40);
        let mut mark_before = match c.crash {
            Crash::Kill => {
                r.kill();
                r.mark()
            }
            Crash::Remount(seed) if every => {
                r.w = None;
                r.fs.power_loss_partial(seed);
                r.fs.restart();
                r.mark()
            }
            Crash::Power(seed) | Crash::Remount(seed) => {
                r.power_loss(Some(seed));
                None
            }
            Crash::TornRoll(seed) => {
                r.w = None;
                r.fs.power_loss_partial(seed);
                let next = r.positions().keys().last().map_or(1, |p| p + 1);
                let ahead = r.fs.paths().into_iter().rev().find(|p| {
                    p.to_string_lossy().ends_with(".open")
                        && r.fs.read(p).is_some_and(|b| {
                            b.len() as u64 == r.cfg.segment_size
                                && kvik_log::header::FileHeader::decode(&b)
                                    .is_some_and(|h| h.base_sequence == 0)
                                && b[64..].iter().all(|&x| x == 0)
                        })
                });
                if let Some(p) = ahead {
                    let mut h =
                        kvik_log::header::FileHeader::decode(&r.fs.read(&p).unwrap()).unwrap();
                    h.base_sequence = next;
                    r.fs.poke(&p, 0, &h.encode());
                    r.fs.poke(&p, 400, &[0x5a; 64]);
                }
                r.fs.restart();
                r.boot = r.boot.wrapping_add(1).max(1);
                None
            }
            Crash::LoseMark { delete } => {
                r.kill();
                let m = log_path("MARK");
                if delete {
                    let _ = r.fs.remove_file(&m);
                } else if r.fs.read(&m).is_some() {
                    r.fs.poke(&m, 0, &[0xee; 8]);
                    r.fs.poke(&m, 4096, &[0xee; 8]);
                }
                None
            }
        };
        // On the same boot, a complete batch above the mark may have been acknowledged on a mark lost with the page
        // cache: once a start finishes, it is gone and a recovery entry after the mark's position is in the log,
        // however the starts before were cut short (`spec.md` §3 Recovery rule 1). Only when the mark is stale (two or
        // more complete batches beyond it, or one in a sealed segment) does rule 3 keep it instead; then a start that
        // keeps it may fail after its entry, and the next one, under rule 1, finds the batch below its mark.
        let mut owed = mark_before.and_then(|m| {
            let all = r.batches();
            let above = all
                .iter()
                .filter(|b| b.last_sequence() > m.last_sequence)
                .count();
            let sealed = r
                .sealed_batches()
                .iter()
                .any(|b| b.last_sequence() > m.last_sequence);
            all.into_iter()
                .find(|b| b.last_sequence() > m.last_sequence)
                .map(|b| (m.last_sequence, b, above >= 2 || sealed))
        });
        let paid = |r: &Rig, owed: &Option<(u64, BatchContent, bool)>| {
            if let Some((k, batch, stale)) = owed {
                let now = r.batches();
                let kept = now.contains(batch);
                let entry = now.iter().any(|b| {
                    matches!(
                        b.items.as_slice(),
                        [Item::Entry(Entry::Recovery(e))] if e.after == *k
                    )
                });
                // The batch beyond the mark may itself be that entry, written by a start whose mark was lost: adopted.
                let is_entry = matches!(
                    batch.items.as_slice(),
                    [Item::Entry(Entry::Recovery(e))] if e.after == *k
                );
                assert!(
                    (entry && (!kept || is_entry)) || (kept && *stale),
                    "a complete batch above position {k}: kept {kept}, entry {entry}, stale {stale}"
                );
            }
        };
        // The start itself may be cut short; the next one must finish.
        if let Some(n) = c.crash_start {
            r.fs.crash_after(n);
            if r.open().is_some() {
                r.check_started();
                paid(&r, &owed);
                continue;
            }
            r.kill();
            // The start that stopped may have written a mark; the next one reads it.
            if mark_before.is_some() || r.mark().is_some_and(|m| m.boot_id == r.boot_id()) {
                mark_before = r.mark();
            }
        }
        // A failed sync or a full disk inside the start; the process then stops, on the same boot or by a power
        // loss, and the next start must finish.
        let armed = match c.start_fault {
            Fault::None | Fault::Stop(_) => false,
            Fault::Sync(p, k) => {
                r.fs.fail_nth(FaultOp::Sync, p, k, io::ErrorKind::Other);
                true
            }
            Fault::Full(b) => {
                r.fs.set_free_space(b);
                true
            }
        };
        if armed {
            let started = r.open();
            r.fs.set_free_space(1 << 40);
            if started.is_some() {
                r.check_started();
                paid(&r, &owed);
                continue;
            }
            match c.after_fault {
                Some(seed) => {
                    r.power_loss(Some(seed));
                    mark_before = None;
                    owed = None;
                }
                None => {
                    r.kill();
                    if mark_before.is_some() || r.mark().is_some_and(|m| m.boot_id == r.boot_id()) {
                        mark_before = r.mark();
                    }
                }
            }
        }
        let before = r.batches();
        let rep = r.open().expect("a start finishes");
        r.check_started();
        paid(&r, &owed);
        // No corruption is injected: nothing is certainly lost, and each recovery entry follows the batch before
        // it directly.
        assert!(
            rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
            "{:?}",
            rep.losses
        );
        let mut next = 1;
        for b in r.batches() {
            if let [Item::Entry(Entry::Recovery(e))] = b.items.as_slice() {
                assert_eq!(e.after, next - 1);
            }
            next = b.last_sequence() + 1;
        }
        // What was kept is the image's committed prefix: under rule 1 up to the mark, otherwise every complete
        // batch, followed by at most a recovery entry.
        let after = r.batches();
        match rep.rule {
            StartRule::SameBoot => {
                let m = mark_before.unwrap_or_else(|| r.mark().unwrap());
                let expect: Vec<_> = before
                    .iter()
                    .filter(|b| b.last_sequence() <= m.last_sequence)
                    .collect();
                // A recovery entry, written or adopted, exactly when a complete batch lay beyond the mark (Q-118 (e)).
                let beyond = before.iter().any(|b| b.last_sequence() > m.last_sequence);
                assert_eq!(rep.entry.is_some(), beyond, "{:?}", rep.entry);
                let extra = usize::from(beyond);
                assert_eq!(after.len(), expect.len() + extra);
                assert_eq!(after[..expect.len()].iter().collect::<Vec<_>>(), expect);
                if let Some(e) = &rep.entry {
                    assert_eq!(e.entry.after, m.last_sequence);
                    assert_eq!(r.mark().unwrap().stamp, e.entry.stamp.unwrap());
                }
            }
            StartRule::Reboot(_) => {
                let extra = usize::from(rep.entry.as_ref().is_some_and(|e| !e.adopted));
                assert_eq!(after.len(), before.len() + extra);
                assert!(after.iter().zip(&before).all(|(a, b)| a == b));
            }
            StartRule::FirstStart => assert!(before.is_empty()),
        }
    }
}

/// The kill loop: every acknowledged entry survives every crash; positions are never reused without a reset point;
/// each start keeps exactly the image's committed prefix and leaves a readable mark and a clean tail. It runs an
/// eighth of the configured cases, each a whole loop: through a runner of its own, since `proptest!` would set the
/// cases from `PROPTEST_CASES` again.
#[test]
fn kill_loop_keeps_every_acknowledged_entry() {
    let d = ProptestConfig::default();
    proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: (d.cases / 8).max(1),
        ..d
    })
    .run(
        &(
            prop::collection::vec(cycle(), 1..8),
            any::<bool>(),
            any::<bool>(),
        ),
        |(cycles, every, small)| {
            kill_loop(&cycles, every, small);
            Ok(())
        },
    )
    .unwrap();
}

/// A long kill loop, for a local soak run: `cargo test --release -p kvik-log --test recovery -- --ignored`.
#[test]
#[ignore]
fn kill_loop_long() {
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 2_000,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &(
                prop::collection::vec(cycle(), 1..40),
                any::<bool>(),
                any::<bool>(),
            ),
            |(c, e, small)| {
                kill_loop(&c, e, small);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn a_full_disk_at_a_start_that_needs_a_segment_leaves_it_read_only() {
    // A store with no batch yet: after a reboot its recovery entry needs a segment, and there is no space for one.
    let mut r = Rig::new(cfg());
    r.w = None;
    for p in r.fs.paths() {
        if p.to_string_lossy().ends_with(".open") {
            r.fs.remove_file(&p).unwrap();
        }
    }
    r.power_loss(None);
    r.fs.set_free_space(100);
    r.fs.clear_ops();
    assert!(r.open().is_none(), "the start is read-only");
    assert!(
        r.fs.paths()
            .iter()
            .all(|p| !p.to_string_lossy().ends_with(".open")),
        "the partial file is deleted"
    );
    r.fs.set_free_space(1 << 40);
    r.open().expect("with space, the start finishes");
    r.check_started();
}

#[test]
fn a_crash_while_creating_the_store_or_in_its_first_start_is_recovered() {
    for n in 0..30 {
        let fs = MemFs::new();
        fs.crash_after(n);
        let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store()));
        if let Ok(store) = store {
            let _ = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0);
        }
        for seed in [None, Some(n as u64)] {
            let fs = fs.clone();
            match seed {
                None => fs.restart(),
                Some(s) => {
                    fs.power_loss_partial(s);
                    fs.restart();
                }
            }
            let mut r = Rig {
                fs,
                boot: 1,
                cfg: cfg(),
                w: None,
                q: IngestQueue::new(cfg().ingest_queue),
                calls: BTreeMap::new(),
                now: Duration::ZERO,
                wall: T0,
                acked: BTreeMap::new(),
            };
            r.open()
                .unwrap_or_else(|| panic!("crash after {n} operations"));
            r.check_started();
            r.submit(vec![fact(1, 0)]);
            r.drain();
            r.check_acked();
        }
    }
}

#[test]
fn a_sealed_segment_above_the_mark_makes_the_mark_stale() {
    // b_one is sealed in segment 1 and b_two committed in segment 2; the mark, synced only on its timer, still names
    // nothing when a remount loses the page cache. Rule 1 would truncate b_two; the mark is stale.
    let mut r = Rig::new(WriterConfig {
        segment_size: 4096,
        min_sync_interval: Duration::ZERO,
        seal_interval: Duration::from_millis(5),
        ..cfg()
    });
    r.submit(vec![fact(1, 0)]);
    r.step();
    r.maint();
    r.advance(10 * MS);
    r.step();
    r.maint();
    r.submit(vec![fact(1, 1)]);
    r.step();
    r.maint();
    assert!(
        r.fs.paths()
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".kvl"))
    );
    assert_eq!(r.acked.len(), 2);
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::Reboot(MarkState::Stale));
    r.check_started();
}

#[test]
fn a_loss_at_the_tail_below_the_mark_is_not_overwritten() {
    // b_one in segment 1, b_two in segment 2 with the mark after it; b_two is then damaged. The positions up to the mark's
    // are lost, and writing resumes after the damaged bytes.
    let mut r = Rig::new(WriterConfig {
        segment_size: 4096,
        min_sync_interval: Duration::ZERO,
        seal_interval: Duration::from_millis(5),
        mark_sync: MarkSync::EveryCommit,
        ..cfg()
    });
    r.submit(vec![fact(1, 0)]);
    r.step();
    r.maint();
    r.advance(10 * MS);
    r.step();
    r.maint();
    r.submit(vec![fact(1, 1)]);
    r.step();
    r.maint();
    let m = r.mark().unwrap();
    assert_eq!((m.file_id, m.last_sequence), (2, 2));
    r.w = None;
    let seg = log_path("00000000-0000000000000002.open");
    r.fs.poke(&seg, 64 + 40, &[0xff; 8]);
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert_eq!(rep.losses.len(), 1);
    assert_eq!(
        (rep.losses[0].first, rep.losses[0].last),
        (Some(2), Some(2))
    );
    let damaged = r.fs.read(&seg).unwrap()[64..m.offset as usize].to_vec();
    r.submit(vec![fact(1, 2)]);
    r.drain();
    assert_eq!(r.fs.read(&seg).unwrap()[64..m.offset as usize], damaged[..]);
    assert_eq!(
        r.positions().keys().copied().collect::<Vec<_>>(),
        vec![1, 3]
    );
}

/// Commits one batch per call into segment 1 and returns after `n` batches, the mark synced at every commit.
fn rig_with_batches(n: i64, cfg: WriterConfig) -> Rig {
    let mut r = Rig::new(cfg);
    for i in 0..n {
        r.submit(vec![fact(1, i)]);
        r.drain();
    }
    r
}

#[test]
fn an_adopted_entry_whose_own_sync_failed_is_written_again() {
    // Q-117: the entry's sync fails (the store turns read-only); the next start on the same boot adopts it. Its
    // bytes are in the page cache but not on disk, and a later sync would not write them unless they are written
    // again.
    let mut r = rig_with_batches(2, cfg());
    r.shutdown().unwrap();
    r.power_loss(None);
    // Step 3 syncs segment 1; the entry's own sync is the next one of a segment.
    r.fs.fail_nth(FaultOp::Sync, ".open", 1, io::ErrorKind::Other);
    assert!(r.open().is_none());
    r.kill();
    let rep = r.open().unwrap();
    assert!(rep.entry.as_ref().unwrap().adopted);
    assert!(rep.rewrote_last_batch);
    r.submit(vec![fact(2, 0)]);
    r.drain();
    r.shutdown().unwrap();
    // A power loss keeps the entry: nothing is lost.
    r.power_loss(None);
    let rep = r.open().unwrap();
    assert!(
        rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
        "{:?}",
        rep.losses
    );
    r.check_started();
}

#[test]
fn exactly_two_batches_beyond_the_mark_on_the_same_boot_make_it_stale() {
    let mut r = Rig::new(WriterConfig {
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    for i in 0..2 {
        r.submit(vec![fact(1, i)]);
        r.step();
    }
    assert_eq!(r.acked.len(), 2);
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::Reboot(MarkState::Stale));
    r.check_started();
}

#[test]
fn after_a_reboot_a_loss_at_the_tail_below_the_mark_ends_at_the_marks_position() {
    let mut r = Rig::new(WriterConfig {
        mark_sync: MarkSync::EveryCommit,
        ..cfg()
    });
    for i in 0..3 {
        r.submit(vec![fact(1, i)]);
        r.drain();
    }
    let m = r.mark().unwrap();
    r.w = None;
    // Damage the last batch, below the mark.
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let last = scan(&bytes, &id, ScanLimit::End).unwrap().batches[2].offset;
    r.fs.poke(&seg, last as usize + 40, &[0xff; 8]);
    r.power_loss(None);
    let rep = r.open().unwrap();
    let e = rep.entry.unwrap();
    assert_eq!(
        (e.entry.after, e.sequence),
        (m.last_sequence, m.last_sequence + 1)
    );
}

#[test]
fn a_write_after_an_entry_inside_its_reported_tail_is_not_adopted() {
    // An entry reporting a 400-byte tail; a batch written after it (inside that extent) is torn, its commit block
    // still readable with a later commit time. Some start resumed after the entry, so it is not adopted.
    let mut r = rig_with_batches(1, cfg());
    r.w = None;
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let end = (bytes.iter().rposition(|&b| b != 0).unwrap() + 1).div_ceil(8) * 8;
    r.fs.poke(&seg, end, &[0x5a; 400]);
    r.power_loss(None);
    let rep = r.open().unwrap();
    assert_eq!(rep.entry.as_ref().unwrap().entry.tail_bytes, 400);
    r.advance(Duration::from_secs(1));
    r.submit(vec![fact(2, 0)]);
    r.drain();
    r.w = None;
    // Tear the batch after the entry (its first block), keep its commit block, and lose the mark.
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let b = scan(&bytes, &id, ScanLimit::End).unwrap().batches[2].offset;
    r.fs.poke(&seg, b as usize + 40, &[0xff; 8]);
    r.fs.remove_file(&log_path("MARK")).unwrap();
    r.kill();
    let rep = r.open().unwrap();
    assert!(!rep.entry.as_ref().unwrap().adopted);
}

#[test]
fn the_header_is_written_again_with_a_segments_first_batch() {
    // The batch that takes segment 2 into use fails its sync: header and batch are in the page cache only. With the
    // mark lost, the start writes both again before its sync.
    let mut r = Rig::new(WriterConfig {
        segment_size: 4096,
        min_sync_interval: Duration::ZERO,
        ..cfg()
    });
    let seg2 = log_path("00000000-0000000000000002.open");
    let mut planned = false;
    for i in 0..200 {
        // Once segment 2 is created ahead, the batch that takes it into use fails its sync.
        if !planned && r.fs.read_durable(&seg2).is_some_and(|b| b.len() == 4096) {
            r.fs.fail_nth(FaultOp::Sync, "0000000000000002", 0, io::ErrorKind::Other);
            planned = true;
        }
        r.submit(vec![fact(1, i)]);
        r.step();
        r.maint();
        if r.w().fault().is_some() {
            break;
        }
    }
    assert!(r.w().fault().is_some());
    r.kill();
    r.fs.remove_file(&log_path("MARK")).unwrap();
    let rep = r.open().unwrap();
    assert!(rep.rewrote_last_batch);
    r.w = None;
    assert!(FileHeaderOf::base(&r.fs.read_durable(&seg2).unwrap()) >= 1);
}

struct FileHeaderOf;
impl FileHeaderOf {
    fn base(b: &[u8]) -> u64 {
        kvik_log::header::FileHeader::decode(b).map_or(0, |h| h.base_sequence)
    }
}

#[test]
fn the_manifests_last_record_is_written_again_at_a_start() {
    // A start records a loss but the record's sync fails; the next start finds the record (in the page cache) and
    // writes it again before its sync, so a power loss keeps it.
    let mut r = rig_with_batches(
        3,
        WriterConfig {
            mark_sync: MarkSync::EveryCommit,
            ..cfg()
        },
    );
    r.shutdown().unwrap();
    r.w = None;
    let seg = log_path("00000000-0000000000000001.open");
    let bytes = r.fs.read(&seg).unwrap();
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: 1,
        generation: 0,
    };
    let second = scan(&bytes, &id, ScanLimit::End).unwrap().batches[1].offset;
    r.fs.poke(&seg, second as usize + 40, &[0xff; 8]);
    r.kill();
    // The repair's sync is the manifest's first; the record's is the second.
    r.fs.fail_nth(FaultOp::Sync, "MANIFEST", 1, io::ErrorKind::Other);
    assert!(r.open().is_none());
    r.kill();
    r.open().unwrap();
    r.w = None;
    r.fs.power_loss();
    let m = read_manifest(&r.fs.read(&dir().join("MANIFEST")).unwrap(), &STORE_ID).unwrap();
    assert_eq!(m.lost_ranges().count(), 1);
}

#[test]
fn a_mark_read_from_the_page_cache_at_a_same_boot_start_is_synced_before_the_writer_resumes() {
    let mut r = rig_with_batches(2, cfg());
    let last = r.acked.len() as u64;
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    let durable = read_mark(&r.fs.read_durable(&log_path("MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(durable.last_sequence, last);
}

/// Found by the differential test against the model (`model_histories.rs`): with the mark synced at every commit, a
/// batch whose mark was written but not synced when the process stopped is kept by the next start on the same boot,
/// read and delivered; that start syncs the mark, so a remount within the boot does not lose the batch.
#[test]
fn with_the_mark_synced_at_every_commit_a_start_syncs_a_mark_read_from_the_page_cache() {
    let mut r = rig_with_batches(1, cfg());
    r.kill();
    r.cfg.mark_sync = MarkSync::EveryCommit;
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    let durable = read_mark(&r.fs.read_durable(&log_path("MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(durable.last_sequence, 1);
    // The file system is unmounted and mounted again within the boot.
    r.w = None;
    r.fs.power_loss();
    r.fs.restart();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.losses.is_empty());
    r.check_started();
}

#[test]
fn a_start_begins_with_a_directory_sync_and_stops_if_it_fails() {
    let mut r = rig_with_batches(1, cfg());
    r.power_loss(None);
    r.fs.clear_ops();
    r.open().unwrap();
    let ops = r.fs.ops();
    assert_eq!((ops[0].op, ops[0].ok), (FaultOp::SyncDir, true));
    assert!(ops[0].path.ends_with("log"));
    // Step 3 syncs the kept segments and then the directory, before the manifest is touched.
    let manifest = ops
        .iter()
        .position(|o| o.path.ends_with("MANIFEST"))
        .unwrap();
    let last_segment_sync = ops[..manifest]
        .iter()
        .rposition(|o| o.op == FaultOp::Sync && o.path.to_string_lossy().ends_with(".open"))
        .unwrap();
    assert!(
        ops[last_segment_sync..manifest]
            .iter()
            .any(|o| o.op == FaultOp::SyncDir && o.path.ends_with("log")),
        "{ops:?}"
    );
    r.power_loss(None);
    r.fs.fail_nth(FaultOp::SyncDir, "log", 0, io::ErrorKind::Other);
    r.fs.clear_ops();
    assert!(r.open().is_none());
    let ops = r.fs.ops();
    assert_eq!(
        ops.len(),
        1,
        "nothing written or deleted after the failed directory sync: {ops:?}"
    );
}

#[test]
fn unused_segments_are_kept_only_with_a_valid_header_the_configured_size_and_zeros() {
    use kvik_log::header::{FileHeader, kind};
    let mut r = rig_with_batches(1, cfg());
    r.shutdown().unwrap();
    r.w = None;
    let mk = |r: &Rig, id: u64, len: usize, junk: bool| {
        let p = log_path(
            &SegmentName {
                generation: 0,
                file_id: id,
                sealed: false,
            }
            .to_string(),
        );
        let mut b = vec![0u8; len];
        b[..64].copy_from_slice(&FileHeader::new(kind::LOG_SEGMENT, 0, id, T0, STORE_ID).encode());
        if junk {
            b[200] = 1;
        }
        let f = r.fs.open(&p, kvik_log::fs::OpenMode::CreateNew).unwrap();
        f.write_at(&b, 0).unwrap();
        f.sync_all().unwrap();
        p
    };
    // Segment 2 is the one created ahead; add a junk one and a short one above it.
    let size = cfg().segment_size as usize;
    let s3 = mk(&r, 3, size, true);
    let s4 = mk(&r, 4, size / 2, false);
    let s5 = mk(&r, 5, size, false);
    r.kill();
    let rep = r.open().unwrap();
    let mut deleted = rep.deleted.clone();
    deleted.sort_unstable();
    assert_eq!(deleted, vec![2, 3, 4]);
    for p in [&s3, &s4] {
        assert!(r.fs.read(p).is_none());
    }
    assert!(
        r.fs.read(&s5).is_some(),
        "the highest valid one is kept as created ahead"
    );
}

#[test]
fn an_unadopted_entry_that_is_its_segments_first_batch_is_written_again() {
    // Q-117 for an entry that is not adopted: the last segment in use holds only a torn tail; the start writes its
    // entry at the header's end and the entry's sync fails, leaving the tail after it. The next start on the same
    // boot cannot adopt the entry (bytes follow it) but writes it again, with the header, before its sync.
    let mut r = rig_with_batches(1, cfg());
    r.shutdown().unwrap();
    r.w = None;
    // Segment 2, created ahead, is taken into use by its header and holds a torn tail.
    let seg2 = log_path("00000000-0000000000000002.open");
    let mut h = kvik_log::header::FileHeader::decode(&r.fs.read(&seg2).unwrap()).unwrap();
    h.base_sequence = 2;
    r.fs.poke(&seg2, 0, &h.encode());
    r.fs.poke(&seg2, 400, &[0x5a; 64]);
    r.power_loss(None);
    // Step 3 syncs segments 1 and 2; the entry's own sync is the next.
    r.fs.fail_nth(FaultOp::Sync, ".open", 2, io::ErrorKind::Other);
    assert!(r.open().is_none());
    r.kill();
    let rep = r.open().unwrap();
    assert!(!rep.entry.as_ref().unwrap().adopted);
    assert!(rep.rewrote_last_batch);
    r.submit(vec![fact(2, 0)]);
    r.drain();
    r.shutdown().unwrap();
    r.power_loss(None);
    let rep = r.open().unwrap();
    assert!(
        rep.losses.iter().all(|l| l.kind == LossKind::PossibleLoss),
        "{:?}",
        rep.losses
    );
    r.check_started();
}

/// A start that truncates a complete batch under rule 1 and stops before its recovery entry is on disk leaves the
/// batch in place, so the next start on the same boot finds it again and writes the entry: positions are never
/// assigned again without a reset point, whichever step the first start stopped at.
#[test]
fn a_rule_1_reset_cut_short_is_done_again_by_the_next_start() {
    for stop in ["MANIFEST", ".open"] {
        let mut r = Rig::new(cfg());
        r.submit(vec![fact(1, 0)]);
        r.drain();
        // A complete batch beyond the mark: synced, its mark write failed.
        r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
        r.submit(vec![fact(1, 1)]);
        r.step();
        let mark = r.mark().unwrap();
        r.kill();
        // The start stops at step 4 (the manifest's sync) or at the entry's own sync (step 5).
        r.fs.fail_nth(FaultOp::Sync, stop, 0, io::ErrorKind::Other);
        assert!(r.open().is_none(), "stopped at {stop}");
        r.kill();
        let rep = r.open().expect("the next start finishes");
        assert_eq!(rep.rule, StartRule::SameBoot);
        let e = rep.entry.as_ref().expect("the reset is done again");
        assert_eq!(e.entry.after, mark.last_sequence);
        assert_eq!(r.mark().unwrap().stamp, e.entry.stamp.unwrap());
        r.check_started();
    }
}

/// A start that wrote its rule 1 recovery entry but stopped before its mark: the next start on the same boot adopts
/// the entry, as rule 3 does (Q-117), so the reset is reported once and its position is not assigned twice.
#[test]
fn a_rule_1_entry_written_before_a_stop_is_adopted() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
    r.submit(vec![fact(1, 1)]);
    r.step();
    let mark = r.mark().unwrap();
    r.kill();
    // The entry is written and synced; the mark's write fails.
    r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
    assert!(r.open().is_none());
    r.kill();
    let first = r
        .batches()
        .into_iter()
        .find_map(|b| match b.items.as_slice() {
            [Item::Entry(Entry::Recovery(e))] => Some(e.clone()),
            _ => None,
        })
        .expect("the entry the stopped start wrote");
    let rep = r.open().expect("the next start finishes");
    assert_eq!(rep.rule, StartRule::SameBoot);
    let e = rep.entry.as_ref().expect("the entry");
    assert!(e.adopted);
    assert_eq!(e.entry, first);
    assert_eq!(e.entry.after, mark.last_sequence);
    assert!(rep.rewrote_last_batch);
    assert!(rep.losses.iter().all(|l| l.kind != LossKind::PossibleLoss));
    assert_eq!(r.mark().unwrap().stamp, first.stamp.unwrap());
    let entries = r
        .batches()
        .iter()
        .filter(|b| matches!(b.items.as_slice(), [Item::Entry(Entry::Recovery(_))]))
        .count();
    assert_eq!(entries, 1);
    r.check_started();
}

/// A rule 1 reset whose recovery entry does not fit after the mark, though the truncated batch did: the entry goes
/// to a new segment. A start stopped at every operation, then a start on the same boot: positions stay unique and
/// the truncated batch is followed by a recovery entry after the mark's position.
#[test]
fn a_rule_1_reset_into_a_new_segment_cut_at_every_operation() {
    let small = WriterConfig {
        segment_size: 4096,
        max_batch_bytes: 2048,
        ..cfg()
    };
    let entry_len = {
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: 1,
            generation: 0,
        };
        let e = kvik_log::entries::Recovery {
            after: 1,
            stamp: Some(kvik_log::mark::ResetStamp {
                sequence: 2,
                nonce: 1,
            }),
            boot_id: [1; 16],
            tail_bytes: 1,
            readable_commits: 1,
            mark_state: MarkState::Readable,
            id_floor: 1,
            consumers: Vec::new(),
        };
        let c = BatchContent {
            first_sequence: 2,
            received_at: T0,
            items: vec![Item::Entry(Entry::Recovery(e))],
        };
        c.encode(&id, 64, T0).unwrap().len() as u64
    };
    // A rig whose open segment has room for one more one-fact batch but not for the entry, with that batch written
    // beyond the mark (its mark write failed); the segment size is chosen so that the batch lands there.
    let setup = |size: u64| {
        let mut r = Rig::new(WriterConfig {
            segment_size: size,
            ..small.clone()
        });
        let mut t = 0;
        loop {
            let m = r.mark().unwrap();
            if m.file_id > 0 && size - m.offset < entry_len {
                break;
            }
            r.submit(vec![fact(1, t)]);
            r.drain();
            t += 1;
        }
        let m = r.mark().unwrap();
        r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
        r.submit(vec![fact(1, t)]);
        r.step();
        let seg = log_path(
            &SegmentName {
                generation: 0,
                file_id: m.file_id,
                sealed: false,
            }
            .to_string(),
        );
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: m.file_id,
            generation: 0,
        };
        let bytes = r.fs.read(&seg)?;
        let found = scan(&bytes, &id, ScanLimit::End).unwrap();
        let beyond = found
            .batches
            .iter()
            .find(|b| b.commit.first_sequence > m.last_sequence)
            .map(|b| BatchContent::decode(b).unwrap())?;
        r.kill();
        Some((r, m, beyond))
    };
    // Every segment size in the range where the batch fits and the entry does not.
    let sizes: Vec<u64> = (4096..4800)
        .step_by(8)
        .filter(|&z| setup(z).is_some())
        .collect();
    assert!(sizes.len() >= 2, "{sizes:?}");
    for size in sizes {
        cut_everywhere(&|| setup(size).expect("the same setup"));
    }
}

/// Runs `setup`, stops the start that follows at every operation in turn, and checks the start after it.
fn cut_everywhere(setup: &dyn Fn() -> (Rig, MarkSlot, BatchContent)) {
    let mut finished = false;
    for n in 0..200 {
        let (mut r, m, beyond) = setup();
        r.fs.crash_after(n);
        let first = r.open();
        let crashed = r.fs.crashed();
        r.kill();
        if first.is_some() && !crashed {
            finished = true;
        }
        if first.is_none() || crashed {
            assert!(r.open().is_some(), "cut at {n}: the next start finishes");
        }
        r.check_started();
        let now = r.batches();
        assert!(
            !now.contains(&beyond),
            "cut at {n}: the truncated batch is gone"
        );
        let entries: Vec<_> = now
            .iter()
            .filter_map(|b| match b.items.as_slice() {
                [Item::Entry(Entry::Recovery(e))] => Some(e.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(entries.len(), 1, "cut at {n}: one entry, reported once");
        assert_eq!(entries[0].after, m.last_sequence, "cut at {n}");
        assert!(
            r.fs.paths()
                .iter()
                .filter(|p| p.to_string_lossy().ends_with(".open"))
                .count()
                >= 2
        );
        if finished {
            // The entry went to a later segment than the mark's, and the start was cut at every operation before.
            assert!(r.mark().unwrap().file_id > m.file_id);
            assert!(n > 5, "{n} operations");
            break;
        }
    }
    assert!(finished, "the uncut start finishes");
}

/// Found by the deep run: a remount leaves two complete batches beyond the mark on disk, so the mark is stale and
/// rule 3 keeps both; that start fails its mark sync after writing its entry, and the next start, under rule 1, finds
/// them below the mark its predecessor wrote. Keeping them is rule 3's choice, not a reset owed by rule 1.
#[test]
fn kill_loop_a_stale_start_that_fails_after_its_entry() {
    let cycles = vec![
        Cycle {
            calls: vec![],
            fault: Fault::None,
            crash: Crash::Kill,
            crash_start: None,
            start_fault: Fault::None,
            after_fault: None,
            clean: false,
        },
        Cycle {
            calls: vec![(1, false)],
            fault: Fault::None,
            crash: Crash::Power(0),
            crash_start: None,
            start_fault: Fault::Sync("MARK", 1),
            after_fault: None,
            clean: false,
        },
        Cycle {
            calls: vec![(1, false)],
            fault: Fault::None,
            crash: Crash::Kill,
            crash_start: Some(5),
            start_fault: Fault::None,
            after_fault: None,
            clean: false,
        },
        Cycle {
            calls: vec![(1, false)],
            fault: Fault::None,
            crash: Crash::Remount(1_678_373_410_284_916_357),
            crash_start: None,
            start_fault: Fault::Sync("MARK", 0),
            after_fault: None,
            clean: false,
        },
    ];
    for small in [false, true] {
        kill_loop(&cycles, true, small);
    }
}

/// A remount within the boot can leave the next segment taken into use by its header alone (base F) while the batch
/// beyond the mark sits in the mark's segment: rule 1 still truncates that batch, a reset point, and the recovery
/// entry opens the header-only segment.
#[test]
fn a_batch_beyond_the_mark_before_a_header_only_segment_is_a_reset() {
    let mut r = Rig::new(cfg());
    r.submit(vec![fact(1, 0)]);
    r.drain();
    r.fs.fail_nth(FaultOp::Write, "MARK", 0, io::ErrorKind::Other);
    r.submit(vec![fact(1, 1)]);
    r.step();
    let mark = r.mark().unwrap();
    r.kill();
    // The segment created ahead, taken into use by its header with base F and nothing after it.
    let ahead =
        r.fs.paths()
            .into_iter()
            .find(|p| {
                SegmentName::parse(p.file_name().unwrap().to_str().unwrap())
                    .is_some_and(|n| n.file_id > mark.file_id && !n.sealed)
            })
            .expect("a segment created ahead");
    let mut h = kvik_log::header::FileHeader::decode(&r.fs.read(&ahead).unwrap()).unwrap();
    assert_eq!(h.base_sequence, 0);
    h.base_sequence = mark.last_sequence + 1;
    r.fs.poke(&ahead, 0, &h.encode());
    let beyond: Vec<_> = r
        .batches()
        .into_iter()
        .filter(|b| b.first_sequence > mark.last_sequence)
        .collect();
    assert_eq!(beyond.len(), 1);
    let rep = r.open().expect("the start finishes");
    assert_eq!(rep.rule, StartRule::SameBoot);
    let e = rep.entry.as_ref().expect("a reset point");
    assert!(!e.adopted);
    assert_eq!(e.entry.after, mark.last_sequence);
    assert!(!r.batches().contains(&beyond[0]), "the batch is truncated");
    assert!(
        r.mark().unwrap().file_id > mark.file_id,
        "the entry opens the next segment"
    );
    assert!(rep.losses.iter().any(|l| l.kind == LossKind::PossibleLoss));
    r.check_started();
}

/// A definition entry of a gauge.
fn definition(id: u32) -> Item {
    use kvik_log::entries::{Definition, Kind, Retention, Timing, Versioning};
    Item::Entry(Entry::Definition(Box::new(Definition {
        id,
        name: format!("s{id}"),
        kind: Kind::Gauge,
        value_type: kvik_log::facts::ValueType::F64,
        scale: None,
        unit: "u".into(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: 1_000_000_000,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: None,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    })))
}

/// Q-122: updates between two mark syncs go to the slot that does not hold the last synced mark, so a power loss that
/// tears the last of two unsynced updates leaves the last synced mark, never an older one (`format.md` A.10 Update).
/// Before, the second update overwrote the synced slot and the start fell back to the mark before it, with a lower
/// highest series id, which the recovery entry's id floor and the new mark then kept.
#[test]
fn a_torn_mark_update_never_falls_back_past_the_last_synced_mark() {
    let mut r = Rig::new(cfg());
    // Each definition forces a mark sync: both slots then hold synced marks, the later one naming series 2.
    r.submit(vec![definition(1)]);
    r.drain();
    r.submit(vec![definition(2)]);
    r.drain();
    let mark_file = log_path("MARK");
    let synced = read_mark(&r.fs.read_durable(&mark_file).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(synced.max_series_id, 2);
    // Two updates the timer has not synced yet.
    r.submit(vec![fact(1, 1)]);
    r.drain();
    r.submit(vec![fact(1, 2)]);
    r.drain();
    let current = r.fs.read(&mark_file).unwrap();
    let last = read_mark(&current, &STORE_ID).unwrap().unwrap();
    assert_eq!(
        last.1.counter,
        synced.counter + 2,
        "both updates are unsynced"
    );
    // A power loss tears the last update: half of its slot reaches the disk.
    r.w = None;
    r.fs.power_loss();
    let at = SLOT_OFFSETS[last.0];
    r.fs.poke(&mark_file, at, &current[at..at + SLOT_LEN / 2]);
    r.fs.restart();
    r.boot += 1;
    let found = read_mark(&r.fs.read(&mark_file).unwrap(), &STORE_ID)
        .unwrap()
        .expect("the other slot is valid")
        .1;
    assert_eq!(found, synced, "the last synced mark");
    let rep = r.open().unwrap();
    let entry = rep.entry.expect("a recovery entry");
    assert_eq!(entry.entry.mark_state, MarkState::Readable);
    assert!(entry.entry.id_floor >= synced.max_series_id);
    r.check_started();
}

/// Q-116 (p): a timed seal takes the segment created ahead into use and the sync of its header fails, leaving the
/// header in the page cache only. The start on the same boot writes the header of the segment it resumes in again,
/// in place, before its first sync, so a batch committed there makes it durable, and once the segment is sealed and
/// the power is lost its `.kvl` header reads in use and its `file_crc32c` verifies.
#[test]
fn a_header_put_in_use_by_a_failed_sync_is_written_again_by_a_same_boot_start() {
    let mut r = Rig::new(WriterConfig {
        seal_interval: Duration::from_secs(1),
        ..cfg()
    });
    r.submit(vec![fact(1, 0)]);
    r.drain();
    let open = r.w().stats().open_file_id.unwrap();
    let ahead =
        r.fs.paths()
            .into_iter()
            .filter_map(|p| SegmentName::parse(p.file_name()?.to_str()?))
            .find(|n| !n.sealed && n.file_id > open)
            .expect("a segment created ahead");
    let path = log_path(&ahead.to_string());
    let base = |b: &[u8]| {
        kvik_log::header::FileHeader::decode(b)
            .unwrap()
            .base_sequence
    };
    // The timed seal: the header write that takes the segment into use is not synced.
    r.fs.fail_nth(FaultOp::Sync, &ahead.to_string(), 0, io::ErrorKind::Other);
    r.advance(Duration::from_secs(2));
    r.step();
    assert!(r.w().fault().is_some());
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.entry.is_none());
    assert_eq!(r.w().stats().open_file_id, Some(ahead.file_id));
    assert_eq!(base(&r.fs.read(&path).unwrap()), 2);
    assert_eq!(
        base(&r.fs.read_durable(&path).unwrap()),
        0,
        "not yet synced"
    );
    // A batch in it: its sync makes the header written again durable.
    r.submit(vec![fact(1, 1)]);
    r.drain();
    assert!(r.w().fault().is_none());
    assert_eq!(r.acked.len(), 2);
    assert_eq!(base(&r.fs.read_durable(&path).unwrap()), 2);
    // Then a timed seal of that segment, and a power loss.
    r.advance(Duration::from_secs(2));
    r.step();
    r.maint();
    assert!(r.w().fault().is_none());
    let sealed = SegmentName {
        sealed: true,
        ..ahead
    };
    r.power_loss(None);
    let bytes =
        r.fs.read_durable(&log_path(&sealed.to_string()))
            .expect("sealed");
    assert_eq!(base(&bytes), 2);
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id: ahead.file_id,
        generation: ahead.generation,
    };
    let seal = scan(&bytes, &id, ScanLimit::End)
        .unwrap()
        .seal
        .expect("a seal");
    assert!(seal.file_crc_matches(&bytes), "file_crc32c verifies");
    let rep = r.open().unwrap();
    assert!(rep.losses.is_empty(), "nothing lost: {rep:?}");
    let facts = r
        .batches()
        .iter()
        .filter(|c| matches!(c.items.as_slice(), [Item::Fact(_)]))
        .count();
    assert_eq!(facts, 2, "both acknowledged batches kept");
    r.check_started();
}

/// The mark on stable storage.
fn durable_mark(r: &Rig) -> MarkSlot {
    read_mark(&r.fs.read_durable(&log_path("MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1
}

/// A power loss that tears the latest mark update in the page cache: half of its slot reaches the disk.
fn tear_last_mark_update(r: &mut Rig) {
    let path = log_path("MARK");
    let current = r.fs.read(&path).unwrap();
    let (slot, _) = read_mark(&current, &STORE_ID).unwrap().unwrap();
    let at = SLOT_OFFSETS[slot];
    r.w = None;
    r.fs.set_sync_hook(None);
    r.fs.power_loss();
    r.fs.poke(&path, at, &current[at..at + SLOT_LEN / 2]);
    r.fs.restart();
    r.boot += 1;
}

/// Q-122, a start's own mark write: a fact batch's update is in the page cache only, the next batch is synced and the
/// process stops before its mark update, and the start on the same boot (rule 1, a reset point) writes its mark. The
/// mark file as read is made durable first, so tearing that write leaves the update before it, not an older mark.
#[test]
fn a_starts_torn_mark_write_never_falls_back_past_the_last_synced_mark() {
    let mut r = Rig::new(cfg());
    r.submit(vec![definition(1)]);
    r.drain();
    r.submit(vec![definition(2)]);
    r.drain();
    let synced = durable_mark(&r);
    assert_eq!(synced.max_series_id, 2);
    r.submit(vec![fact(1, 1)]);
    r.drain();
    // The next batch: its write and sync, then the process stops before the mark's update.
    r.submit(vec![fact(1, 2)]);
    r.fs.crash_after(2);
    r.step();
    r.kill();
    // The start stops right after its own mark write, before that write's sync.
    let fs = r.fs.clone();
    r.fs.set_sync_hook(Some(Arc::new(move |p: &std::path::Path| {
        let last = fs.ops().last().cloned();
        if p.ends_with("MARK")
            && last.is_some_and(|o| o.op == FaultOp::Write && o.len == SLOT_LEN as u64)
        {
            fs.crash_after(0);
        }
    })));
    assert!(r.open().is_none(), "the start stops at its mark's sync");
    tear_last_mark_update(&mut r);
    let found = read_mark(&r.fs.read(&log_path("MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert!(found.counter > synced.counter, "found {found:?}");
    assert_eq!(found.max_series_id, 2);
    let rep = r.open().unwrap();
    assert!(rep.entry.unwrap().entry.id_floor >= 2);
    r.check_started();
}

/// Q-122 with a failed mark sync: the update whose sync failed is no longer dirty, so a start on the same boot writes
/// its slot again before syncing it; the next update then goes to the other slot, and tearing it leaves that mark.
#[test]
fn a_mark_whose_sync_failed_is_written_again_by_a_same_boot_start() {
    let mut r = Rig::new(cfg());
    r.submit(vec![definition(1)]);
    r.drain();
    r.submit(vec![definition(2)]);
    r.drain();
    let synced = durable_mark(&r);
    r.submit(vec![fact(1, 1)]);
    r.drain();
    // The timed mark sync fails: the store is read-only.
    r.fs.fail_nth(FaultOp::Sync, "MARK", 0, io::ErrorKind::Other);
    r.advance(Duration::from_secs(2));
    r.step();
    assert!(r.w().fault().is_some());
    r.kill();
    let rep = r.open().unwrap();
    assert_eq!(rep.rule, StartRule::SameBoot);
    assert!(rep.entry.is_none());
    let kept = durable_mark(&r);
    assert_eq!(
        kept.counter,
        synced.counter + 1,
        "the update is on stable storage"
    );
    r.submit(vec![fact(1, 2)]);
    r.drain();
    tear_last_mark_update(&mut r);
    let found = read_mark(&r.fs.read(&log_path("MARK")).unwrap(), &STORE_ID)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(found, kept, "the mark synced last");
    r.open().unwrap();
    r.check_started();
}
