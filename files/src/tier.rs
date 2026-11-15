//! Copies in other tiers (`spec.md` §8 Hashes): the copy-verified record (`0x27`, `format.md` A.14) as a typed view.
//! Its codec over `Body::Other` and its fold arrive with roadmap 8.1c.

use crate::catalogue::Tier;
use crate::desc::FileRef;

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
