//! Differential test of the log against the reference model (`kvik-model`) on generated histories (roadmap 4.7).
//!
//! A history is a sequence of operations: series definitions (observed, scheduled and forecast; versioned or not;
//! gauges, counters, interval series, states and text), readings with corrections, revisions, duplicates and
//! values recorded without one, aliases, retirements, retention changes, site-zone changes, clock steps, crashes on
//! the same boot, power losses, remounts, failed syncs and reader refreshes. Each history runs through the model and
//! through the log: a [`LogWriter`] driven by hand on a [`MemFs`], restarted (with recovery) after every crash, and a
//! [`LogReader`] in another handle. The model decides what each batch holds (stamping, flags, the duplicate rule);
//! the log must keep exactly what the model says is committed, at the positions the model gives, and the reader must
//! return exactly the log up to the durable mark.
//!
//! Recovery entries are compared with the model's: both write one, with a new reset stamp, after every start under
//! `spec.md` §3 Recovery rule 3 and after a start under rule 1 that truncates a complete batch, at F = K + 1 (no
//! position is named above K here: no cursor file, derived file or lost range exists), with the same K, mark state,
//! id floor and stamp sequence. Where the history fixes the tail (a clean power loss, a torn batch, a truncated
//! complete batch), the log reports a possible loss at the entry exactly when the model lists one there; after a stop
//! at an arbitrary operation the file system chooses, and the tails are not compared.
//!
//! The differences the specification allows, made explicit here:
//! - **Starts cut short.** The model has no start cut short: it starts once where the log starts twice, and a cut
//!   start's entry may be adopted (`spec.md` §3 Recovery rule 3, Adoption) or not, so after one the log may hold
//!   more or fewer recovery entries than the model. From then on entries are compared with recovery entries left out
//!   on both sides, positions renumbered by the entries before them.
//! - **Unacknowledged batches.** A batch whose commit was cut short may be kept or not (`spec.md` §3 Write or sync
//!   failure, §9 rows 7 and 7a). For faults the model names, the log is made to stop at the same point; for stops at
//!   arbitrary operations the log decides, and the model is given the fault that has the same outcome.
//! - **Open questions.** A torn batch alone in a segment whose header on disk reads unused is deleted unreported
//!   (Q-116 (f), (l) and (p)); accepted only when the header on disk does read unused.
//!
//! Left out, with the reason:
//! - Routing changes: entry type 5 since roadmap 5.2; routing and delivery are tested against the model at the
//!   interface (`kvik/tests/consumers.rs`).
//! - The timed mark sync in the model: the model can lose acknowledged batches above its last synced mark
//!   (`MarkSync::Timed`, `Model::remount`, `Model::power_loss_losing`), but the log syncs its mark on its own timer, so
//!   the model runs with the mark synced at every commit; remounts run with the log's mark synced at every commit too,
//!   and are a crash on the same boot otherwise. Matching the two timers is left to the differential tests of roadmap
//!   5.2 and 5.3.
//! - Wall-clock steps backwards: the batch's `received_at` is then open (Q-115 (i)); the wall clock only moves on.
//! - Defaults of a definition: resolving them is the registry's (roadmap 5.1); every definition here sets them.
//! - Corruption below the mark and tzdata entries: covered by `recovery.rs` and `model_differential.rs`.
//! - Write errors, a full disk and clean shutdowns: covered by `recovery.rs` (the kill loop) and `writer.rs`; the
//!   model has no counterpart for a full disk at the log's level. A failed mark sync, which the model has no
//!   counterpart for either, is followed at once by a crash on the same boot.
//! - A batch whose sync failed surviving a remount: a remount runs only with the mark synced at every commit, and a
//!   failed sync's bytes never reach the disk in the in-memory file system unless written again.
//! - A reader voiding what it read: it reads only up to the mark, which a start on the same boot never moves back
//!   here (a remount that loses mark updates is left out above), so its voids remove nothing.

