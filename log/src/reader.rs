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
//!   rule 1, then a reset point), and a damaged tail is truncated under rule 3. Both write a recovery entry, which
//!   may lie below where the reader had reached; so the reader keeps a checksum over all the bytes of each of its
//!   recent batches, and when the last one no longer reads the same, it voids what it took above the last that
//!   does ([`VoidCause::Rewritten`]) and reads on from there, where the entry is.
//! - **The mark again after the scan.** If its reset stamp changed meanwhile, or it moved backwards, a start may have
//!   rewritten what was just read: the pass is discarded and returns nothing. Every start that removes a complete
//!   batch writes a recovery entry with a new stamp (`spec.md` §3 Recovery), so a changed stamp is the sign.
//! - **Recovery entries.** A reader that reads a recovery entry voids what it took above the entry's `after`
//!   (`spec.md` §7 Readers). [`ReaderState::stamp`] with `durable` is the saved point a later holder keeps; its
//!   check against the resets since is built with roadmap 5.3.
//!
//! Between refreshes a reader keeps the bytes of the segment it follows up to the mark, so that a refresh reads only
//! what is new: at most one segment's bytes per reader handle while it follows a segment. A whole-file read of an
//! earlier segment is freed when the refresh ends, and the kept bytes are freed when another segment is followed.
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
//! let alias = Item::from(Entry::Alias { id: 1, name: "hall".into() });
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
use crate::content::BatchContent;
use crate::content_error::BodyError;
use crate::crc::crc32c;
use crate::error::FormatError;
use crate::fs::Fs;
use crate::header::HEADER_LEN;
use crate::lease::{LEASE_EXPIRY, LEASE_REFRESH, LeaseSlot, lease_name};
use crate::manifest::{LostRange, ManifestError};
use crate::mark::{MarkSlot, ResetStamp};
use crate::scan::{Found, InvalidRegion, Resume, ScanLimit, Scanner, set_aside};
use crate::setaside::SetAside;
use crate::store::{LEASES_DIR, LOG_DIR, StoreError, read_descriptor};
use crate::writer::{Clock, Position};
use files::read_part;

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
    /// The restored ends in force, from the latest restore record: set-aside batches are skipped (`spec.md` §8
    /// Restore step 1).
    pub set_aside: SetAside,
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
            set_aside: SetAside::default(),
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
    /// The mark's reset stamp: with `durable`, the saved point a later handle can check (`spec.md` §7 Readers).
    pub stamp: ResetStamp,
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
        self.content.recovery()
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
    /// A batch the reader returned no longer reads the same: a start truncated it (`spec.md` §3 Recovery).
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
    /// CRC32C over the batch's bytes, commit block included; computed when the pass that returned it ends, for the
    /// [`RECENT`] batches still remembered then.
    crc: Option<u32>,
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
    /// Regions returned, by file and byte span: one met again with a new extent is returned again (Q-200).
    regions: BTreeSet<(u64, u64, u64)>,
}

/// The bytes of the segment followed up to the mark, kept between refreshes so that a refresh reads and zeroes only
/// what is new; empty after a whole-file read and when another segment is followed.
#[derive(Default)]
struct Scratch {
    file_id: Option<u64>,
    bytes: Vec<u8>,
    /// The bytes are the whole of this earlier segment, kept because a bounded refresh stopped inside it.
    whole: Option<u64>,
}

