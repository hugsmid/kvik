//! Step 4 of the rebuild: the new manifest written as `MANIFEST.new`, synced and renamed over `MANIFEST`, and the
//! procedure of the operator call `rebuild_manifest`.

use std::io;
use std::path::Path;

use kvik_log::fs::{Fs, OpenMode};
use kvik_log::header::{FileHeader, kind as file_kind};

use super::{FoundLoss, LocalFile, RebuildError, RebuildInput, RebuildPlan, Verifier, plan};
use crate::appender::{Appender, Gap, OpenError, Opened, RebuildState, read_file, remove_leftover};
use crate::image::{ReadError, read};
use crate::record::{EncodeError, Record};
use crate::{MANIFEST, MANIFEST_NEW};

/// The bytes of a manifest holding `records`.
pub fn encode_manifest(
    records: &[Record],
    store_id: [u8; 16],
    generation: u32,
    created_at: i64,
) -> Result<Vec<u8>, EncodeError> {
    let mut out = FileHeader::new(file_kind::MANIFEST, generation, 0, created_at, store_id)
        .encode()
        .to_vec();
    for r in records {
        out.extend(r.encode()?);
    }
    Ok(out)
}

/// Writes `bytes` as `MANIFEST.new`, syncs it, renames it over `MANIFEST` and syncs the directory (`spec.md` §7
/// step 4). No handle may hold `MANIFEST` open. A crash leaves the old manifest, or the new one, and perhaps a
/// `MANIFEST.new` that the next open removes.
pub fn replace_manifest(fs: &dyn Fs, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    remove_leftover(fs, dir)?;
    let new = dir.join(MANIFEST_NEW);
    let f = fs.open(&new, OpenMode::CreateNew)?;
    f.write_at(bytes, 0)?;
    f.sync_all()?;
    drop(f);
    fs.rename(&new, &dir.join(MANIFEST))?;
    fs.sync_dir(dir)
}

/// Writes the rebuilt manifest ([`replace_manifest`]).
pub fn write(
    fs: &dyn Fs,
    dir: &Path,
    store_id: [u8; 16],
    plan: &RebuildPlan,
    created_at: i64,
) -> Result<(), WriteError> {
    let generation = plan.records.first().map_or(0, |r| r.generation);
    let bytes = encode_manifest(&plan.records, store_id, generation, created_at)?;
    replace_manifest(fs, dir, &bytes)?;
    Ok(())
}

/// Why writing a rebuilt or repaired manifest failed.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A file-system operation failed.
    #[error("manifest: {0}")]
    Io(#[from] io::Error),
    /// A record cannot be encoded.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Why `rebuild_manifest` failed.
#[derive(Debug, thiserror::Error)]
pub enum RebuildManifestError {
    /// The manifest could not be read.
    #[error(transparent)]
    Read(#[from] ReadError),
    /// It cannot be rebuilt.
    #[error(transparent)]
    Rebuild(#[from] RebuildError),
    /// The new manifest could not be written.
    #[error(transparent)]
    Write(#[from] WriteError),
    /// The new manifest could not be opened.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// The new manifest is not readable as written.
    #[error("the rebuilt manifest does not read back")]
    NotReadBack,
    /// Reading the old manifest failed.
    #[error("manifest: {0}")]
    Io(#[from] io::Error),
}

/// Whether the rebuild is final or provisional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finality {
    /// The mirror path and deep have answered that they hold no copy, or are not configured.
    Final,
    /// A copy source cannot be reached: uploads, evictions, removals and retirements are held until both answer.
    Provisional,
}

/// The local inputs of a rebuild besides the manifest.
#[derive(Clone, Copy, Debug)]
pub struct LocalState<'a> {
    /// Every local file found.
    pub files: &'a [LocalFile],
    /// The losses the scan of the log finds.
    pub losses: &'a [FoundLoss],
    /// The highest epoch any lease file names.
    pub lease_epoch: u64,
    /// The mark's `file_id`.
    pub mark_file_id: u64,
}

/// The operator call `rebuild_manifest` and the automatic rebuild (`spec.md` §7 Manifest): reads `MANIFEST`,
/// plans the rebuild, writes it, and opens the new manifest, provisional or final. No handle may hold `MANIFEST`.
pub fn rebuild_manifest(
    fs: &dyn Fs,
    dir: &Path,
    store_id: [u8; 16],
    state: LocalState<'_>,
    verifier: &dyn Verifier,
    finality: Finality,
    written_at: i64,
) -> Result<(Appender, RebuildPlan), RebuildManifestError> {
    remove_leftover(fs, dir)?;
    let bytes = read_file(fs, &dir.join(MANIFEST))?;
    let image = read(&bytes, &store_id)?;
    let p = plan(
        &RebuildInput {
            image: &image,
            files: state.files,
            losses: state.losses,
            lease_epoch: state.lease_epoch,
            mark_file_id: state.mark_file_id,
            written_at,
        },
        verifier,
    )?;
    write(fs, dir, store_id, &p, written_at)?;
    let Opened::Ready(mut app) = Appender::open(fs, dir, store_id, state.lease_epoch)? else {
        return Err(RebuildManifestError::NotReadBack);
    };
    // In this run every corrupt span is known, with the epochs around it; after a restart only the first is, from the
    // rebuild record's fields 1 and 2.
    let gaps: Vec<Gap> = image
        .corrupt
        .iter()
        .map(|s| Gap {
            after: s.after_epoch,
            before: s.before_epoch,
        })
        .collect();
    match finality {
        Finality::Provisional if !gaps.is_empty() => app.set_rebuild_state(
            RebuildState::Provisional {
                epoch: p.rebuild_epoch,
            },
            gaps,
        ),
        _ => app.set_rebuild_state(
            RebuildState::Final {
                epoch: p.rebuild_epoch,
            },
            Vec::new(),
        ),
    }
    Ok((*app, p))
}