mod convert;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use convert::*;
use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{Entry as LogEntry, MarkState};
use kvik_log::fs::{FaultOp, Fs, MemFs};
use kvik_log::header::FileHeader;
use kvik_log::manifest::LossCause;
use kvik_log::mark::{MarkSlot, read_mark};
use kvik_log::name::SegmentName;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Ack, Call, Clock, CommitError, IngestQueue, LogWriter, LossKind, MarkSync, StartReport,
    StartRule, WriterConfig, create_segment, seal_segment,
};
use kvik_model::engine::{
    Config, CrashPoint, Entry as ModelEntry, EntryBody, Fault, Model, WriteOutcome,
};
use kvik_model::recovery::{LossKind as ModelLossKind, MarkState as ModelMarkState};
use kvik_model::registry::{ChangedAtKind, RegistryEvent, RetentionClass, SeriesOptions, Timing};
use kvik_model::summary::CounterRules;
use kvik_model::types::{
    IntervalSpec, Kind, Position, Quality, Reading, SECOND, SeriesId, TSource, Value, ValueType,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

const STORE_ID: [u8; 16] = [9; 16];
/// 2026-01-01T00:00:00Z: the model's wall clock at the start of every history.
const T0: i64 = 1_767_225_600 * SECOND;
const DAY: i64 = 86_400 * SECOND;
const QUALITIES: [Quality; 4] = [
    Quality::Good,
    Quality::Estimated,
    Quality::Suspect,
    Quality::Bad,
];
/// Site zones a history may change to.
const ZONES: [&str; 3] = ["Etc/GMT-1", "Etc/GMT+3", "UTC"];

// ----- histories -----

/// A series definition, every option given (the defaults are the registry's to resolve, roadmap 5.1).
#[derive(Clone, Debug)]
struct SeriesSpec {
    kind: u8,
    timing: u8,
    versioned: u8,
    record_on_change: bool,
    interval: u8,
    phase: u8,
    lateness: u8,
    wraps: bool,
    ceiling: u8,
    unit: u8,
}

/// A reading, resolved against the series defined when it is emitted.
#[derive(Clone, Debug)]
struct ReadingSpec {
    pick: u8,
    slot: u8,
    value: u8,
    /// 0: a value; 1: none; 2: not finite; 3: negative.
    special: u8,
    quality: u8,
    revise: bool,
    version: u8,
    host: bool,
    ahead: bool,
}

#[derive(Clone, Debug)]
enum Op {
    Define(SeriesSpec),
    /// Readings committed in one batch, with a fault the model names.
    Ingest(Vec<ReadingSpec>, Option<Fault>),
    /// Readings whose commit stops after `n` more file operations, then a kill (same boot) or a power loss that
    /// keeps a part of the unsynced writes chosen by the seed.
    Stop {
        readings: Vec<ReadingSpec>,
        n: usize,
        power: bool,
        seed: u64,
    },
    Rename(u8),
    Retire(u8, u8),
    Retention(u8, Option<u8>),
    SiteZone(u8),
    /// Milliseconds.
    Advance(u32),
    ClockSync(bool),
    Crash,
    PowerLoss,
    Remount,
    Refresh,
    /// A crash (or a power loss), then a start that stops after `n` file operations, then a start on the same boot,
    /// or with `then`, a power loss that keeps a part of the unsynced writes chosen by it and a start.
    CutStart {
        n: usize,
        power: bool,
        then: Option<u64>,
    },
    /// The timed mark sync fails, the process stops and starts again on the same boot; with a seed, a power loss that
    /// keeps a part of the unsynced writes follows. Under a mark synced at every commit, or a clean mark, a crash.
    MarkSyncFails(Option<u64>),
}

fn series_spec() -> impl Strategy<Value = SeriesSpec> {
    (
        0..6u8,
        0..3u8,
        0..3u8,
        any::<bool>(),
        0..4u8,
        0..8u8,
        0..8u8,
        any::<bool>(),
        any::<u8>(),
        0..3u8,
    )
        .prop_map(
            |(kind, timing, versioned, roc, interval, phase, lateness, wraps, ceiling, unit)| {
                SeriesSpec {
                    kind,
                    timing,
                    versioned,
                    record_on_change: roc,
                    interval,
                    phase,
                    lateness,
                    wraps,
                    ceiling,
                    unit,
                }
            },
        )
}

fn reading_spec() -> impl Strategy<Value = ReadingSpec> {
    (
        any::<u8>(),
        0..24u8,
        0..6u8,
        prop_oneof![12 => Just(0u8), 1 => Just(1u8), 1 => Just(2u8), 1 => Just(3u8)],
        prop_oneof![5 => Just(0u8), 1 => 1..4u8],
        prop::bool::weighted(0.1),
        0..4u8,
        prop::bool::weighted(0.1),
        prop::bool::weighted(0.1),
    )
        .prop_map(
            |(pick, slot, value, special, quality, revise, version, host, ahead)| ReadingSpec {
                pick,
                slot,
                value,
                special,
                quality,
                revise,
                version,
                host,
                ahead,
            },
        )
}

fn readings() -> impl Strategy<Value = Vec<ReadingSpec>> {
    prop::collection::vec(reading_spec(), 1..12)
}

fn outcome() -> impl Strategy<Value = WriteOutcome> {
    prop_oneof![
        Just(WriteOutcome::Lost),
        Just(WriteOutcome::Torn),
        Just(WriteOutcome::Survived)
    ]
}

/// The faults of a commit that concern the log (a crash during delivery is a crash after the commit, here).
fn fault() -> impl Strategy<Value = Fault> {
    prop_oneof![
        Just(Fault::Crash(CrashPoint::BeforeWrite)),
        outcome().prop_map(|o| Fault::Crash(CrashPoint::AfterWrite(o))),
        Just(Fault::Crash(CrashPoint::AfterSyncBeforeMark)),
        Just(Fault::Crash(CrashPoint::AfterSync)),
        outcome().prop_map(Fault::PowerLoss),
        outcome().prop_map(Fault::SyncError),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => series_spec().prop_map(Op::Define),
        10 => (readings(), prop::option::weighted(0.25, fault())).prop_map(|(r, f)| Op::Ingest(r, f)),
        2 => (readings(), 0..12usize, any::<bool>(), any::<u64>())
            .prop_map(|(readings, n, power, seed)| Op::Stop { readings, n, power, seed }),
        1 => any::<u8>().prop_map(Op::Rename),
        1 => (any::<u8>(), 0..40u8).prop_map(|(s, at)| Op::Retire(s, at)),
        1 => (any::<u8>(), prop::option::of(0..60u8)).prop_map(|(s, p)| Op::Retention(s, p)),
        1 => (0..3u8).prop_map(Op::SiteZone),
        3 => prop_oneof![1..100u32, 100..400_000u32].prop_map(Op::Advance),
        1 => any::<bool>().prop_map(Op::ClockSync),
        1 => Just(Op::Crash),
        1 => Just(Op::PowerLoss),
        1 => Just(Op::Remount),
        1 => (0..40usize, any::<bool>(), prop::option::of(any::<u64>()))
            .prop_map(|(n, power, then)| Op::CutStart { n, power, then }),
        1 => prop::option::of(any::<u64>()).prop_map(Op::MarkSyncFails),
        2 => Just(Op::Refresh),
    ]
}

/// A history: some series first, then any operations; and whether the mark is synced at every commit.
fn history(ops: usize) -> impl Strategy<Value = (Vec<Op>, bool)> {
    (
        prop::collection::vec(series_spec().prop_map(Op::Define), 1..5),
        prop::collection::vec(op(), 1..ops),
        any::<bool>(),
    )
        .prop_map(|(mut first, rest, every)| {
            first.extend(rest);
            (first, every)
        })
}

// ----- coverage -----

/// What the histories reached, counted over every history of a run; the long variant requires each.
#[derive(Clone, Copy, Debug)]
enum Count {
    Rule1Resets,
    Reboots,
    StopsKept,
    StopsLost,
    TornReported,
    TornDeleted,
    CutStarts,
    TornAfterCutStarts,
    MarkSyncFailures,
    Adoptions,
    Voids,
    SealedSegments,
}

const COUNTS: usize = 12;
static REACHED: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

fn count(c: Count) {
    REACHED[c as usize].fetch_add(1, Ordering::Relaxed);
}

fn reached(c: Count) -> u64 {
    REACHED[c as usize].load(Ordering::Relaxed)
}

/// The configuration of a property test with `cases / divisor` cases: each case is a whole history. Passed to a
/// runner of our own, since `proptest!` would set the cases from `PROPTEST_CASES` again.
fn config(divisor: u32) -> ProptestConfig {
    let d = ProptestConfig::default();
    ProptestConfig {
        cases: (d.cases / divisor).max(1),
        ..d
    }
}

// ----- the log side -----

#[derive(Debug)]
struct TestClock(Mutex<(Duration, i64)>);

impl Clock for TestClock {
    fn monotonic(&self) -> Duration {
        self.0.lock().unwrap().0
    }
    fn wall(&self) -> i64 {
        self.0.lock().unwrap().1
    }
}

/// A stop planned at the sync of the batch being committed: armed before a step, fired by the sync hook.
#[derive(Clone, Copy, Debug)]
enum Arm {
    /// Every operation fails after `n` more: 0 stops before the sync completes, 1 after it, 2 after the mark's
    /// positional write.
    Stop(usize),
    /// The sync fails.
    FailSync,
}

/// A write of a step and the bytes it wrote.
type Write = (PathBuf, u64, Vec<u8>);

/// What the start after a crash must report.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Rule 1; with `Some(true)` a complete batch lay beyond the mark, so the start writes a recovery entry with a new
    /// reset stamp and reports a possible loss (`spec.md` §3 Recovery rule 1); with `Some(false)` none did, and
    /// nothing is reported; `None` when either may be.
    SameBoot(Option<bool>),
    /// Rule 3: a recovery entry written, and the tail it reports.
    Reboot(Tail),
    /// After a start cut short: either rule, and an entry the cut start wrote may be adopted.
    Any,
}

/// The tail a start under rule 3 finds.
#[derive(Clone, Copy, Debug)]
enum Tail {
    /// Torn or not: the file system chose.
    Either,
    /// Nothing torn: no loss is reported.
    Clean,
    /// A batch torn in this segment: reported as a possible loss, unless the segment reads as unused, its header
    /// being put in use by no write that reached the disk, and is deleted with the torn bytes (Q-116 (f), (l) and
    /// (p)).
    Torn { file_id: u64 },
}

