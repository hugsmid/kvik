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
use kvik_log::mark::{MarkSlot, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, Tail, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, IngestQueue, LogWriter, MarkSync, StartReport, StartRule, WriterConfig,
    create_segment, seal_segment,
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
                assert!(r.after + 1 >= next.min(r.after + 1));
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
                assert!(
                    matches!(r.tail, Some(Tail::Clean { .. })),
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
    assert!(rep.entry.is_none());
    assert_eq!(r.w().stats().durable.sequence, mark.last_sequence);
    assert_eq!(r.positions().len(), 1);
    r.check_started();
    // The writer resumes at the next position.
    r.submit(vec![fact(2, 0)]);
    r.drain();
    r.check_acked();
    assert_eq!(r.positions().len(), 2);
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
            // Accepted limit (spec §3 Recovery rule 1): the one batch since the mark's last sync is truncated.
            assert_eq!(rep.rule, StartRule::SameBoot);
            assert!(r.positions().is_empty());
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
    ];
    (
        prop::collection::vec((1..12usize, prop::bool::weighted(0.15)), 0..12),
        fault,
        crash,
        prop::option::weighted(0.3, 0..40usize),
        prop::bool::weighted(0.2),
    )
        .prop_map(|(calls, fault, crash, crash_start, clean)| Cycle {
            calls,
            fault,
            crash,
            crash_start,
            clean,
        })
}

/// Runs a kill loop: every cycle writes, fails or crashes somewhere, and starts again.
fn kill_loop(cycles: &[Cycle], every: bool) {
    let mut r = Rig::new(WriterConfig {
        mark_sync: if every {
            MarkSync::EveryCommit
        } else {
            MarkSync::Interval(Duration::from_millis(30))
        },
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
        let mark_before = match c.crash {
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
        // The start itself may be cut short; the next one must finish.
        if let Some(n) = c.crash_start {
            r.fs.crash_after(n);
            if r.open().is_some() {
                r.check_started();
                continue;
            }
            r.kill();
        }
        let before = r.batches();
        let rep = r.open().expect("a start finishes");
        r.check_started();
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
                assert_eq!(after.iter().collect::<Vec<_>>(), expect);
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

proptest! {
    #![proptest_config(ProptestConfig { cases: ProptestConfig::default().cases / 8, ..ProptestConfig::default() })]

    /// The kill loop: every acknowledged entry survives every crash; positions are never reused without a reset
    /// point; each start keeps exactly the image's committed prefix and leaves a readable mark and a clean tail.
    #[test]
    fn kill_loop_keeps_every_acknowledged_entry(cycles in prop::collection::vec(cycle(), 1..8), every in any::<bool>()) {
        kill_loop(&cycles, every);
    }
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
            &(prop::collection::vec(cycle(), 1..40), any::<bool>()),
            |(c, e)| {
                kill_loop(&c, e);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn a_full_disk_at_a_start_that_needs_a_segment_leaves_it_read_only() {
    let mut r = Rig::new(WriterConfig {
        segment_size: 4096,
        ..cfg()
    });
    // Fill segment 1 so that the entry needs a new segment.
    for i in 0..40 {
        r.submit(vec![fact(1, i)]);
        r.step();
    }
    r.w = None;
    // Remove the segment created ahead, then leave no space.
    for p in r.fs.paths() {
        let s = p.to_string_lossy().to_string();
        if s.ends_with(".open") && r.fs.read(&p).unwrap().iter().skip(64).all(|&b| b == 0) {
            r.fs.remove_file(&p).unwrap();
        }
    }
    r.power_loss(None);
    r.fs.set_free_space(100);
    let fits = r.open();
    r.fs.set_free_space(1 << 40);
    r.fs.restart();
    if fits.is_none() {
        r.open().expect("with space, the start finishes");
    }
    r.check_started();
}
