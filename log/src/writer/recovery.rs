//! The start of a writer (`spec.md` §3 Recovery rules 1 to 3 and Order of a start; `format.md` A.4 states recovery
//! may find, A.10): the first start of a new store creates the mark; any other start scans the unsealed segments,
//! decides by the mark and the boot identity what is kept, truncates, records losses in the manifest, writes or
//! adopts a recovery entry, writes the mark, and hands the writer the state it resumes from.
//!
//! Decisions are made from what was read before anything is written (`plan`); `start` then performs them in the
//! order of a start. A failed sync at any step leaves the writer read-only and writes nothing more.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::batch::{BatchBuilder, SegmentIdentity};
use crate::body::IndexRecord;
use crate::content::{BatchContent, Item};
use crate::entries::{Entry, IntervalLength, MarkState, Recovery};
use crate::fs::{FsFile, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::manifest::{
    LossCause, LostRange, ManifestError, ManifestRead, RECORD_LOST_RANGE, encode_record,
    read_manifest,
};
use crate::mark::{MARK_LEN, MarkSlot, SLOT_OFFSETS, encode_mark_file, read_mark, read_slots};
use crate::name::SegmentName;
use crate::scan::{HeaderState, ScanLimit, Tail, scan};
use crate::segment::SealSummary;
use crate::store::{MANIFEST, MARK, StoreDir, StoreError};

use super::maint::{AheadSegment, CreateError, SealJob, SegmentRequest, create_segment};
use super::{Fault, FaultOp, Halt, OpenSegment, WriterConfig, reach};

/// Which rule a start applied (`spec.md` §3 Recovery).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StartRule {
    /// The first start of a new store: the mark was created.
    FirstStart,
    /// Rule 1: the mark's boot is the current one; everything after the mark was truncated.
    SameBoot,
    /// Rule 3: another boot, or the mark missing, unreadable or stale; a recovery entry was written or adopted.
    Reboot(MarkState),
}

/// Whether a loss is certain (`api.md` `LossReport::kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossKind {
    /// Positions that held data are gone.
    DataLost,
    /// Positions above the last kept one may have been acknowledged and are assigned again after a recovery entry.
    PossibleLoss,
}

/// A loss found by this start (`api.md` `LossReport`, the part the log knows).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FoundLoss {
    /// Certain or possible.
    pub kind: LossKind,
    /// The cause.
    pub cause: LossCause,
    /// The loss's identity: the first position of its range when first reported.
    pub identity: u64,
    /// The first lost position, if any position is lost.
    pub first: Option<u64>,
    /// The last lost position.
    pub last: Option<u64>,
    /// The segment.
    pub file_id: u64,
    /// Bytes.
    pub bytes: u64,
    /// Readable commit blocks in a torn tail.
    pub readable_commits: u32,
    /// Whether a manifest record holds it.
    pub recorded: bool,
}

/// The recovery entry a start wrote or adopted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryReport {
    /// Its position's sequence, F.
    pub sequence: u64,
    /// Its content.
    pub entry: Recovery,
    /// Adopted from an earlier start rather than written.
    pub adopted: bool,
}

/// What a start found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartReport {
    /// The rule applied.
    pub rule: StartRule,
    /// Bytes zero-filled after the mark (rule 1) or after the recovery entry (rule 3).
    pub truncated_bytes: u64,
    /// Losses found.
    pub losses: Vec<FoundLoss>,
    /// The recovery entry, under rule 3.
    pub entry: Option<EntryReport>,
    /// Whether the last complete batch was written again before the sync (Q-87).
    pub rewrote_last_batch: bool,
    /// Segments whose header was rewritten from the file name and the first batch.
    pub headers_rewritten: Vec<u64>,
    /// Unused segments deleted.
    pub deleted: Vec<u64>,
    /// Segments handed over to be sealed again from step 1.
    pub to_seal: Vec<u64>,
}

/// The state the writer resumes from.
pub(super) struct Resume {
    pub mark: Option<Box<dyn FsFile>>,
    pub mark_slot: MarkSlot,
    pub mark_next_slot: usize,
    pub mark_dirty: bool,
    pub open: Option<OpenSegment>,
    pub ahead: Option<AheadSegment>,
    pub seals: Vec<SealJob>,
    pub next_file_id: u64,
    pub last_sequence: u64,
    pub last_commit_time: i64,
    pub max_series_id: u32,
    pub intervals: HashMap<u32, IntervalLength>,
    pub fault: Option<Fault>,
    pub report: StartReport,
}

/// A complete batch of an unsealed segment.
struct B {
    offset: u64,
    end: u64,
    first: u64,
    last: u64,
    commit_time: i64,
    content: BatchContent,
    bytes: Vec<u8>,
}

impl B {
    fn recovery(&self) -> Option<&Recovery> {
        match self.content.items.as_slice() {
            [Item::Entry(Entry::Recovery(r))] => Some(r),
            _ => None,
        }
    }
}

/// An unsealed segment as scanned.
struct Seg {
    id: SegmentIdentity,
    path: PathBuf,
    len: u64,
    header: Option<FileHeader>,
    batches: Vec<B>,
    /// Readable commit blocks in a torn tail: (offset, commit time).
    tail_commits: Vec<(u64, i64)>,
    /// Series ids defined by entries blocks still readable in a torn tail (`spec.md` §2.1, id floor).
    tail_ids: Vec<u32>,
    last_nonzero: u64,
}

impl Seg {
    fn in_use(&self) -> bool {
        self.header.is_some_and(|h| h.base_sequence >= 1) || !self.batches.is_empty()
    }
    fn base(&self) -> Option<u64> {
        self.header
            .filter(|h| h.base_sequence >= 1)
            .map(|h| h.base_sequence)
            .or(self.batches.first().map(|b| b.first))
    }
}

enum MarkRead {
    Missing,
    Unreadable,
    Readable { slot: MarkSlot, index: usize },
}

fn fault(op: FaultOp, path: &std::path::Path, e: &io::Error) -> Fault {
    Fault {
        op,
        path: path.to_path_buf(),
        kind: e.kind(),
        message: e.to_string(),
    }
}