/// The tail a power loss leaves of a batch with this outcome and these writes.
fn tail_of(outcome: WriteOutcome, writes: &[Write]) -> Tail {
    match (outcome, writes.last()) {
        (WriteOutcome::Torn, Some((path, _, _))) => Tail::Torn {
            file_id: path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(SegmentName::parse)
                .expect("a batch is written to a segment")
                .file_id,
        },
        _ => Tail::Clean,
    }
}

/// The log: a writer driven by hand on an in-memory file system, and a reader in another handle.
struct Log {
    fs: MemFs,
    boot: u8,
    cfg: WriterConfig,
    every: bool,
    w: Option<LogWriter>,
    q: IngestQueue,
    clock: Arc<TestClock>,
    reader: Option<LogReader>,
    /// What the reader returned, by sequence, with its voids applied.
    view: BTreeMap<u64, Item>,
    /// Every acknowledged entry, by sequence.
    acked: BTreeMap<u64, Item>,
    /// A batch whose sync failed: its outcome at the next power loss and its writes.
    pending: Option<(WriteOutcome, Vec<Write>)>,
    /// After a power loss, the segment of the torn batch reads unused on disk: its header never reached it.
    torn_header_unused: bool,
    /// The mark as the last start found it, if readable.
    before: Option<MarkSlot>,
    armed: Arc<Mutex<Option<Arm>>>,
}

fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

fn mark_path() -> PathBuf {
    Path::new("store").join("log").join("MARK")
}

impl Log {
    fn new(every: bool) -> Self {
        let cfg = WriterConfig {
            segment_size: 8 << 10,
            max_batch_bytes: 4 << 10,
            min_sync_interval: Duration::ZERO,
            mark_sync: if every {
                MarkSync::EveryCommit
            } else {
                MarkSync::Interval(Duration::from_millis(30))
            },
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            ..WriterConfig::default()
        };
        let fs = MemFs::new();
        let armed: Arc<Mutex<Option<Arm>>> = Arc::default();
        let (hook_fs, hook_armed) = (fs.clone(), armed.clone());
        // Fires at the sync of a segment after a batch was written to it in this step (a segment taken into use
        // without a batch has only its header written).
        fs.set_sync_hook(Some(Arc::new(move |path: &Path| {
            if !path.to_string_lossy().ends_with(".open") {
                return;
            }
            let mut armed = hook_armed.lock().unwrap();
            let Some(arm) = *armed else { return };
            let batch_written = hook_fs
                .ops()
                .iter()
                .any(|o| o.op == FaultOp::Write && o.ok && o.path == path && o.offset >= 64);
            if !batch_written {
                return;
            }
            *armed = None;
            match arm {
                Arm::Stop(n) => hook_fs.crash_after(n),
                Arm::FailSync => hook_fs.fail_nth(
                    FaultOp::Sync,
                    &path.to_string_lossy(),
                    0,
                    io::ErrorKind::Other,
                ),
            }
        })));
        Self {
            fs,
            boot: 1,
            q: IngestQueue::new(cfg.ingest_queue),
            cfg,
            every,
            w: None,
            clock: Arc::new(TestClock(Mutex::new((Duration::ZERO, T0)))),
            reader: None,
            view: BTreeMap::new(),
            acked: BTreeMap::new(),
            pending: None,
            torn_header_unused: false,
            before: None,
            armed,
        }
    }

    /// Starts the writer with the file system stopping after `n` more operations; the start may finish or not.
    fn open_cut(&mut self, n: usize, now: Duration, wall: i64) {
        self.w = None;
        self.fs.crash_after(n);
        if let Ok(store) = StoreDir::open(
            Arc::new(self.fs.clone()),
            Path::new("store"),
            Some(&new_store()),
        ) {
            let _ = LogWriter::open(store, self.cfg.clone(), Some([self.boot; 16]), now, wall);
        }
        self.kill();
    }

    /// Starts the writer; every start in these histories finishes.
    fn open(&mut self, now: Duration, wall: i64) -> StartReport {
        self.w = None;
        self.before = self
            .fs
            .read(&mark_path())
            .and_then(|b| read_mark(&b, &STORE_ID).ok().flatten())
            .map(|m| m.1);
        let store = StoreDir::open(
            Arc::new(self.fs.clone()),
            Path::new("store"),
            Some(&new_store()),
        )
        .expect("the store opens");
        let w = LogWriter::open(store, self.cfg.clone(), Some([self.boot; 16]), now, wall)
            .expect("the writer opens");
        assert!(w.fault().is_none(), "a start failed: {:?}", w.fault());
        let report = w.start_report().clone();
        self.w = Some(w);
        self.q = IngestQueue::new(self.cfg.ingest_queue);
        self.maint(now, wall);
        report
    }

    fn read_only(&self) -> bool {
        self.w.as_ref().is_some_and(|w| w.fault().is_some())
    }

    /// The maintenance thread's work: segments created ahead, seals.
    fn maint(&mut self, now: Duration, wall: i64) {
        let Some(w) = self.w.as_mut() else { return };
        for _ in 0..2 {
            if let Some(req) = w.segment_request(wall) {
                let r = create_segment(&self.fs, &req, &w.halt());
                w.segment_created(r);
            }
            for job in w.take_seal_jobs(now) {
                let r = seal_segment(&self.fs, &job, &w.halt());
                w.seal_done(job, r, now);
            }
        }
    }

    /// A turn of the writer with nothing queued (timed mark syncs and seals), then maintenance.
    fn idle(&mut self, now: Duration, wall: i64) {
        let Some(w) = self.w.as_mut() else { return };
        let step = w.step(now, wall, &mut self.q);
        assert!(step.done.is_empty());
        self.maint(now, wall);
    }

    fn push(&mut self, items: Vec<Item>) -> u64 {
        let call = Call::new(items, self.cfg.call_limit()).expect("a model batch makes a call");
        self.q.push(call).expect("room in the queue")
    }

    /// Commits a call without a fault.
    fn commit(&mut self, items: Vec<Item>, now: Duration, wall: i64) -> Result<Ack, CommitError> {
        let id = self.push(items);
        for _ in 0..4 {
            let step = self
                .w
                .as_mut()
                .expect("a running writer")
                .step(now, wall, &mut self.q);
            if let Some((_, r)) = step.done.into_iter().find(|d| d.0 == id) {
                return r;
            }
            self.maint(now, wall);
        }
        panic!("the call was not committed");
    }

    /// Commits a call in one step with `arm` planned at its sync, or with the file system stopping after `stop`
    /// more operations; returns its result, if it has one, and the writes of the step.
    fn commit_stopped(
        &mut self,
        items: Vec<Item>,
        arm: Option<Arm>,
        stop: Option<usize>,
        now: Duration,
        wall: i64,
    ) -> (Option<Result<Ack, CommitError>>, Vec<Write>) {
        self.idle(now, wall);
        let id = self.push(items);
        self.fs.clear_ops();
        *self.armed.lock().unwrap() = arm;
        if let Some(n) = stop {
            self.fs.crash_after(n);
        }
        let step = self
            .w
            .as_mut()
            .expect("a running writer")
            .step(now, wall, &mut self.q);
        let unfired = self.armed.lock().unwrap().take();
        assert!(unfired.is_none(), "the batch's sync was not reached");
        let writes = self
            .fs
            .ops()
            .into_iter()
            .filter(|o| o.op == FaultOp::Write && o.ok)
            .map(|o| {
                let b = self.fs.read(&o.path).expect("a written file");
                let (at, len) = (o.offset as usize, o.len as usize);
                (o.path, o.offset, b[at..at + len].to_vec())
            })
            .collect();
        let r = step.done.into_iter().find(|d| d.0 == id).map(|d| d.1);
        (r, writes)
    }

