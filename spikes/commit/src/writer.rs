//! The commit path: a bounded ingest queue, a group-commit loop, the durable mark and the disk-space marks.
//!
//! One writer thread forms batches from the queue, writes each batch, syncs it, updates the mark, and only then
//! acknowledges it. A batch is written only after the previous batch's sync has returned (`spec.md` §3 Recovery
//! rule 2 relies on this).

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::format::{self, Mark, Reading};
use crate::fs::{FileSystem, LogFile, Space};

/// Name of the mark file.
pub const MARK_NAME: &str = "log/MARK";

/// Name of a log segment while it is open.
pub fn segment_name(generation: u32, file_id: u64) -> String {
    format!("log/{generation}-{file_id:016x}.open")
}

/// When the mark file is synced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkSync {
    /// Never while running (measurement only).
    Off,
    /// On the writer thread, after a batch has been acknowledged, once the interval has passed.
    Inline(Duration),
    /// On its own thread, every interval (`mark_sync_interval`).
    Thread(Duration),
    /// After every mark update, before the acknowledgement: the rule for a platform without a boot identifier.
    EveryBatch,
}

/// Writer configuration. The defaults are the values in `spec.md` §3.
#[derive(Clone, Debug)]
pub struct Config {
    /// Preallocated size of a segment.
    pub segment_len: u64,
    /// Group-commit window: how long the first reading of a batch may wait for more. Zero means no added delay.
    pub linger: Duration,
    /// A batch closes at this many readings.
    pub max_batch_readings: usize,
    /// A batch closes at about this many encoded bytes.
    pub max_batch_bytes: usize,
    /// Capacity of the ingest queue.
    pub queue_capacity: usize,
    /// Mark sync rule.
    pub mark_sync: MarkSync,
    /// Create the next segment on a background thread as soon as the current one is opened.
    pub preallocate_ahead: bool,
    /// How often free space is checked.
    pub space_check_interval: Duration,
    /// Low mark: below this percentage of the file system ...
    pub low_mark_percent: u64,
    /// ... or below this many bytes, whichever is larger.
    pub low_mark_floor: u64,
    /// Critical mark: below this many free bytes ingest waits.
    pub critical_mark: u64,
    /// Boot identifier written into the mark.
    pub boot_id: [u8; 16],
    /// Store identifier written into segment headers.
    pub store_id: [u8; 16],
}

impl Default for Config {
    fn default() -> Config {
        Config {
            segment_len: 64 << 20,
            linger: Duration::ZERO,
            max_batch_readings: 1 << 20,
            max_batch_bytes: 1 << 20,
            queue_capacity: 65_536,
            // The rule the measurements recommend: once a second, on the writer thread.
            mark_sync: MarkSync::Inline(Duration::from_secs(1)),
            preallocate_ahead: true,
            space_check_interval: Duration::from_millis(100),
            low_mark_percent: 5,
            low_mark_floor: 2 << 30,
            critical_mark: 512 << 20,
            boot_id: [0; 16],
            store_id: [0; 16],
        }
    }
}

/// Health of the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Normal.
    Ok,
    /// Free space below the low mark: compaction would pause; ingest continues.
    Low,
    /// Free space below the critical mark: ingest waits.
    Critical,
    /// A write or sync failed: read-only until restart.
    Fatal,
}

impl Health {
    fn from_u8(v: u8) -> Health {
        match v {
            0 => Health::Ok,
            1 => Health::Low,
            2 => Health::Critical,
            _ => Health::Fatal,
        }
    }
}

/// A reading in the queue with the time it was emitted.
#[derive(Clone, Copy, Debug)]
pub struct Emitted {
    /// The reading.
    pub reading: Reading,
    /// When the adapter emitted it (for an open-loop load, the scheduled time).
    pub emitted_at: Instant,
}

/// What the writer reports for each acknowledged batch.
#[derive(Debug)]
pub struct Committed<'a> {
    /// First sequence of the batch.
    pub first_sequence: u64,
    /// Last sequence of the batch.
    pub last_sequence: u64,
    /// Emit times of the batch's readings, in sequence order.
    pub emitted: &'a [Instant],
    /// When the batch was acknowledged: after its sync and the mark update.
    pub acked_at: Instant,
    /// Encoded bytes of the batch.
    pub bytes: u64,
}

