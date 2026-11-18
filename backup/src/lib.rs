//! Backup and restore for the kvik time-series store (`docs/spec.md` §8 Deep, Encryption, Generations, Restore).
//!
//! - [`envelope`]: how a file or record is sealed before upload.
//! - [`keys`]: the only place an object key is built or parsed.
//! - [`port`]: the manifest as the upload driver sees it.
//! - [`upload`]: the upload driver, a step function the store runs on its own thread.
//! - [`marker`]: the last-uploaded marker a restore checks the records it reads against.
//! - [`plan`]: what a restore installs, from the records it read.
//! - [`restore`]: the restore driver.
//! - [`uploaded`]: the uploaded form of a manifest record and the chain of epochs it names.
//!
//! The drivers own no thread, clock or sleep: the caller passes `now` and acts on `Retry { after }`.

#![forbid(unsafe_code)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use kvik_log::fs::Fs;
// Declared ahead of use for the cipher, its nonce derivation and the upload hashes (roadmap 8.1b, 8.1c).
use {chacha20poly1305 as _, hmac as _, sha2 as _};

pub mod envelope;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod keys;
pub mod marker;
pub mod plan;
pub mod port;
pub mod restore;
pub mod upload;
pub mod uploaded;

/// The future a driver call returns: boxed, `Send` and independent of any async runtime.
pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The local store directory a driver reads from or installs into; paths inside it follow `kvik_log::name`.
#[derive(Clone, Debug)]
pub struct LocalStore {
    /// The file system.
    pub fs: Arc<dyn Fs>,
    /// The store directory.
    pub dir: PathBuf,
}