    /// The process stops; the machine keeps running.
    fn kill(&mut self) {
        self.w = None;
        self.fs.restart();
        self.pending = None;
    }

    /// The machine loses power, with `fate` for the writes of a batch whose sync did not complete, and boots again.
    /// Every process stops, the reader's too.
    fn power_loss(&mut self, fate: Option<(WriteOutcome, Vec<Write>)>) {
        self.w = None;
        self.reader = None;
        self.view.clear();
        self.fs.power_loss();
        if let Some((outcome, writes)) = fate {
            let keep = match outcome {
                WriteOutcome::Lost => 0,
                WriteOutcome::Torn | WriteOutcome::Survived => writes.len(),
            };
            for (k, (path, at, bytes)) in writes.iter().enumerate().take(keep) {
                let cut = if outcome == WriteOutcome::Torn && k + 1 == writes.len() {
                    bytes.len() / 2 / 8 * 8
                } else {
                    bytes.len()
                };
                self.fs.poke(path, *at as usize, &bytes[..cut]);
            }
            self.torn_header_unused = outcome == WriteOutcome::Torn
                && writes.last().is_some_and(|(path, _, _)| {
                    self.fs
                        .read_durable(path)
                        .and_then(|b| FileHeader::decode(&b))
                        .is_none_or(|h| h.base_sequence == 0)
                });
        }
        self.fs.restart();
        self.boot += 1;
        self.pending = None;
    }

    /// A power loss that keeps a part of the unsynced writes chosen by `seed`.
    fn power_loss_partial(&mut self, seed: u64) {
        self.w = None;
        self.reader = None;
        self.view.clear();
        let counter = |b: Option<Vec<u8>>| {
            b.and_then(|b| read_mark(&b, &STORE_ID).ok().flatten())
                .map_or(0, |m| m.1.counter)
        };
        let synced = counter(self.fs.read_durable(&mark_path()));
        self.fs.power_loss_partial(seed);
        // Updates since the last mark sync never overwrite its slot (`format.md` A.10 Update, Q-122).
        assert!(
            counter(self.fs.read(&mark_path())) >= synced,
            "a power loss left a mark older than the last one synced"
        );
        self.fs.restart();
        self.boot += 1;
        self.pending = None;
    }

    /// The file system is unmounted and mounted again within the boot: the page cache is lost, the processes keep
    /// running.
    fn remount(&mut self) {
        self.w = None;
        self.fs.power_loss();
        self.fs.restart();
        self.pending = None;
    }

    fn mark(&self) -> MarkSlot {
        let b = self.fs.read(&mark_path()).expect("a mark file");
        read_mark(&b, &STORE_ID)
            .expect("a readable mark")
            .expect("a valid slot")
            .1
    }

    /// Every complete batch of every segment, by first sequence: the current contents, as the page cache holds
    /// them.
    fn log(&self) -> BTreeMap<u64, BatchContent> {
        let mut out = BTreeMap::new();
        for p in self.fs.paths() {
            let Some(name) = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(SegmentName::parse)
            else {
                continue;
            };
            let id = SegmentIdentity {
                store_id: STORE_ID,
                file_id: name.file_id,
                generation: name.generation,
            };
            let bytes = self.fs.read(&p).expect("a segment");
            for b in &scan(&bytes, &id, ScanLimit::End)
                .expect("a segment scans")
                .batches
            {
                let c = BatchContent::decode(b).expect("a batch decodes");
                if let Some(old) = out.insert(c.first_sequence, c.clone()) {
                    assert_eq!(old, c, "two batches at sequence {}", c.first_sequence);
                }
            }
        }
        out
    }

    /// The log up to the durable mark: what is committed.
    fn committed(&self) -> BTreeMap<u64, BatchContent> {
        let last = self.mark().last_sequence;
        let mut log = self.log();
        log.retain(|_, b| b.last_sequence() <= last);
        log
    }

    /// Refreshes the reader, opening it first if there is none, and applies what it found to its view.
    fn refresh(&mut self, now: Duration, wall: i64) -> u64 {
        *self.clock.0.lock().unwrap() = (now, wall);
        if self.reader.is_none() {
            let fs: Arc<dyn Fs> = Arc::new(self.fs.clone());
            let cfg = ReaderConfig::new(
                "store",
                fs,
                self.clock.clone(),
                Some([self.boot; 16]),
                [7; 16],
            );
            self.reader = Some(LogReader::open(cfg).expect("a reader opens"));
            self.view.clear();
        }
        let update = self.reader.as_mut().unwrap().refresh().expect("a refresh");
        for event in update.events {
            match event {
                ReadEvent::Batch(b) => {
                    for (k, item) in b.content.items.into_iter().enumerate() {
                        let seq = b.first.sequence + k as u64;
                        assert!(
                            self.view.keys().next_back().is_none_or(|&l| l < seq),
                            "the reader returned sequence {seq} out of order"
                        );
                        self.view.insert(seq, item);
                    }
                }
                ReadEvent::Void { above, .. } => {
                    count(Count::Voids);
                    self.view.retain(|&s, _| s <= above.sequence);
                }
                ReadEvent::Corruption { .. } => panic!("no corruption is injected"),
            }
        }
        update.state.durable.sequence
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        // The hook holds a clone of the file system.
        self.fs.set_sync_hook(None);
    }
}

// ----- the model and the log together -----

/// A defined series, as readings need it.
#[derive(Clone, Copy, Debug)]
struct Series {
    id: SeriesId,
    value_type: ValueType,
    interval: Option<IntervalSpec>,
    versioned: bool,
}

struct World {
    m: Model,
    log: Log,
    series: Vec<Series>,
    types: BTreeMap<u32, ValueType>,
    /// A model position plus this is the log's: recovery entries in the log less those in the model, zero until a
    /// start is cut short.
    offset: i64,
    /// A start was cut short: the recovery entries need not match one for one.
    cut: bool,
    names: u32,
    zones: i64,
}

fn item_of(e: &ModelEntry, types: &BTreeMap<u32, ValueType>) -> Item {
    match &e.body {
        EntryBody::Fact(f) => Item::Fact(fact_to_row(f, types[&f.series.0])),
        EntryBody::Registry(ev) => Item::Entry(event_to_entry(ev)),
        EntryBody::Route(_) => unreachable!("no routing change is generated"),
        EntryBody::Recovery(_) => unreachable!("a recovery entry is never committed"),
    }
}

impl World {
    fn new(every: bool) -> Self {
        let mut w = Self {
            m: Model::new(Config::default(), vec![], T0),
            log: Log::new(every),
            series: Vec::new(),
            types: BTreeMap::new(),
            offset: 0,
            cut: false,
            names: 0,
            zones: 0,
        };
        let (now, wall) = w.clocks();
        let r = w.log.open(now, wall);
        assert_eq!(r.rule, StartRule::FirstStart);
        w
    }

    fn clocks(&self) -> (Duration, i64) {
        (
            Duration::from_nanos(u64::try_from(self.m.mono).unwrap()),
            self.m.wall,
        )
    }

    fn last_model_position(&self) -> Option<Position> {
        self.m.entries().ok()?.last().map(|e| e.position)
    }

