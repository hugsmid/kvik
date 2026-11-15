//! The manifest as the upload driver sees it. The store implements it on its manifest thread's handle; the driver
//! never holds an `Appender`.

use std::io;
use std::sync::Arc;

use kvik_files::appender::{AppendError, Settled};
use kvik_files::catalogue::Catalogue;
use kvik_files::record::{Body, Record};

/// The manifest writer, through the store.
pub trait ManifestPort: Send {
    /// Appends a record holding `body` after the previous record's sync; the port supplies `written_at`. Returns
    /// its epoch.
    fn append(&mut self, body: Body) -> Result<u64, PortError>;
    /// Makes sure a synced successor of `epoch` exists, appending an empty record if `epoch` is the last.
    fn settle(&mut self, epoch: u64) -> Result<Settled, PortError>;
    /// Whether a provisional rebuild holds uploads (`spec.md` §7 Deep unreachable).
    fn holds(&self) -> bool;
    /// The fold as of the last synced record.
    fn snapshot(&self) -> Arc<Catalogue>;
    /// The records after epoch `after`, in epoch order, as written. A record after `after` that cannot be read is
    /// an error, never skipped: the driver uploads no record after it (`spec.md` §8 Generations; Q-282 (e)).
    fn records_after(&self, after: u64) -> Result<Vec<Record>, PortError>;
}

/// Why a [`ManifestPort`] call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PortError {
    /// The store is read-only.
    #[error("the store is read-only")]
    ReadOnly,
    /// The manifest writer has stopped.
    #[error("the manifest writer has stopped")]
    Closed,
    /// The append failed.
    #[error(transparent)]
    Append(#[from] AppendError),
    /// Reading the manifest failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}