impl std::fmt::Debug for Scratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Scratch({:?}, {} bytes)", self.file_id, self.bytes.len())
    }
}

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
    scratch: Scratch,
    /// What [`LogReader::lookup`] keeps of sealed segments' indexes and of segments' batches.
    caches: lookup::Caches,
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
                stamp: ResetStamp::default(),
                lease_renewed_at: wall,
            },
            view: View::default(),
            lost: Vec::new(),
            scratch: Scratch::default(),
            caches: lookup::Caches::default(),
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
        r.state.stamp = mark.stamp;
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

    /// Bytes kept between refreshes: those of the segment followed, up to the mark (module documentation).
    pub fn kept_bytes(&self) -> usize {
        self.scratch.bytes.capacity()
    }

    /// Frees the bytes kept between refreshes, for a reader that will look facts up rather than refresh; a later
    /// refresh reads its segment again from the point reached.
    pub fn release_bytes(&mut self) {
        self.scratch = Scratch::default();
    }

    /// The lease file's path.
    pub fn lease_path(&self) -> &Path {
        &self.lease_path
    }

    /// Re-reads the mark, the manifest and the batches appended since the last refresh, below the mark only, and
    /// renews the lease naming the manifest's current epoch (`api.md` `refresh`). On an error nothing the reader
    /// holds changes and nothing it found is lost: the next refresh finds it again.
    pub fn refresh(&mut self) -> Result<Update, ReaderError> {
        self.refresh_inner(None).map(|(u, _)| u)
    }

    /// [`LogReader::refresh`] that stops after the batch that brings the items it returns to `max_items` or more,
    /// so that a reader taking a long log holds one part of it at a time. Returns whether more may follow; until a
    /// call returns `false`, [`ReaderState::durable`] is the last position returned rather than the mark's.
    pub fn refresh_some(&mut self, max_items: usize) -> Result<(Update, bool), ReaderError> {
        self.refresh_inner(Some(max_items.max(1)))
    }

    fn refresh_inner(&mut self, budget: Option<usize>) -> Result<(Update, bool), ReaderError> {
        let mark = self.read_mark()?;
        let (epoch, lost) = self.read_manifest()?;
        // The lease names the epoch before any file is read under it (`spec.md` §7 Readers).
        let renewed = self.renew_slot(epoch, mark.generation)?;
        let mut view = self.view.clone();
        let mut events = Vec::new();
        let mut scratch = std::mem::take(&mut self.scratch);
        let read = self.read_log(&mut view, &mark, &mut events, &mut scratch, budget);
        if scratch.file_id.is_none() && scratch.whole.is_none() {
            scratch.bytes = Vec::new();
        }
        self.scratch = scratch;
        // The lease was written whatever the log held: keep its state.
        self.commit_lease(renewed);
        let more = read?;
        // A mark with another reset stamp, or one that moved backwards, while the files were read: a start may have
        // rewritten what was read under the first one. Discard the pass; the next refresh checks what it holds and
        // reads again.
        if let Ok(again) = self.read_mark()
            && behind(&again, &mark)
        {
            if self.scratch.whole.take().is_some() {
                self.scratch.bytes = Vec::new();
            }
            // Nothing was taken: more may follow, so a caller reading in parts reads again.
            return Ok((
                Update {
                    state: self.state,
                    events: Vec::new(),
                },
                true,
            ));
        }
        self.view = view;
        self.lost = lost;
        self.state.durable = if more {
            self.view.taken
        } else {
            Position {
                generation: mark.generation,
                sequence: mark.last_sequence,
            }
        };
        self.state.generation = mark.generation;
        self.state.stamp = mark.stamp;
        Ok((
            Update {
                state: self.state,
                events,
            },
            more,
        ))
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

    fn read_log(
        &self,
        view: &mut View,
        mark: &MarkSlot,
        events: &mut Vec<ReadEvent>,
        scratch: &mut Scratch,
        budget: Option<usize>,
    ) -> Result<bool, ReaderError> {
        let mut items = 0usize;
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
        // Segments of earlier generations are sealed and read whole, whichever segment the mark names (Q-268).
        names.retain(|n| {
            n.file_id > 0
                && n.file_id >= view.file_id
                && (n.file_id <= mark.file_id || n.generation < mark.generation)
        });
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
            let resumed = if file_id == view.file_id {
                view.resume
            } else {
                None
            };
            let from = resumed.unwrap_or(Resume {
                offset: HEADER_LEN as u64,
                last_sequence: None,
            });
            if let ScanLimit::Mark(m) = limit {
                let m = m.max(HEADER_LEN as u64);
                // The mark is behind the point reached in its segment, or at it: nothing new is below it. At it, the
                // pass that reached that point checked the header.
                if from.offset > m || (resumed.is_some() && from.offset == m) {
                    break;
                }
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
            let whole = matches!(limit, ScanLimit::End);
            let follows = (!whole).then_some(file_id);
            let kept = whole && resumed.is_some() && scratch.whole == Some(file_id);
            scratch.whole = None;
            if !kept {
                if scratch.file_id != follows {
                    scratch.bytes = Vec::new();
                }
                scratch.file_id = follows;
                read_part(&mut scratch.bytes, &*f, from.offset, len, whole)
                    .map_err(io_err(&path))?;
            }
            let bytes = &scratch.bytes[..];
            let id = SegmentIdentity {
                store_id: self.store_id,
                file_id,
                generation: n.generation,
            };
            let refused = |error| ReaderError::Unsupported {
                path: path.clone(),
                error,
            };
            let mut s = Scanner::resume(bytes, &id, limit, from)
                .map_err(refused)?
                .keeping_content();
            view.file_id = file_id;
            view.resume = Some(from);
            loop {
                let before = s.resume_point();
                let Some(found) = s.next() else { break };
                match found.map_err(refused)? {
                    Found::Region(region) => {
                        if view.regions.insert((file_id, region.start, region.end)) {
                            events.push(ReadEvent::Corruption { file_id, region });
                        }
                    }
                    Found::Batch(b) if set_aside(&b, &self.cfg.set_aside) => {}
                    Found::Batch(b) => {
                        let g = b.commit.generation;
                        let last = Position {
                            generation: g,
                            sequence: b.commit.last_sequence,
                        };
                        // Never above the mark's last position, from any segment.
                        if g == mark.generation && last.sequence > mark.last_sequence {
                            view.resume = Some(before);
                            fingerprint(&mut view.recent, bytes);
                            return Ok(false);
                        }
                        let content = match s.take_content() {
                            Some(c) => c,
                            None => {
                                BatchContent::decode(&b).map_err(|error| ReaderError::Content {
                                    path: path.clone(),
                                    offset: b.offset,
                                    error,
                                })?
                            }
                        };
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
                                crc: None,
                                first: rb.first,
                                last,
                            });
                            if view.recent.len() > RECENT {
                                view.recent.pop_front();
                            }
                            items += rb.content.items.len();
                            events.push(ReadEvent::Batch(rb));
                        }
                    }
                }
                view.resume = Some(s.resume_point());
                if budget.is_some_and(|b| items >= b) {
                    fingerprint(&mut view.recent, bytes);
                    if whole {
                        scratch.whole = Some(file_id);
                    }
                    return Ok(true);
                }
            }
            fingerprint(&mut view.recent, bytes);
        }
        Ok(false)
    }

    /// Checks that the last batch returned still reads the same. If not, voids what was taken above the last
    /// remembered batch that does, and reads again from there; with none left, from the start of the oldest
    /// remembered batch's segment, where every recovery entry is applied again: the one the truncating start wrote
    /// voids the rest, and an earlier one may void batches that are intact, which are then returned again
    /// (`spec.md` §3 Recovery).
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

/// Computes the checksum of the remembered batches that have none, all of them read from `bytes` in this pass.
fn fingerprint(recent: &mut VecDeque<Fingerprint>, bytes: &[u8]) {
    for fp in recent.iter_mut().rev().take_while(|fp| fp.crc.is_none()) {
        fp.crc = Some(crc32c(&bytes[fp.offset as usize..fp.end as usize]));
    }
}

/// Whether mark `b` is not a later state of mark `a`: another generation or reset stamp, a lower last position, or
/// an earlier place.
fn behind(b: &MarkSlot, a: &MarkSlot) -> bool {
    b.generation != a.generation
        || b.stamp != a.stamp
        || b.last_sequence < a.last_sequence
        || (b.file_id, b.offset) < (a.file_id, a.offset)
}

mod files;
mod lookup;
#[cfg(test)]
mod tests;

pub use lookup::{FoundFact, LOOKUP_CACHE_BYTES, Lookup, PIECE, Wanted};