    /// The model committed `entries` in one batch: the log commits them and must acknowledge them at the mapped
    /// positions.
    fn commit(&mut self, entries: &[ModelEntry]) {
        let items: Vec<Item> = entries.iter().map(|e| item_of(e, &self.types)).collect();
        let (now, wall) = self.clocks();
        let ack = self
            .log
            .commit(items.clone(), now, wall)
            .expect("the log acknowledges what the model committed");
        assert_eq!(ack.first.generation, entries[0].position.generation);
        assert_eq!(
            ack.first.sequence,
            entries[0]
                .position
                .sequence
                .wrapping_add_signed(self.offset),
            "the acknowledged position"
        );
        assert_eq!(
            ack.last.sequence - ack.first.sequence + 1,
            items.len() as u64
        );
        for (k, item) in items.into_iter().enumerate() {
            self.log.acked.insert(ack.first.sequence + k as u64, item);
        }
    }

    /// A model that is read-only after a failed sync: the log refuses a call too.
    fn expect_read_only(&mut self) {
        assert!(
            self.log.read_only(),
            "the model is read-only, the log is not"
        );
        let (now, wall) = self.clocks();
        let alias = Item::Entry(LogEntry::Alias {
            id: 0,
            name: "refused".into(),
        });
        assert_eq!(
            self.log.commit(vec![alias], now, wall),
            Err(CommitError::ReadOnly)
        );
    }

    /// A registry operation: whatever entry the model commits, the log commits too.
    fn registry(&mut self, op: impl FnOnce(&mut Model)) -> Option<RegistryEvent> {
        let before = self.last_model_position();
        let read_only = self.m.is_read_only();
        op(&mut self.m);
        let entries = self.m.entries().expect("the model is up");
        let e = entries
            .last()
            .filter(|e| Some(e.position) != before)?
            .clone();
        assert!(!read_only, "a read-only model committed an entry");
        self.commit(std::slice::from_ref(&e));
        match e.body {
            EntryBody::Registry(ev) => Some(ev),
            _ => unreachable!("a registry operation commits a registry entry"),
        }
    }

    fn registry_or_read_only(&mut self, op: impl FnOnce(&mut Model)) -> Option<RegistryEvent> {
        let read_only = self.m.is_read_only();
        let ev = self.registry(op);
        if read_only {
            self.expect_read_only();
        }
        ev
    }

    fn define(&mut self, s: &SeriesSpec) {
        let e = [SECOND, 10 * SECOND, 60 * SECOND, 900 * SECOND][usize::from(s.interval)];
        let (kind, value_type) = [
            (Kind::Gauge, ValueType::Number),
            (Kind::Counter, ValueType::Number),
            (Kind::Interval, ValueType::Number),
            (Kind::State, ValueType::Boolean),
            (Kind::State, ValueType::Name),
            (Kind::Text, ValueType::Text),
        ][usize::from(s.kind)];
        let timing = [Timing::Observed, Timing::Scheduled, Timing::Forecast][usize::from(s.timing)];
        let versioned = if timing == Timing::Forecast {
            Some(ChangedAtKind::Time)
        } else {
            [
                None,
                Some(ChangedAtKind::Time),
                Some(ChangedAtKind::Version),
            ][usize::from(s.versioned)]
        };
        let interval = (kind == Kind::Interval).then(|| IntervalSpec {
            length: e,
            phase: e / 8 * i64::from(s.phase),
        });
        let options = SeriesOptions {
            versioned,
            timing,
            expected_interval: Some(e),
            record_on_change: s.record_on_change && kind != Kind::Interval,
            counter: if kind == Kind::Counter {
                CounterRules {
                    wrap_at: s.wraps.then_some(65_536.0),
                    reset_ceiling: f64::from(u32::from(s.ceiling) * 3_921) / 1e6,
                }
            } else {
                CounterRules::default()
            },
            lateness_bound: Some(e * (1 + i64::from(s.lateness))),
            interval,
            durability: kvik_model::registry::Durability::Synced,
        };
        self.names += 1;
        let name = format!("line/s{}", self.names);
        let unit = ["", "W", "%"][usize::from(s.unit)];
        let ev = self.registry_or_read_only(|m| {
            let _ = m.define_with(&name, kind, value_type, unit, Some(3 * e), options);
        });
        if let Some(RegistryEvent::Define { id, .. }) = ev {
            self.types.insert(id.0, value_type);
            self.series.push(Series {
                id,
                value_type,
                interval,
                versioned: versioned.is_some(),
            });
        }
    }

    fn reading(&self, r: &ReadingSpec) -> Option<(Reading, bool)> {
        let s = *self
            .series
            .get(usize::from(r.pick) % self.series.len().max(1))?;
        let base = if r.ahead {
            self.m.wall + 120 * SECOND
        } else {
            T0
        };
        let slot = i64::from(r.slot);
        let t = match s.interval {
            Some(iv) => base - base.rem_euclid(iv.length) + iv.phase + slot * iv.length,
            None => base + slot * 10 * SECOND,
        };
        let value = match (r.special, s.value_type) {
            (1, _) => Value::Absent,
            (2, ValueType::Number) => Value::Num(f64::NAN),
            (3, ValueType::Number) => Value::Num(-1.0 - f64::from(r.value)),
            (_, ValueType::Number) => Value::Num(1.5 * f64::from(r.value)),
            (_, ValueType::Boolean) => Value::Bool(r.value.is_multiple_of(2)),
            (_, ValueType::Name) => Value::Str(STATES[usize::from(r.value) % 3].into()),
            (_, ValueType::Text) => Value::Str(format!("text {}", r.value)),
        };
        let reading = Reading {
            series: s.id,
            t,
            value,
            quality: QUALITIES[usize::from(r.quality)],
            changed_at: s.versioned.then_some(u64::from(r.version)),
            t_source: if r.host {
                TSource::Host
            } else {
                TSource::Device
            },
        };
        Some((reading, r.revise && !s.versioned))
    }

    /// Emits the readings to the model and returns the batch its next commit forms, from a copy of the model; `None`
    /// when the model is read-only.
    fn emit(&mut self, readings: &[ReadingSpec]) -> Option<Vec<ModelEntry>> {
        for r in readings {
            if let Some((reading, revise)) = self.reading(r) {
                let _ = if revise {
                    self.m.emit_revise(reading)
                } else {
                    self.m.emit(reading)
                };
            }
        }
        if self.m.is_read_only() {
            return None;
        }
        let mut probe = self.m.clone();
        let report = probe.commit(None).expect("the copy commits");
        let entries = probe.entries().expect("the copy is up");
        Some(
            report
                .positions
                .iter()
                .map(|p| {
                    entries
                        .iter()
                        .find(|e| e.position == *p)
                        .expect("a committed position")
                        .clone()
                })
                .collect(),
        )
    }

