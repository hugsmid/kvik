//! Copies in other tiers (`spec.md` §8 Hashes): the copy-verified record (`0x27`, `format.md` A.14) as a typed view
//! over `Body::Other`, and its codec.

use kvik_log::Element;

use crate::catalogue::Tier;
use crate::desc::FileRef;
use crate::fields::{u64_at, u64_of};
use crate::record::{Body, RecordError, record_type};

/// A verified copy in the deep tier: the object's size and SHA-256 as the bucket reported them (ciphertext).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepCopy {
    /// The object's size.
    pub size: u64,
    /// The object's SHA-256, as reported.
    pub sha256: [u8; 32],
}

/// A copy-verified record (`0x27`): `file` has a verified copy in `tier`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyVerified {
    /// The file.
    pub file: FileRef,
    /// The tier holding the copy.
    pub tier: Tier,
    /// The copy.
    pub copy: DeepCopy,
}

/// A tier's code in a record field (`format.md` A.14): 1 hot, 2 cold, 3 deep.
pub fn tier_code(t: Tier) -> u8 {
    match t {
        Tier::Hot => 1,
        Tier::Cold => 2,
        Tier::Deep => 3,
    }
}

/// The tier a record field's code names.
pub fn tier_of(code: u8) -> Option<Tier> {
    match code {
        1 => Some(Tier::Hot),
        2 => Some(Tier::Cold),
        3 => Some(Tier::Deep),
        _ => None,
    }
}

impl CopyVerified {
    /// The record body, fields in ascending id order (`format.md` A.14 `0x27`).
    pub fn to_body(&self) -> Body {
        let mut file = vec![self.file.kind, 0, 0, 0];
        file.extend_from_slice(&self.file.file_id.to_le_bytes());
        Body::Other {
            record_type: record_type::TIER_MOVE,
            skippable: false,
            fields: vec![
                (1, file),
                (2, vec![tier_code(self.tier)]),
                (3, self.copy.size.to_le_bytes().to_vec()),
                (4, self.copy.sha256.to_vec()),
            ],
        }
    }

    /// The copy-verified record `b` holds; `None` if `b` is not a `0x27`. Fields from `0x8000` are ignorable.
    pub fn from_body(b: &Body) -> Option<Result<Self, RecordError>> {
        match b {
            Body::Other {
                record_type: record_type::TIER_MOVE,
                fields,
                ..
            } => Some(decode(fields)),
            _ => None,
        }
    }
}

fn decode(fields: &[(u16, Vec<u8>)]) -> Result<CopyVerified, RecordError> {
    let mut v: [Option<&[u8]>; 4] = [None; 4];
    for (id, value) in fields {
        match id {
            1..=4 => {
                let slot = &mut v[usize::from(*id) - 1];
                if slot.is_some() {
                    return Err(RecordError::Invalid("a field appears twice"));
                }
                *slot = Some(value);
            }
            0x8000.. => {}
            _ => {
                return Err(RecordError::Unsupported(Element::Field {
                    entry_type: record_type::TIER_MOVE,
                    field: *id,
                }));
            }
        }
    }
    let [Some(file), Some(tier), Some(size), Some(sha)] = v else {
        return Err(RecordError::Invalid("a required field is missing"));
    };
    if file.len() != 12 || file[1..4] != [0, 0, 0] {
        return Err(RecordError::Invalid("a file reference not 12 bytes"));
    }
    let tier = match tier {
        [c] => tier_of(*c).ok_or(RecordError::Invalid("a tier out of range"))?,
        _ => return Err(RecordError::Invalid("a tier not 1 byte")),
    };
    let size = u64_of(size).ok_or(RecordError::Invalid("a size not 8 bytes"))?;
    let sha256: [u8; 32] = sha
        .try_into()
        .map_err(|_| RecordError::Invalid("a hash not 32 bytes"))?;
    Ok(CopyVerified {
        file: FileRef {
            kind: file[0],
            file_id: u64_at(file, 4),
        },
        tier,
        copy: DeepCopy { size, sha256 },
    })
}
