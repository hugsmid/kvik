//! The uploaded form of a manifest record (`format.md` A.17 Uploaded forms): the epoch of the record before it in the
//! manifest it was written in, then its A.2 block. A restore checks the chain those epochs make (Q-610).

use kvik_files::record::{Body, EncodeError, Record, RecordError};
use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
use kvik_log::crc::crc32c;

/// The bytes before the block: the predecessor's epoch, `u64`.
pub const PREVIOUS_LEN: usize = 8;

/// A manifest record as uploaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadedRecord {
    /// The epoch of the record before it in its manifest; 0 for a manifest's first record (Q-610).
    pub previous: u64,
    /// The record.
    pub record: Record,
}

/// Whether `r` opens a rebuilt manifest: a rebuild record that was not carried (Q-108).
pub fn opens_manifest(r: &Record) -> bool {
    matches!(r.body, Body::Rebuild(_)) && r.old_epoch.is_none()
}

fn invalid(what: &'static str) -> RecordError {
    RecordError::Invalid(what)
}

/// The format A.17 check on a predecessor, shared by both directions.
fn previous_ok(previous: u64, r: &Record) -> Result<(), &'static str> {
    if previous >= r.epoch {
        return Err("an uploaded record naming a predecessor at or above its own epoch");
    }
    if previous != 0 && opens_manifest(r) {
        return Err("a rebuild record opening a manifest naming a predecessor");
    }
    Ok(())
}

impl UploadedRecord {
    /// The plaintext to seal.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        previous_ok(self.previous, &self.record).map_err(EncodeError::Invalid)?;
        let block = self.record.encode()?;
        let mut out = Vec::with_capacity(PREVIOUS_LEN + block.len());
        out.extend_from_slice(&self.previous.to_le_bytes());
        out.extend_from_slice(&block);
        Ok(out)
    }

    /// The record an opened object's plaintext holds: the predecessor, then exactly one A.14 block.
    pub fn decode(b: &[u8]) -> Result<Self, RecordError> {
        let (previous, block) = b
            .split_first_chunk::<PREVIOUS_LEN>()
            .ok_or(invalid("an uploaded record shorter than its predecessor"))?;
        let previous = u64::from_le_bytes(*previous);
        let record = decode_block(block)?;
        previous_ok(previous, &record).map_err(invalid)?;
        Ok(Self { previous, record })
    }
}

fn decode_block(b: &[u8]) -> Result<Record, RecordError> {
    let raw: &[u8; BLOCK_HEADER_LEN] = b
        .get(..BLOCK_HEADER_LEN)
        .and_then(|h| h.try_into().ok())
        .ok_or(invalid("an uploaded record shorter than a block header"))?;
    let h = BlockHeader::decode(raw)
        .filter(|h| h.block_type != 0)
        .ok_or(invalid("an uploaded record whose block header is invalid"))?;
    if u64::try_from(b.len()).ok() != Some(h.block_len()) {
        return Err(invalid("an uploaded record that is not exactly one block"));
    }
    let len = usize::try_from(h.body_len).map_err(|_| invalid("an uploaded record too long"))?;
    let body = b
        .get(BLOCK_HEADER_LEN..BLOCK_HEADER_LEN.saturating_add(len))
        .ok_or(invalid("an uploaded record shorter than its body"))?;
    if crc32c(body) != h.body_crc32c {
        return Err(invalid("an uploaded record whose body checksum fails"));
    }
    Record::decode(h.block_type, h.skippable(), body)
}

#[cfg(test)]
mod tests {
    use kvik_files::record::Rebuild;
    use kvik_files::restore::RestoreRecord;
    use kvik_log::mark::ResetStamp;

    use super::*;

    fn up(previous: u64, record: Record) -> Vec<u8> {
        UploadedRecord { previous, record }.encode().unwrap()
    }