    fn ingest(&mut self, readings: &[ReadingSpec], fault: Option<Fault>) {
        let Some(batch) = self.emit(readings) else {
            assert_eq!(
                self.m.commit(fault).err(),
                Some(kvik_model::engine::Error::ReadOnly)
            );
            self.expect_read_only();
            return;
        };
        if batch.is_empty() {
            // Every reading was a duplicate or refused: no batch, so no fault either.
            let r = self.m.commit(fault).expect("an empty commit");
            assert!(r.positions.is_empty());
            return;
        }
        let Some(fault) = fault else {
            let r = self.m.commit(None).expect("the model commits");
            assert_eq!(
                r.positions,
                batch.iter().map(|e| e.position).collect::<Vec<_>>()
            );
            self.commit(&batch);
            return;
        };
        let items: Vec<Item> = batch.iter().map(|e| item_of(e, &self.types)).collect();
        let (now, wall) = self.clocks();
        assert!(self.m.commit(Some(fault)).is_err());
        let arm = |a| Some(a);
        let expect = match fault {
            Fault::Crash(CrashPoint::BeforeWrite) => {
                // The call is in the queue when the process stops.
                self.log.push(items);
                self.log.kill();
                Expect::SameBoot(Some(false))
            }
            Fault::Crash(point) => {
                let n = match point {
                    CrashPoint::AfterWrite(_) => 0,
                    CrashPoint::AfterSyncBeforeMark => 1,
                    _ => 2,
                };
                let (r, _) = self
                    .log
                    .commit_stopped(items, arm(Arm::Stop(n)), None, now, wall);
                if n < 2 {
                    assert!(
                        !matches!(r, Some(Ok(_))),
                        "acknowledged before its mark moved"
                    );
                }
                self.log.kill();
                // Before the mark moved, the batch lies complete beyond it.
                Expect::SameBoot(Some(n < 2))
            }
            Fault::PowerLoss(outcome) => {
                let (r, writes) =
                    self.log
                        .commit_stopped(items, arm(Arm::Stop(0)), None, now, wall);
                assert!(!matches!(r, Some(Ok(_))), "acknowledged before its sync");
                let tail = tail_of(outcome, &writes);
                self.log.power_loss(Some((outcome, writes)));
                Expect::Reboot(tail)
            }
            Fault::SyncError(outcome) => {
                let (r, writes) =
                    self.log
                        .commit_stopped(items, arm(Arm::FailSync), None, now, wall);
                assert_eq!(r, Some(Err(CommitError::OutcomeUnknown)));
                assert!(self.log.read_only() && self.m.is_read_only());
                self.log.pending = Some((outcome, writes));
                return;
            }
        };
        self.restart(expect);
    }

    /// A commit stopped at an arbitrary file operation: the log decides whether the batch is kept, and the model is
    /// given the fault with that outcome.
    fn stop(&mut self, readings: &[ReadingSpec], n: usize, power: bool, seed: u64) {
        let Some(batch) = self.emit(readings) else {
            assert_eq!(
                self.m.commit(None).err(),
                Some(kvik_model::engine::Error::ReadOnly)
            );
            self.expect_read_only();
            return;
        };
        if batch.is_empty() {
            assert!(
                self.m
                    .commit(None)
                    .expect("an empty commit")
                    .positions
                    .is_empty()
            );
            return;
        }
        let items: Vec<Item> = batch.iter().map(|e| item_of(e, &self.types)).collect();
        let (now, wall) = self.clocks();
        let first = batch[0].position.sequence.wrapping_add_signed(self.offset);
        let (r, _) = self
            .log
            .commit_stopped(items.clone(), None, Some(n), now, wall);
        let acked = matches!(r, Some(Ok(_)));
        if let Some(Ok(a)) = r {
            assert_eq!(a.first.sequence, first);
        }
        if power {
            self.log.power_loss_partial(seed);
        } else {
            self.log.kill();
        }
        let report = self.log.open(now, wall);
        let kept = self.log.log().get(&first).is_some_and(|b| {
            b.items == items && b.last_sequence() <= self.log.mark().last_sequence
        });
        assert!(kept || !acked, "an acknowledged batch was lost");
        count(if kept {
            Count::StopsKept
        } else {
            Count::StopsLost
        });
        if acked {
            for (k, item) in items.into_iter().enumerate() {
                self.log.acked.insert(first + k as u64, item);
            }
        }
        let fault = match (power, kept) {
            (false, true) => Fault::Crash(CrashPoint::AfterSync),
            // A complete batch beyond the mark, truncated: a reset point (rule 1).
            (false, false) if report.entry.is_some() => {
                Fault::Crash(CrashPoint::AfterWrite(WriteOutcome::Lost))
            }
            (false, false) => Fault::Crash(CrashPoint::BeforeWrite),
            (true, true) => Fault::PowerLoss(WriteOutcome::Survived),
            // Torn when the log found torn bytes.
            (true, false) if !report.losses.is_empty() => Fault::PowerLoss(WriteOutcome::Torn),
            (true, false) => Fault::PowerLoss(WriteOutcome::Lost),
        };
        assert!(self.m.commit(Some(fault)).is_err());
        self.m.restart();
        let expect = if power {
            Expect::Reboot(Tail::Either)
        } else {
            Expect::SameBoot(None)
        };
        self.started(&report, expect);
    }

    /// Starts the model and the log again after a crash, and checks what the log's start reported.
    fn restart(&mut self, expect: Expect) {
        self.m.restart();
        assert!(
            self.m.is_up(),
            "the model starts: {:?}",
            self.m.disk.open_refused
        );
        let (now, wall) = self.clocks();
        let report = self.log.open(now, wall);
        self.started(&report, expect);
    }

    fn started(&mut self, report: &StartReport, expect: Expect) {
        // The log as the start left it, against the model; this also finds the recovery entries.
        self.check();
        match expect {
            Expect::SameBoot(reset) => {
                assert_eq!(report.rule, StartRule::SameBoot, "{report:?}");
                if let Some(reset) = reset {
                    assert_eq!(report.entry.is_some(), reset, "a reset point: {report:?}");
                }
                let before = self.log.before.expect("a readable mark on the same boot");
                match &report.entry {
                    None => {
                        assert!(report.losses.is_empty(), "{report:?}");
                        assert_eq!(self.log.mark().stamp, before.stamp, "no reset");
                    }
                    Some(entry) => {
                        count(Count::Rule1Resets);
                        assert!(self.cut || self.model_tail(entry.sequence) == Some((1, false)));
                        // K is the mark's last position; the truncated batch may have been acknowledged.
                        assert_eq!(entry.entry.after, before.last_sequence);
                        assert_eq!(entry.entry.mark_state, MarkState::Readable);
                        assert!(!entry.adopted, "nothing to adopt: every start finished");
                        self.check_entry(report, true);
                        assert_eq!(report.losses.len(), 1, "{report:?}");
                        assert!(report.losses[0].bytes > 0);
                    }
                }
            }
            Expect::Reboot(tail) => {
                assert!(matches!(report.rule, StartRule::Reboot(_)), "{report:?}");
                count(Count::Reboots);
                let entry = report.entry.as_ref().expect("a recovery entry");
                assert!(!entry.adopted, "nothing to adopt: every start finished");
                self.check_entry(report, !matches!(tail, Tail::Either));
                let reported = !report.losses.is_empty();
                let model = self.model_tail(entry.sequence).filter(|_| !self.cut);
                match tail {
                    Tail::Either => {}
                    Tail::Clean => {
                        assert!(!reported, "nothing torn: {report:?}");
                        assert!(model.is_none_or(|t| t == (0, false)), "{model:?}");
                    }
                    Tail::Torn { file_id } if reported => {
                        count(Count::TornReported);
                        assert!(!report.deleted.contains(&file_id));
                        assert!(model.is_none_or(|t| t == (0, true)), "{model:?}");
                    }
                    Tail::Torn { file_id } => {
                        count(Count::TornDeleted);
                        assert!(
                            self.log.torn_header_unused && report.deleted.contains(&file_id),
                            "a torn tail is reported, unless its segment reads unused: {report:?}"
                        );
                    }
                }
            }
            Expect::Any => {
                if report.entry.as_ref().is_some_and(|e| e.adopted) {
                    count(Count::Adoptions);
                }
                if report.entry.is_some() {
                    self.check_entry(report, false);
                }
            }
        }
        self.log.torn_header_unused = false;
    }

