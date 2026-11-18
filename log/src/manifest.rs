//! The manifest's framing (`format.md` A.14) as far as the log's start needs it (`spec.md` §3 Order of a start,
//! step 4; §7 Manifest): reading its records, telling a torn last record from a corrupt one, and the empty and
//! lost-range records. Publish, restore, quarantine and rebuild records belong to later roadmap items; their typed
//! layouts are in the `kvik-files` crate, and this reader passes the publish-and-retire, sealed-segment and reserved
//! (restore, quarantine, tier-move) records over as field lists, taking only their epochs.

use std::collections::BTreeSet;

use crate::block::{BLOCK_HEADER_LEN, MAX_BODY_LEN, encode_block, read_block, read_block_with};
use crate::crc::PrefixCrc;
use crate::error::{Element, FormatError};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::scan::{FailedReads, ScanStats};

/// An empty record.
pub const RECORD_EMPTY: u8 = 0x20;
/// A publish-and-retire record (laid out by roadmap 6.3).
pub const RECORD_PUBLISH: u8 = 0x21;
/// A sealed-log-segment record (laid out by roadmap 6.3).
pub const RECORD_SEALED: u8 = 0x22;
/// A restore record (laid out by roadmap 8.3); read as a field list until then (`format.md` A.14).
pub const RECORD_RESTORE: u8 = 0x25;
/// A quarantine record (laid out by roadmap 8.1); read as a field list until then.
pub const RECORD_QUARANTINE: u8 = 0x26;
/// A tier-move record (laid out by roadmap 8.1); read as a field list until then.
pub const RECORD_TIER_MOVE: u8 = 0x27;
/// A lost-range record.
pub const RECORD_LOST_RANGE: u8 = 0x23;
/// An acknowledgement record.
pub const RECORD_ACKNOWLEDGEMENT: u8 = 0x24;
/// A rebuild record.
pub const RECORD_REBUILD: u8 = 0x28;
/// Bytes of a record body before its field list.
const RECORD_PREFIX: usize = 24;
/// The bytes of a restore record's field 9 before its file description (`format.md` A.14 `0x25`).
const RESTORED_FILE_FIXED: usize = 44;

/// A position as written in a record: `(generation, sequence)`, 12 bytes.
pub type RecordPosition = (u32, u64);

/// Why a loss happened (A.14, lost-range field 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LossCause {
    /// Damage below the mark or before a complete batch (`spec.md` §3 Recovery rule 2).
    Corruption = 1,
    /// Positions between the last kept position and a recovery entry (rule 3).
    TruncatedTail = 2,
    /// A file lost in the bucket.
    LostInBucket = 3,
    /// A checksum-valid block whose content fails its checks.
    InvalidContent = 4,
    /// Found again by a manifest rebuild with no cause given by a valid record or a recovery entry (`spec.md` §7
    /// Manifest).
    Unknown = 5,
}

impl LossCause {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Self::Corruption,
            2 => Self::TruncatedTail,
            3 => Self::LostInBucket,
            4 => Self::InvalidContent,
            5 => Self::Unknown,
            _ => return None,
        })
    }
}

/// A lost-range record's fields (A.14).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LostRange {
    /// 1: the loss's identity, the first position of its range when it was first reported.
    pub identity: RecordPosition,
    /// 2: the first lost position.
    pub first: RecordPosition,
    /// 3: the last lost position.
    pub last: RecordPosition,
    /// 4: the segment.
    pub file_id: u64,
    /// 5: byte offset.
    pub offset: u64,
    /// 6: byte length.
    pub length: u64,
    /// 7: the cause.
    pub cause: LossCause,
    /// 8: the boot id of the start that found it.
    pub boot_id: [u8; 16],
    /// 9: the id floor.
    pub id_floor: u32,
    /// 10: the commit time before the loss.
    pub time_before: Option<i64>,
    /// 11: the commit time after it (absent at the tail).
    pub time_after: Option<i64>,
    /// 12: the record extends the loss named by its identity.
    pub extends: bool,
    /// 13: the reset stamp in force when the start recorded it (`spec.md` §7 Readers): the mark's, or with the mark
    /// lost that of the last kept recovery entry; absent when none was known, which counts as later than every
    /// reset (Q-124 (c)).
    pub stamp: Option<crate::mark::ResetStamp>,
}

