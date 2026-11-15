//! The uploaded form of a manifest record: the A.14 block alone (`format.md` A.17 Uploaded forms).

use kvik_files::record::{Record, RecordError};
use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
use kvik_log::crc::crc32c;

fn invalid(what: &'static str) -> RecordError {
    RecordError::Invalid(what)
}

/// The record an uploaded object's plaintext holds: exactly one A.14 block.
pub(crate) fn decode_block(b: &[u8]) -> Result<Record, RecordError> {
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
    use kvik_files::record::Body;
    use kvik_files::restore::RestoreRecord;
    use kvik_log::mark::ResetStamp;

    use super::*;

    #[test]
    fn a_restore_record_round_trips_through_an_uploaded_block() {
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
        let block = Record::new(1, 3, 77, r.to_body()).encode().unwrap();
        let back = decode_block(&block).unwrap();
        assert_eq!(back.generation, 3);
        assert_eq!(RestoreRecord::from_body(&back.body), Some(Ok(r)));
    }

    #[test]
    fn an_uploaded_record_is_exactly_one_checksummed_block() {
        let block = Record::new(4, 0, 1, Body::Empty).encode().unwrap();
        assert_eq!(decode_block(&block).unwrap().epoch, 4);
        assert!(decode_block(&block[..block.len() - 1]).is_err());
        assert!(decode_block(&[block.as_slice(), &[0; 8]].concat()).is_err());
        let mut bad = block.clone();
        bad[BLOCK_HEADER_LEN] ^= 1;
        assert!(decode_block(&bad).is_err());
        assert!(decode_block(&block[..4]).is_err());
    }
}