    /// The tail the model's recovery entry at `sequence` names: complete batches and whether one was torn.
    fn model_tail(&self, sequence: u64) -> Option<(u32, bool)> {
        self.m
            .recovery_entries()
            .get(&Position::new(0, sequence))
            .map(|e| (e.tail.batches, e.tail.torn))
    }

    /// The recovery entry a start wrote: at F = K + 1, the last batch, with the id floor and a new reset stamp that
    /// the mark carries; at most one possible loss, identified by F.
    /// With `known`, the history fixes the tail, so the log reports a possible loss exactly when the model does.
    fn check_entry(&mut self, report: &StartReport, known: bool) {
        let entry = report.entry.as_ref().expect("a recovery entry");
        assert_eq!(entry.sequence, entry.entry.after + 1, "F = K + 1");
        let mark = self.log.mark();
        assert_eq!(
            entry.sequence, mark.last_sequence,
            "the entry is the last batch"
        );
        let stamp = entry
            .entry
            .stamp
            .expect("a recovery entry carries a reset stamp");
        assert_eq!(stamp.sequence, entry.sequence);
        assert_ne!(stamp.nonce, 0);
        assert_eq!(mark.stamp, stamp, "the mark carries the entry's stamp");
        if let Some(before) = self.log.before {
            assert_ne!(before.stamp, stamp, "a new reset stamp");
        }
        // The model's entry at the same position, while the two match one for one: the same id floor, and, where
        // the history fixes the tail, a possible loss in both or neither.
        if !self.cut {
            let p = Position::new(0, entry.sequence);
            let model = self
                .m
                .recovery_entries()
                .get(&p)
                .cloned()
                .expect("the model's entry");
            assert_eq!(
                entry.entry.id_floor, model.id_floor,
                "the id floor: {report:?}"
            );
            let possible = self
                .m
                .losses()
                .into_iter()
                .any(|l| l.kind == ModelLossKind::PossibleLoss && l.identity == p);
            if known && !self.log.torn_header_unused {
                let reported = report
                    .losses
                    .iter()
                    .any(|l| l.kind == LossKind::PossibleLoss && l.identity == entry.sequence);
                assert_eq!(
                    reported, possible,
                    "a possible loss in both or neither: {report:?}, {model:?}"
                );
            }
        }
        if let Some(before) = self.log.before {
            assert!(entry.entry.id_floor >= before.max_series_id, "{report:?}");
        }
        assert!(report.losses.len() <= 1);
        for l in &report.losses {
            assert_eq!(l.kind, LossKind::PossibleLoss);
            assert_eq!(l.cause, LossCause::TruncatedTail);
            assert_eq!(l.identity, entry.sequence);
            assert!(
                l.bytes > 0
                    || l.readable_commits > 0
                    || entry.entry.mark_state != MarkState::Readable
            );
        }
    }

    fn crash(&mut self) {
        self.m.crash();
        // A batch whose sync failed lies complete beyond the mark.
        let reset = self.log.pending.is_some();
        self.log.kill();
        self.restart(Expect::SameBoot(Some(reset)));
    }

    fn power_loss(&mut self) {
        let tail = self
            .log
            .pending
            .as_ref()
            .map_or(Tail::Clean, |(outcome, writes)| tail_of(*outcome, writes));
        self.m.power_loss();
        let fate = self.log.pending.take();
        self.log.power_loss(fate);
        self.restart(Expect::Reboot(tail));
    }

    fn cut_start(&mut self, n: usize, power: bool, then: Option<u64>) {
        if power {
            self.m.power_loss();
            let fate = self.log.pending.take();
            self.log.power_loss(fate);
        } else {
            self.m.crash();
            self.log.kill();
        }
        self.m.restart();
        let (now, wall) = self.clocks();
        self.log.open_cut(n, now, wall);
        count(Count::CutStarts);
        self.cut = true;
        if let Some(seed) = then {
            // Whatever the cut start wrote of the mark, a power loss leaves no mark older than the last synced one.
            count(Count::TornAfterCutStarts);
            self.m.power_loss();
            self.m.restart();
            self.log.power_loss_partial(seed);
        }
        let report = self.log.open(now, wall);
        self.started(&report, Expect::Any);
    }

    fn mark_sync_fails(&mut self, seed: Option<u64>) {
        let dirty = self
            .log
            .w
            .as_ref()
            .is_some_and(|w| w.fault().is_none() && w.stats().mark_synced < w.stats().durable);
        if self.log.every || !dirty {
            self.crash();
            return;
        }
        self.log
            .fs
            .fail_nth(FaultOp::Sync, "MARK", 0, io::ErrorKind::Other);
        self.m.advance(40_000_000);
        let (now, wall) = self.clocks();
        self.log.idle(now, wall);
        assert!(self.log.read_only(), "the timed mark sync failed");
        count(Count::MarkSyncFailures);
        // The model has no failed mark sync: the process stops at once, on the same boot.
        self.m.crash();
        self.log.kill();
        self.restart(Expect::SameBoot(Some(false)));
        if let Some(seed) = seed {
            self.m.power_loss();
            self.log.power_loss_partial(seed);
            self.restart(Expect::Reboot(Tail::Either));
        }
    }

    fn remount(&mut self) {
        if !self.log.every {
            self.crash();
            return;
        }
        // A batch whose sync failed never reached the disk in the in-memory file system.
        if self.m.disk.tail_fate.is_some() {
            self.m.disk.tail_fate = Some(WriteOutcome::Lost);
        }
        self.m.remount();
        self.log.remount();
        self.restart(Expect::SameBoot(Some(false)));
    }

    fn refresh(&mut self) {
        let (now, wall) = self.clocks();
        let durable = self.log.refresh(now, wall);
        assert_eq!(
            durable,
            self.log.mark().last_sequence,
            "the reader read the current mark"
        );
        let committed: BTreeMap<u64, Item> = self
            .log
            .committed()
            .into_values()
            .flat_map(|b| {
                let first = b.first_sequence;
                b.items
                    .into_iter()
                    .enumerate()
                    .map(move |(k, item)| (first + k as u64, item))
            })
            .collect();
        assert!(
            self.log.view == committed,
            "the reader's view is the committed log"
        );
    }