/// One valid record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Offset of its block header.
    pub offset: u64,
    /// Just past its block, padding included.
    pub end: u64,
    /// Its type.
    pub record_type: u8,
    /// Its epoch.
    pub epoch: u64,
    /// The generation it was written in.
    pub generation: u32,
    /// A lost range, for a record of that type.
    pub lost: Option<LostRange>,
}

/// A manifest as read at a start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRead {
    /// The valid records, in order.
    pub records: Vec<Record>,
    /// Just past the last valid record.
    pub valid_end: u64,
    /// Whether anything but zeros, or any byte at all, follows the last valid record: a torn tail to drop.
    pub torn: bool,
}

/// Why a manifest cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// No valid header, or one of another kind or store.
    #[error("the manifest's header is invalid")]
    Header,
    /// An invalid record followed by a valid one: it was synced, so it is corrupt. Also a search that passed part of a
    /// window under the scan bound's window rule and found no valid record after (Q-650). Fetching it again or
    /// rebuilding the manifest is roadmap 6.3 (Q-116 (a)).
    #[error("the manifest record at offset {0} is corrupt")]
    Corrupt(u64),
    /// A version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

/// The fields of a field list, in order; [`fields_fit`] first tells whether every field fits.
fn fields(body: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let h = body.get(at..at.checked_add(4)?)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        let v = body.get(at + 4..at + 4 + len)?;
        at = (at + 4 + len).div_ceil(4) * 4;
        Some((id, v))
    })
}

/// Whether every field of a field list fits in it, read without keeping them (Q-651).
fn fields_fit(body: &[u8]) -> bool {
    let mut at = 0usize;
    while at < body.len() {
        let Some(h) = body.get(at..at + 4) else {
            return false;
        };
        let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        if body.get(at + 4..at + 4 + len).is_none() {
            return false;
        }
        at = (at + 4 + len).div_ceil(4) * 4;
    }
    true
}

fn position(v: &[u8]) -> Option<RecordPosition> {
    (v.len() == 12).then(|| {
        (
            u32::from_le_bytes(v[..4].try_into().unwrap_or_default()),
            u64::from_le_bytes(v[4..].try_into().unwrap_or_default()),
        )
    })
}

fn u64_of(v: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(v.try_into().ok()?))
}

/// A lost range from a field list that fits, each field's first occurrence taken, in one pass (Q-651).
fn decode_lost(list: &[u8]) -> Option<LostRange> {
    let mut first: [Option<&[u8]>; 14] = [None; 14];
    for (id, v) in fields(list) {
        if let Some(slot) = first.get_mut(usize::from(id)) {
            slot.get_or_insert(v);
        }
    }
    let get = |id: u16| first[usize::from(id)];
    Some(LostRange {
        identity: position(get(1)?)?,
        first: position(get(2)?)?,
        last: position(get(3)?)?,
        file_id: u64_of(get(4)?)?,
        offset: u64_of(get(5)?)?,
        length: u64_of(get(6)?)?,
        cause: LossCause::from_code(*get(7)?.first()?)?,
        boot_id: get(8)?.try_into().ok()?,
        id_floor: u32::from_le_bytes(get(9)?.try_into().ok()?),
        time_before: get(10).and_then(u64_of).map(|t| t as i64),
        time_after: get(11).and_then(u64_of).map(|t| t as i64),
        extends: get(12).is_some(),
        stamp: get(13).and_then(crate::mark::ResetStamp::decode),
    })
}

/// The record reads of one manifest read: body checksums from a prefix table asked at rising offsets, so a read
/// checksums at most twice the file (`format.md` A, common rules: scan bound). `bytes` start at file offset `base`;
/// the failed reads' windows are in file offsets.
struct Search<'a> {
    bytes: &'a [u8],
    base: u64,
    sums: PrefixCrc<'a>,
    headers: u64,
    body: u64,
    failed: FailedReads,
}

impl<'a> Search<'a> {
    fn new(bytes: &'a [u8], base: u64) -> Self {
        Self {
            bytes,
            base,
            sums: PrefixCrc::new(bytes),
            headers: 0,
            body: 0,
            failed: FailedReads::new(u64::from(MAX_BODY_LEN)),
        }
    }

