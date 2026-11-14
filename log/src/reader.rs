//! A reader process (`spec.md` §7 Readers, `api.md` Reader processes): reads the log of a store while its writer,
//! in this or another process, writes it. It takes no lock and owns no thread; every call is blocking file I/O.
//!
//! What a reader trusts (`format.md` A.2, A.10, A.13):
//!
//! - **The mark, read by path at every refresh.** Both slots are read and the valid slot with the higher counter
//!   wins; a slot of another store is invalid; with neither slot valid the reader reads again, 8 times, then
//!   returns [`ReaderError::MarkUnreadable`]. A missing mark is unreadable too (Q-118 (a)).
//! - **Nothing above the mark's offset.** The segment the mark points into is read only below the mark's offset,
//!   also once it is sealed or renamed; earlier segments are read whole, with what follows their last batch (a
//!   seal being written) left alone, and no batch above the mark's last position is taken from them. Below the
//!   mark, whatever is not a complete batch is corruption, passed by resynchronisation and reported.
//! - **The last batch it returned, checked again before every refresh.** A writer's restart never changes a
//!   complete batch below the mark it acts on, but that mark can be older than the one the reader read: a start
//!   after a page-cache loss on the same boot truncates the batch beyond the mark on disk (`spec.md` §3 Recovery
//!   rule 1, accepted limit), and a damaged tail is truncated under rule 3. The reader keeps a checksum over all
//!   the bytes of each of its recent batches (the commit block's `batch_crc32c` reaches the bodies since Q-119;
//!   the whole-batch sum stays until the reset stamp decides, Q-118 (e)); when
//!   the last one no longer reads the same, it voids what it took above the last that does
//!   ([`VoidCause::Rewritten`], Q-118 (e)).
//! - **The mark again after the scan.** If it moved backwards meanwhile, a start may have rewritten what was just
//!   read: the pass is discarded and returns nothing (Q-118 (l)).
//! - **Recovery entries.** A reader that reads a recovery entry voids what it took above the entry's `after`
//!   (`spec.md` §7 Readers). How a position kept beyond the life of a reader is checked against a later truncation
//!   or restore is open (Q-46): this reader voids only what it took itself.
//!
//! The reader names in its lease (`format.md` A.15) the manifest epoch it took: it writes the lease at open,
//! renews it at every [`LogReader::refresh`], which takes the manifest's current epoch, and through
//! [`LogReader::keep_alive`] once half of `lease_refresh` has passed. Renewal opens the lease by path, so a lease
//! the writer deleted as expired is created again (with a new epoch). The lease is deleted on close.
//!
//! ```
//! use std::path::Path;
//! use std::sync::Arc;
//! use std::time::Duration;
//! use kvik_log::content::Item;
//! use kvik_log::entries::Entry;
//! use kvik_log::fs::MemFs;
//! use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig};
//! use kvik_log::store::{NewStore, StoreDir};
//! use kvik_log::writer::{Log, LogWriter, SystemClock, WriterConfig};
//!
//! let fs = Arc::new(MemFs::new());
//! let new = NewStore {
//!     store_id: [1; 16],
//!     created_at: 0,
//!     site_zone: "Etc/UTC".into(),
//!     tzdata: "2026a".into(),
//!     software: "example".into(),
//! };
//! let store = StoreDir::open(fs.clone(), Path::new("store"), Some(&new)).unwrap();
//! let writer = LogWriter::open(store, WriterConfig::default(), Some([2; 16]), Duration::ZERO, 0).unwrap();
//! let log = Log::start(writer, Arc::new(SystemClock::default())).unwrap();
//! let alias = Item::Entry(Entry::Alias { id: 1, name: "hall".into() });
//! log.submit(vec![alias.clone()]).unwrap().wait().unwrap();
//!
//! // Another handle on the same files: no lock, no thread.
//! let clock = Arc::new(SystemClock::default());
//! let mut reader = LogReader::open(ReaderConfig::new("store", fs, clock, Some([2; 16]), [7; 16])).unwrap();
//! let update = reader.refresh().unwrap();
//! assert_eq!(update.state.durable.sequence, 1);
//! assert!(matches!(&update.events[..], [ReadEvent::Batch(b)] if b.content.items == [alias]));
//! log.shutdown().unwrap();
//! ```

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::batch::SegmentIdentity;
use crate::content::{BatchContent, Item};
use crate::content_error::BodyError;
use crate::crc::crc32c;
use crate::entries::Entry;
use crate::error::FormatError;
use crate::fs::{Fs, FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::lease::{LEASE_EXPIRY, LEASE_REFRESH, LeaseSlot, SLOT_OFFSET, lease_name};
use crate::manifest::{LostRange, ManifestError, read_manifest};
use crate::mark::{MARK_LEN, MarkSlot, read_mark};
use crate::name::SegmentName;
use crate::scan::{Found, InvalidRegion, Resume, ScanLimit, Scanner};
use crate::store::{LEASES_DIR, LOG_DIR, MANIFEST, MARK, StoreError, read_all, read_descriptor};
use crate::writer::{Clock, Position};

/// How many times a reader reads the mark again when neither slot is valid (`format.md` A.10).
pub const MARK_RETRIES: u32 = 8;
/// The longest segment file a reader reads into memory: the largest `segment_size` (1 GiB, `format.md` A.4) and
/// room for the seal blocks.
pub const MAX_SEGMENT_FILE: u64 = (1 << 30) + (32 << 20);
/// How many recently returned batches the reader keeps a checksum of, to find what a rewrite left.
const RECENT: usize = 64;

/// How a reader is opened (`api.md` `ReaderConfig`).
#[derive(Clone, Debug)]
pub struct ReaderConfig {
    /// The store directory.
    pub dir: PathBuf,
    /// The file system.
    pub fs: Arc<dyn Fs>,
    /// The clocks: monotonic for the lease's renewal, wall for its creation time.
    pub clock: Arc<dyn Clock>,
    /// The boot identity, from the same source as the mark's; `None` where the platform has none.
    pub boot_id: Option<[u8; 16]>,
    /// 16 random bytes per reader handle: the lease's name.
    pub reader_id: [u8; 16],
    /// The longest time between renewals of the lease; at most 30 minutes (`spec.md` §7 Readers): a longer value
    /// is taken as 30 minutes.
    pub lease_refresh: Duration,
}

impl ReaderConfig {
    /// A configuration with the default `lease_refresh` of 30 minutes.
    pub fn new(
        dir: impl Into<PathBuf>,
        fs: Arc<dyn Fs>,
        clock: Arc<dyn Clock>,
        boot_id: Option<[u8; 16]>,
        reader_id: [u8; 16],
    ) -> Self {
        Self {
            dir: dir.into(),
            fs,
            clock,
            boot_id,
            reader_id,
            lease_refresh: LEASE_REFRESH,
        }
    }
}

/// Why a reader call failed. A failed call changes nothing the reader holds.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReaderError {
    /// The store descriptor is missing or invalid.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Neither slot of the mark was valid after 8 more reads, or there is no mark (`format.md` A.10).
    #[error("the durable mark is unreadable")]
    MarkUnreadable,
    /// A version refusal in one file: the reader reads nothing at or past it (`format.md` A, common rules).
    #[error("{path}: {error}")]
    Unsupported {
        /// The file.
        path: PathBuf,
        /// The refusal.
        error: FormatError,
    },
    /// A complete batch whose content fails a check of the batch as a whole.
    #[error("{path} at offset {offset}: {error:?}")]
    Content {
        /// The segment.
        path: PathBuf,
        /// The batch's offset.
        offset: u64,
        /// The failure.
        error: BodyError,
    },
    /// A segment file longer than [`MAX_SEGMENT_FILE`]: not read.
    #[error("{path} is {len} bytes, longer than any segment")]
    TooLong {
        /// The segment.
        path: PathBuf,
        /// Its length.
        len: u64,
    },
    /// The manifest cannot be read.
    #[error(transparent)]
    Manifest(ManifestError),
    /// A segment the reader must read was not found, under either extension.
    #[error("segment {file_id} is missing")]
    SegmentMissing {
        /// Its file id.
        file_id: u64,
    },
    /// A file operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ReaderError + '_ {
    move |source| ReaderError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// What the reader holds after a call (`api.md` `ReaderState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReaderState {
    /// The mark's position when last read: nothing above it has been returned since.
    pub durable: Position,
    /// The manifest epoch the lease names.
    pub epoch: u64,
    /// The mark's generation.
    pub generation: u32,
    /// When the lease was last written, wall clock.
    pub lease_renewed_at: i64,
}

