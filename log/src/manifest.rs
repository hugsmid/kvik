//! The manifest's framing (`format.md` A.14) as far as the log's start needs it (`spec.md` §3 Order of a start,
//! step 4; §7 Manifest): reading its records, telling a torn last record from a corrupt one, and the empty and
//! lost-range records. Publish, restore, quarantine and rebuild records belong to later roadmap items; their typed
//! layouts are in the `kvik-files` crate, and this reader passes the publish-and-retire, sealed-segment and reserved
//! (restore, quarantine, tier-move) records over as field lists, taking only their epochs.

use crate::block::{BLOCK_HEADER_LEN, encode_block, read_block};
use crate::error::{Element, FormatError};
use crate::header::{FileHeader, HEADER_LEN, kind};

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
    /// An invalid record followed by a valid one: it was synced, so it is corrupt. Fetching it again or
    /// rebuilding the manifest is roadmap 6.3 (Q-116 (a)).
    #[error("the manifest record at offset {0} is corrupt")]
    Corrupt(u64),
    /// A version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

fn fields(body: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let h = body.get(at..at + 4)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        out.push((id, body.get(at + 4..at + 4 + len)?));
        at = (at + 4 + len).div_ceil(4) * 4;
    }
    Some(out)
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

fn decode_lost(list: &[(u16, &[u8])]) -> Option<LostRange> {
    let get = |id: u16| list.iter().find(|(i, _)| *i == id).map(|(_, v)| *v);
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
    })
}

/// The record at `at`, if it is a valid record whose epoch is above `min_epoch`.
fn record_at(bytes: &[u8], at: u64, min_epoch: u64) -> Result<Option<Record>, ManifestError> {
    let Ok(b) = read_block(bytes, at, bytes.len() as u64) else {
        return Ok(None);
    };
    let t = b.header.block_type;
    let body = b.body;
    if body.len() < RECORD_PREFIX {
        return Ok(None);
    }
    let epoch = u64::from_le_bytes(body[..8].try_into().unwrap_or_default());
    let generation = u32::from_le_bytes(body[8..12].try_into().unwrap_or_default());
    let Some(list) = fields(&body[RECORD_PREFIX..]) else {
        return Ok(None);
    };
    if epoch <= min_epoch {
        return Ok(None);
    }
    let lost = match t {
        RECORD_LOST_RANGE => match decode_lost(&list) {
            Some(l) => Some(l),
            None => return Ok(None),
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
                offset: at,
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

/// Reads a manifest (A.14, `spec.md` §7 Manifest): its records up to the first invalid one; an invalid record
/// followed, at an 8-byte boundary, by a valid record of a higher epoch is corrupt.
pub fn read_manifest(bytes: &[u8], store_id: &[u8; 16]) -> Result<ManifestRead, ManifestError> {
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
    let mut records: Vec<Record> = Vec::new();
    let mut at = HEADER_LEN as u64;
    while at < bytes.len() as u64 {
        let min = records.last().map_or(0, |r| r.epoch);
        match record_at(bytes, at, min)? {
            Some(r) => {
                at = r.end;
                records.push(r);
            }
            None => break,
        }
    }
    let valid_end = at.min(bytes.len() as u64);
    // Search forward for a valid record of a higher epoch: if there is one, the invalid record was synced.
    let min = records.last().map_or(0, |r| r.epoch);
    let mut probe = valid_end + 8;
    while probe + (BLOCK_HEADER_LEN as u64) <= bytes.len() as u64 {
        if record_at(bytes, probe, min)?.is_some() {
            return Err(ManifestError::Corrupt(valid_end));
        }
        probe += 8;
    }
    Ok(ManifestRead {
        valid_end,
        torn: (bytes.len() as u64) > valid_end,
        records,
    })
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
}