    fn stats(&self) -> ScanStats {
        ScanStats {
            bytes_checksummed: self.sums.checksummed(),
            headers_visited: self.headers,
            header_checksums: self.headers,
            bytes_body_checked: self.body,
            failed_bytes_peak: self.failed.peak(),
        }
    }

    /// The record at `at`, if it is a valid record whose epoch is above `min_epoch`.
    fn record_at(&mut self, at: u64, min_epoch: u64) -> Result<Option<Record>, ManifestError> {
        let bytes = self.bytes;
        let sums = &mut self.sums;
        sums.forget_below(at);
        let crc = |start, body: &[u8]| {
            sums.range(start, body.len() as u64)
                .unwrap_or_else(|| crate::crc::crc32c(body))
        };
        self.headers += 1;
        let Ok(b) = read_block_with(bytes, at, bytes.len() as u64, crc) else {
            return Ok(None);
        };
        let t = b.header.block_type;
        let body = b.body;
        if body.len() < RECORD_PREFIX {
            return Ok(None);
        }
        let epoch = u64::from_le_bytes(body[..8].try_into().unwrap_or_default());
        if epoch <= min_epoch {
            return Ok(None);
        }
        let generation = u32::from_le_bytes(body[8..12].try_into().unwrap_or_default());
        self.body += body.len() as u64;
        let list = &body[RECORD_PREFIX..];
        if !fields_fit(list) {
            self.failed.charge(self.base + at, body.len() as u64);
            return Ok(None);
        }
        let lost = match t {
            RECORD_LOST_RANGE => match decode_lost(list) {
                Some(l) => Some(l),
                None => {
                    self.failed.charge(self.base + at, body.len() as u64);
                    return Ok(None);
                }
            },
            RECORD_EMPTY
            | RECORD_PUBLISH
            | RECORD_SEALED
            | RECORD_ACKNOWLEDGEMENT
            | RECORD_REBUILD
            | RECORD_RESTORE
            | RECORD_QUARANTINE
            | RECORD_TIER_MOVE => None,
            _ if b.header.skippable() => None,
            _ => {
                return Err(ManifestError::Unsupported(FormatError::Unsupported {
                    offset: self.base + at,
                    element: Element::BlockType(t),
                }));
            }
        };
        Ok(Some(Record {
            offset: at,
            end: b.next_offset(),
            record_type: t,
            epoch,
            generation,
            lost,
        }))
    }
}

/// Reads a manifest (A.14, `spec.md` §7 Manifest): its records up to the first invalid one; an invalid record
/// followed, at an 8-byte boundary, by a valid record of a higher epoch is corrupt.
pub fn read_manifest(bytes: &[u8], store_id: &[u8; 16]) -> Result<ManifestRead, ManifestError> {
    read_manifest_with_stats(bytes, store_id).0
}

/// [`read_manifest`], with the work it did; [`ScanStats::within_manifest_bound`] holds for every input (`format.md` A, common
/// rules: scan bound).
pub fn read_manifest_with_stats(
    bytes: &[u8],
    store_id: &[u8; 16],
) -> (Result<ManifestRead, ManifestError>, ScanStats) {
    let mut search = Search::new(&bytes[HEADER_LEN.min(bytes.len())..], HEADER_LEN as u64);
    let r = read_all(bytes, store_id, &mut search);
    (r, search.stats())
}

fn read_all(
    bytes: &[u8],
    store_id: &[u8; 16],
    search: &mut Search<'_>,
) -> Result<ManifestRead, ManifestError> {
    let h = FileHeader::decode(bytes).ok_or(ManifestError::Header)?;
    if h.format_major > crate::FORMAT_MAJOR {
        return Err(ManifestError::Unsupported(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(h.format_major),
        }));
    }
    if h.kind != kind::MANIFEST || &h.store_id != store_id {
        return Err(ManifestError::Header);
    }
    let (records, valid_end) = records_in(search, 0)?;
    Ok(ManifestRead {
        valid_end,
        torn: (bytes.len() as u64) > valid_end,
        records,
    })
}

