//! Publish (compaction design §3.2, last paragraph): a run's verified outputs renamed to their final names, `data/`
//! synced, then one publish record naming every output with its layout, synced. What a stopped run leaves is cleaned
//! up at the next start (§3.5), and a rebuild judges outputs no valid record names by compacting their inputs again
//! ([`Recompaction`]).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use kvik_chunk::file::{Class, CompactedFile, Identity, Layout};
use kvik_files::appender::{AppendError, Appender};
use kvik_files::catalogue::Catalogue;
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::layout::CompactedLayout;
use kvik_files::rebuild::{LocalFile, Verdict, Verifier};
use kvik_files::record::{Body, Publish};
use kvik_log::fs::{Fs, OpenMode};
use kvik_log::setaside::SetAside;
use sha2::{Digest, Sha256};

use super::Output;
use super::plan::candidates;
use super::run::{DATA_DIR, RunInput, read_whole, run};

/// Why a run's outputs were not published.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PublishError {
    /// A rename or the directory sync failed; the outputs were removed and the run is planned again.
    #[error("publish: {0}")]
    Io(#[from] io::Error),
    /// The manifest refused the record before writing it; the outputs were removed.
    #[error(transparent)]
    Refused(AppendError),
    /// The record's write or sync failed: it may be on disk, so the outputs stay for the next start to judge.
    #[error("publish record: {0}")]
    Unsure(AppendError),
}

/// The final name of a compacted segment (`format-compacted.md` B.1 Name).
pub(crate) fn final_path(dir: &Path, generation: u32, file_id: u64) -> PathBuf {
    dir.join(DATA_DIR)
        .join(format!("{generation:08x}-{file_id:016x}.kvc"))
}

/// The generation and `file_id` a final name gives.
fn parse_final(name: &str) -> Option<(u32, u64)> {
    let stem = name.strip_suffix(".kvc")?;
    let (g, id) = stem.split_once('-')?;
    if g.len() != 8 || id.len() != 16 {
        return None;
    }
    Some((
        u32::from_str_radix(g, 16).ok()?,
        u64::from_str_radix(id, 16).ok()?,
    ))
}

/// What a publish record says of an output: its file description.
pub(crate) fn description(o: &Output) -> FileDescription {
    let l = &o.layout;
    FileDescription {
        kind: kind::COMPACTED,
        class: match l.class {
            Class::Current => FileClass::Current,
            Class::Revision => FileClass::Revision,
        },
        generation: l.generation,
        file_id: l.file_id,
        range: Some(l.range),
        hash: o.sha256,
        inputs: l
            .inputs
            .iter()
            .map(|i| FileRef {
                kind: i.kind,
                file_id: i.file_id,
            })
            .collect(),
    }
}

/// Publish record field 3 of a file whose layout the reader gives (Q-1850).
pub(crate) fn layout(l: &Layout) -> CompactedLayout {
    CompactedLayout {
        file_id: l.file_id,
        level: l.level as u8,
        period_start: l.period_start,
        period_len: l.period_len,
        run_id: l.run_id,
        run_count: l.run_count,
        min_t: l.min_t,
        max_reach: l.max_reach,
        first_commit_time: l.first_commit_time,
        last_commit_time: l.last_commit_time,
        fact_count: l.fact_count,
        positions: l.positions.clone(),
        has_entries: l.has_entries,
        has_set_aside: l.has_set_aside,
        complete: l.complete,
        name_range: l.name_range.clone(),
    }
}

/// The publish record of `outputs`: every output, ascending by `file_id`, each with its layout; a run retires
/// nothing.
pub(crate) fn record(outputs: &[Output]) -> Body {
    let mut outs: Vec<&Output> = outputs.iter().collect();
    outs.sort_by_key(|o| o.id.file_id);
    Body::Publish(Publish {
        published: outs.iter().map(|o| description(o)).collect(),
        retired: Vec::new(),
        layouts: outs.iter().map(|o| layout(&o.layout)).collect(),
    })
}

/// Publishes a run's verified outputs: renames each to its final name, syncs `data/`, appends one publish record and
/// syncs it; returns its epoch. No output is visible to a reader of the manifest before the record is synced.
pub(crate) fn publish(
    fs: &dyn Fs,
    dir: &Path,
    manifest: &mut Appender,
    outputs: &[Output],
    written_at: i64,
) -> Result<u64, PublishError> {
    // An unsure record may be on disk naming final names these outputs would take (Q-2125).
    if manifest.is_faulted() {
        return Err(PublishError::Refused(AppendError::ReadOnly));
    }
    let finals: Vec<PathBuf> = outputs
        .iter()
        .map(|o| final_path(dir, o.id.generation, o.id.file_id))
        .collect();
    let moved = outputs
        .iter()
        .zip(&finals)
        .try_for_each(|(o, f)| fs.rename(&o.path, f))
        .and_then(|()| fs.sync_dir(&dir.join(DATA_DIR)));
    if let Err(e) = moved {
        remove(fs, dir, outputs, &finals);
        return Err(e.into());
    }
    match manifest.append(record(outputs), written_at) {
        Ok(epoch) => Ok(epoch),
        Err(e @ AppendError::Io(_)) => Err(PublishError::Unsure(e)),
        Err(e) => {
            remove(fs, dir, outputs, &finals);
            Err(PublishError::Refused(e))
        }
    }
}

