//! Sources of F (`spec.md` §3 Recovery rule 3): the local files, other than the log, that name positions.
//!
//! A start that writes a recovery entry places it at F = 1 + the highest sequence of the current generation among
//! the last kept position and every position named by a local cursor file, manifest record or derived file, so no
//! position such a file names is assigned again. The manifest is read by the start itself; every other kind of file
//! contributes through a [`PositionSource`], listed in [`sources`]: the registry file (`format.md` A.16), cursor
//! files (A.15) and the derived files (A.18) so far. Each later kind of derived file (compacted chunks, summary
//! trees) adds its own source there, with a test that a position it names raises F.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use crate::error::FormatError;
use crate::fs::Fs;

/// What a start tells a source.
#[derive(Clone, Copy, Debug)]
pub struct SourceContext<'a> {
    /// The store's file system.
    pub fs: &'a dyn Fs,
    /// The store directory.
    pub store_dir: &'a Path,
    /// The store id every file must carry.
    pub store_id: [u8; 16],
    /// The current generation: only its positions count toward F.
    pub generation: u32,
    /// The consumers named by the routing changes the start has seen (the registry file and the unsealed segments),
    /// for a source that reports consumers whose file is missing.
    pub consumers: &'a BTreeSet<String>,
}

/// What a source found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceReport {
    /// The highest sequence of the current generation its files name; 0 for none.
    pub highest: u64,
    /// Consumers to name in a recovery entry (field 47): those whose cursor file is missing or unreadable.
    pub named: Vec<String>,
}

/// A kind of local file that names positions. A file that cannot be read is passed over or reported in
/// [`SourceReport::named`]; only a version refusal, which stops the start, is an error.
pub trait PositionSource: Send + Sync + fmt::Debug {
    /// The kind of file, for messages.
    fn kind(&self) -> &'static str;
    /// Reads the store's files of this kind.
    fn read(&self, ctx: &SourceContext<'_>) -> Result<SourceReport, FormatError>;
}

/// The registry file as a source of F: its `through` (A.16).
#[derive(Clone, Copy, Debug, Default)]
pub struct RegistrySource;

impl PositionSource for RegistrySource {
    fn kind(&self) -> &'static str {
        "registry file"
    }

    fn read(&self, ctx: &SourceContext<'_>) -> Result<SourceReport, FormatError> {
        let f = crate::registry::read_latest(ctx.fs, ctx.store_dir, &ctx.store_id)?;
        Ok(SourceReport {
            highest: f
                .filter(|(_, f)| f.generation == ctx.generation)
                .map_or(0, |(_, f)| f.through),
            named: Vec::new(),
        })
    }
}

/// Every source a start reads, in order.
pub fn sources() -> Vec<Box<dyn PositionSource>> {
    vec![
        Box::new(RegistrySource),
        Box::new(crate::cursor::CursorSource),
        Box::new(crate::directory::DerivedSource),
    ]
}

/// Reads every source: the highest sequence any names and the consumers to name, sorted and without repeats.
pub fn read_all(ctx: &SourceContext<'_>) -> Result<SourceReport, FormatError> {
    let mut highest = 0;
    let mut named = BTreeSet::new();
    for s in sources() {
        let r = s.read(ctx)?;
        highest = highest.max(r.highest);
        named.extend(r.named);
    }
    Ok(SourceReport {
        highest,
        named: named.into_iter().collect(),
    })
}
