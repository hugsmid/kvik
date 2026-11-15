//! The envelope every uploaded object is sealed in (`spec.md` §8 Encryption, `format.md` A.17): authenticated chunks
//! of [`CHUNK`] plaintext bytes behind a header naming the version, the algorithm and the key check.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use chacha20poly1305::{AeadInOut, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use kvik_log::crc::crc32c;
use sha2::{Digest, Sha256};

#[doc(hidden)]
pub mod fuzz;

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

const MAGIC: [u8; 4] = *b"KVKE";
const VERSION: u8 = 1;
const ALGORITHM: u8 = 1;
const HEADER: usize = 64;
const TAG: usize = 16;
const BASE: usize = 19;
const C: u64 = CHUNK as u64;
const FRAME: u64 = C + TAG as u64;
/// format A.17: at most 2^32 chunks.
const MAX_LEN: u64 = (1 << 32) * C;

/// The envelope of `format.md` A.17: XChaCha20-Poly1305 under keys derived from the application's key and the store
/// id, with a nonce base derived from the object key and the plaintext, so sealing is deterministic.
#[derive(Clone)]
pub struct ChunkedXChaCha {
    cipher: XChaCha20Poly1305,
    nonce_key: [u8; 32],
    check: [u8; 8],
}

impl fmt::Debug for ChunkedXChaCha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChunkedXChaCha(..)")
    }
}

impl ChunkedXChaCha {
    /// The envelope of store `store_id` under the application's `key`.
    pub fn new(key: [u8; 32], store_id: [u8; 16]) -> Self {
        let derive = |label: &[u8]| hmac(&key, &[label, &store_id]);
        let mut check = [0u8; 8];
        check.copy_from_slice(&derive(b"kvik envelope 1 key check")[..8]);
        Self {
            cipher: XChaCha20Poly1305::new(&derive(b"kvik envelope 1 encryption").into()),
            nonce_key: derive(b"kvik envelope 1 nonce"),
            check,
        }
    }

    /// The header fields, after the checks of format A.17 Reading up to the key check.
    fn header(&self, header: &[u8]) -> Result<Header, EnvelopeError> {
        let h: [u8; HEADER] = header
            .get(..HEADER)
            .and_then(|h| h.try_into().ok())
            .ok_or(EnvelopeError::Truncated)?;
        if h[..4] != MAGIC || crc32c(&h[..60]).to_le_bytes() != h[60..] {
            return Err(DAMAGED_HEADER);
        }
        if h[4] != VERSION || h[5] != ALGORITHM {
            return Err(EnvelopeError::Unsupported {
                version: h[4],
                algorithm: h[5],
            });
        }
        if h[6..8] != [0; 2] || h[43..60] != [0; 17] {
            return Err(DAMAGED_HEADER);
        }
        if h[16..24] != self.check {
            return Err(EnvelopeError::WrongKey);
        }
        let len = u64::from_le_bytes(h[8..16].try_into().unwrap_or_default());
        let chunks = chunks(len).ok_or(EnvelopeError::TooLarge { size: len })?;
        let mut base = [0u8; BASE];
        base.copy_from_slice(&h[24..24 + BASE]);
        Ok(Header {
            bytes: h,
            len,
            chunks,
            base,
        })
    }

    /// Opens chunks `first..` from `bytes`, which must hold them exactly through chunk `last`, into `out`.
    fn open_chunks(
        &self,
        object_key: &str,
        h: &Header,
        chunks: Range<u64>,
        mut bytes: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), EnvelopeError> {
        let aad = aad(&h.bytes, object_key);
        for i in chunks.clone() {
            let plain = usize::try_from(h.plain_len(i)).map_err(|_| EnvelopeError::Truncated)?;
            if bytes.len() < plain + TAG {
                return Err(EnvelopeError::Truncated);
            }
            let (frame, rest) = bytes.split_at(plain + TAG);
            let (cipher, tag) = frame.split_at(plain);
            let at = out.len();
            out.extend_from_slice(cipher);
            let tag = Tag::try_from(tag).map_err(|_| EnvelopeError::Truncated)?;
            let nonce = nonce(&h.base, i, i + 1 == h.chunks);
            if self
                .cipher
                .decrypt_inout_detached(&nonce, &aad, (&mut out[at..]).into(), &tag)
                .is_err()
            {
                out.truncate(at);
                return Err(EnvelopeError::Authentication { chunk: i });
            }
            bytes = rest;
        }
        if bytes.is_empty() {
            Ok(())
        } else {
            Err(EnvelopeError::Authentication { chunk: chunks.end })
        }
    }
}

/// A header that fails its magic, checksum or reserved bytes: damage, never `WrongKey` or `Unsupported` (format
/// A.17 Reading). Every chunk binds the header, so chunk 0 cannot authenticate (Q-270).
const DAMAGED_HEADER: EnvelopeError = EnvelopeError::Authentication { chunk: 0 };

struct Header {
    bytes: [u8; HEADER],
    len: u64,
    chunks: u64,
    base: [u8; BASE],
}

impl Header {
    fn plain_len(&self, chunk: u64) -> u64 {
        self.len.saturating_sub(chunk.saturating_mul(C)).min(C)
    }

    /// Object bytes of chunks `first..=last`; at most the object's size, which [`chunks`] bounds.
    fn frames_len(&self, first: u64, last: u64) -> u64 {
        (last - first) * FRAME + self.plain_len(last) + TAG as u64
    }
}