/// Removes every output under either name, then syncs `data/`; best effort, as the next start removes what is left.
fn remove(fs: &dyn Fs, dir: &Path, outputs: &[Output], finals: &[PathBuf]) {
    for p in outputs.iter().map(|o| &o.path).chain(finals) {
        let _ = fs.remove_file(p);
    }
    let _ = fs.sync_dir(&dir.join(DATA_DIR));
}

/// What the start's clean-up of `data/` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CleanUp {
    /// Files removed.
    pub(crate) removed: Vec<PathBuf>,
    /// Compacted segments no valid record names, kept because the manifest is not known valid in this start or a
    /// rebuild holds them.
    pub(crate) kept: Vec<PathBuf>,
}

/// The start's clean-up of `data/` (§3.5): temporary outputs, spill files and index extractions are removed; a
/// compacted segment no valid record names is removed only when this start knows the manifest valid (no corrupt
/// record, no rebuild in this start), and never while `held` names it.
pub(crate) fn clean_data(
    fs: &dyn Fs,
    dir: &Path,
    catalogue: &Catalogue,
    manifest_valid: bool,
    held: &BTreeSet<FileRef>,
) -> io::Result<CleanUp> {
    let data = dir.join(DATA_DIR);
    let entries = match fs.read_dir(&data) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(CleanUp::default()),
        Err(e) => return Err(e),
    };
    let mut out = CleanUp::default();
    for e in entries {
        let path = data.join(&e.name);
        let temporary = [".tmp", ".spill"].iter().any(|s| e.name.ends_with(s));
        let unnamed = parse_final(&e.name).map(|(_, file_id)| FileRef {
            kind: kind::COMPACTED,
            file_id,
        });
        let unnamed = unnamed.filter(|f| catalogue.file(*f).is_none());
        if temporary || unnamed.is_some_and(|f| manifest_valid && !held.contains(&f)) {
            fs.remove_file(&path)?;
            out.removed.push(path);
        } else if unnamed.is_some() {
            out.kept.push(path);
        }
    }
    if !out.removed.is_empty() {
        fs.sync_dir(&data)?;
    }
    Ok(out)
}

/// The rebuild's verifier (§3.5): compacts a run's input segments again, with the run's identity, and compares each
/// output found with the file of the same identity. The run is deterministic, so a file that is not byte for byte
/// the one compaction makes fails; a run whose found files all match but are fewer than its count cannot be judged.
pub(crate) struct Recompaction<'a> {
    /// The store's file system.
    pub(crate) fs: &'a dyn Fs,
    /// The store directory.
    pub(crate) dir: &'a Path,
    /// The store's id.
    pub(crate) store_id: [u8; 16],
    /// Positions set aside beyond a restored end.
    pub(crate) set_aside: &'a SetAside,
    /// Weeks whose merge is published.
    pub(crate) weeks_merged: &'a BTreeSet<i64>,
    /// The definition of a series in force below the run's first position.
    pub(crate) registry: &'a (dyn Fn(u32) -> Option<kvik_log::entries::Definition> + Sync),
}

impl Recompaction<'_> {
    fn judge(&self, outputs: &[&LocalFile]) -> io::Result<Verdict> {
        let Some(first) = outputs.first() else {
            return Ok(Verdict::CannotSay);
        };
        let Some(l) = first.layout.as_ref() else {
            return Ok(Verdict::CannotSay);
        };
        let d = &first.description;
        if d.inputs.iter().any(|i| i.kind != kind::LOG_SEGMENT) {
            return Ok(Verdict::CannotSay);
        }
        let path = final_path(self.dir, d.generation, d.file_id);
        let bytes = read_whole(&*self.fs.open(&path, OpenMode::Read)?)?;
        let id = Identity {
            store_id: self.store_id,
            generation: d.generation,
            file_id: d.file_id,
        };
        let Ok(file) = CompactedFile::open(bytes, &id, None) else {
            return Ok(Verdict::Fails);
        };
        let created_at = file.header().created_at;
        let wanted: BTreeSet<u64> = d.inputs.iter().map(|i| i.file_id).collect();
        let segments: Vec<_> = candidates(self.fs, self.dir, &self.store_id)?
            .into_iter()
            .filter(|c| wanted.contains(&c.file_id))
            .collect();
        if segments.len() != wanted.len() {
            return Ok(Verdict::CannotSay);
        }
        let input = RunInput {
            fs: self.fs,
            dir: self.dir,
            store_id: self.store_id,
            generation: d.generation,
            first_file_id: l.run_id,
            created_at,
            set_aside: self.set_aside,
            weeks_merged: self.weeks_merged,
            registry: self.registry,
        };
        let Ok(again) = run(&input, &segments) else {
            return Ok(Verdict::CannotSay);
        };
        let mut verdict = if again.len() == outputs.len() {
            Verdict::Holds
        } else {
            Verdict::CannotSay
        };
        for o in outputs {
            let d = &o.description;
            let same = match again.iter().find(|a| a.id.file_id == d.file_id) {
                Some(a) => {
                    let found = read_whole(&*self.fs.open(
                        &final_path(self.dir, d.generation, d.file_id),
                        OpenMode::Read,
                    )?)?;
                    a.sha256 == <[u8; 32]>::from(Sha256::digest(&found))
                }
                None => false,
            };
            if !same {
                verdict = Verdict::Fails;
            }
        }
        for a in &again {
            let _ = self.fs.remove_file(&a.path);
        }
        Ok(verdict)
    }
}

impl Verifier for Recompaction<'_> {
    fn verify(&self, outputs: &[&LocalFile], _inputs: &[&LocalFile]) -> Verdict {
        self.judge(outputs).unwrap_or(Verdict::CannotSay)
    }
}