/// Reads the records in `tail`, the bytes of a manifest from file offset `base` (the end of a valid record, or
/// the header's end) to its end, as [`read_manifest`] reads them past that point: each valid with an epoch above
/// the one before, the first above `last_epoch`. Returns the records, with file offsets, and the file offset just
/// past the last; an invalid record followed by a valid one of a higher epoch is corrupt (`spec.md` §7 Manifest).
pub fn read_records(
    tail: &[u8],
    base: u64,
    last_epoch: u64,
) -> Result<(Vec<Record>, u64), ManifestError> {
    read_records_with_stats(tail, base, last_epoch).0
}

/// [`read_records`], with the work it did; the failed reads' budget applies per window of file offsets.
pub fn read_records_with_stats(
    tail: &[u8],
    base: u64,
    last_epoch: u64,
) -> (Result<(Vec<Record>, u64), ManifestError>, ScanStats) {
    let mut search = Search::new(tail, base);
    let r = records_in(&mut search, last_epoch);
    (r, search.stats())
}

fn records_in(
    search: &mut Search<'_>,
    last_epoch: u64,
) -> Result<(Vec<Record>, u64), ManifestError> {
    let len = search.bytes.len() as u64;
    let base = search.base;
    let mut records: Vec<Record> = Vec::new();
    let mut at = 0u64;
    while at < len {
        let min = records.last().map_or(last_epoch, |r| r.epoch);
        match search.record_at(at, min)? {
            Some(r) => {
                at = r.end;
                records.push(r);
            }
            None => break,
        }
    }
    let valid_end = at.min(len);
    // Search forward for a valid record of a higher epoch: if there is one, the invalid record was synced. Past
    // the failed reads' budget of a window, the rest of it is passed as damaged (`format.md` A, common rules: scan
    // bound).
    let min = records.last().map_or(last_epoch, |r| r.epoch);
    let mut probe = valid_end + 8;
    let mut passed = false;
    while probe + (BLOCK_HEADER_LEN as u64) <= len {
        let next = search.failed.next_from(base + probe) - base;
        if next != probe {
            passed = true;
            probe = next;
            continue;
        }
        if search.record_at(probe, min)?.is_some() {
            return Err(ManifestError::Corrupt(base + valid_end));
        }
        probe += 8;
    }
    // Bytes the search did not look at are not known to be a torn record: corrupt, with no later record (Q-650).
    if passed {
        return Err(ManifestError::Corrupt(base + valid_end));
    }
    for r in &mut records {
        r.offset += base;
        r.end += base;
    }
    Ok((records, base + valid_end))
}

fn push_field(out: &mut Vec<u8>, id: u16, value: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

fn pos_bytes(p: RecordPosition) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..4].copy_from_slice(&p.0.to_le_bytes());
    b[4..].copy_from_slice(&p.1.to_le_bytes());
    b
}

/// The bytes of a record block.
pub fn encode_record(
    record_type: u8,
    epoch: u64,
    generation: u32,
    written_at: i64,
    lost: Option<&LostRange>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&epoch.to_le_bytes());
    body.extend_from_slice(&generation.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&written_at.to_le_bytes());
    if let Some(l) = lost {
        push_field(&mut body, 1, &pos_bytes(l.identity));
        push_field(&mut body, 2, &pos_bytes(l.first));
        push_field(&mut body, 3, &pos_bytes(l.last));
        push_field(&mut body, 4, &l.file_id.to_le_bytes());
        push_field(&mut body, 5, &l.offset.to_le_bytes());
        push_field(&mut body, 6, &l.length.to_le_bytes());
        push_field(&mut body, 7, &[l.cause as u8]);
        push_field(&mut body, 8, &l.boot_id);
        push_field(&mut body, 9, &l.id_floor.to_le_bytes());
        if let Some(t) = l.time_before {
            push_field(&mut body, 10, &t.to_le_bytes());
        }
        if let Some(t) = l.time_after {
            push_field(&mut body, 11, &t.to_le_bytes());
        }
        if l.extends {
            push_field(&mut body, 12, &[]);
        }
        if let Some(stamp) = l.stamp {
            push_field(&mut body, 13, &stamp.encode());
        }
    }
    let mut out = Vec::new();
    // A record body this short always fits a block.
    let _ = encode_block(record_type, 0, &body, &mut out);
    out
}