/// A complete batch returned by the reader.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadBatch {
    /// The segment.
    pub file_id: u64,
    /// The batch's file offset.
    pub offset: u64,
    /// Its first position.
    pub first: Position,
    /// Its last position.
    pub last: Position,
    /// Its commit time.
    pub commit_time: i64,
    /// Its content.
    pub content: BatchContent,
}

impl ReadBatch {
    /// The recovery entry this batch holds, if it is one (`spec.md` §3 Recovery rule 3).
    pub fn recovery(&self) -> Option<&crate::entries::Recovery> {
        match self.content.items.as_slice() {
            [Item::Entry(Entry::Recovery(r))] => Some(r),
            _ => None,
        }
    }
}

/// Why the reader voided what it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoidCause {
    /// A recovery entry at this position (`spec.md` §7 Readers).
    Recovery {
        /// The entry's position.
        entry: Position,
    },
    /// A batch the reader returned no longer reads the same: a start truncated it (Q-118 (e)).
    Rewritten,
    /// The mark's generation changed (Q-118 (c)).
    Generation {
        /// The generation read so far.
        from: u32,
        /// The mark's.
        to: u32,
    },
}

/// One finding of a refresh, in log order.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadEvent {
    /// A complete batch above everything taken before.
    Batch(ReadBatch),
    /// Everything taken above `above` is void; later batches may take those positions again.
    Void {
        /// The last position still valid; `Position::default()` voids everything.
        above: Position,
        /// Why.
        cause: VoidCause,
    },
    /// An invalid region below the mark: the positions between the batches around it are lost (`spec.md` §3
    /// Recovery rule 2).
    Corruption {
        /// The segment.
        file_id: u64,
        /// The region.
        region: InvalidRegion,
    },
}