fn manifest_error(e: ManifestError) -> StoreError {
    match e {
        ManifestError::Unsupported(f) => StoreError::Unsupported(f),
        ManifestError::Header => StoreError::Corrupt("manifest header"),
        ManifestError::Corrupt(_) => StoreError::ManifestCorrupt,
    }
}

fn scan_segment(store: &StoreDir, name: SegmentName, path: PathBuf) -> Result<Seg, StoreError> {
    let bytes = store.read_file(&path)?;
    let id = SegmentIdentity {
        store_id: store.descriptor().store_id,
        file_id: name.file_id,
        generation: name.generation,
    };
    let r = scan(&bytes, &id, ScanLimit::End).map_err(StoreError::Unsupported)?;
    let mut batches = Vec::new();
    for b in &r.batches {
        let content = BatchContent::decode(b).map_err(|_| StoreError::Corrupt("batch content"))?;
        batches.push(B {
            offset: b.offset,
            end: b.end(),
            first: b.commit.first_sequence,
            last: b.commit.last_sequence,
            commit_time: b.commit.commit_time,
            content,
            bytes: bytes[b.offset as usize..b.end() as usize].to_vec(),
        });
    }
    // Only the last batch's bytes can be written again (Q-87).
    let n = batches.len();
    for b in batches.iter_mut().take(n.saturating_sub(1)) {
        b.bytes = Vec::new();
    }
    let tail_commits = match &r.tail {
        Some(Tail::Torn { commits, .. }) => commits
            .iter()
            .map(|c| (c.offset, c.commit.commit_time))
            .collect(),
        _ => Vec::new(),
    };
    // Definitions still readable in the torn tail: the valid entries blocks from its start.
    let mut tail_ids = Vec::new();
    if let Some(Tail::Torn { start, .. }) = &r.tail {
        let mut at = *start;
        while let Ok(b) = crate::block::read_block(&bytes, at, bytes.len() as u64) {
            if b.header.block_type == crate::block::block_type::ENTRIES
                && let Ok(e) = crate::entries::EntriesBlock::decode(b.body)
            {
                tail_ids.extend(e.entries.iter().filter_map(|x| match x {
                    Entry::Definition(d) => Some(d.id),
                    _ => None,
                }));
            }
            at = b.next_offset();
        }
    }
    let header = match r.header {
        HeaderState::Valid(h) => Some(h),
        HeaderState::Invalid => None,
    };
    let last_nonzero = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1) as u64;
    Ok(Seg {
        id,
        path,
        len: bytes.len() as u64,
        header,
        batches,
        tail_commits,
        tail_ids,
        last_nonzero,
    })
}

/// The decisions of a start, made before anything is written.
struct Plan {
    rule: StartRule,
    /// Index into `segs` of the segment the writer resumes in, after truncation.
    resume_in: Option<usize>,
    /// Kept batches per segment: how many of each segment's batches are kept.
    kept: Vec<usize>,
    /// Zero-fill ranges `(segment index, from, to)`.
    zero: Vec<(usize, u64, u64)>,
    /// Losses found (rule 2 regions and recovery-entry gaps).
    losses: Vec<LostRange>,
    /// Rule 3: K, the tail's start and end in the resume segment, and the readable commits in it.
    k: u64,
    tail: Option<(usize, u64, u64, u32)>,
    /// Rule 3: the adopted entry (segment index, batch index).
    adopt: Option<(usize, usize)>,
    /// Write the last complete batch again (Q-87): segment and batch index.
    rewrite_last: Option<(usize, usize)>,
    /// Where writing resumes in `resume_in`.
    write_offset: u64,
    last_sequence: u64,
}

#[allow(clippy::too_many_arguments)]
fn lost(
    first: u64,
    last: u64,
    file_id: u64,
    offset: u64,
    length: u64,
    cause: LossCause,
    boot: [u8; 16],
    times: (Option<i64>, Option<i64>),
) -> LostRange {
    LostRange {
        identity: (0, first),
        first: (0, first),
        last: (0, last),
        file_id,
        offset,
        length,
        cause,
        boot_id: boot,
        id_floor: 0,
        time_before: times.0,
        time_after: times.1,
        extends: false,
    }
}

