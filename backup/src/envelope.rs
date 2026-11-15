//! The envelope every uploaded object is sealed in (`spec.md` §8 Encryption): authenticated chunks of [`CHUNK`]
//! plaintext bytes behind a header naming the version, the algorithm and the key check. The cipher is roadmap 8.1b.

use std::ops::Range;
use std::sync::Arc;

/// Plaintext bytes per chunk.
pub const CHUNK: usize = 65_536;

/// A short check value of the key, carried in each header so that a wrong key is told apart from damage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCheck(pub [u8; 8]);

/// Seals and opens objects. Forwarded for `Arc<E>` and `Box<E>`, as the sink is.
pub trait Envelope: Send + Sync {
    /// The key's check value.
    fn key_check(&self) -> KeyCheck;
    /// Deterministic in (key, `object_key`, `plaintext`): a `put` repeated after a crash must send identical bytes
    /// (`put` accepts a repeat only with identical content).
    fn seal(&self, object_key: &str, plaintext: &[u8]) -> Result<Vec<u8>, EnvelopeError>;
    /// The plaintext of a whole object.
    fn open(&self, object_key: &str, object: &[u8]) -> Result<Vec<u8>, EnvelopeError>;
    /// Bytes of the header.
    fn header_len(&self) -> u64;
    /// Checks a header alone: [`EnvelopeError::WrongKey`] from its first [`Envelope::header_len`] bytes.
    fn check_header(&self, header: &[u8]) -> Result<(), EnvelopeError>;
    /// The object bytes, whole chunks, that hold plaintext bytes `plain`.
    fn span(&self, plain: Range<u64>) -> Range<u64>;
    /// The plaintext bytes `plain` from `bytes`, the object bytes [`Envelope::span`] named.
    fn open_span(
        &self,
        object_key: &str,
        header: &[u8],
        plain: Range<u64>,
        bytes: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError>;
}

/// Why an object cannot be sealed or opened.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EnvelopeError {
    /// The header's key check is not this key's.
    #[error("the object was sealed with another key")]
    WrongKey,
    /// A version or algorithm this build does not read.
    #[error("unsupported envelope version {version}, algorithm {algorithm}")]
    Unsupported {
        /// The version.
        version: u8,
        /// The algorithm.
        algorithm: u8,
    },
    /// Fewer bytes than the header or a chunk needs.
    #[error("the object is truncated")]
    Truncated,
    /// A chunk fails authentication.
    #[error("chunk {chunk} fails authentication")]
    Authentication {
        /// The chunk's index.
        chunk: u64,
    },
    /// Plaintext larger than an envelope holds.
    #[error("{size} bytes are more than an envelope holds")]
    TooLarge {
        /// The plaintext's size.
        size: u64,
    },
}

macro_rules! forward_envelope {
    ($ty:ty) => {
        impl<E: Envelope + ?Sized> Envelope for $ty {
            fn key_check(&self) -> KeyCheck {
                (**self).key_check()
            }
            fn seal(&self, object_key: &str, plaintext: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
                (**self).seal(object_key, plaintext)
            }
            fn open(&self, object_key: &str, object: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
                (**self).open(object_key, object)
            }
            fn header_len(&self) -> u64 {
                (**self).header_len()
            }
            fn check_header(&self, header: &[u8]) -> Result<(), EnvelopeError> {
                (**self).check_header(header)
            }
            fn span(&self, plain: Range<u64>) -> Range<u64> {
                (**self).span(plain)
            }
            fn open_span(
                &self,
                object_key: &str,
                header: &[u8],
                plain: Range<u64>,
                bytes: &[u8],
            ) -> Result<Vec<u8>, EnvelopeError> {
                (**self).open_span(object_key, header, plain, bytes)
            }
        }
    };
}

forward_envelope!(Arc<E>);
forward_envelope!(Box<E>);