/// A manifest holding its header and one empty record of epoch 1 (A.12 creation; Q-115 (g)).
pub fn encode_initial(store_id: [u8; 16], created_at: i64) -> Vec<u8> {
    let mut out = FileHeader::new(kind::MANIFEST, 0, 0, created_at, store_id)
        .encode()
        .to_vec();
    out.extend(encode_record(RECORD_EMPTY, 1, 0, created_at, None));
    out
}

/// The generation and file id of each log segment that valid records name (`format.md` A.14): field 1 of a
/// sealed-segment record, fields 1 and 2 of a publish-and-retire record and field 9 of a restore record (past its
/// tier, copy size and copy hash), each a file description of kind 1 (Q-221 (b)). A value too short to hold a kind,
/// generation and `file_id` names nothing.
pub fn listed_segments(bytes: &[u8], read: &ManifestRead) -> BTreeSet<(u32, u64)> {
    let mut out = BTreeSet::new();
    for r in &read.records {
        let (named, skip): (&[u16], usize) = match r.record_type {
            RECORD_SEALED => (&[1], 0),
            RECORD_PUBLISH => (&[1, 2], 0),
            RECORD_RESTORE => (&[9], RESTORED_FILE_FIXED),
            _ => continue,
        };
        let Ok(b) = read_block(bytes, r.offset, bytes.len() as u64) else {
            continue;
        };
        let Some(list) = b.body.get(RECORD_PREFIX..).filter(|l| fields_fit(l)) else {
            continue;
        };
        for (_, v) in fields(list).filter(|(id, _)| named.contains(id)) {
            let Some(v) = v.get(skip..) else {
                continue;
            };
            if v.first() == Some(&kind::LOG_SEGMENT)
                && let Some(g) = v.get(4..8).and_then(|b| b.try_into().ok())
                && let Some(id) = v.get(8..16).and_then(u64_of)
            {
                out.insert((u32::from_le_bytes(g), id));
            }
        }
    }
    out
}

impl ManifestRead {
    /// The lost ranges recorded.
    pub fn lost_ranges(&self) -> impl Iterator<Item = &LostRange> {
        self.records.iter().filter_map(|r| r.lost.as_ref())
    }

    /// The last epoch.
    pub fn last_epoch(&self) -> u64 {
        self.records.last().map_or(0, |r| r.epoch)
    }

    /// The highest sequence of generation `g` named by a record (`spec.md` §3 Recovery rule 3, F).
    pub fn highest_sequence(&self, g: u32) -> u64 {
        self.lost_ranges()
            .flat_map(|l| [l.identity, l.first, l.last])
            .filter(|p| p.0 == g)
            .map(|p| p.1)
            .max()
            .unwrap_or(0)
    }
}

/// Steps 1, 4 and 6 of a start's manifest work (`spec.md` §3 Order of a start), so that the store can route them
/// through its own manifest writer.
pub trait Recorder: Send {
    /// Step 1: reads the manifest.
    fn read(&mut self, store_id: &[u8; 16]) -> Result<ManifestRead, RecorderError>;
    /// Step 4: drops a torn tail, writes the last valid record again in place and syncs.
    fn repair(&mut self) -> Result<(), RecorderError>;
    /// Steps 4 and 6: appends a lost-range record with an epoch above `epoch_floor`, syncs, and returns its epoch;
    /// `None` when a record already holds every position of `loss`.
    fn record_loss(
        &mut self,
        loss: &LostRange,
        epoch_floor: u64,
        wall: i64,
    ) -> Result<Option<u64>, RecorderError>;
}