fn plan(
    segs: &[Seg],
    mark: &MarkRead,
    boot: [u8; 16],
    sealed_last: Option<(u64, u64, i64)>,
    manifest_highest: u64,
) -> Plan {
    let in_use: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].in_use()).collect();
    // Segments in use below the last sealed one are left from a seal that did not finish: they are sealed again and
    // take no part in the position chain.
    let chain: Vec<usize> = in_use
        .iter()
        .copied()
        .filter(|&i| sealed_last.is_none_or(|s| segs[i].id.file_id > s.0))
        .collect();
    let last_in_use = in_use.last().copied();
    let mut kept: Vec<usize> = segs.iter().map(|s| s.batches.len()).collect();
    let mut zero = Vec::new();
    // Rule selection.
    let (rule, mark_slot) = match mark {
        MarkRead::Readable { slot, .. } => {
            let beyond = |i: usize, b: &B| {
                segs[i].id.file_id > slot.file_id
                    || (segs[i].id.file_id == slot.file_id && b.offset >= slot.offset)
            };
            let mut n: usize = chain
                .iter()
                .map(|&i| segs[i].batches.iter().filter(|b| beyond(i, b)).count())
                .sum();
            // A sealed segment holding positions above the mark: the mark cannot be current, as it moved past each
            // of that segment's batches before the seal started (Q-116 (j)).
            if sealed_last.is_some_and(|s| s.1 > slot.last_sequence) {
                n += 2;
            }
            let rule = if slot.boot_id != boot {
                StartRule::Reboot(MarkState::Readable)
            } else if n <= 1 {
                StartRule::SameBoot
            } else {
                StartRule::Reboot(MarkState::Stale)
            };
            if rule == StartRule::SameBoot {
                // Truncate everything after the mark: in the mark's segment from its offset, in every later segment
                // in use from the header's end.
                // A segment that is not the last in use holds no batch beyond a current mark, and may hold seal
                // blocks, which are never truncated (A.4).
                for &i in in_use.iter().filter(|&&i| Some(i) == last_in_use) {
                    let s = &segs[i];
                    if s.id.file_id < slot.file_id {
                        continue;
                    }
                    let from = if s.id.file_id == slot.file_id {
                        slot.offset.max(HEADER_LEN as u64)
                    } else {
                        HEADER_LEN as u64
                    };
                    kept[i] = s.batches.iter().filter(|b| b.end <= from).count();
                    if s.last_nonzero > from {
                        zero.push((i, from, s.last_nonzero));
                    }
                }
            }
            (rule, Some(*slot))
        }
        MarkRead::Missing => (StartRule::Reboot(MarkState::Missing), None),
        MarkRead::Unreadable => (StartRule::Reboot(MarkState::Unreadable), None),
    };
    // Losses: gaps in the kept positions (rule 2 regions and recovery-entry gaps), then the tail below the mark.
    let mut losses = Vec::new();
    let mut expect: Option<u64> = sealed_last.map(|(_, l, _)| l + 1);
    let mut prev: Option<(usize, u64, i64)> = None;
    let mut last_kept: Option<(usize, usize)> = None;
    for &i in &chain {
        let s = &segs[i];
        let mut e = expect.or(s.base()).unwrap_or(1);
        // A segment whose base is above the previous segment's last position lost positions between them (A.4,
        // rule 2), unless its first batch is a recovery entry, whose own gap is checked below.
        let starts_with_entry = s.batches.first().is_some_and(|b| b.recovery().is_some());
        if let Some(b) = s.base().filter(|_| !starts_with_entry) {
            if b > e {
                losses.push(lost(
                    e,
                    b - 1,
                    s.id.file_id,
                    HEADER_LEN as u64,
                    0,
                    LossCause::Corruption,
                    boot,
                    (prev.map(|p| p.2), None),
                ));
            }
            e = e.max(b);
        }
        for (j, b) in s.batches.iter().take(kept[i]).enumerate() {
            let from = match prev {
                Some((pi, end, _)) if pi == i => end,
                _ => HEADER_LEN as u64,
            };
            let times = (prev.map(|p| p.2), Some(b.commit_time));
            let gap_to = match b.recovery() {
                Some(r) => {
                    // Positions between K and F were recorded by the start that wrote the entry.
                    if r.after + 1 < b.first {
                        losses.push(lost(
                            r.after + 1,
                            b.first - 1,
                            s.id.file_id,
                            b.offset,
                            0,
                            LossCause::TruncatedTail,
                            boot,
                            times,
                        ));
                    }
                    r.after + 1
                }
                None => b.first,
            };
            if gap_to > e {
                losses.push(lost(
                    e,
                    gap_to - 1,
                    s.id.file_id,
                    from,
                    b.offset - from,
                    LossCause::Corruption,
                    boot,
                    times,
                ));
            }
            e = b.last + 1;
            prev = Some((i, b.end, b.commit_time));
            last_kept = Some((i, j));
        }
        expect = Some(e);
    }
    let kept_last = expect
        .map_or(0, |e| e - 1)
        .max(sealed_last.map_or(0, |s| s.1));
    // The tail below a valid mark of any boot: positions up to the mark's are lost (rule 2).
    let mut k = kept_last;
    if let Some(m) = mark_slot
        && m.last_sequence > kept_last
    {
        let (file, at) = match last_kept {
            Some((i, j)) => (segs[i].id.file_id, segs[i].batches[j].end),
            None => (m.file_id, HEADER_LEN as u64),
        };
        let length = if file == m.file_id {
            m.offset.saturating_sub(at)
        } else {
            0
        };
        losses.push(lost(
            kept_last + 1,
            m.last_sequence,
            file,
            at,
            length,
            LossCause::Corruption,
            boot,
            (prev.map(|p| p.2), None),
        ));
        k = m.last_sequence;
    }
    // Where writing resumes: the last segment in use, after its kept bytes, and after a lost region at its tail
    // below the mark, which is never overwritten.
    let resume_in = last_in_use;
    let mut write_offset = match (resume_in, last_kept) {
        (Some(r), Some((i, j))) if r == i => segs[i].batches[j].end,
        _ => HEADER_LEN as u64,
    };
    if let (Some(r), Some(m)) = (resume_in, mark_slot)
        && segs[r].id.file_id == m.file_id
    {
        write_offset = write_offset.max(m.offset);
    }
    let mut p = Plan {
        rule,
        resume_in,
        kept,
        zero,
        losses,
        k,
        tail: None,
        adopt: None,
        rewrite_last: None,
        write_offset,
        last_sequence: k,
    };
    if let StartRule::Reboot(state) = rule {
        // The torn tail of the last segment in use: from the write offset to the last non-zero byte.
        if let Some(r) = resume_in {
            let s = &segs[r];
            let end = s.last_nonzero.max(p.write_offset);
            let commits = s
                .tail_commits
                .iter()
                .filter(|(o, _)| *o >= p.write_offset)
                .count() as u32;
            p.tail = Some((r, p.write_offset, end, commits));
            // Adoption: the last kept batch is a recovery entry the mark does not cover, and nothing after it shows
            // a later write.
            if let Some((i, j)) = last_kept
                && i == r
            {
                let b = &s.batches[j];
                let covered = mark_slot.is_some_and(|m| m.last_sequence >= b.first);
                if let (Some(rec), false) = (b.recovery(), covered) {
                    // The tail it reported began where it was written; as the first batch of its segment it may have
                    // gone to a new segment, so then only its own length counts.
                    let reported = if j == 0 { 0 } else { rec.tail_bytes };
                    let within = end <= b.offset + reported.max(b.end - b.offset);
                    let later = s
                        .tail_commits
                        .iter()
                        .any(|(o, t)| *o >= b.end && *t >= b.commit_time);
                    if within && !later {
                        p.adopt = Some((i, j));
                        p.last_sequence = b.first;
                    }
                }
            }
        }
        if p.adopt.is_none() {
            p.last_sequence = 1 + k.max(manifest_highest);
        }
        // Q-87: with no readable mark of another boot, the last complete batch is written again before the sync; so
        // is an adopted entry, whose own sync may have failed (Q-117).
        if state != MarkState::Readable || p.adopt.is_some() {
            p.rewrite_last = last_kept;
        }
    } else {
        p.last_sequence = k.max(mark_slot.map_or(0, |m| m.last_sequence));
    }
    p
}