/// The result of a refresh.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    /// What the reader holds now.
    pub state: ReaderState,
    /// What it found since the last refresh.
    pub events: Vec<ReadEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    file_id: u64,
    /// The batch's offset and end.
    offset: u64,
    end: u64,
    /// CRC32C over the batch's bytes, commit block included.
    crc: u32,
    first: Position,
    last: Position,
}

/// Where reading continues, what was taken, and what is remembered to check it.
#[derive(Clone, Debug, Default)]
struct View {
    generation: Option<u32>,
    /// The segment being read (0 before the first) and the point in it; `None` from its start.
    file_id: u64,
    resume: Option<Resume>,
    taken: Position,
    recent: VecDeque<Fingerprint>,
    regions: BTreeSet<(u64, u64)>,
}

/// A segment opened by name, under the extension it was found with.
type OpenSegment = (SegmentName, Box<dyn FsFile>);

/// A reader of the log. See the module documentation.
#[derive(Debug)]
pub struct LogReader {
    cfg: ReaderConfig,
    store_id: [u8; 16],
    log_dir: PathBuf,
    lease_path: PathBuf,
    lease: LeaseSlot,
    renewed_at: Duration,
    state: ReaderState,
    view: View,
    lost: Vec<LostRange>,
    closed: bool,
}

