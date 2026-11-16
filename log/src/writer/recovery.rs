//! The start of a writer (`spec.md` §3 Recovery rules 1 to 3 and Order of a start; `format.md` A.4 states recovery
//! may find, A.10): the first start of a new store creates the mark; any other start scans the unsealed segments,
//! decides by the mark and the boot identity what is kept, truncates, records losses in the manifest, writes or
//! adopts a recovery entry, writes the mark, and hands the writer the state it resumes from.
//!
//! Decisions are made from what was read before anything is written (`plan`); `start` then performs them in the
//! order of a start. A failed sync at any step leaves the writer read-only and writes nothing more.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
use crate::mark::{
    MARK_LEN, MarkSlot, ResetStamp, SLOT_OFFSETS, encode_mark_file, read_mark, read_slots,
};
use crate::name::SegmentName;
use crate::scan::{HeaderState, ScanLimit, ScanReport, Tail, scan, scan_ignoring_seal};
use crate::segment::SealSummary;
use crate::store::{LEASES_DIR, MANIFEST, MARK, StoreDir, StoreError};

use super::generation::{Gen, scan_sealed};
use super::maint::{AheadSegment, CreateError, SealJob, SegmentRequest, create_segment};
use super::{Fault, FaultOp, Halt, OpenSegment, StartOptions, WriterConfig, reach};

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
    /// Seal blocks that hold to the structural rule were found and bound the scan.
    sealed: bool,
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

/// Whether the seal blocks a scan found hold to the structural rule of A.4: they start exactly at the end of the last
/// complete batch, and the trailer's `batch_count` and `last_sequence` equal the scan's. Seal blocks written by the
/// sealer always do; a forgery inside a data block cannot, as its block header and prefix come first (Q-109).
fn seal_holds(r: &ScanReport<'_>) -> bool {
    let Some(seal) = &r.seal else {
        return false;
    };
    let end = r.batches.last().map_or(HEADER_LEN as u64, |b| b.end());
    seal.start == end
        && seal.trailer.batch_count == r.batches.len() as u64
        && Some(seal.trailer.last_sequence) == r.last_sequence()
}

/// Scans an unsealed segment. Seal blocks that fail [`seal_holds`], and any seal blocks when `ignore_seal` is set
/// (the last segment in use, where a seal is never expected), are read as the bytes of whatever precedes them, so
/// that a torn tail, its commit blocks and its definitions are seen (Q-109).
fn scan_segment(
    store: &StoreDir,
    name: SegmentName,
    path: PathBuf,
    ignore_seal: bool,
) -> Result<Seg, StoreError> {
    let bytes = store.read_file(&path)?;
    let id = SegmentIdentity {
        store_id: store.descriptor().store_id,
        file_id: name.file_id,
        generation: name.generation,
    };
    let mut r = scan(&bytes, &id, ScanLimit::End).map_err(StoreError::Unsupported)?;
    let mut sealed = r.seal.is_some();
    if sealed && (ignore_seal || !seal_holds(&r)) {
        r = scan_ignoring_seal(&bytes, &id).map_err(StoreError::Unsupported)?;
        sealed = false;
    }
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
        sealed,
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
    /// Rule 1 truncates a complete batch: a recovery entry is written as under rule 3, a reset point
    /// (`spec.md` §3 Recovery rule 1).
    reset: bool,
    /// Batches superseded by a recovery entry in a later segment whose `after` is below them: a reset that wrote its
    /// entry in a new segment and stopped before zeroing them. They are zeroed once the entry is on disk.
    superseded: Vec<(usize, usize)>,
    /// Unused segments beyond the mark's segment (any, with no readable mark) with non-zero bytes after their header:
    /// part of the entry's tail, deleted once an entry that reports them is on disk (Q-125, Q-320).
    unused_dirty: Vec<usize>,
    /// Their bytes after the header, and the commit blocks still readable in them.
    unused_tail: (u64, u32),
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
        stamp: None,
    }
}

