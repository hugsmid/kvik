//! File descriptions: what a manifest record says about a file (`format.md` A.14, file description), and what the
//! file's own header and trailer say about itself (every file is self-describing).

use kvik_log::manifest::RecordPosition;

use crate::fields::{position_bytes, position_of, u32_at, u64_at};

/// File kinds a manifest record may name (`format.md` A.1).
pub mod kind {
    /// A log segment.
    pub const LOG_SEGMENT: u8 = 1;
    /// A compacted segment.
    pub const COMPACTED: u8 = 2;
}

/// A file's identity: `file_id` is unique per store and kind (`format.md` A.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileRef {
    /// The file kind.
    pub kind: u8,
    /// The file id.
    pub file_id: u64,
}

/// A compacted file's class (`spec.md` §7: current facts and revisions go to separate chunks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileClass {
    /// No class: a log segment.
    None = 0,
    /// Current chunks.
    Current = 1,
    /// Revision chunks.
    Revision = 2,
}

impl FileClass {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Self::None,
            1 => Self::Current,
            2 => Self::Revision,
            _ => return None,
        })
    }
}

/// What a record says about a file: its kind, class, identity, position range, plaintext hash and inputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileDescription {
    /// The file kind ([`kind`]).
    pub kind: u8,
    /// The class.
    pub class: FileClass,
    /// The generation it was created in.
    pub generation: u32,
    /// The file id.
    pub file_id: u64,
    /// The first and last position it holds; `None` for a log segment that holds none.
    pub range: Option<(RecordPosition, RecordPosition)>,
    /// SHA-256 of the plaintext, as at seal.
    pub hash: [u8; 32],
    /// The files it was compacted from, in ascending order; empty for a log segment.
    pub inputs: Vec<FileRef>,
}

/// Bytes of a file description before its inputs.
const FIXED: usize = 80;
/// Bytes of one input.
const INPUT: usize = 12;
/// Flag bit 0: the file holds positions, and `first` and `last` give them.
const HAS_RANGE: u8 = 1;

impl FileDescription {
    /// Its identity.
    pub fn file_ref(&self) -> FileRef {
        FileRef {
            kind: self.kind,
            file_id: self.file_id,
        }
    }

    /// Whether the two position ranges share a position.
    pub fn overlaps(&self, other: &FileDescription) -> bool {
        match (self.range, other.range) {
            (Some((a0, a1)), Some((b0, b1))) => a0 <= b1 && b0 <= a1,
            _ => false,
        }
    }

    /// Whether its range holds `p`.
    pub fn holds(&self, p: RecordPosition) -> bool {
        self.range.is_some_and(|(a, b)| a <= p && p <= b)
    }

    /// Checks the rules a record applies to a description (`format.md` A.14): a log segment has no class and no
    /// inputs; a compacted file has a class, a range and at least one input, each a log segment or compacted file
    /// other than itself, in strictly ascending order; a range's first position is at or below its last.
    pub fn check(&self) -> Result<(), &'static str> {
        match self.kind {
            kind::LOG_SEGMENT => {
                if self.class != FileClass::None || !self.inputs.is_empty() {
                    return Err("a log segment has a class or inputs");
                }
            }
            kind::COMPACTED => {
                if self.class == FileClass::None || self.range.is_none() || self.inputs.is_empty() {
                    return Err("a compacted file lacks its class, range or inputs");
                }
            }
            _ => return Err("a file kind a record may not name"),
        }
        if let Some((a, b)) = self.range
            && a > b
        {
            return Err("a range whose first position is above its last");
        }
        if !self.inputs.windows(2).all(|w| w[0] < w[1]) {
            return Err("inputs not in strictly ascending order");
        }
        if self.inputs.iter().any(|i| {
            !matches!(i.kind, kind::LOG_SEGMENT | kind::COMPACTED) || *i == self.file_ref()
        }) {
            return Err("an input that is not a log segment or compacted file, or the file itself");
        }
        Ok(())
    }

    /// The field value (`format.md` A.14): 0 kind `u8`; 1 class `u8`; 2 flags `u8` (bit 0: holds positions);
    /// 3 reserved; 4 generation `u32`; 8 `file_id` `u64`; 16 first position; 28 last position; 40 plaintext SHA-256;
    /// 72 input count `u32`; 76 reserved `u32`; 80 the inputs, 12 bytes each: kind `u8`, 3 reserved, `file_id` `u64`.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; FIXED + INPUT * self.inputs.len()];
        b[0] = self.kind;
        b[1] = self.class as u8;
        b[4..8].copy_from_slice(&self.generation.to_le_bytes());
        b[8..16].copy_from_slice(&self.file_id.to_le_bytes());
        if let Some((first, last)) = self.range {
            b[2] = HAS_RANGE;
            b[16..28].copy_from_slice(&position_bytes(first));
            b[28..40].copy_from_slice(&position_bytes(last));
        }
        b[40..72].copy_from_slice(&self.hash);
        b[72..76].copy_from_slice(&(self.inputs.len() as u32).to_le_bytes());
        for (i, r) in self.inputs.iter().enumerate() {
            let at = FIXED + INPUT * i;
            b[at] = r.kind;
            b[at + 4..at + 12].copy_from_slice(&r.file_id.to_le_bytes());
        }
        b
    }

    /// Decodes and checks a field value. Reserved bytes and flag bits other than bit 0 are not read.
    pub fn decode(v: &[u8]) -> Result<Self, &'static str> {
        if v.len() < FIXED {
            return Err("a file description shorter than 80 bytes");
        }
        let count = u32_at(v, 72) as usize;
        if count.checked_mul(INPUT).and_then(|n| n.checked_add(FIXED)) != Some(v.len()) {
            return Err("a file description whose length does not match its input count");
        }
        let class = FileClass::from_code(v[1]).ok_or("an unknown file class")?;
        let range = (v[2] & HAS_RANGE != 0).then(|| {
            (
                position_of(&v[16..28]).unwrap_or_default(),
                position_of(&v[28..40]).unwrap_or_default(),
            )
        });
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&v[40..72]);
        let inputs = (0..count)
            .map(|i| {
                let at = FIXED + INPUT * i;
                FileRef {
                    kind: v[at],
                    file_id: u64_at(v, at + 4),
                }
            })
            .collect();
        let d = Self {
            kind: v[0],
            class,
            generation: u32_at(v, 4),
            file_id: u64_at(v, 8),
            range,
            hash,
            inputs,
        };
        d.check()?;
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_round_trips_and_its_rules_are_checked() {
        let d = FileDescription {
            kind: kind::COMPACTED,
            class: FileClass::Revision,
            generation: 2,
            file_id: 9,
            range: Some(((2, 5), (2, 70))),
            hash: [7; 32],
            inputs: vec![
                FileRef {
                    kind: 1,
                    file_id: 3,
                },
                FileRef {
                    kind: 2,
                    file_id: 4,
                },
            ],
        };
        let b = d.encode();
        assert_eq!(b.len(), 104);
        assert_eq!(FileDescription::decode(&b), Ok(d.clone()));
        let mut short = b.clone();
        short.pop();
        assert!(FileDescription::decode(&short).is_err());
        let mut own = d.clone();
        own.inputs.push(FileRef {
            kind: 2,
            file_id: 9,
        });
        assert!(own.check().is_err());
        let mut seg = d.clone();
        seg.kind = kind::LOG_SEGMENT;
        assert!(seg.check().is_err());
        let mut backwards = d;
        backwards.range = Some(((2, 8), (2, 7)));
        assert!(backwards.check().is_err());
    }
}