impl LogReader {
    /// Opens a reader on the store in `cfg.dir`: reads the descriptor without the lock, reads the mark and the
    /// manifest, and writes the lease naming the manifest's epoch. Reads no segment: the first
    /// [`LogReader::refresh`] returns the log from its start.
    pub fn open(mut cfg: ReaderConfig) -> Result<Self, ReaderError> {
        cfg.lease_refresh = cfg.lease_refresh.min(LEASE_REFRESH);
        let store_id = read_descriptor(&*cfg.fs, &cfg.dir)?.store_id;
        let log_dir = cfg.dir.join(LOG_DIR);
        let lease_path = cfg.dir.join(LEASES_DIR).join(lease_name(&cfg.reader_id));
        let now = cfg.clock.monotonic();
        let wall = cfg.clock.wall();
        let lease = LeaseSlot {
            epoch: 0,
            refresh_counter: 0,
            boot_id: cfg.boot_id.unwrap_or_default(),
            created_at: wall,
        };
        let mut r = Self {
            store_id,
            log_dir,
            lease_path,
            lease,
            renewed_at: now,
            state: ReaderState {
                durable: Position::default(),
                epoch: 0,
                generation: 0,
                lease_renewed_at: wall,
            },
            view: View::default(),
            lost: Vec::new(),
            closed: false,
            cfg,
        };
        let mark = r.read_mark()?;
        let (epoch, lost) = r.read_manifest()?;
        r.renew(epoch, mark.generation)?;
        r.lost = lost;
        r.state.durable = Position {
            generation: mark.generation,
            sequence: mark.last_sequence,
        };
        r.state.generation = mark.generation;
        Ok(r)
    }

    /// What the reader holds.
    pub fn state(&self) -> ReaderState {
        self.state
    }

    /// The lost ranges the manifest held at the last refresh (`api.md`: a reader's health reports what the files
    /// hold).
    pub fn lost_ranges(&self) -> &[LostRange] {
        &self.lost
    }

    /// The lease file's path.
    pub fn lease_path(&self) -> &Path {
        &self.lease_path
    }

    /// Re-reads the mark, the manifest and the batches appended since the last refresh, below the mark only, and
    /// renews the lease naming the manifest's current epoch (`api.md` `refresh`). On an error nothing the reader
    /// holds changes and nothing it found is lost: the next refresh finds it again.
    pub fn refresh(&mut self) -> Result<Update, ReaderError> {
        let mark = self.read_mark()?;
        let (epoch, lost) = self.read_manifest()?;
        // The lease names the epoch before any file is read under it (`spec.md` §7 Readers).
        let renewed = self.renew_slot(epoch, mark.generation)?;
        let mut view = self.view.clone();
        let mut events = Vec::new();
        let read = self.read_log(&mut view, &mark, &mut events);
        // The lease was written whatever the log held: keep its state.
        self.commit_lease(renewed);
        read?;
        // A mark that moved backwards while the files were read: a start may have rewritten what was read under the
        // first one. Discard the pass; the next refresh checks what it holds and reads again (Q-118 (l)).
        if let Ok(again) = self.read_mark()
            && behind(&again, &mark)
        {
            return Ok(Update {
                state: self.state,
                events: Vec::new(),
            });
        }
        self.view = view;
        self.lost = lost;
        self.state.durable = Position {
            generation: mark.generation,
            sequence: mark.last_sequence,
        };
        self.state.generation = mark.generation;
        Ok(Update {
            state: self.state,
            events,
        })
    }

    /// Renews the lease if half of `lease_refresh` has passed since it was written, keeping its epoch, or, once
    /// the lease may have expired, with the manifest's current epoch. Returns whether it wrote the lease. A read
    /// call that does not refresh calls this (`api.md` Reader processes).
    pub fn keep_alive(&mut self) -> Result<bool, ReaderError> {
        let elapsed = self.cfg.clock.monotonic().saturating_sub(self.renewed_at);
        if elapsed < self.cfg.lease_refresh / 2 {
            return Ok(false);
        }
        let epoch = if elapsed >= LEASE_EXPIRY {
            self.read_manifest()?.0
        } else {
            self.lease.epoch
        };
        self.renew(epoch, self.state.generation)?;
        Ok(true)
    }

