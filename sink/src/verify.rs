//! Verified copies in the deep tier (spec §8 Hashes). [`Verified`] is made
//! only by [`Verified::check`], which [`upload`] and [`check`] call.

use std::sync::Arc;

use kvik::BackupSink;
pub use kvik::{Stored, Verified, VerifyError};
use sha2::{Digest, Sha256};

/// The SHA-256 of some bytes.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Uploads bytes, then verifies the copy by its reported size and SHA-256.
pub async fn upload<S: BackupSink + ?Sized>(
    sink: &S,
    key: &str,
    bytes: Arc<[u8]>,
) -> Result<Verified, VerifyError> {
    let expected = Stored::new(bytes.len() as u64, sha256(&bytes));
    let stored = sink.put(key, bytes, expected.sha256).await?;
    if stored != expected {
        return Err(VerifyError::Reported {
            key: key.into(),
            expected,
            reported: Some(stored),
        });
    }
    Verified::check(sink, key, &expected).await
}

/// The scheduled check: the bucket still reports the expected size and
/// SHA-256. It proves what the bucket reports, not the bytes.
pub async fn check<S: BackupSink + ?Sized>(
    sink: &S,
    key: &str,
    expected: &Stored,
) -> Result<Verified, VerifyError> {
    Verified::check(sink, key, expected).await
}

/// The sampled download: the whole object's SHA-256 is the expected one
/// (decrypting and the plaintext hash are the engine's part).
pub async fn download<S: BackupSink + ?Sized>(
    sink: &S,
    key: &str,
    sha256_expected: [u8; 32],
) -> Result<Vec<u8>, VerifyError> {
    let bytes = sink.get(key, None).await?;
    if sha256(&bytes) != sha256_expected {
        return Err(VerifyError::Content { key: key.into() });
    }
    Ok(bytes)
}