    #[test]
    fn a_restore_record_round_trips_through_its_uploaded_form() {
        let r = RestoreRecord {
            generation: 3,
            stamp: ResetStamp {
                sequence: 1,
                nonce: 0xfeed,
            },
            source_generation: 2,
            restored_end: (2, 40),
            carried_ends: Vec::new(),
            losses: Vec::new(),
            acknowledgements: Vec::new(),
            entry_losses: Vec::new(),
            files: Vec::new(),
            drill: false,
        };
        let back = UploadedRecord::decode(&up(0, Record::new(1, 3, 77, r.to_body()))).unwrap();
        assert_eq!(back.previous, 0);
        assert_eq!(back.record.generation, 3);
        assert_eq!(RestoreRecord::from_body(&back.record.body), Some(Ok(r)));
    }

    #[test]
    fn an_uploaded_record_is_its_predecessor_then_exactly_one_checksummed_block() {
        let b = up(3, Record::new(4, 0, 1, Body::Empty));
        let back = UploadedRecord::decode(&b).unwrap();
        assert_eq!((back.previous, back.record.epoch), (3, 4));
        assert!(UploadedRecord::decode(&b[..b.len() - 1]).is_err());
        assert!(UploadedRecord::decode(&[b.as_slice(), &[0; 8]].concat()).is_err());
        let mut bad = b.clone();
        bad[PREVIOUS_LEN + BLOCK_HEADER_LEN] ^= 1;
        assert!(UploadedRecord::decode(&bad).is_err());
        assert!(UploadedRecord::decode(&b[..PREVIOUS_LEN + 4]).is_err());
        assert!(UploadedRecord::decode(&b[..PREVIOUS_LEN - 1]).is_err());
        assert!(UploadedRecord::decode(&b[PREVIOUS_LEN..]).is_err());
    }

    /// Format A.17 by hand: `previous` 3, then the empty record's block (type `0x20`, body length 24, body and header
    /// checksums, then epoch 5, generation 2, reserved, `written_at` 77).
    const VECTOR: &str = concat!(
        "0300000000000000",
        "2000000018000000",
        "1594bd0d0a12bf31",
        "0500000000000000",
        "0200000000000000",
        "4d00000000000000",
    );

    #[test]
    fn the_uploaded_form_of_an_empty_record_is_the_format_vector() {
        let u = UploadedRecord {
            previous: 3,
            record: Record::new(5, 2, 77, Body::Empty),
        };
        let b = u.encode().unwrap();
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(hex, VECTOR);
        assert_eq!(&b[PREVIOUS_LEN..], u.record.encode().unwrap().as_slice());
        assert_eq!(UploadedRecord::decode(&b).unwrap(), u);
    }

    #[test]
    fn a_predecessor_at_or_above_the_epoch_is_refused_both_ways() {
        for previous in [4, 5, u64::MAX] {
            let u = UploadedRecord {
                previous,
                record: Record::new(4, 0, 1, Body::Empty),
            };
            assert!(u.encode().is_err());
            let mut b = up(3, u.record.clone());
            b[..PREVIOUS_LEN].copy_from_slice(&previous.to_le_bytes());
            assert!(UploadedRecord::decode(&b).is_err());
        }
    }

    #[test]
    fn a_rebuild_record_names_a_predecessor_only_when_carried() {
        let opening = Record::new(9, 0, 1, Body::Rebuild(Rebuild::default()));
        assert!(UploadedRecord::decode(&up(0, opening.clone())).is_ok());
        let named = UploadedRecord {
            previous: 7,
            record: opening.clone(),
        };
        assert!(named.encode().is_err());
        let mut b = up(0, opening);
        b[..PREVIOUS_LEN].copy_from_slice(&7u64.to_le_bytes());
        assert!(UploadedRecord::decode(&b).is_err());
        let carried = Record {
            old_epoch: Some(2),
            ..Record::new(9, 0, 1, Body::Rebuild(Rebuild::default()))
        };
        assert!(UploadedRecord::decode(&up(7, carried)).is_ok());
    }
}