    /// Closes the reader and deletes its lease.
    pub fn close(mut self) -> Result<(), ReaderError> {
        self.closed = true;
        match self.cfg.fs.remove_file(&self.lease_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&self.lease_path)(e)),
        }
    }

    fn read_mark(&self) -> Result<MarkSlot, ReaderError> {
        let path = self.log_dir.join(MARK);
        for _ in 0..=MARK_RETRIES {
            let bytes = match self.cfg.fs.open(&path, OpenMode::Read) {
                Ok(f) => read_upto(&*f, MARK_LEN).map_err(io_err(&path))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(io_err(&path)(e)),
            };
            match read_mark(&bytes, &self.store_id) {
                Ok(Some((_, m))) => return Ok(m),
                Ok(None) => std::thread::yield_now(),
                Err(error) => return Err(ReaderError::Unsupported { path, error }),
            }
        }
        Err(ReaderError::MarkUnreadable)
    }

    fn read_manifest(&self) -> Result<(u64, Vec<LostRange>), ReaderError> {
        let path = self.cfg.dir.join(MANIFEST);
        let bytes = read_all(&*self.cfg.fs, &path).map_err(io_err(&path))?;
        let m = read_manifest(&bytes, &self.store_id).map_err(ReaderError::Manifest)?;
        Ok((m.last_epoch(), m.lost_ranges().cloned().collect()))
    }

    fn renew(&mut self, epoch: u64, generation: u32) -> Result<(), ReaderError> {
        let s = self.renew_slot(epoch, generation)?;
        self.commit_lease(s);
        Ok(())
    }

    /// Writes the lease with the next counter, by path; creates it again if the writer deleted it.
    fn renew_slot(&self, epoch: u64, generation: u32) -> Result<LeaseSlot, ReaderError> {
        let slot = LeaseSlot {
            epoch,
            refresh_counter: self.lease.refresh_counter + 1,
            ..self.lease
        };
        let fs = &*self.cfg.fs;
        let path = &self.lease_path;
        match fs.open(path, OpenMode::ReadWrite) {
            // The slot alone, in place, when the header is the lease's own; else the whole file.
            Ok(f) => {
                let mut h = [0u8; HEADER_LEN];
                let ok = read_at_full(&*f, &mut h, 0).is_ok_and(|n| n == HEADER_LEN)
                    && FileHeader::decode(&h)
                        .is_some_and(|h| h.kind == kind::LEASE && h.store_id == self.store_id);
                if ok {
                    f.write_at(&slot.encode(), SLOT_OFFSET as u64)
                } else {
                    f.write_at(&slot.encode_file(self.store_id, generation), 0)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs
                .open(path, OpenMode::CreateNew)
                .and_then(|f| f.write_at(&slot.encode_file(self.store_id, generation), 0)),
            Err(e) => Err(e),
        }
        .map_err(io_err(path))?;
        Ok(slot)
    }

    fn commit_lease(&mut self, slot: LeaseSlot) {
        self.lease = slot;
        self.renewed_at = self.cfg.clock.monotonic();
        self.state.lease_renewed_at = self.cfg.clock.wall();
        self.state.epoch = slot.epoch;
    }

    /// The segments of the log by file id, under either extension; none for a missing directory.
    fn segments(&self) -> Result<Vec<SegmentName>, ReaderError> {
        let mut v: Vec<SegmentName> = match self.cfg.fs.read_dir(&self.log_dir) {
            Ok(v) => v
                .into_iter()
                .filter_map(|e| SegmentName::parse(&e.name))
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_err(&self.log_dir)(e)),
        };
        v.sort_by_key(|n| (n.file_id, n.sealed));
        v.dedup_by_key(|n| n.file_id);
        Ok(v)
    }

    /// Opens a segment by name; if it is not found, lists the directory and tries the other extension once
    /// (`format.md` A.13). `None` if it is gone.
    fn open_segment(&self, name: SegmentName) -> Result<Option<OpenSegment>, ReaderError> {
        for n in [
            name,
            SegmentName {
                sealed: !name.sealed,
                ..name
            },
        ] {
            let path = self.log_dir.join(n.to_string());
            match self.cfg.fs.open(&path, OpenMode::Read) {
                Ok(f) => return Ok(Some((n, f))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path)(e)),
            }
        }
        Ok(None)
    }

    fn find_segment(&self, file_id: u64) -> Result<Option<SegmentName>, ReaderError> {
        Ok(self.segments()?.into_iter().find(|n| n.file_id == file_id))
    }

    /// Whether the batch at `fp` still reads the same: the same bytes at the same place.
    fn still_reads(&self, fp: &Fingerprint) -> Result<bool, ReaderError> {
        let Some(name) = self.find_segment(fp.file_id)? else {
            return Ok(false);
        };
        let Some((n, f)) = self.open_segment(name)? else {
            return Ok(false);
        };
        let path = self.log_dir.join(n.to_string());
        let len =
            usize::try_from(fp.end - fp.offset).map_err(|e| io_err(&path)(io::Error::other(e)))?;
        let mut b = vec![0u8; len];
        let got = read_at_full(&*f, &mut b, fp.offset).map_err(io_err(&path))?;
        Ok(got == len && crc32c(&b) == fp.crc)
    }

    fn read_log(
        &self,
        view: &mut View,
        mark: &MarkSlot,
        events: &mut Vec<ReadEvent>,
    ) -> Result<(), ReaderError> {
        // A new generation: everything taken is void and the log is read again from its start (Q-118 (c)).
        match view.generation {
            Some(g) if g != mark.generation => {
                if view.taken != Position::default() {
                    events.push(ReadEvent::Void {
                        above: Position::default(),
                        cause: VoidCause::Generation {
                            from: g,
                            to: mark.generation,
                        },
                    });
                }
                *view = View::default();
            }
            _ => {}
        }
        view.generation = Some(mark.generation);
        self.check_recent(view, events)?;
        let mut names = self.segments()?;
        names.retain(|n| n.file_id > 0 && n.file_id >= view.file_id && n.file_id <= mark.file_id);
        if mark.file_id > 0
            && mark.file_id >= view.file_id
            && names.last().is_none_or(|n| n.file_id != mark.file_id)
        {
            // Renamed between the listing and now, or gone.
            match self.find_segment(mark.file_id)? {
                Some(n) => names.push(n),
                None => {
                    return Err(ReaderError::SegmentMissing {
                        file_id: mark.file_id,
                    });
                }
            }
        }
        for name in names {
            let file_id = name.file_id;
            let limit = if file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let from = if file_id == view.file_id {
                view.resume
            } else {
                None
            };
            let from = from.unwrap_or(Resume {
                offset: HEADER_LEN as u64,
                last_sequence: None,
            });
            if let ScanLimit::Mark(m) = limit
                && from.offset > m.max(HEADER_LEN as u64)
            {
                // The mark is behind the point reached in its segment: nothing new is below it.
                break;
            }
            // An earlier segment that is gone would leave its positions unread: an error, as for the mark's.
            let Some((n, f)) = self.open_segment(name)? else {
                return Err(ReaderError::SegmentMissing { file_id });
            };
            let path = self.log_dir.join(n.to_string());
            let len = match limit {
                ScanLimit::Mark(m) => m,
                ScanLimit::End => f.len().map_err(io_err(&path))?,
            };
            if len > MAX_SEGMENT_FILE {
                return Err(ReaderError::TooLong { path, len });
            }
            let bytes = read_part(&*f, from.offset, len).map_err(io_err(&path))?;
            let id = SegmentIdentity {
                store_id: self.store_id,
                file_id,
                generation: n.generation,
            };
            let refused = |error| ReaderError::Unsupported {
                path: path.clone(),
                error,
            };
            let mut s = Scanner::resume(&bytes, &id, limit, from).map_err(refused)?;
            view.file_id = file_id;
            view.resume = Some(from);
            loop {
                let before = s.resume_point();
                let Some(found) = s.next() else { break };
                match found.map_err(refused)? {
                    Found::Region(region) => {
                        if view.regions.insert((file_id, region.start)) {
                            events.push(ReadEvent::Corruption { file_id, region });
                        }
                    }
                    Found::Batch(b) => {
                        let g = b.commit.generation;
                        let last = Position {
                            generation: g,
                            sequence: b.commit.last_sequence,
                        };
                        // Never above the mark's last position, from any segment.
                        if g == mark.generation && last.sequence > mark.last_sequence {
                            view.resume = Some(before);
                            return Ok(());
                        }
                        let content =
                            BatchContent::decode(&b).map_err(|error| ReaderError::Content {
                                path: path.clone(),
                                offset: b.offset,
                                error,
                            })?;
                        let crc = crc32c(&bytes[b.offset as usize..b.end() as usize]);
                        let rb = ReadBatch {
                            file_id,
                            offset: b.offset,
                            first: Position {
                                generation: g,
                                sequence: b.commit.first_sequence,
                            },
                            last,
                            commit_time: b.commit.commit_time,
                            content,
                        };
                        // Reading only moves forward past batches not read before, or past the last batch that
                        // still reads the same, so every entry met is applied: one written again at the offset
                        // of an earlier one (adopted, Q-117) still removed what followed.
                        if let Some(r) = rb.recovery() {
                            let after = Position {
                                generation: g,
                                sequence: r.after,
                            };
                            if after < view.taken {
                                events.push(ReadEvent::Void {
                                    above: after,
                                    cause: VoidCause::Recovery { entry: rb.first },
                                });
                                view.taken = after;
                                view.recent.retain(|f| f.last <= after);
                            }
                        }
                        if last > view.taken {
                            view.taken = last;
                            view.recent.push_back(Fingerprint {
                                file_id,
                                offset: b.offset,
                                end: b.end(),
                                crc,
                                first: rb.first,
                                last,
                            });
                            if view.recent.len() > RECENT {
                                view.recent.pop_front();
                            }
                            events.push(ReadEvent::Batch(rb));
                        }
                    }
                }
                view.resume = Some(s.resume_point());
            }
        }
        Ok(())
    }

    /// Checks that the last batch returned still reads the same. If not, voids what was taken above the last
    /// remembered batch that does, and reads again from there; with none left, from the start of the oldest
    /// remembered batch's segment, where every recovery entry is applied again: the one the truncating start wrote
    /// voids the rest, and an earlier one may void batches that are intact, which are then returned again
    /// (Q-118 (e)).
    fn check_recent(
        &self,
        view: &mut View,
        events: &mut Vec<ReadEvent>,
    ) -> Result<(), ReaderError> {
        let Some(last) = view.recent.back() else {
            return Ok(());
        };
        if self.still_reads(last)? {
            return Ok(());
        }
        let mut keep = 0;
        for fp in &view.recent {
            if !self.still_reads(fp)? {
                break;
            }
            keep += 1;
        }
        let oldest = view.recent[0];
        let above = match keep {
            0 => Position {
                generation: oldest.first.generation,
                sequence: oldest.first.sequence.saturating_sub(1),
            },
            k => view.recent[k - 1].last,
        };
        if above < view.taken {
            events.push(ReadEvent::Void {
                above,
                cause: VoidCause::Rewritten,
            });
            view.taken = above;
        }
        view.recent.truncate(keep);
        match view.recent.back() {
            Some(fp) => {
                view.file_id = fp.file_id;
                view.resume = Some(Resume {
                    offset: fp.end,
                    last_sequence: Some(fp.last.sequence),
                });
            }
            None => {
                view.file_id = oldest.file_id;
                view.resume = None;
            }
        }
        Ok(())
    }
}