/// What a start returns before the writer is built.
pub(super) fn start(
    store: &StoreDir,
    cfg: &WriterConfig,
    boot_id: Option<[u8; 16]>,
    now: Duration,
    wall: i64,
    halt: &Arc<Halt>,
) -> Result<Resume, StoreError> {
    let fs = store.fs().clone();
    let log_dir = store.log_dir();
    let store_id = store.descriptor().store_id;
    let boot = boot_id.unwrap_or_default();
    let mut report = StartReport {
        rule: StartRule::FirstStart,
        truncated_bytes: 0,
        losses: Vec::new(),
        entry: None,
        rewrote_last_batch: false,
        headers_rewritten: Vec::new(),
        deleted: Vec::new(),
        to_seal: Vec::new(),
    };
    // Step 1: sync the log directory; read the manifest and the mark; scan every unsealed segment.
    let entries = match fs.read_dir(&log_dir) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    if !entries.is_empty()
        && let Err(e) = fs.sync_dir(&log_dir)
    {
        return Ok(read_only(fault(FaultOp::Sync, &log_dir, &e), report, halt));
    }
    let manifest_bytes = store.read_file(&store.dir().join(MANIFEST))?;
    let manifest = read_manifest(&manifest_bytes, &store_id).map_err(manifest_error)?;
    let mark_path = log_dir.join(MARK);
    let mark_bytes = match store.read_file(&mark_path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mark = match &mark_bytes {
        None => MarkRead::Missing,
        Some(b) => match read_mark(b, &store_id).map_err(StoreError::Unsupported)? {
            Some((index, slot)) => MarkRead::Readable { slot, index },
            None => MarkRead::Unreadable,
        },
    };
    let mut segs = Vec::new();
    let mut sealed: Vec<(SegmentName, PathBuf)> = Vec::new();
    let mut max_id = 0u64;
    for e in &entries {
        let Some(name) = SegmentName::parse(&e.name) else {
            continue;
        };
        max_id = max_id.max(name.file_id);
        let path = log_dir.join(&e.name);
        if name.sealed {
            sealed.push((name, path));
        } else {
            segs.push(scan_segment(store, name, path)?);
        }
    }
    segs.sort_by_key(|s| s.id.file_id);
    sealed.sort();
    if let MarkRead::Readable { slot, .. } = &mark {
        max_id = max_id.max(slot.file_id);
    }
    // The last sealed segment's last position and commit time.
    let sealed_last = match sealed.last() {
        Some((name, path)) => {
            let bytes = store.read_file(path)?;
            let id = SegmentIdentity {
                store_id,
                file_id: name.file_id,
                generation: name.generation,
            };
            let r = scan(&bytes, &id, ScanLimit::End).map_err(StoreError::Unsupported)?;
            // Without a valid trailer, its last complete batch.
            r.seal
                .map(|s| {
                    (
                        name.file_id,
                        s.trailer.last_sequence,
                        s.trailer.last_commit_time,
                    )
                })
                .or_else(|| {
                    r.batches
                        .last()
                        .map(|b| (name.file_id, b.commit.last_sequence, b.commit.commit_time))
                })
        }
        None => None,
    };
    let empty_log = segs.iter().all(|s| !s.in_use()) && sealed.is_empty();
    // Unused segments: the highest one of the configured size with a valid header is kept as created ahead.
    let unused: Vec<usize> = (0..segs.len()).filter(|&i| !segs[i].in_use()).collect();
    let keep = unused.iter().rev().copied().find(|&i| {
        let s = &segs[i];
        s.header.is_some() && s.len == cfg.segment_size && s.last_nonzero <= HEADER_LEN as u64
    });
    let last_in_use_id = segs
        .iter()
        .filter(|s| s.in_use())
        .map(|s| s.id.file_id)
        .max();
    let keep = keep.filter(|&i| last_in_use_id.is_none_or(|l| segs[i].id.file_id > l));
    // Unused segments not kept are deleted once the kept segments are synced (after step 3).
    let to_delete: Vec<usize> = unused
        .iter()
        .copied()
        .filter(|&i| Some(i) != keep)
        .collect();
    let ahead = match keep {
        Some(i) => {
            let s = &segs[i];
            let f = match fs.open(&s.path, OpenMode::ReadWrite) {
                Ok(f) => f,
                Err(e) => return Ok(read_only(fault(FaultOp::Write, &s.path, &e), report, halt)),
            };
            let h = s.header.expect("kept for its valid header");
            Some(AheadSegment {
                file: f,
                id: s.id,
                path: s.path.clone(),
                len: s.len,
                created_at: h.created_at,
            })
        }
        None => None,
    };
    let in_use_idx: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].in_use()).collect();

    // The first start of a new store, or a first start that stopped before its mark was readable.
    if empty_log && !matches!(mark, MarkRead::Readable { .. }) && manifest.highest_sequence(0) == 0
    {
        for &i in &to_delete {
            if let Err(e) = fs.remove_file(&segs[i].path) {
                return Ok(read_only(
                    fault(FaultOp::Write, &segs[i].path, &e),
                    report,
                    halt,
                ));
            }
            report.deleted.push(segs[i].id.file_id);
        }
        let slot = MarkSlot {
            generation: 0,
            store_id,
            boot_id: boot,
            counter: 1,
            file_id: 0,
            offset: 0,
            last_sequence: 0,
            max_series_id: 0,
        };
        let (file, created) = match mark_bytes {
            None => (fs.open(&mark_path, OpenMode::CreateNew)?, true),
            Some(_) => (fs.open(&mark_path, OpenMode::ReadWrite)?, false),
        };
        let r = file
            .write_at(&encode_mark_file([Some(slot), None]), 0)
            .and_then(|()| {
                if created {
                    file.sync_all()
                } else {
                    file.sync_data()
                }
            })
            .and_then(|()| fs.sync_dir(&log_dir));
        if let Err(e) = r {
            return Ok(read_only(
                fault(FaultOp::MarkSync, &mark_path, &e),
                report,
                halt,
            ));
        }
        return Ok(Resume {
            mark: Some(file),
            mark_slot: slot,
            mark_next_slot: 1,
            mark_dirty: false,
            open: None,
            ahead,
            seals: Vec::new(),
            next_file_id: max_id + 1,
            last_sequence: 0,
            last_commit_time: i64::MIN,
            max_series_id: 0,
            intervals: HashMap::new(),
            fault: None,
            report,
        });
    }

    let p = plan(
        &segs,
        &mark,
        boot,
        sealed_last,
        manifest.highest_sequence(0),
    );
    report.rule = p.rule;
    // What was found is reported even if the start cannot record it.
    report.losses = p
        .losses
        .iter()
        .map(|l| found(l, LossKind::DataLost, false))
        .collect();
    let ro = |f: Fault, report: StartReport| Ok(read_only(f, report, halt));

    // Headers of segments in use that are invalid or not yet taken into use, rewritten from the name and the first
    // batch (A.4).
    let mut header_rewritten = Vec::new();
    for &i in &in_use_idx {
        let s = &segs[i];
        if s.header.is_some_and(|h| h.base_sequence >= 1) {
            continue;
        }
        let first = s.batches[0].first;
        let mut h = FileHeader::new(
            kind::LOG_SEGMENT,
            s.id.generation,
            s.id.file_id,
            s.header.map_or(0, |h| h.created_at),
            store_id,
        );
        h.base_sequence = first;
        if let Err(e) = fs
            .open(&s.path, OpenMode::ReadWrite)
            .and_then(|f| f.write_at(&h.encode(), 0))
        {
            return ro(fault(FaultOp::Write, &s.path, &e), report);
        }
        header_rewritten.push(i);
        report.headers_rewritten.push(s.id.file_id);
    }

    // Open every segment in use.
    let mut files: BTreeMap<usize, Box<dyn FsFile>> = BTreeMap::new();
    for &i in &in_use_idx {
        match fs.open(&segs[i].path, OpenMode::ReadWrite) {
            Ok(f) => {
                files.insert(i, f);
            }
            Err(e) => return ro(fault(FaultOp::Write, &segs[i].path, &e), report),
        }
    }
    let zero_fill = |f: &dyn FsFile, from: u64, to: u64| -> io::Result<()> {
        // Truncation is zero-filling, from the end backwards (A.4).
        const CHUNK: u64 = 64 << 10;
        let mut end = to;
        while end > from {
            let start = end.saturating_sub(CHUNK).max(from);
            f.write_at(&vec![0u8; (end - start) as usize], start)?;
            end = start;
        }
        Ok(())
    };

    match p.rule {
        StartRule::SameBoot => {
            // Step 2: truncate after the mark; sync the truncated segments, those holding a lost range, and the
            // directory.
            for &(i, from, to) in &p.zero {
                if let Err(e) = zero_fill(&*files[&i], from, to) {
                    return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                }
                report.truncated_bytes += to - from;
            }
            let mut to_sync: Vec<usize> = p.zero.iter().map(|z| z.0).collect();
            to_sync.extend(header_rewritten.iter().copied());
            for l in &p.losses {
                if let Some(i) = segs.iter().position(|s| s.id.file_id == l.file_id) {
                    to_sync.push(i);
                }
            }
            to_sync.sort_unstable();
            to_sync.dedup();
            for i in to_sync {
                if let Some(f) = files.get(&i)
                    && let Err(e) = f.sync_all()
                {
                    return ro(fault(FaultOp::Sync, &segs[i].path, &e), report);
                }
            }
        }
        StartRule::Reboot(_) => {
            // Step 3: the last complete batch written again (Q-87), then the kept segments and the directory
            // synced.
            if let Some((i, j)) = p.rewrite_last {
                let b = &segs[i].batches[j];
                let mut r = files[&i].write_at(&b.bytes, b.offset);
                if r.is_ok()
                    && j == 0
                    && !header_rewritten.contains(&i)
                    && let Some(h) = segs[i].header
                {
                    r = files[&i].write_at(&h.encode(), 0);
                }
                if let Err(e) = r {
                    return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                }
                report.rewrote_last_batch = true;
            }
            for &i in &in_use_idx {
                if let Err(e) = files[&i].sync_all() {
                    return ro(fault(FaultOp::Sync, &segs[i].path, &e), report);
                }
            }
        }
        StartRule::FirstStart => unreachable!("handled above"),
    }
    if let Err(e) = fs.sync_dir(&log_dir) {
        return ro(fault(FaultOp::Sync, &log_dir, &e), report);
    }
    for &i in &to_delete {
        if let Err(e) = fs.remove_file(&segs[i].path) {
            return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
        }
        report.deleted.push(segs[i].id.file_id);
    }

    // The id floor: the mark's highest series id, and every id defined or used in the kept log.
    let mut id_floor = match &mark {
        MarkRead::Readable { slot, .. } => slot.max_series_id,
        _ => 0,
    };
    let mut intervals = HashMap::new();
    if matches!(p.rule, StartRule::Reboot(_)) {
        for s in &segs {
            id_floor = s.tail_ids.iter().copied().fold(id_floor, u32::max);
        }
    }
    for &i in &in_use_idx {
        for b in segs[i].batches.iter().take(p.kept[i]) {
            for item in &b.content.items {
                match item {
                    Item::Fact(r) => id_floor = id_floor.max(r.series_id),
                    Item::Entry(Entry::Definition(d)) => {
                        id_floor = id_floor.max(d.id);
                        if let Some(iv) = &d.interval {
                            intervals.insert(d.id, iv.clone());
                        }
                    }
                    Item::Entry(Entry::Recovery(r)) => id_floor = id_floor.max(r.id_floor),
                    _ => {}
                }
            }
        }
    }

    // Step 4: repair the manifest's tail and record the losses found.
    let mut losses = p.losses.clone();
    for l in &mut losses {
        l.id_floor = id_floor;
    }
    let mut epoch = manifest.last_epoch();
    let manifest_path = store.dir().join(MANIFEST);
    let mfile = match fs.open(&manifest_path, OpenMode::ReadWrite) {
        Ok(f) => f,
        Err(e) => return ro(fault(FaultOp::Write, &manifest_path, &e), report),
    };
    if let Err(e) = repair_manifest(&*mfile, &manifest_bytes, &manifest) {
        return ro(fault(FaultOp::Sync, &manifest_path, &e), report);
    }
    for (k, l) in losses.iter().enumerate() {
        match record_loss(&*mfile, &manifest, l, &mut epoch, wall, &manifest_path) {
            Ok(r) => report.losses[k].recorded = r,
            Err(f) => return ro(f, report),
        }
    }

    // Steps 5 and 6 (rule 3): write or adopt the recovery entry, zero the rest of the torn tail, write the mark.
    let open_idx = p.resume_in;
    let mut write_offset = p.write_offset;
    let mut ahead = ahead;
    let mut next_file_id = max_id + 1;
    let mut new_open: Option<(Box<dyn FsFile>, SegmentIdentity, u64, PathBuf)> = None;
    let mut entry_batch: Option<(u64, BatchContent, i64)> = None;
    let mut last_commit_time = segs
        .iter()
        .flat_map(|s| s.batches.iter().map(|b| b.commit_time))
        .chain(sealed_last.map(|s| s.2))
        .max()
        .unwrap_or(i64::MIN);
    let mark_slot;
    let mark_next_slot;
    let mark_file: Option<Box<dyn FsFile>>;
    let mut mark_dirty = false;
    if let StartRule::Reboot(state) = p.rule {
        let f_seq = p.last_sequence;
        if let Some((i, j)) = p.adopt {
            let b = &segs[i].batches[j];
            report.entry = Some(EntryReport {
                sequence: b.first,
                entry: b.recovery().cloned().expect("an adopted recovery entry"),
                adopted: true,
            });
            write_offset = b.end;
            if let Some((_, _, end, _)) = p.tail
                && end > b.end
            {
                if let Err(e) =
                    zero_fill(&*files[&i], b.end, end).and_then(|()| files[&i].sync_data())
                {
                    return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                }
                report.truncated_bytes += end - b.end;
            }
        } else {
            let (tail_bytes, readable) = p.tail.map_or((0, 0), |(_, s, e, c)| (e - s, c));
            let entry = Recovery {
                after: p.k,
                boot_id: boot,
                tail_bytes,
                readable_commits: readable,
                mark_state: state,
                id_floor,
                consumers: Vec::new(),
            };
            let commit_time = wall.max(last_commit_time);
            let content = BatchContent {
                first_sequence: f_seq,
                received_at: commit_time,
                items: vec![Item::Entry(Entry::Recovery(entry.clone()))],
            };
            let builder: BatchBuilder = content.builder().expect("a recovery entry encodes");
            let need = builder.data_len() + 64;
            let fits = open_idx.is_some_and(|i| segs[i].len >= write_offset + need);
            // In a new segment when the open one is full, or when no segment is in use.
            let (file, id, at, path): (&dyn FsFile, SegmentIdentity, u64, PathBuf) = if fits {
                let i = open_idx.expect("fits");
                // The recovery batch as the segment's first batch: its header is rewritten with F (A.4).
                if p.kept[i] == 0 {
                    let mut h = FileHeader::new(
                        kind::LOG_SEGMENT,
                        segs[i].id.generation,
                        segs[i].id.file_id,
                        segs[i].header.map_or(wall, |h| h.created_at),
                        store_id,
                    );
                    h.base_sequence = f_seq;
                    if let Err(e) = files[&i].write_at(&h.encode(), 0) {
                        return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                    }
                }
                (&*files[&i], segs[i].id, write_offset, segs[i].path.clone())
            } else {
                let a = match ahead.take() {
                    Some(a) => a,
                    None => {
                        let req = SegmentRequest {
                            id: SegmentIdentity {
                                store_id,
                                file_id: next_file_id,
                                generation: 0,
                            },
                            log_dir: log_dir.clone(),
                            len: cfg.segment_size,
                            created_at: wall,
                        };
                        next_file_id += 1;
                        match create_segment(&*fs, &req, halt) {
                            Ok(a) => a,
                            Err(CreateError::Fault(f)) => return ro(f, report),
                            Err(CreateError::NoSpace) | Err(CreateError::Halted) => {
                                let e = io::Error::new(
                                    io::ErrorKind::StorageFull,
                                    "no space for the recovery entry's segment",
                                );
                                return ro(fault(FaultOp::Create, &req.path(), &e), report);
                            }
                        }
                    }
                };
                let mut h = FileHeader::new(
                    kind::LOG_SEGMENT,
                    a.id.generation,
                    a.id.file_id,
                    a.created_at,
                    store_id,
                );
                h.base_sequence = f_seq;
                if let Err(e) = a.file.write_at(&h.encode(), 0) {
                    return ro(fault(FaultOp::Write, &a.path, &e), report);
                }
                new_open = Some((a.file, a.id, a.len, a.path));
                let n = new_open.as_ref().expect("just set");
                (&*n.0, n.1, HEADER_LEN as u64, n.3.clone())
            };
            let bytes = builder
                .encode(&id, at, commit_time)
                .expect("a recovery batch encodes");
            if let Err(e) = file.write_at(&bytes, at).and_then(|()| file.sync_data()) {
                return ro(fault(FaultOp::Sync, &path, &e), report);
            }
            // The rest of the torn tail is zeroed after the entry is on disk.
            if let Some((ti, ts, te, _)) = p.tail {
                let from = if fits { at + bytes.len() as u64 } else { ts };
                if te > from {
                    let r = zero_fill(&*files[&ti], from, te).and_then(|()| files[&ti].sync_data());
                    if let Err(e) = r {
                        return ro(fault(FaultOp::Write, &segs[ti].path, &e), report);
                    }
                    report.truncated_bytes += te - from;
                }
            }
            write_offset = at + bytes.len() as u64;
            last_commit_time = commit_time;
            entry_batch = Some((at, content, commit_time));
            if tail_bytes > 0 || readable > 0 {
                report.losses.push(FoundLoss {
                    kind: LossKind::PossibleLoss,
                    cause: LossCause::TruncatedTail,
                    identity: f_seq,
                    first: None,
                    last: None,
                    file_id: p.tail.map_or(0, |t| segs[t.0].id.file_id),
                    bytes: tail_bytes,
                    readable_commits: readable,
                    recorded: true,
                });
            }
            report.entry = Some(EntryReport {
                sequence: f_seq,
                entry,
                adopted: false,
            });
        }
        // Step 6: the mark, updated in place (or created), synced with the directory.
        let (entry_file, entry_end) = match &new_open {
            Some(n) => (n.1.file_id, write_offset),
            None => (open_idx.map_or(0, |i| segs[i].id.file_id), write_offset),
        };
        let (counter, slot_index) = match &mark {
            MarkRead::Readable { slot, index } => (slot.counter + 1, 1 - index),
            _ => {
                let best = mark_bytes
                    .as_deref()
                    .map(|b| read_slots(b, &store_id))
                    .and_then(|s| {
                        s.iter()
                            .enumerate()
                            .filter_map(|(i, r)| match r {
                                crate::mark::SlotRead::Valid(m) => Some((i, m.counter)),
                                _ => None,
                            })
                            .max_by_key(|x| x.1)
                    });
                best.map_or((1, 0), |(i, c)| (c + 1, 1 - i))
            }
        };
        mark_slot = MarkSlot {
            generation: 0,
            store_id,
            boot_id: boot,
            counter,
            file_id: entry_file,
            offset: entry_end,
            last_sequence: f_seq,
            max_series_id: id_floor,
        };
        let r = match mark_bytes {
            None => fs.open(&mark_path, OpenMode::CreateNew).and_then(|f| {
                let mut img = vec![0u8; MARK_LEN];
                img[..crate::mark::SLOT_LEN].copy_from_slice(&mark_slot.encode());
                f.write_at(&img, 0)?;
                f.sync_all()?;
                Ok(f)
            }),
            Some(_) => fs.open(&mark_path, OpenMode::ReadWrite).and_then(|f| {
                f.write_at(&mark_slot.encode(), SLOT_OFFSETS[slot_index] as u64)?;
                f.sync_data()?;
                Ok(f)
            }),
        };
        let r = r.and_then(|f| fs.sync_dir(&log_dir).map(|()| f));
        match r {
            Ok(f) => mark_file = Some(f),
            Err(e) => return ro(fault(FaultOp::MarkSync, &mark_path, &e), report),
        }
        mark_next_slot = if mark_bytes.is_none() {
            1
        } else {
            1 - slot_index
        };
        // Then the entry's gap, if any, is recorded.
        if report.entry.as_ref().is_some_and(|e| !e.adopted) && f_seq > p.k + 1 {
            let l = LostRange {
                id_floor,
                ..lost(
                    p.k + 1,
                    f_seq - 1,
                    entry_file,
                    0,
                    0,
                    LossCause::TruncatedTail,
                    boot,
                    (None, None),
                )
            };
            match record_loss(&*mfile, &manifest, &l, &mut epoch, wall, &manifest_path) {
                Ok(r) => report.losses.push(found(&l, LossKind::DataLost, r)),
                Err(f) => return ro(f, report),
            }
        }
    } else {
        let MarkRead::Readable { slot, index } = &mark else {
            unreachable!("rule 1 needs a readable mark")
        };
        mark_slot = *slot;
        mark_next_slot = 1 - index;
        mark_file = match fs.open(&mark_path, OpenMode::ReadWrite) {
            Ok(f) => Some(f),
            Err(e) => return ro(fault(FaultOp::MarkWrite, &mark_path, &e), report),
        };
        // The mark read on this boot may not be on stable storage yet: the timer syncs it.
        mark_dirty = true;
        id_floor = id_floor.max(slot.max_series_id);
    }

    // The segment the writer resumes in, and the segments in use before it, to be sealed again.
    let mut seals = Vec::new();
    let mut open = None;
    if let Some((file, id, len, _)) = new_open {
        let content = entry_batch.as_ref().map(|e| &e.1);
        open = Some(open_segment(
            file,
            id,
            len,
            write_offset,
            content
                .into_iter()
                .map(|c| (c, entry_batch.as_ref().map_or(0, |e| e.2), c.first_sequence)),
            &intervals,
            now,
        ));
    }
    for &i in &in_use_idx {
        let s = &segs[i];
        let mut batches: Vec<(&BatchContent, i64, u64)> = s
            .batches
            .iter()
            .take(p.kept[i])
            .map(|b| (&b.content, b.commit_time, b.first))
            .collect();
        let file = files.remove(&i).expect("opened");
        if open.is_none() && Some(i) == open_idx {
            if let Some((_, c, t)) = &entry_batch {
                batches.push((c, *t, c.first_sequence));
            }
            open = Some(open_segment(
                file,
                s.id,
                s.len,
                write_offset,
                batches.into_iter(),
                &intervals,
                now,
            ));
            continue;
        }
        let end = s.batches[..p.kept[i]].last().map(|b| b.end);
        let Some(end) = end else {
            // An empty segment is never sealed.
            continue;
        };
        let seg = open_segment(file, s.id, s.len, end, batches.into_iter(), &intervals, now);
        if let Some(summary) = seg.summary {
            report.to_seal.push(s.id.file_id);
            seals.push(SealJob {
                file: seg.file,
                id: seg.id,
                log_dir: log_dir.clone(),
                end,
                summary,
                records: seg.index.into_values().collect(),
                has_entries: seg.has_entries,
            });
        }
    }
    // A segment created ahead whose id is not above the one in use cannot be taken into use: removed.
    if let (Some(a), Some(o)) = (&ahead, &open)
        && a.id.file_id <= o.id.file_id
    {
        let path = a.path.clone();
        let id = a.id.file_id;
        ahead = None;
        if let Err(e) = fs.remove_file(&path) {
            return ro(fault(FaultOp::Write, &path, &e), report);
        }
        report.deleted.push(id);
    }
    Ok(Resume {
        mark: mark_file,
        mark_slot,
        mark_next_slot,
        mark_dirty,
        open,
        ahead,
        seals,
        next_file_id,
        last_sequence: p.last_sequence,
        last_commit_time,
        max_series_id: id_floor,
        intervals,
        fault: None,
        report,
    })
}