/// The chunk count of a plaintext of `len` bytes, if an envelope holds it.
fn chunks(len: u64) -> Option<u64> {
    (len <= MAX_LEN).then(|| len.div_ceil(C).max(1))
}

/// The object size of a plaintext of `len` bytes (format A.17 Size and ranges).
fn object_size(len: u64) -> Option<u64> {
    (HEADER as u64)
        .checked_add(len)?
        .checked_add((TAG as u64).checked_mul(chunks(len)?)?)
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn nonce(base: &[u8; BASE], chunk: u64, last: bool) -> XNonce {
    let mut n = [0u8; 24];
    n[..BASE].copy_from_slice(base);
    n[BASE..BASE + 4].copy_from_slice(&(chunk as u32).to_le_bytes());
    n[23] = u8::from(last);
    XNonce::from(n)
}

fn aad(header: &[u8; HEADER], object_key: &str) -> Vec<u8> {
    let mut a = Vec::with_capacity(HEADER + object_key.len());
    a.extend_from_slice(header);
    a.extend_from_slice(object_key.as_bytes());
    a
}

impl Envelope for ChunkedXChaCha {
    fn key_check(&self) -> KeyCheck {
        KeyCheck(self.check)
    }

    fn seal(&self, object_key: &str, plaintext: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let len = plaintext.len() as u64;
        let too_large = EnvelopeError::TooLarge { size: len };
        let n = chunks(len).ok_or(too_large.clone())?;
        let size = object_size(len)
            .and_then(|s| usize::try_from(s).ok())
            .ok_or(too_large.clone())?;
        let digest: [u8; 32] = Sha256::digest(plaintext).into();
        let mut base = [0u8; BASE];
        base.copy_from_slice(&hmac(&self.nonce_key, &[object_key.as_bytes(), &digest])[..BASE]);
        let mut h = [0u8; HEADER];
        h[..4].copy_from_slice(&MAGIC);
        h[4] = VERSION;
        h[5] = ALGORITHM;
        h[8..16].copy_from_slice(&len.to_le_bytes());
        h[16..24].copy_from_slice(&self.check);
        h[24..24 + BASE].copy_from_slice(&base);
        let crc = crc32c(&h[..60]);
        h[60..].copy_from_slice(&crc.to_le_bytes());
        let aad = aad(&h, object_key);
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&h);
        let mut rest = plaintext;
        for i in 0..n {
            let (chunk, tail) = rest.split_at(rest.len().min(CHUNK));
            rest = tail;
            let at = out.len();
            out.extend_from_slice(chunk);
            let tag = self
                .cipher
                .encrypt_inout_detached(&nonce(&base, i, i + 1 == n), &aad, (&mut out[at..]).into())
                .map_err(|_| too_large.clone())?;
            out.extend_from_slice(&tag);
        }
        Ok(out)
    }

    fn open(&self, object_key: &str, object: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
        let h = self.header(object)?;
        let size = object_size(h.len).ok_or(EnvelopeError::TooLarge { size: h.len })?;
        let have = object.len() as u64;
        if have < size {
            return Err(EnvelopeError::Truncated);
        }
        if have > size {
            return Err(EnvelopeError::Authentication { chunk: h.chunks });
        }
        let mut out = Vec::with_capacity(object.len() - HEADER);
        self.open_chunks(object_key, &h, 0..h.chunks, &object[HEADER..], &mut out)?;
        Ok(out)
    }

    fn header_len(&self) -> u64 {
        HEADER as u64
    }

    fn check_header(&self, header: &[u8]) -> Result<(), EnvelopeError> {
        self.header(header).map(|_| ())
    }

    /// Whole frames, so it may run past the object's end in the last chunk (the caller clamps); empty for an empty
    /// range.
    fn span(&self, plain: Range<u64>) -> Range<u64> {
        let first = plain.start / C;
        let end = if plain.start < plain.end {
            plain.end.div_ceil(C)
        } else {
            first
        };
        let at = |chunk: u64| (HEADER as u64).saturating_add(chunk.saturating_mul(FRAME));
        at(first)..at(end)
    }

    fn open_span(
        &self,
        object_key: &str,
        header: &[u8],
        plain: Range<u64>,
        bytes: &[u8],
    ) -> Result<Vec<u8>, EnvelopeError> {
        let h = self.header(header)?;
        if plain.start > plain.end || plain.end > h.len {
            return Err(EnvelopeError::Truncated);
        }
        if plain.start == plain.end {
            return Ok(Vec::new());
        }
        let (first, last) = (plain.start / C, (plain.end - 1) / C);
        let want = h.frames_len(first, last);
        let have = bytes.len() as u64;
        if have < want {
            return Err(EnvelopeError::Truncated);
        }
        if have > want {
            return Err(EnvelopeError::Authentication { chunk: last + 1 });
        }
        let mut out = Vec::with_capacity(bytes.len());
        self.open_chunks(object_key, &h, first..last + 1, bytes, &mut out)?;
        let base = first * C;
        let (lo, hi) = (plain.start - base, plain.end - base);
        let (Ok(lo), Ok(hi)) = (usize::try_from(lo), usize::try_from(hi)) else {
            return Err(EnvelopeError::Truncated);
        };
        out.truncate(hi);
        out.drain(..lo);
        Ok(out)
    }
}