impl Drop for LogReader {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.cfg.fs.remove_file(&self.lease_path);
        }
    }
}

/// Whether mark `b` is behind mark `a`: another generation, a lower last position, or an earlier place.
fn behind(b: &MarkSlot, a: &MarkSlot) -> bool {
    b.generation != a.generation
        || b.last_sequence < a.last_sequence
        || (b.file_id, b.offset) < (a.file_id, a.offset)
}

/// Reads up to `n` bytes from the start of `f`.
fn read_upto(f: &dyn FsFile, n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    let got = read_at_full(f, &mut b, 0)?;
    b.truncate(got);
    Ok(b)
}

/// Fills `buf` from `offset` until the end of the file; returns the bytes read.
fn read_at_full(f: &dyn FsFile, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        let n = f.read_at(&mut buf[at..], offset + at as u64)?;
        if n == 0 {
            break;
        }
        at += n;
    }
    Ok(at)
}

/// A buffer of `len` bytes holding the file's header and its bytes from `from` to `len`; the bytes between are
/// zero and never looked at (a scan resumed at `from` reads only forward). Bytes past the end of the file are zero.
fn read_part(f: &dyn FsFile, from: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(io::Error::other)?;
    let mut b = vec![0u8; len];
    let head = len.min(HEADER_LEN);
    read_at_full(f, &mut b[..head], 0)?;
    let from = usize::try_from(from).map_err(io::Error::other)?.min(len);
    if from < len {
        read_at_full(f, &mut b[from..], from as u64)?;
    }
    Ok(b)
}
