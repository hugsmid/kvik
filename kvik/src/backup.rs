//! Backup and restore (spec §8; `docs/api.md` Backup and restore). Not
//! supported by the model backend yet: every call returns
//! [`BackupError::NotSupportedYet`](crate::BackupError::NotSupportedYet).
//! The sink trait and its types live in `kvik-sink-api` and are re-exported
//! here unchanged; an S3-compatible implementation and an in-memory fake are
//! in the `kvik-sink` crate.

pub use kvik_sink_api::{
    BackupSink, ListPage, SinkError, SinkFuture, Stored, Verified, VerifyError,
};

/// How files are encrypted before upload (spec §8 Encryption).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Encryption {
    /// A 256-bit key supplied by the application.
    Key([u8; 32]),
}

impl std::fmt::Debug for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Encryption::Key(..)")
    }
}

/// How a restore brings detail back (spec §8 Restore).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreMode {
    /// Serve from the checkpoint's summaries first, then bring detail back.
    #[default]
    SummariesFirst,
    /// A drill: restore read-only, claim and upload nothing.
    Drill,
}

/// What [`Store::rebuild`](crate::Store::rebuild) recomputes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rebuild {
    /// Every derived structure, from the facts.
    #[default]
    Derived,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[test]
    fn the_reexported_sink_items_are_the_leaf_crates() {
        let _: fn(kvik_sink_api::Stored) -> crate::Stored = |s| s;
        let _: fn(kvik_sink_api::ListPage) -> crate::ListPage = |p| p;
        let _: fn(kvik_sink_api::SinkError) -> crate::SinkError = |e| e;
        let _: fn(kvik_sink_api::Verified) -> crate::Verified = |v| v;
        let _: fn(kvik_sink_api::VerifyError) -> crate::VerifyError = |e| e;
        let _: fn(kvik_sink_api::SinkFuture<'static, ()>) -> crate::SinkFuture<'static, ()> = |f| f;
        let _: fn(Arc<dyn kvik_sink_api::BackupSink>) -> Arc<dyn crate::BackupSink> = |s| s;
    }
}