fn found(l: &LostRange, kind: LossKind, recorded: bool) -> FoundLoss {
    FoundLoss {
        kind,
        cause: l.cause,
        identity: l.identity.1,
        first: Some(l.first.1),
        last: Some(l.last.1),
        file_id: l.file_id,
        bytes: l.length,
        readable_commits: 0,
        recorded,
    }
}

/// A segment as the writer holds it, rebuilt from its kept batches.
fn open_segment<'a>(
    file: Box<dyn FsFile>,
    id: SegmentIdentity,
    len: u64,
    write_offset: u64,
    batches: impl Iterator<Item = (&'a BatchContent, i64, u64)>,
    intervals: &HashMap<u32, IntervalLength>,
    now: Duration,
) -> OpenSegment {
    let mut summary: Option<SealSummary> = None;
    let mut index: BTreeMap<u32, IndexRecord> = BTreeMap::new();
    let mut has_entries = false;
    for (c, t, first) in batches {
        let last = c.last_sequence();
        summary = Some(match summary {
            None => SealSummary {
                entry_count: last + 1 - first,
                first_sequence: first,
                last_sequence: last,
                first_commit_time: t,
                last_commit_time: t,
                batch_count: 1,
            },
            Some(s) => SealSummary {
                entry_count: s.entry_count + (last + 1 - first),
                last_sequence: last,
                last_commit_time: t,
                batch_count: s.batch_count + 1,
                ..s
            },
        });
        for item in &c.items {
            match item {
                Item::Fact(r) => {
                    let reach = reach(r.t, intervals.get(&r.series_id));
                    let rec = index.entry(r.series_id).or_insert(IndexRecord {
                        series_id: r.series_id,
                        fact_count: 0,
                        min_t: r.t,
                        max_t: reach,
                    });
                    rec.fact_count += 1;
                    rec.min_t = rec.min_t.min(r.t);
                    rec.max_t = rec.max_t.max(reach);
                }
                Item::Entry(_) => has_entries = true,
            }
        }
    }
    OpenSegment {
        file,
        id,
        len,
        write_offset,
        summary,
        has_entries,
        index,
        in_use_since: now,
    }
}

