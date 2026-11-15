//! Reading a manifest file whole (`format.md` A.14, `spec.md` §7 Manifest): every valid record, on both sides of
//! each corrupt one, and the torn tail.
//!
//! An invalid record is passed by searching forward, at 8-byte boundaries, for a valid record with a higher epoch.
//! An invalid record followed by a valid one was synced and is **corrupt**; an invalid last record is a **torn
//! tail**. `kvik_log::manifest::read_manifest`, which the log's start uses, stops at the first corrupt record; this
//! reader goes on, as a manifest rebuild must carry every valid record before and after it.

use kvik_log::FormatError;
use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader, MAX_BODY_LEN};
use kvik_log::crc::crc32c;
use kvik_log::header::{FileHeader, HEADER_LEN, kind};

use crate::record::{RECORD_PREFIX, Record, RecordError};

/// A valid record and where it lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Offset of its block header.
    pub offset: u64,
    /// Just past its block, padding included.
    pub end: u64,
    /// The record.
    pub record: Record,
}

/// The bytes between two valid records that hold no valid record: one or more corrupt records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorruptSpan {
    /// Where the first invalid byte starts (the end of the last valid record before it, or of the header).
    pub offset: u64,
    /// Where the next valid record starts.
    pub end: u64,
    /// The epoch of the last valid record before it, 0 for none.
    pub after_epoch: u64,
    /// The epoch of the next valid record.
    pub before_epoch: u64,
}

impl CorruptSpan {
    /// The corrupt record's epoch as presumed: the last valid epoch before it + 1 (`format.md` A.14, rebuild record
    /// field 1).
    pub fn presumed_epoch(&self) -> u64 {
        self.after_epoch + 1
    }
}

/// A manifest as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The file header.
    pub header: FileHeader,
    /// The valid records, in order.
    pub records: Vec<Placed>,
    /// The corrupt spans, in order.
    pub corrupt: Vec<CorruptSpan>,
    /// Just past the last valid record (the header's end when there is none).
    pub valid_end: u64,
    /// Whether any byte follows the last valid record: a torn tail to drop.
    pub torn: bool,
}

/// Why a manifest cannot be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// No valid header, or one of another kind or store.
    #[error("the manifest's header is invalid")]
    Header,
    /// A version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

/// The valid record at `at` whose epoch is above `min_epoch`, if there is one. A checksum-valid block of an unknown
/// critical type or holding an unknown critical field is a version refusal, as `kvik_log::manifest` has it.
pub(crate) fn record_at(
    bytes: &[u8],
    at: u64,
    min_epoch: u64,
) -> Result<Option<Placed>, FormatError> {
    let Some(start) = usize::try_from(at).ok() else {
        return Ok(None);
    };
    let Some(raw) = bytes.get(start..start.saturating_add(BLOCK_HEADER_LEN)) else {
        return Ok(None);
    };
    let raw: &[u8; BLOCK_HEADER_LEN] = raw.try_into().unwrap_or(&[0; BLOCK_HEADER_LEN]);
    if raw.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let Some(h) = BlockHeader::decode(raw).filter(|h| h.block_type != 0) else {
        return Ok(None);
    };
    // Judged on the header and the body's first bytes before the body is checksummed, so that a search over bytes
    // that imitate headers claiming long bodies checksums only candidates that could be the record looked for: a
    // body that holds the record prefix and fits in the file, and an epoch above the previous record's.
    if h.body_len > MAX_BODY_LEN || (h.body_len as usize) < RECORD_PREFIX {
        return Ok(None);
    }
    let body_start = start + BLOCK_HEADER_LEN;
    let Some(body) = bytes.get(body_start..body_start + h.body_len as usize) else {
        return Ok(None);
    };
    if u64::from_le_bytes(body[..8].try_into().unwrap_or_default()) <= min_epoch {
        return Ok(None);
    }
    if crc32c(body) != h.body_crc32c {
        return Ok(None);
    }
    match Record::decode(h.block_type, h.skippable(), body) {
        Ok(r) if r.epoch > min_epoch => Ok(Some(Placed {
            offset: at,
            end: at + h.block_len(),
            record: r,
        })),
        Ok(_) | Err(RecordError::Invalid(_)) => Ok(None),
        Err(RecordError::Unsupported(element)) => Err(FormatError::Unsupported {
            offset: at,
            element,
        }),
    }
}

/// Reads a whole manifest file.
pub fn read(bytes: &[u8], store_id: &[u8; 16]) -> Result<Image, ReadError> {
    let header = FileHeader::decode(bytes).ok_or(ReadError::Header)?;
    if header.format_major > kvik_log::FORMAT_MAJOR {
        return Err(ReadError::Unsupported(FormatError::Unsupported {
            offset: 0,
            element: kvik_log::Element::FormatMajor(header.format_major),
        }));
    }
    if header.kind != kind::MANIFEST || &header.store_id != store_id {
        return Err(ReadError::Header);
    }
    let len = bytes.len() as u64;
    let mut records: Vec<Placed> = Vec::new();
    let mut corrupt = Vec::new();
    let mut at = HEADER_LEN as u64;
    loop {
        let min = records.last().map_or(0, |p| p.record.epoch);
        if at < len
            && let Some(p) = record_at(bytes, at, min).map_err(ReadError::Unsupported)?
        {
            at = p.end;
            records.push(p);
            continue;
        }
        // An invalid record, or the end: search forward for a valid record of a higher epoch.
        let mut probe = at + 8;
        let mut found = None;
        while probe + (BLOCK_HEADER_LEN as u64) <= len {
            if let Some(p) = record_at(bytes, probe, min).map_err(ReadError::Unsupported)? {
                found = Some(p);
                break;
            }
            probe += 8;
        }
        match found {
            Some(p) => {
                corrupt.push(CorruptSpan {
                    offset: at,
                    end: p.offset,
                    after_epoch: min,
                    before_epoch: p.record.epoch,
                });
                at = p.end;
                records.push(p);
            }
            None => break,
        }
    }
    let valid_end = at.min(len);
    Ok(Image {
        header,
        records,
        corrupt,
        valid_end,
        torn: len > valid_end,
    })
}

impl Image {
    /// The last valid epoch, 0 for none.
    pub fn last_epoch(&self) -> u64 {
        self.records.last().map_or(0, |p| p.record.epoch)
    }

    /// Whether a corrupt record was found.
    pub fn is_corrupt(&self) -> bool {
        !self.corrupt.is_empty()
    }

    /// The valid records, in order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.iter().map(|p| &p.record)
    }
}