    /// The committed log is what the model holds, recovery entries included, at the model's positions; every
    /// acknowledged entry is in it. After a start cut short, entries are compared with recovery entries left out on
    /// both sides.
    fn check(&mut self) {
        let committed = self.log.committed();
        let mut got = Vec::new();
        let mut log_rec = Vec::new();
        let mut last = 0u64;
        for b in committed.values() {
            assert_eq!(
                b.first_sequence,
                last + 1,
                "dense positions: no lost range is made"
            );
            if let [Item::Entry(LogEntry::Recovery(r))] = b.items.as_slice() {
                assert_eq!(
                    r.after, last,
                    "a recovery entry follows the last kept position"
                );
                log_rec.push((b.first_sequence, r.clone()));
            } else {
                for (k, item) in b.items.iter().enumerate() {
                    let seq = b.first_sequence + k as u64 - log_rec.len() as u64;
                    got.push((seq, b.received_at, item));
                }
            }
            last = b.last_sequence();
        }
        let mut want = Vec::new();
        let mut model_rec = Vec::new();
        for e in self.m.disk.live_entries().into_values() {
            if let EntryBody::Recovery(r) = &e.body {
                model_rec.push((e.position, r.clone()));
            } else {
                want.push((e.position.sequence - model_rec.len() as u64, e));
            }
        }
        assert_eq!(got.len(), want.len(), "entries in the log and in the model");
        for ((seq, at, item), (wseq, w)) in got.iter().zip(&want) {
            assert_eq!(seq, wseq);
            let p = w.position;
            assert_eq!(*at, w.received_at, "received_at at {p:?}");
            match (item, &w.body) {
                (Item::Fact(row), EntryBody::Fact(f)) => {
                    assert_eq!(&row_to_fact(row, p, *at), f);
                }
                (Item::Entry(e), EntryBody::Registry(ev)) => assert_eq!(&entry_to_event(e), ev),
                (item, body) => panic!("at {p:?}: {item:?} in the log, {body:?} in the model"),
            }
        }
        if !self.cut {
            assert_eq!(
                log_rec.len(),
                model_rec.len(),
                "recovery entries in the log and in the model"
            );
            for ((seq, l), (p, m)) in log_rec.iter().zip(&model_rec) {
                assert_eq!(*seq, p.sequence, "the recovery entry's position");
                assert_eq!(l.after, m.after.sequence, "K at {p:?}");
                assert_eq!(l.id_floor, m.id_floor, "the id floor at {p:?}");
                assert_eq!(
                    l.stamp.map(|s| s.sequence),
                    Some(m.stamp.sequence),
                    "the stamp at {p:?}"
                );
                let state = match m.mark_state {
                    ModelMarkState::Readable => MarkState::Readable,
                    ModelMarkState::Missing => MarkState::Missing,
                    ModelMarkState::Unreadable => MarkState::Unreadable,
                    ModelMarkState::Stale => MarkState::Stale,
                };
                assert_eq!(l.mark_state, state, "the mark state at {p:?}");
            }
        }
        for (seq, item) in &self.log.acked {
            let found = committed
                .range(..=seq)
                .next_back()
                .and_then(|(first, b)| b.items.get(usize::try_from(seq - first).ok()?));
            assert_eq!(found, Some(item), "acknowledged entry at {seq}");
        }
        self.offset = log_rec.len() as i64 - model_rec.len() as i64;
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Define(s) => self.define(s),
            Op::Ingest(r, f) => self.ingest(r, *f),
            Op::Stop {
                readings,
                n,
                power,
                seed,
            } => self.stop(readings, *n, *power, *seed),
            Op::Rename(pick) => {
                if let Some(s) = self.pick(*pick) {
                    self.names += 1;
                    let name = format!("alias/a{}", self.names);
                    self.registry_or_read_only(|m| {
                        let _ = m.rename(s.id, &name);
                    });
                }
            }
            Op::Retire(pick, at) => {
                if let Some(s) = self.pick(*pick) {
                    let at = T0 + i64::from(*at) * 10 * SECOND;
                    self.registry_or_read_only(|m| {
                        let _ = m.retire(s.id, at);
                    });
                }
            }
            Op::Retention(pick, period) => {
                if let Some(s) = self.pick(*pick) {
                    let class =
                        period.map_or(RetentionClass::LongTerm, |p| RetentionClass::Diagnostic {
                            period: (1 + i64::from(p)) * DAY,
                        });
                    self.registry_or_read_only(|m| {
                        let _ = m.set_retention(s.id, class);
                    });
                }
            }
            Op::SiteZone(z) => {
                let zone = ZONES[usize::from(*z)];
                let effective = T0 + (self.zones + 1) * DAY;
                let ev = self.registry_or_read_only(|m| {
                    let _ = m.set_site_zone(zone, effective);
                });
                if ev.is_some() {
                    // The latest effective time is in force; a start is refused unless it is configured (§5.7).
                    self.zones += 1;
                    self.m.config.site_zone = zone.into();
                }
            }
            Op::Advance(ms) => self.m.advance(i64::from(*ms) * 1_000_000),
            Op::ClockSync(synced) => self.m.clock_synchronised = *synced,
            Op::Crash => self.crash(),
            Op::PowerLoss => self.power_loss(),
            Op::Remount => self.remount(),
            Op::CutStart { n, power, then } => self.cut_start(*n, *power, *then),
            Op::MarkSyncFails(seed) => self.mark_sync_fails(*seed),
            Op::Refresh => self.refresh(),
        }
        let (now, wall) = self.clocks();
        self.log.idle(now, wall);
        assert_eq!(
            self.log.read_only(),
            self.m.is_read_only(),
            "read-only in both or neither"
        );
        self.check();
    }

    fn pick(&self, pick: u8) -> Option<Series> {
        (!self.series.is_empty()).then(|| self.series[usize::from(pick) % self.series.len()])
    }
}

fn run(ops: &[Op], every: bool) {
    let mut w = World::new(every);
    for op in ops {
        w.apply(op);
    }
    w.refresh();
    let sealed = w
        .log
        .fs
        .paths()
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
        .count();
    REACHED[Count::SealedSegments as usize].fetch_add(sealed as u64, Ordering::Relaxed);
}

/// On every generated history, the log keeps exactly what the model says is committed, recovery entries included, at
/// the model's positions (renumbered after a start cut short), and the reader returns exactly the log up to the mark.
#[test]
fn the_log_keeps_what_the_model_commits() {
    TestRunner::new(config(8))
        .run(&history(40), |(ops, every)| {
            run(&ops, every);
            Ok(())
        })
        .unwrap();
}

/// Longer histories, for a local soak run: `cargo test --release -p kvik-log --test model_histories -- --ignored`.
/// Every kind of start and stop the harness can make must be reached.
#[test]
#[ignore]
fn the_log_keeps_what_the_model_commits_long() {
    TestRunner::new(ProptestConfig {
        cases: 500,
        ..ProptestConfig::default()
    })
    .run(&history(200), |(ops, every)| {
        run(&ops, every);
        Ok(())
    })
    .unwrap();
    let floors = [
        (Count::Rule1Resets, 100),
        (Count::Reboots, 100),
        (Count::StopsKept, 50),
        (Count::StopsLost, 20),
        (Count::TornReported, 20),
        (Count::CutStarts, 100),
        (Count::TornAfterCutStarts, 50),
        (Count::MarkSyncFailures, 50),
        (Count::Adoptions, 1),
        (Count::SealedSegments, 100),
    ];
    for (c, floor) in floors {
        println!("{c:?}: {}", reached(c));
        assert!(
            reached(c) >= floor,
            "{c:?} reached {} times, below {floor}",
            reached(c)
        );
    }
    // Reached rarely or never, and so not required: the readings of Q-116 (p), and voids (see "Left out").
    for c in [Count::TornDeleted, Count::Voids] {
        println!("{c:?}: {}", reached(c));
    }
}