/// Why an emit was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The writer is read-only after a failure, or has stopped.
    ReadOnly,
    /// `try_emit` only: the queue is full.
    Full,
}

#[derive(Default)]
struct Shared {
    read_only: AtomicBool,
    health: AtomicU8,
    fatal: Mutex<Option<String>>,
    back_pressure_ns: AtomicU64,
    back_pressure_waits: AtomicU64,
    mark_sync_ns: Mutex<Vec<u64>>,
}

impl Shared {
    fn fail(&self, what: &str, e: &io::Error) {
        let mut fatal = self.fatal.lock().unwrap_or_else(|e| e.into_inner());
        if fatal.is_none() {
            *fatal = Some(format!("{what}: {e}"));
        }
        // Read-only first: whoever sees the fatal health also sees the refusal.
        self.read_only.store(true, Ordering::SeqCst);
        self.health.store(3, Ordering::SeqCst);
    }
}

/// A handle adapters emit through. Cloneable; the writer stops once every handle is dropped.
#[derive(Clone)]
pub struct Ingest {
    tx: SyncSender<Emitted>,
    shared: Arc<Shared>,
}

impl Ingest {
    /// Emits a reading, waiting while the queue is full (back-pressure). The wait is measured.
    pub fn emit(&self, reading: Reading, emitted_at: Instant) -> Result<(), Refused> {
        if self.shared.read_only.load(Ordering::SeqCst) {
            return Err(Refused::ReadOnly);
        }
        match self.tx.try_send(Emitted {
            reading,
            emitted_at,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(Refused::ReadOnly),
            Err(TrySendError::Full(e)) => {
                let start = Instant::now();
                let r = self.tx.send(e).map_err(|_| Refused::ReadOnly);
                self.shared
                    .back_pressure_ns
                    .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                self.shared
                    .back_pressure_waits
                    .fetch_add(1, Ordering::Relaxed);
                r
            }
        }
    }

    /// Emits a reading without waiting.
    pub fn try_emit(&self, reading: Reading, emitted_at: Instant) -> Result<(), Refused> {
        if self.shared.read_only.load(Ordering::SeqCst) {
            return Err(Refused::ReadOnly);
        }
        self.tx
            .try_send(Emitted {
                reading,
                emitted_at,
            })
            .map_err(|e| match e {
                TrySendError::Full(_) => Refused::Full,
                TrySendError::Disconnected(_) => Refused::ReadOnly,
            })
    }

    /// Current health.
    pub fn health(&self) -> Health {
        Health::from_u8(self.shared.health.load(Ordering::SeqCst))
    }

    /// Total time emits have waited on a full queue, and how many emits waited.
    pub fn back_pressure(&self) -> (Duration, u64) {
        (
            Duration::from_nanos(self.shared.back_pressure_ns.load(Ordering::Relaxed)),
            self.shared.back_pressure_waits.load(Ordering::Relaxed),
        )
    }
}

/// What the writer did, returned by [`Writer::join`].
#[derive(Debug, Default)]
pub struct WriterReport {
    /// Batches acknowledged.
    pub batches: u64,
    /// Readings acknowledged.
    pub readings: u64,
    /// Bytes written to log segments by batches.
    pub log_bytes: u64,
    /// 4 KiB pages the batches touched, summed over batches: the least the file system writes back.
    pub pages_touched: u64,
    /// Log syncs issued.
    pub syncs: u64,
    /// Segments created after the first.
    pub segments_rolled: u64,
    /// Time the writer waited for the next segment at each roll.
    pub roll_waits: Vec<Duration>,
    /// Duration of each mark sync while running (not the clean-shutdown sync).
    pub mark_syncs: Vec<Duration>,
    /// The failure that made the writer read-only, if any.
    pub fatal: Option<String>,
}

/// The running writer.
pub struct Writer {
    thread: JoinHandle<WriterReport>,
    mark_thread: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
    shared: Arc<Shared>,
}

/// The per-batch callback.
pub type OnCommit = Box<dyn FnMut(&Committed<'_>) + Send>;

impl Writer {
    /// Creates the first segment and the mark, and starts the writer.
    pub fn start(
        fs: Arc<dyn FileSystem>,
        config: Config,
        on_commit: OnCommit,
    ) -> io::Result<(Writer, Ingest)> {
        let generation = 1;
        let segment = create_segment(&*fs, &config, generation, 1, 1)?;
        let mut mark_file = fs.open(MARK_NAME)?;
        let mark = Mark {
            seq: 0,
            boot_id: config.boot_id,
            generation,
            file_id: 1,
            offset: format::FILE_HEADER_LEN as u64,
            last_sequence: 0,
        };
        mark_file.write_at(0, &mark.encode())?;
        mark_file.sync()?;
        fs.sync_log_dir()?;

        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::sync_channel(config.queue_capacity);
        let mark_thread = match config.mark_sync {
            MarkSync::Thread(interval) => {
                let mut handle = fs.open(MARK_NAME)?;
                let shared = Arc::clone(&shared);
                let (stop, stopped) = mpsc::channel::<()>();
                let t = thread::Builder::new()
                    .name("mark-sync".into())
                    .spawn(move || {
                        while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                            let start = Instant::now();
                            let r = handle.sync();
                            let ns = start.elapsed().as_nanos() as u64;
                            shared
                                .mark_sync_ns
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(ns);
                            if let Err(e) = r {
                                shared.fail("mark sync", &e);
                                return;
                            }
                        }
                        // Stopped after the writer: the clean-shutdown sync, on this same handle and never
                        // after a failure (a failed sync is not retried).
                        if !shared.read_only.load(Ordering::SeqCst)
                            && let Err(e) = handle.sync()
                        {
                            shared.fail("mark sync at shutdown", &e);
                        }
                    })?;
                Some((stop, t))
            }
            _ => None,
        };
        let mut state = WriterState {
            fs,
            config,
            generation,
            file_id: 1,
            segment,
            offset: format::FILE_HEADER_LEN as u64,
            next_segment: None,
            mark_file,
            mark,
            next_sequence: 1,
            last_commit_time: 0,
            last_mark_sync: Instant::now(),
            mark_dirty: false,
            last_space_check: None,
            shared: Arc::clone(&shared),
            on_commit,
            report: WriterReport::default(),
        };
        state.prepare_next_segment();
        let thread = thread::Builder::new()
            .name("writer".into())
            .spawn(move || state.run(rx))?;
        Ok((
            Writer {
                thread,
                mark_thread,
                shared: Arc::clone(&shared),
            },
            Ingest { tx, shared },
        ))
    }

    /// Current health.
    pub fn health(&self) -> Health {
        Health::from_u8(self.shared.health.load(Ordering::SeqCst))
    }

    /// Waits for the writer to stop (every [`Ingest`] dropped, or a failure) and returns its report.
    pub fn join(self) -> WriterReport {
        let mut report = self.thread.join().expect("writer thread panicked");
        if let Some((stop, t)) = self.mark_thread {
            drop(stop);
            t.join().expect("mark thread panicked");
        }
        let ns = std::mem::take(
            &mut *self
                .shared
                .mark_sync_ns
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        report
            .mark_syncs
            .extend(ns.into_iter().map(Duration::from_nanos));
        report.fatal = self
            .shared
            .fatal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        report
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn create_segment(
    fs: &dyn FileSystem,
    config: &Config,
    generation: u32,
    file_id: u64,
    base_sequence: u64,
) -> io::Result<Box<dyn LogFile>> {
    let header = format::log_segment_header(
        generation,
        file_id,
        base_sequence,
        now_ns(),
        config.store_id,
    );
    fs.create_preallocated(
        &segment_name(generation, file_id),
        config.segment_len,
        &header,
    )
}

type NextSegment = Receiver<io::Result<Box<dyn LogFile>>>;

struct WriterState {
    fs: Arc<dyn FileSystem>,
    config: Config,
    generation: u32,
    file_id: u64,
    segment: Box<dyn LogFile>,
    offset: u64,
    next_segment: Option<NextSegment>,
    mark_file: Box<dyn LogFile>,
    mark: Mark,
    next_sequence: u64,
    last_commit_time: i64,
    last_mark_sync: Instant,
    /// The mark has been updated since its last sync.
    mark_dirty: bool,
    last_space_check: Option<Instant>,
    shared: Arc<Shared>,
    on_commit: OnCommit,
    report: WriterReport,
}

impl WriterState {
    fn run(mut self, rx: Receiver<Emitted>) -> WriterReport {
        let mut batch: Vec<Emitted> = Vec::new();
        loop {
            if self.shared.read_only.load(Ordering::SeqCst) {
                break;
            }
            if !self.wait_for_space() {
                break;
            }
            batch.clear();
            match rx.recv_timeout(self.config.space_check_interval) {
                Ok(e) => batch.push(e),
                Err(RecvTimeoutError::Timeout) => {
                    if !self.idle_mark_sync() {
                        break;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Clean shutdown: the mark is synced, unless the writer is already read-only (a failed
                    // sync is never retried) or the mark thread does it on its own handle.
                    let thread = matches!(self.config.mark_sync, MarkSync::Thread(_));
                    if !thread
                        && !self.shared.read_only.load(Ordering::SeqCst)
                        && let Err(e) = self.sync_mark(false)
                    {
                        self.shared.fail("mark sync at shutdown", &e);
                    }
                    break;
                }
            }
            self.fill(&rx, &mut batch);
            if let Err((what, e)) = self.commit(&batch) {
                self.shared.fail(what, &e);
                break;
            }
        }
        // Dropping `rx` here refuses every queued and future emit: none of them was acknowledged.
        self.report
    }

    /// Adds to `batch` what the group-commit rule allows: whatever is queued (no added delay when idle), or,
    /// with a window, whatever arrives before it closes.
    fn fill(&self, rx: &Receiver<Emitted>, batch: &mut Vec<Emitted>) {
        let limit = self
            .config
            .max_batch_readings
            .min(self.config.max_batch_bytes / 24)
            .max(1);
        let deadline = Instant::now() + self.config.linger;
        while batch.len() < limit {
            if self.config.linger.is_zero() {
                match rx.try_recv() {
                    Ok(e) => batch.push(e),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(e) => batch.push(e),
                    Err(_) => break,
                }
            }
        }
    }

    /// Checks free space when due. Below the critical mark, waits without taking from the queue, so emits wait.
    /// Returns false if the writer must stop.
    fn wait_for_space(&mut self) -> bool {
        let due = self
            .last_space_check
            .is_none_or(|t| t.elapsed() >= self.config.space_check_interval);
        if !due {
            return true;
        }
        loop {
            self.last_space_check = Some(Instant::now());
            let space = match self.fs.space() {
                Ok(s) => s,
                Err(e) => {
                    self.shared.fail("free-space check", &e);
                    return false;
                }
            };
            let health = self.health_for(space);
            // Never overwrite a fatal health set by the mark thread.
            let _ = self
                .shared
                .health
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |h| {
                    (h != Health::Fatal as u8).then_some(health as u8)
                });
            if health != Health::Critical {
                return true;
            }
            thread::sleep(
                self.config
                    .space_check_interval
                    .min(Duration::from_millis(10)),
            );
            if self.shared.read_only.load(Ordering::SeqCst) || !self.idle_mark_sync() {
                return false;
            }
        }
    }

    fn health_for(&self, space: Space) -> Health {
        let low =
            (space.total / 100 * self.config.low_mark_percent).max(self.config.low_mark_floor);
        if space.free < self.config.critical_mark {
            Health::Critical
        } else if space.free < low {
            Health::Low
        } else {
            Health::Ok
        }
    }

    fn prepare_next_segment(&mut self) {
        if !self.config.preallocate_ahead {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let fs = Arc::clone(&self.fs);
        let config = self.config.clone();
        let (generation, file_id) = (self.generation, self.file_id + 1);
        thread::spawn(move || {
            // The base sequence is not known yet: 0 here, rewritten when the segment is taken into use.
            let _ = tx.send(create_segment(&*fs, &config, generation, file_id, 0));
        });
        self.next_segment = Some(rx);
    }

    fn roll(&mut self) -> io::Result<()> {
        let start = Instant::now();
        let file_id = self.file_id + 1;
        let next = match self.next_segment.take() {
            Some(rx) => {
                let mut next = rx
                    .recv()
                    .map_err(|_| io::Error::other("segment creation thread stopped"))??;
                // The header gets its base sequence now; the first batch's sync makes it durable.
                let header = format::log_segment_header(
                    self.generation,
                    file_id,
                    self.next_sequence,
                    now_ns(),
                    self.config.store_id,
                );
                next.write_at(0, &header)?;
                next
            }
            None => create_segment(
                &*self.fs,
                &self.config,
                self.generation,
                file_id,
                self.next_sequence,
            )?,
        };
        self.report.roll_waits.push(start.elapsed());
        self.report.segments_rolled += 1;
        // Sealing (segment index, trailer, truncate, rename, directory sync) is outside this spike.
        self.segment = next;
        self.file_id = file_id;
        self.offset = format::FILE_HEADER_LEN as u64;
        self.prepare_next_segment();
        Ok(())
    }

    fn commit(&mut self, batch: &[Emitted]) -> Result<(), (&'static str, io::Error)> {
        let rows: Vec<Reading> = batch.iter().map(|e| e.reading).collect();
        let now = now_ns();
        let commit_time = now.max(self.last_commit_time);
        let encoded =
            format::encode_batch(self.generation, self.next_sequence, commit_time, now, &rows);
        let len = encoded.bytes.len() as u64;
        if self.offset + len > self.config.segment_len {
            if format::FILE_HEADER_LEN as u64 + len > self.config.segment_len {
                return Err(("batch", io::Error::other("batch larger than a segment")));
            }
            self.roll().map_err(|e| ("segment creation", e))?;
        }
        self.segment
            .write_at(self.offset, &encoded.bytes)
            .map_err(|e| ("log write", e))?;
        self.report.syncs += 1;
        self.segment.sync().map_err(|e| ("log sync", e))?;
        let start_page = self.offset / 4096;
        self.offset += len;
        self.report.pages_touched += self.offset.div_ceil(4096) - start_page;
        self.last_commit_time = commit_time;
        self.next_sequence = encoded.last_sequence + 1;
        self.update_mark().map_err(|e| ("mark write", e))?;
        if self.config.mark_sync == MarkSync::EveryBatch {
            self.sync_mark(true).map_err(|e| ("mark sync", e))?;
        }
        let acked_at = Instant::now();
        let emitted: Vec<Instant> = batch.iter().map(|e| e.emitted_at).collect();
        (self.on_commit)(&Committed {
            first_sequence: encoded.first_sequence,
            last_sequence: encoded.last_sequence,
            emitted: &emitted,
            acked_at,
            bytes: len,
        });
        self.report.batches += 1;
        self.report.readings += batch.len() as u64;
        self.report.log_bytes += len;
        if let MarkSync::Inline(interval) = self.config.mark_sync
            && self.last_mark_sync.elapsed() >= interval
        {
            self.sync_mark(true).map_err(|e| ("mark sync", e))?;
        }
        Ok(())
    }

    /// Sequence-locked update (A.10): `seq` to odd, the fields, `seq` to even.
    fn update_mark(&mut self) -> io::Result<()> {
        self.mark.seq = self.mark.seq.wrapping_add(1);
        self.mark_file.write_at(12, &self.mark.seq.to_le_bytes())?;
        self.mark.file_id = self.file_id;
        self.mark.offset = self.offset;
        self.mark.last_sequence = self.next_sequence - 1;
        self.mark_file.write_at(16, &self.mark.encode()[16..])?;
        self.mark.seq = self.mark.seq.wrapping_add(1);
        self.mark_dirty = true;
        self.mark_file.write_at(12, &self.mark.seq.to_le_bytes())
    }

    /// With the writer-thread rule, syncs a mark that changed and has waited an interval while no batch came:
    /// the mark on disk then lags by about one interval, as §3 says, even when ingest stops. Returns false if
    /// the writer must stop.
    fn idle_mark_sync(&mut self) -> bool {
        if let MarkSync::Inline(interval) = self.config.mark_sync
            && self.mark_dirty
            && self.last_mark_sync.elapsed() >= interval
            && let Err(e) = self.sync_mark(true)
        {
            self.shared.fail("mark sync", &e);
            return false;
        }
        true
    }

    /// Syncs the mark file; `record` adds the duration to the report (not for the shutdown sync).
    fn sync_mark(&mut self, record: bool) -> io::Result<()> {
        self.mark_dirty = false;
        let start = Instant::now();
        let r = self.mark_file.sync();
        if record {
            self.shared
                .mark_sync_ns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(start.elapsed().as_nanos() as u64);
        }
        self.last_mark_sync = Instant::now();
        r
    }
}