/// Step 4's repair: drop a torn last record and truncate after the last valid record, write the last valid record
/// again in place, and sync.
fn repair_manifest(f: &dyn FsFile, bytes: &[u8], m: &ManifestRead) -> io::Result<()> {
    if m.torn {
        f.set_len(m.valid_end)?;
    }
    if let Some(r) = m.records.last() {
        f.write_at(&bytes[r.offset as usize..r.end as usize], r.offset)?;
    }
    if m.torn { f.sync_all() } else { f.sync_data() }
}

/// Appends a lost-range record unless one holds every position of `l`; returns whether a record holds it.
fn record_loss(
    f: &dyn FsFile,
    m: &ManifestRead,
    l: &LostRange,
    epoch: &mut u64,
    wall: i64,
    path: &std::path::Path,
) -> Result<bool, Fault> {
    let recorded: Vec<&LostRange> = m.lost_ranges().collect();
    let overlap = recorded
        .iter()
        .find(|r| r.first.1 <= l.last.1 && l.first.1 <= r.last.1);
    let mut rec = l.clone();
    if let Some(r) = overlap {
        // Recorded already, or extended under the recorded loss's identity.
        let lo = recorded
            .iter()
            .filter(|x| x.identity == r.identity)
            .map(|x| x.first.1)
            .min()
            .unwrap_or(r.first.1);
        let hi = recorded
            .iter()
            .filter(|x| x.identity == r.identity)
            .map(|x| x.last.1)
            .max()
            .unwrap_or(r.last.1);
        if l.first.1 >= lo && l.last.1 <= hi {
            return Ok(true);
        }
        rec.identity = r.identity;
        rec.first = (0, l.first.1.min(lo));
        rec.last = (0, l.last.1.max(hi));
        rec.extends = true;
    }
    *epoch += 1;
    let bytes = encode_record(RECORD_LOST_RANGE, *epoch, 0, wall, Some(&rec));
    let at = f.len().map_err(|e| fault(FaultOp::Write, path, &e))?;
    f.write_at(&bytes, at)
        .and_then(|()| f.sync_data())
        .map_err(|e| fault(FaultOp::Sync, path, &e))?;
    Ok(true)
}

fn read_only(f: Fault, report: StartReport, halt: &Halt) -> Resume {
    halt.set(f.clone());
    Resume {
        mark: None,
        mark_slot: MarkSlot {
            generation: 0,
            store_id: [0; 16],
            boot_id: [0; 16],
            counter: 0,
            file_id: 0,
            offset: 0,
            last_sequence: 0,
            max_series_id: 0,
        },
        mark_next_slot: 0,
        mark_dirty: false,
        open: None,
        ahead: None,
        seals: Vec::new(),
        next_file_id: 0,
        last_sequence: 0,
        last_commit_time: i64::MIN,
        max_series_id: 0,
        intervals: HashMap::new(),
        fault: Some(f),
        report,
    }
}