fn plan(
    segs: &[Seg],
    mark: &MarkRead,
    boot: [u8; 16],
    sealed_last: Option<(u64, u64, i64)>,
    no_sealed: bool,
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
    let mut reset = false;
    // Complete batches beyond a readable mark of the current boot.
    // Rule 1: a recovery entry beyond the mark written by an earlier start that stopped before its mark (Q-117).
    let mut adopt_beyond: Option<(usize, usize)> = None;
    // Beyond a readable mark every unused segment with non-zero bytes after its header is reported, wherever segments
    // in use lie (a start cut after taking a new segment into use for its entry leaves one above it); with no
    // readable mark, every such segment (Q-125, Q-320). In the mark's segment the mark's position makes it a rule 2
    // loss; below it, the gap rule's (Q-321).
    let mark_file = match mark {
        MarkRead::Readable { slot, .. } => Some(slot.file_id),
        _ => None,
    };
    let unused_dirty: Vec<usize> = (0..segs.len())
        .filter(|&i| {
            !segs[i].in_use()
                && mark_file.is_none_or(|f| segs[i].id.file_id > f)
                && segs[i].last_nonzero > HEADER_LEN as u64
        })
        .collect();
    let unused_tail = unused_dirty.iter().fold((0u64, 0u32), |(b, c), &i| {
        let n = u32::try_from(segs[i].tail_commits.len()).unwrap_or(u32::MAX);
        (
            b.saturating_add(segs[i].last_nonzero - HEADER_LEN as u64),
            c.saturating_add(n),
        )
    });
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
                // blocks, which are never truncated (A.4). One exception: the mark's segment holds a complete batch
                // or, with no valid seal blocks, any non-zero byte beyond the mark, and every later segment in use
                // holds only its header, as a start leaves that took the next segment into use for its recovery
                // entry and stopped before writing it, or a remount within the boot. A batch beyond the mark rules
                // out seal blocks there (the mark passes every batch of a segment before its seal).
                let only_headers_after = |i: usize| {
                    in_use
                        .iter()
                        .filter(|&&j| segs[j].id.file_id > segs[i].id.file_id)
                        .all(|&j| segs[j].batches.is_empty())
                };
                let targets: Vec<usize> = in_use
                    .iter()
                    .copied()
                    .filter(|&i| {
                        Some(i) == last_in_use
                            || (segs[i].id.file_id == slot.file_id
                                && (segs[i].batches.iter().any(|b| beyond(i, b))
                                    || !segs[i].sealed
                                        && segs[i].last_nonzero
                                            > slot.offset.max(HEADER_LEN as u64))
                                && only_headers_after(i))
                    })
                    .collect();
                let from_of = |i: usize| {
                    if segs[i].id.file_id == slot.file_id {
                        slot.offset.max(HEADER_LEN as u64)
                    } else {
                        HEADER_LEN as u64
                    }
                };
                // A complete batch beyond the mark, or any non-zero byte there (a batch whose bytes were damaged
                // after it was acknowledged), may have been acknowledged (a mark sync lost with the page cache within
                // the boot): its truncation is a reset point (`spec.md` §3 Recovery rule 1, Q-125).
                reset = n == 1
                    || !unused_dirty.is_empty()
                    || targets.iter().any(|&i| {
                        segs[i].id.file_id >= slot.file_id && segs[i].last_nonzero > from_of(i)
                    });
                for i in targets {
                    let s = &segs[i];
                    if s.id.file_id < slot.file_id {
                        continue;
                    }
                    let from = from_of(i);
                    kept[i] = s.batches.iter().filter(|b| b.end <= from).count();
                    // The one complete batch beyond the mark is a recovery entry after the mark's position, written
                    // at the write position by a start that stopped before its mark, and nothing after it shows a
                    // later write: adopted, as under rule 3.
                    if reset
                        && let Some(b) = s.batches.get(kept[i])
                        && let Some(rec) = b.recovery()
                        && rec.after == slot.last_sequence
                        && !unused_dirty
                            .iter()
                            .any(|&u| segs[u].id.file_id > segs[i].id.file_id)
                        && b.offset == from
                        && s.last_nonzero <= b.offset + rec.tail_bytes.max(b.end - b.offset)
                        && !s
                            .tail_commits
                            .iter()
                            .any(|(o, t)| *o >= b.end && *t >= b.commit_time)
                    {
                        adopt_beyond = Some((i, kept[i]));
                        kept[i] += 1;
                    }
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
    // With no sealed segment the log of generation 0 starts at position 1, so a lowest segment damaged, zeroed or
    // missing leaves a gap below the first one kept (Q-321).
    let log_start = chain
        .first()
        .filter(|&&i| no_sealed && segs[i].id.generation == 0)
        .map(|_| 1);
    let mut expect: Option<u64> = sealed_last.map(|(_, l, _)| l + 1).or(log_start);
    let mut prev: Option<(usize, u64, i64)> = None;
    let mut last_kept: Option<(usize, usize)> = None;
    let mut kept_order: Vec<(usize, usize, u64)> = Vec::new();
    let mut superseded: Vec<(usize, usize)> = Vec::new();
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
            // A recovery entry, first in its segment, whose `after` is below exactly one batch kept before it, that
            // batch the last of its own segment: a rule 1 reset that wrote its entry in a new segment and stopped
            // before zeroing the batch it truncated (`spec.md` §3 Recovery rule 1). That batch is dropped, and what
            // precedes the entry is the batch before it.
            if let Some(r) = b.recovery()
                && j == 0
                && r.after + 1 < e
                && let Some(&(pi, pj, plast)) = kept_order.last()
                && pi != i
                && kept[pi] == pj + 1
                && plast > r.after
                && kept_order
                    .len()
                    .checked_sub(2)
                    .is_none_or(|x| kept_order[x].2 <= r.after)
            {
                kept_order.pop();
                kept[pi] = pj;
                superseded.push((pi, pj));
                e = r.after + 1;
                prev = kept_order.last().map(|&(qi, qj, _)| {
                    let q = &segs[qi].batches[qj];
                    (qi, q.end, q.commit_time)
                });
            }
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
            kept_order.push((i, j, b.last));
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
        reset,
        superseded,
        unused_dirty,
        unused_tail,
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
                // An unused segment with bytes in a later file may have been written after the entry by the writer,
                // with the mark lost or its current slot damaged: not adopted, so that a new entry reports those
                // bytes (Q-320).
                let written_after = p
                    .unused_dirty
                    .iter()
                    .any(|&u| segs[u].id.file_id > segs[i].id.file_id);
                if let (Some(rec), false, false) = (b.recovery(), covered, written_after) {
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
        // is a recovery entry the mark does not cover, adopted or not, whose own sync may have failed (Q-117).
        let uncovered_entry = last_kept.is_some_and(|(i, j)| {
            let b = &segs[i].batches[j];
            b.recovery().is_some() && mark_slot.is_none_or(|m| m.last_sequence < b.first)
        });
        if state != MarkState::Readable || uncovered_entry {
            p.rewrite_last = last_kept;
        }
    } else if p.reset {
        // The entry is written over the truncated bytes, and only then are the rest zeroed (steps 5 and 6), so a start
        // that stops before the entry is on disk leaves the batch for the next start to find. The zero-fill range,
        // in the segment the writer resumes in, is the tail the entry reports.
        // Under rule 1 `tail` only counts: every zero-fill range of `zero` is the reported tail.
        let total: u64 = p.zero.iter().map(|z| z.2 - z.1).sum();
        let seg = p.zero.first().map(|z| z.0).or(resume_in);
        if let Some((i, j)) = adopt_beyond {
            let b = &segs[i].batches[j];
            p.adopt = Some((i, j));
            p.rewrite_last = Some((i, j));
            p.write_offset = b.end;
            p.last_sequence = b.first;
            p.tail = seg.map(|r| (r, 0, total, 0));
        } else {
            // The readable commit blocks in the truncated bytes, as rule 3 counts them: one per complete batch,
            // and those still readable in a torn tail.
            let commits: usize = p
                .zero
                .iter()
                .map(|&(i, from, _)| {
                    segs[i].batches.iter().filter(|b| b.offset >= from).count()
                        + segs[i]
                            .tail_commits
                            .iter()
                            .filter(|(o, _)| *o >= from)
                            .count()
                })
                .sum();
            p.tail = seg.map(|r| (r, 0, total, u32::try_from(commits).unwrap_or(u32::MAX)));
            p.last_sequence = 1 + k.max(manifest_highest);
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
    opts: &mut StartOptions,
) -> Result<Resume, StoreError> {
    let generation = Gen::of(opts.generation.as_ref())?;
    let g = generation.g;
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
    // The registry file (A.16) is a derived file: its `through` is a source of F, and every id it names counts toward
    // the id floor (`spec.md` §2.1, §3 Recovery rule 3).
    let registry = crate::registry::read_latest(&*fs, store.dir(), &store_id)
        .map_err(StoreError::Unsupported)?
        .map(|(_, f)| f)
        .filter(|f| f.generation == g);
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
    if let MarkRead::Readable { slot, .. } = &mark {
        generation.check(slot.generation, false)?;
    }
    let mut segs = Vec::new();
    let mut sealed: Vec<(SegmentName, PathBuf)> = Vec::new();
    let mut max_id = 0u64;
    for e in &entries {
        let Some(name) = SegmentName::parse(&e.name) else {
            continue;
        };
        generation.check(name.generation, name.sealed)?;
        max_id = max_id.max(name.file_id);
        let path = log_dir.join(&e.name);
        if name.sealed {
            sealed.push((name, path));
        } else {
            segs.push(scan_segment(store, name, path, false)?);
        }
    }
    segs.sort_by_key(|s| s.id.file_id);
    // In the last segment in use a seal is never expected (A.4 Sealing): one found there is read as tail bytes.
    if let Some(last) = segs.iter_mut().rfind(|s| s.in_use())
        && last.sealed
    {
        let name = SegmentName {
            generation: last.id.generation,
            file_id: last.id.file_id,
            sealed: false,
        };
        *last = scan_segment(store, name, last.path.clone(), true)?;
    }
    sealed.sort();
    if let MarkRead::Readable { slot, .. } = &mark {
        max_id = max_id.max(slot.file_id);
    }
    // Sealed segments of earlier generations take no part in this generation's chain (Q-261).
    let own_sealed: Vec<&(SegmentName, PathBuf)> =
        sealed.iter().filter(|(n, _)| n.generation == g).collect();
    // The last sealed segment's last position and commit time.
    let sealed_last = match own_sealed.last() {
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
    let empty_log = segs.iter().all(|s| !s.in_use()) && own_sealed.is_empty();
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
    // Bytes in an unused segment are reported by the recovery entry of rule 3 first (Q-320).
    let unused_written = unused
        .iter()
        .any(|&i| segs[i].last_nonzero > HEADER_LEN as u64);
    // Routing changes, ids and intervals sealed above the registry file's `through` (Q-130), in earlier generations
    // too, past what is set aside (Q-262). The file is written at a start, a clean shutdown and each routing change,
    // so after a crash this reads every segment sealed since the last of those (possibly the whole previous run), and
    // with no registry file of this generation every sealed segment; the cost is Q-116 (n).
    let sealed_scan = scan_sealed(
        store,
        &sealed,
        registry.as_ref().map(|f| (g, f.through)),
        &generation,
    );
    let first_mark = match &mark {
        MarkRead::Readable { slot, .. } => generation.is_first_mark(slot),
        _ => true,
    };
    if empty_log && !unused_written && first_mark && manifest.highest_sequence(g) == 0 {
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
        let mut intervals = HashMap::new();
        let mut floor = sealed_scan.id_floor;
        if let Some(f) = &registry {
            floor = floor.max(f.floor());
            for (_, _, e) in &f.entries {
                if let Entry::Definition(d) = e
                    && let Some(iv) = &d.interval
                {
                    intervals.insert(d.id, iv.clone());
                }
            }
        }
        intervals.extend(sealed_scan.intervals.iter().cloned());
        let slot = generation.first_mark(store_id, boot, floor);
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
            last_sequence: slot.last_sequence,
            last_commit_time: i64::MIN,
            max_series_id: floor,
            intervals,
            fault: None,
            report,
        });
    }

    // The other sources of F (`sources`): the registry file, cursor files. The consumers named by routing changes
    // tell a missing cursor file from one that never existed.
    let mut consumers = BTreeSet::new();
    let routing = registry
        .iter()
        .flat_map(|f| f.entries.iter().map(|(_, _, e)| e))
        .chain(segs.iter().flat_map(|s| {
            s.batches.iter().flat_map(|b| {
                b.content.items.iter().filter_map(|i| match i {
                    Item::Entry(e) => Some(e),
                    Item::Fact(_) => None,
                })
            })
        }));
    for e in routing {
        if let Entry::Routing(r) = e {
            consumers.insert(r.consumer.clone());
        }
    }
    consumers.extend(sealed_scan.consumers.iter().cloned());
    let sourced = super::sources::read_all(&super::sources::SourceContext {
        fs: &*fs,
        store_dir: store.dir(),
        store_id,
        generation: g,
        consumers: &consumers,
    })
    .map_err(StoreError::Unsupported)?;
    // F is at or above the generation's first position.
    let mut p = plan(
        &segs,
        &mark,
        boot,
        sealed_last,
        own_sealed.is_empty(),
        manifest
            .highest_sequence(g)
            .max(sourced.highest)
            .max(generation.first - 1),
    );
    for l in &mut p.losses {
        generation.place(l);
    }
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

    // Zero-fills `ranges` once a recovery entry is on disk, from past the entry in its own segment, and syncs each
    // segment zeroed; returns the bytes zeroed.
    let zero_after = |ranges: &[(usize, u64, u64)],
                      entry_seg: Option<usize>,
                      entry_end: u64|
     -> Result<u64, (usize, io::Error)> {
        let mut n = 0;
        for &(i, from, to) in ranges {
            let from = if Some(i) == entry_seg {
                from.max(entry_end)
            } else {
                from
            };
            if to > from {
                zero_fill(&*files[&i], from, to)
                    .and_then(|()| files[&i].sync_data())
                    .map_err(|e| (i, e))?;
                n += to - from;
            }
        }
        Ok(n)
    };
    // Batches superseded by an entry a stopped start wrote in a later segment (`Plan::superseded`).
    let zero_superseded = || {
        let ranges: Vec<(usize, u64, u64)> = p
            .superseded
            .iter()
            .map(|&(i, j)| (i, segs[i].batches[j].offset, segs[i].batches[j].end))
            .collect();
        zero_after(&ranges, None, 0)
    };

    // The header of the segment the writer resumes in is written again in place before its first sync: a header
    // taken into use by a sync that failed may be in the page cache only (Q-116 (p)). A header rewritten above, or
    // with the last batch below, is not written twice.
    if let Some(r) = p.resume_in
        && !header_rewritten.contains(&r)
        && p.rewrite_last.is_none_or(|(i, j)| i != r || j != 0)
        && let Some(h) = segs[r].header.filter(|h| h.base_sequence >= 1)
        && let Err(e) = files[&r].write_at(&h.encode(), 0)
    {
        return ro(fault(FaultOp::Write, &segs[r].path, &e), report);
    }

    match p.rule {
        StartRule::SameBoot => {
            // Step 2: truncate after the mark; sync the truncated segments, those holding a lost range, and the
            // directory. At a reset point (a complete batch or any non-zero byte beyond the mark) nothing is zeroed
            // before the recovery entry is written (steps 5 and 6); an adopted entry is written again in place
            // (Q-117).
            if !p.reset {
                for &(i, from, to) in &p.zero {
                    if let Err(e) = zero_fill(&*files[&i], from, to) {
                        return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                    }
                    report.truncated_bytes += to - from;
                }
            }
            let mut to_sync: Vec<usize> = if p.reset {
                Vec::new()
            } else {
                p.zero.iter().map(|z| z.0).collect()
            };
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
                to_sync.push(i);
            }
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
    for &i in to_delete.iter().filter(|i| !p.unused_dirty.contains(i)) {
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
    // The registry file holds definitions from sealed segments too: the segment index's reach for every series
    // (Q-115 (f)); the kept log's own definitions follow and override.
    if let Some(f) = &registry {
        id_floor = id_floor.max(f.floor());
        for (_, _, e) in &f.entries {
            if let Entry::Definition(d) = e
                && let Some(iv) = &d.interval
            {
                intervals.insert(d.id, iv.clone());
            }
        }
    }
    id_floor = id_floor.max(sealed_scan.id_floor);
    intervals.extend(sealed_scan.intervals.iter().cloned());
    if matches!(p.rule, StartRule::Reboot(_)) {
        for s in &segs {
            id_floor = s.tail_ids.iter().copied().fold(id_floor, u32::max);
        }
    }
    // A batch superseded by a stopped reset's entry may have been acknowledged too.
    for &(i, j) in &p.superseded {
        for item in &segs[i].batches[j].content.items {
            match item {
                Item::Fact(r) => id_floor = id_floor.max(r.series_id),
                Item::Entry(Entry::Definition(d)) => id_floor = id_floor.max(d.id),
                _ => {}
            }
        }
    }
    // A complete batch truncated by rule 1 may have been acknowledged: its ids stay assigned.
    if p.reset {
        for &i in &in_use_idx {
            for b in segs[i].batches.iter().skip(p.kept[i]) {
                for item in &b.content.items {
                    match item {
                        Item::Fact(r) => id_floor = id_floor.max(r.series_id),
                        Item::Entry(Entry::Definition(d)) => id_floor = id_floor.max(d.id),
                        _ => {}
                    }
                }
            }
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

    // The reset stamp in force, which each loss recorded here carries (Q-124 (c)): the mark's, or with the mark
    // lost that of the last kept recovery entry; none known otherwise.
    let stamp_in_force = match &mark {
        MarkRead::Readable { slot, .. } => Some(slot.stamp),
        _ => in_use_idx
            .iter()
            .rev()
            .flat_map(|&i| segs[i].batches.iter().take(p.kept[i]).rev())
            .find_map(|b| b.recovery().and_then(|r| r.stamp)),
    };
    // Step 4: repair the manifest's tail and record the losses found.
    let mut losses = p.losses.clone();
    for l in &mut losses {
        l.id_floor = id_floor;
        l.stamp = stamp_in_force;
    }
    // New epochs continue above every epoch named by the last valid record and by any lease file (§7 Manifest).
    let leases = store.dir().join(LEASES_DIR);
    let mut epoch = manifest
        .last_epoch()
        .max(crate::lease::highest_epoch(&*fs, &leases, &store_id)?);
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
    let mark_dirty = false;
    let entry_state = match p.rule {
        StartRule::Reboot(state) => Some(state),
        StartRule::SameBoot if p.reset => Some(MarkState::Readable),
        _ => None,
    };
    let stamp;
    if let Some(state) = entry_state {
        let f_seq = p.last_sequence;
        if let Some((i, j)) = p.adopt {
            let b = &segs[i].batches[j];
            let rec = b.recovery().cloned().expect("an adopted recovery entry");
            // An entry written before the stamp existed gets a fresh one in the mark (`spec.md` §7 Readers).
            stamp = rec.stamp.unwrap_or_else(|| ResetStamp::new_at(b.first));
            report.entry = Some(EntryReport {
                sequence: b.first,
                entry: rec,
                adopted: true,
            });
            write_offset = b.end;
            if p.reset {
                match zero_after(&p.zero, Some(i), b.end) {
                    Ok(n) => report.truncated_bytes += n,
                    Err((i, e)) => return ro(fault(FaultOp::Write, &segs[i].path, &e), report),
                }
            } else if let Some((_, _, end, _)) = p.tail
                && end > b.end
            {
                if let Err(e) =
                    zero_fill(&*files[&i], b.end, end).and_then(|()| files[&i].sync_data())
                {
                    return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                }
                report.truncated_bytes += end - b.end;
            }
            match zero_superseded() {
                Ok(n) => report.truncated_bytes += n,
                Err((i, e)) => return ro(fault(FaultOp::Write, &segs[i].path, &e), report),
            }
        } else {
            let (tail_bytes, readable) = p.tail.map_or((0, 0), |(_, s, e, c)| (e - s, c));
            let (tail_bytes, readable) = (
                tail_bytes.saturating_add(p.unused_tail.0),
                readable.saturating_add(p.unused_tail.1),
            );
            stamp = ResetStamp::new_at(f_seq);
            let entry = Recovery {
                after: p.k,
                stamp: Some(stamp),
                boot_id: boot,
                tail_bytes,
                readable_commits: readable,
                mark_state: state,
                id_floor,
                consumers: sourced.named.clone(),
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
            let mut header_after: Option<FileHeader> = None;
            // In a new segment when the open one is full, or when no segment is in use.
            let (file, id, at, path): (&dyn FsFile, SegmentIdentity, u64, PathBuf) = if fits {
                let i = open_idx.expect("fits");
                // The recovery batch as the segment's first batch: its header is rewritten with F (A.4), after the
                // entry, so that no stop leaves a segment in use by its header alone.
                if p.kept[i] == 0 {
                    let mut h = FileHeader::new(
                        kind::LOG_SEGMENT,
                        segs[i].id.generation,
                        segs[i].id.file_id,
                        segs[i].header.map_or(wall, |h| h.created_at),
                        store_id,
                    );
                    h.base_sequence = f_seq;
                    header_after = Some(h);
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
                                generation: g,
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
                header_after = Some(h);
                new_open = Some((a.file, a.id, a.len, a.path));
                let n = new_open.as_ref().expect("just set");
                (&*n.0, n.1, HEADER_LEN as u64, n.3.clone())
            };
            let bytes = builder
                .encode(&id, at, commit_time)
                .expect("a recovery batch encodes");
            let r = file.write_at(&bytes, at).and_then(|()| match header_after {
                Some(h) => file.write_at(&h.encode(), 0),
                None => Ok(()),
            });
            if let Err(e) = r.and_then(|()| file.sync_data()) {
                return ro(fault(FaultOp::Sync, &path, &e), report);
            }
            // The rest of the torn tail, or the bytes rule 1 truncates, are zeroed after the entry is on disk.
            let entry_seg = if fits { open_idx } else { None };
            if p.reset {
                if let Err((i, e)) = zero_after(&p.zero, entry_seg, at + bytes.len() as u64) {
                    return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
                }
                // Every byte after the mark is truncated, those the entry covers included.
                report.truncated_bytes += p.zero.iter().map(|z| z.2 - z.1).sum::<u64>();
            } else if let Some((ti, ts, te, _)) = p.tail {
                let from = if fits { at + bytes.len() as u64 } else { ts };
                if te > from {
                    let r = zero_fill(&*files[&ti], from, te).and_then(|()| files[&ti].sync_data());
                    if let Err(e) = r {
                        return ro(fault(FaultOp::Write, &segs[ti].path, &e), report);
                    }
                    report.truncated_bytes += te - from;
                }
            }
            match zero_superseded() {
                Ok(n) => report.truncated_bytes += n,
                Err((i, e)) => return ro(fault(FaultOp::Write, &segs[i].path, &e), report),
            }
            write_offset = at + bytes.len() as u64;
            last_commit_time = commit_time;
            entry_batch = Some((at, content, commit_time));
            // A possible loss: a non-empty tail, a mark not readable or stale, or a missing or unreadable cursor
            // file (`spec.md` §11 Health; Q-124 (a)); its identity is the entry's position (Q-124 (b)).
            if tail_bytes > 0
                || readable > 0
                || state != MarkState::Readable
                || !entry.consumers.is_empty()
            {
                report.losses.push(FoundLoss {
                    kind: LossKind::PossibleLoss,
                    cause: LossCause::TruncatedTail,
                    identity: f_seq,
                    first: None,
                    last: None,
                    file_id: p
                        .tail
                        .map(|t| segs[t.0].id.file_id)
                        .or(p.unused_dirty.first().map(|&i| segs[i].id.file_id))
                        .unwrap_or(0),
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
        for &i in &p.unused_dirty {
            if let Err(e) = fs.remove_file(&segs[i].path) {
                return ro(fault(FaultOp::Write, &segs[i].path, &e), report);
            }
            report.deleted.push(segs[i].id.file_id);
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
            generation: g,
            store_id,
            boot_id: boot,
            counter,
            file_id: entry_file,
            offset: entry_end,
            last_sequence: f_seq,
            max_series_id: id_floor,
            stamp,
        };
        let r = match &mark_bytes {
            None => fs.open(&mark_path, OpenMode::CreateNew).and_then(|f| {
                let mut img = vec![0u8; MARK_LEN];
                img[..crate::mark::SLOT_LEN].copy_from_slice(&mark_slot.encode());
                f.write_at(&img, 0)?;
                f.sync_all()?;
                Ok(f)
            }),
            // The file as read is written again in place and synced first: either slot may hold an update whose
            // sync failed or never ran, and the slot written next must not be the last durable mark (A.10 Update).
            Some(read) => fs.open(&mark_path, OpenMode::ReadWrite).and_then(|f| {
                f.write_at(read, 0)?;
                f.sync_data()?;
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
            let mut l = LostRange {
                id_floor,
                stamp: Some(stamp),
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
            generation.place(&mut l);
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
        // The mark read on this boot may not be on stable storage: its slot is written again in place, as read, and
        // synced, since bytes whose earlier sync failed may no longer be dirty (compare Q-87). The slot it is in then
        // holds the last synced mark, and updates go to the other (A.10 Update).
        let at = SLOT_OFFSETS[*index];
        let read = mark_bytes.as_deref().expect("a readable mark was read");
        let f = match fs.open(&mark_path, OpenMode::ReadWrite) {
            Ok(f) => f,
            Err(e) => return ro(fault(FaultOp::MarkWrite, &mark_path, &e), report),
        };
        if let Err(e) = f.write_at(&read[at..at + crate::mark::SLOT_LEN], at as u64) {
            return ro(fault(FaultOp::MarkWrite, &mark_path, &e), report);
        }
        if let Err(e) = f.sync_data() {
            return ro(fault(FaultOp::MarkSync, &mark_path, &e), report);
        }
        mark_file = Some(f);
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
    // Positions, not bare sequences: a restore records losses of earlier generations here (Q-261).
    let overlap = recorded
        .iter()
        .find(|r| r.first <= l.last && l.first <= r.last);
    let mut rec = l.clone();
    if let Some(r) = overlap {
        // Recorded already, or extended under the recorded loss's identity.
        let lo = recorded
            .iter()
            .filter(|x| x.identity == r.identity)
            .map(|x| x.first)
            .min()
            .unwrap_or(r.first);
        let hi = recorded
            .iter()
            .filter(|x| x.identity == r.identity)
            .map(|x| x.last)
            .max()
            .unwrap_or(r.last);
        if l.first >= lo && l.last <= hi {
            return Ok(true);
        }
        rec.identity = r.identity;
        rec.first = l.first.min(lo);
        rec.last = l.last.max(hi);
        rec.extends = true;
    }
    // A lease may name any epoch: with none left above it, the start stays read-only rather than wrapping.
    *epoch = epoch.checked_add(1).ok_or_else(|| {
        fault(
            FaultOp::Write,
            path,
            &io::Error::other("no manifest epoch is left above the manifest's and the leases'"),
        )
    })?;
    let bytes = encode_record(RECORD_LOST_RANGE, *epoch, rec.first.0, wall, Some(&rec));
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
            stamp: ResetStamp::default(),
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