/// Why a [`Recorder`] call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecorderError {
    /// The manifest cannot be used.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// A write failed.
    #[error("manifest write failed: {0}")]
    Write(std::io::Error),
    /// A sync failed.
    #[error("manifest sync failed: {0}")]
    Sync(std::io::Error),
    /// No epoch is left above the floor.
    #[error("no manifest epoch is left")]
    EpochsExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lost(first: u64, last: u64) -> LostRange {
        LostRange {
            identity: (0, first),
            first: (0, first),
            last: (0, last),
            file_id: 1,
            offset: 64,
            length: 128,
            cause: LossCause::Corruption,
            boot_id: [1; 16],
            id_floor: 3,
            time_before: Some(5),
            time_after: None,
            extends: false,
            stamp: None,
        }
    }

    #[test]
    fn records_round_trip_and_a_torn_tail_is_told_from_a_corrupt_record() {
        let mut m = encode_initial([2; 16], 0);
        m.extend(encode_record(RECORD_LOST_RANGE, 2, 0, 1, Some(&lost(4, 9))));
        let r = read_manifest(&m, &[2; 16]).unwrap();
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.records[1].lost, Some(lost(4, 9)));
        assert!(!r.torn);
        assert_eq!(r.highest_sequence(0), 9);
        // A torn last record.
        let mut torn = m.clone();
        let third = encode_record(RECORD_EMPTY, 3, 0, 2, None);
        torn.extend(&third[..20]);
        let r = read_manifest(&torn, &[2; 16]).unwrap();
        assert_eq!(
            (r.records.len(), r.valid_end, r.torn),
            (2, m.len() as u64, true)
        );
        // A damaged record followed by a valid one is corrupt.
        let mut bad = m.clone();
        bad.extend(&third);
        bad.extend(encode_record(RECORD_EMPTY, 4, 0, 3, None));
        let at = m.len() + 20;
        bad[at] ^= 1;
        assert_eq!(
            read_manifest(&bad, &[2; 16]),
            Err(ManifestError::Corrupt(m.len() as u64))
        );
        // An unknown critical record type is a refusal.
        let mut unk = m.clone();
        let mut x = Vec::new();
        encode_block(0x30, 0, &[3u8; 24], &mut x).unwrap();
        x[16..24].copy_from_slice(&5u64.to_le_bytes());
        let mut y = Vec::new();
        encode_block(0x30, 0, &x[16..40], &mut y).unwrap();
        unk.extend(y);
        assert!(matches!(
            read_manifest(&unk, &[2; 16]),
            Err(ManifestError::Unsupported(_))
        ));
    }

    /// Q-651: the start's reader takes a lost-range field's first occurrence, in one pass, as it did field by field:
    /// a valid identity then an invalid one is read, an invalid one then a valid one is a corrupt record.
    #[test]
    fn a_lost_range_field_given_twice_is_read_from_its_first_occurrence() {
        let with_identity_twice = |first: &[u8], second: &[u8]| {
            let record = encode_record(RECORD_LOST_RANGE, 2, 0, 1, Some(&lost(4, 9)));
            let body = &record[16..];
            // The prefix, field 1, a second field 1, then the rest.
            let mut again = body[..24].to_vec();
            push_field(&mut again, 1, first);
            push_field(&mut again, 1, second);
            again.extend_from_slice(&body[24 + 16..]);
            let mut m = encode_initial([2; 16], 0);
            encode_block(RECORD_LOST_RANGE, 0, &again, &mut m).unwrap();
            m.extend(encode_record(RECORD_EMPTY, 3, 0, 2, None));
            read_manifest(&m, &[2; 16])
        };
        let valid = pos_bytes((0, 4));
        let r = with_identity_twice(&valid, &[1, 2, 3]).unwrap();
        assert_eq!(r.records.len(), 3);
        assert_eq!(r.records[1].lost, Some(lost(4, 9)));
        assert!(matches!(
            with_identity_twice(&[1, 2, 3], &valid),
            Err(ManifestError::Corrupt(_))
        ));
        assert_eq!(
            with_identity_twice(&valid, &pos_bytes((0, 5)))
                .unwrap()
                .records[1]
                .lost
                .as_ref()
                .map(|l| l.identity),
            Some((0, 4))
        );
    }

    #[test]
    fn reserved_and_later_record_types_are_passed_over_with_their_epochs() {
        let mut m = encode_initial([2; 16], 0);
        for (t, e) in [
            (RECORD_PUBLISH, 2u64),
            (RECORD_SEALED, 3),
            (RECORD_RESTORE, 4),
            (RECORD_QUARANTINE, 5),
            (RECORD_TIER_MOVE, 6),
        ] {
            let mut body = Vec::new();
            body.extend_from_slice(&e.to_le_bytes());
            body.extend_from_slice(&[0; 16]);
            body.extend_from_slice(&[1, 0, 4, 0, 9, 9, 9, 9]);
            encode_block(t, 0, &body, &mut m).unwrap();
        }
        let r = read_manifest(&m, &[2; 16]).unwrap();
        assert_eq!(r.last_epoch(), 6);
        assert_eq!(r.records.len(), 6);
        assert!(r.records.iter().all(|x| x.lost.is_none()));
    }

    /// `n` block headers 16 bytes apart after a damaged word, each claiming a body that runs to the end of the file
    /// with a wrong checksum: every one is read by the forward search.
    fn nested_records(n: u64) -> Vec<u8> {
        let mut f = FileHeader::new(kind::MANIFEST, 0, 1, 0, [2; 16])
            .encode()
            .to_vec();
        f.extend_from_slice(&[0xFF; 8]);
        let base = f.len() as u64;
        let len = base + 16 * n + 64;
        for i in 0..n {
            let o = base + 16 * i;
            let h = crate::block::BlockHeader {
                block_type: 0x50,
                flags: crate::block::FLAG_SKIPPABLE,
                body_len: (len - o - 16) as u32,
                body_crc32c: 0xDEAD_BEEF,
            };
            f.extend_from_slice(&h.encode());
        }
        f.resize(len as usize, 0x11);
        f
    }

    #[test]
    fn the_forward_search_over_nested_records_checksums_linear_bytes() {
        for n in [500u64, 1_000, 2_000] {
            let f = nested_records(n);
            let len = f.len() as u64;
            let bound = 2 * len + 12 * (len / 8);
            // Each candidate's body checksummed directly, as before the prefix table.
            let direct: u64 = (0..n).map(|i| len - 88 - 16 * i).sum();
            assert!(direct > bound);
            crate::crc::BYTES_HASHED.with(|c| c.set(0));
            let r = read_manifest(&f, &[2; 16]).unwrap();
            let hashed = crate::crc::BYTES_HASHED.with(|c| c.get());
            assert!(r.records.is_empty() && r.torn);
            // Twice the file in bodies, and a 12-byte header checksum per 8-byte step.
            assert!(hashed <= bound, "{hashed} bytes hashed for {len}");
        }
    }

    #[test]
    fn records_of_a_lower_epoch_have_no_fields_parsed() {
        let mut m = encode_initial([2; 16], 0);
        m.extend(encode_record(RECORD_EMPTY, 9, 0, 1, None));
        let kept = m.len() as u64 - HEADER_LEN as u64 - 2 * BLOCK_HEADER_LEN as u64;
        m.extend_from_slice(&[0xFF; 8]);
        let mut body = 3u64.to_le_bytes().to_vec();
        body.resize(4_096, 0);
        for _ in 0..50 {
            encode_block(0x50, crate::block::FLAG_SKIPPABLE, &body, &mut m).unwrap();
        }
        let (r, stats) = read_manifest_with_stats(&m, &[2; 16]);
        let r = r.unwrap();
        assert!(r.torn);
        assert_eq!(stats.bytes_body_checked, kept);
        // The records and the damaged word, then one header per step of the forward search.
        let steps = (m.len() as u64 - 16 - (r.valid_end + 8)) / 8 + 1;
        assert_eq!(stats.headers_visited, 3 + steps);
    }

    #[test]
    fn a_header_of_a_newer_major_another_kind_or_another_store_is_refused() {
        let record = encode_record(RECORD_EMPTY, 1, 0, 0, None);
        let with = |h: FileHeader| [h.encode().to_vec(), record.clone()].concat();
        let mut newer = FileHeader::new(kind::MANIFEST, 0, 0, 0, [2; 16]);
        newer.format_major = crate::FORMAT_MAJOR + 1;
        assert!(matches!(
            read_manifest(&with(newer), &[2; 16]),
            Err(ManifestError::Unsupported(_))
        ));
        let segment = FileHeader::new(kind::LOG_SEGMENT, 0, 0, 0, [2; 16]);
        assert_eq!(
            read_manifest(&with(segment), &[2; 16]),
            Err(ManifestError::Header)
        );
        let other = FileHeader::new(kind::MANIFEST, 0, 0, 0, [3; 16]);
        assert_eq!(
            read_manifest(&with(other), &[2; 16]),
            Err(ManifestError::Header)
        );
        // A whole manifest is read with one header per record and no search.
        let (r, stats) = read_manifest_with_stats(
            &with(FileHeader::new(kind::MANIFEST, 0, 0, 0, [2; 16])),
            &[2; 16],
        );
        assert!(!r.unwrap().torn);
        assert_eq!(stats.headers_visited, 1);
    }
}
