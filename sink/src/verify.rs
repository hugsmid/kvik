//! Verified copies in the deep tier (spec §8 Hashes): a file is backed up
//! only once the bucket's reported hash equals the manifest's. [`Verified`]
//! can be made only by the checks here, so code that may evict a file
//! (roadmap 8.1) can ask for one.

use std::sync::Arc;

use kvik::{BackupSink, SinkError, Stored};
use sha2::{Digest, Sha256};

/// Proof that the bucket reported the expected size and SHA-256 for a key
/// when asked after the upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    key: String,
    stored: Stored,
}

impl Verified {
    /// The key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What the bucket reported.
    pub fn stored(&self) -> &Stored {
        &self.stored
    }
}

/// Why a copy is not verified.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The sink failed.
    #[error(transparent)]
    Sink(#[from] SinkError),
    /// The bucket reports something other than what was uploaded, or nothing.
    #[error("{key}: the bucket reports {reported:?}, expected {expected:?}")]
    Reported {
        /// The key.
        key: String,
        /// What was uploaded.
        expected: Stored,
        /// What the bucket reports.
        reported: Option<Stored>,
    },
    /// Downloaded bytes whose SHA-256 is not the expected one.
    #[error("{key}: downloaded bytes do not have the expected SHA-256")]
    Content {
        /// The key.
        key: String,
    },
}

/// The SHA-256 of some bytes.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Uploads bytes and verifies the copy: the bucket must accept the SHA-256
/// sent with them, `put` must return the expected size and hash, and a
/// separate `stat` must report them again.
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
    check(sink, key, &expected).await
}

/// Re-reads the reported size and hash of a copy (the scheduled check).
pub async fn check<S: BackupSink + ?Sized>(
    sink: &S,
    key: &str,
    expected: &Stored,
) -> Result<Verified, VerifyError> {
    match sink.stat(key).await? {
        Some(s) if &s == expected => Ok(Verified {
            key: key.into(),
            stored: s,
        }),
        reported => Err(VerifyError::Reported {
            key: key.into(),
            expected: expected.clone(),
            reported,
        }),
    }
}

/// Downloads a whole object and checks its SHA-256 (the sampled download;
/// decrypting and checking the plaintext hash is the engine's part).
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
