//! The new directory: what a restore finds there, and its writes (`spec.md` §8 Restore, Failures and end; Q-39 (f)).
//! The restore record reaches `MANIFEST` whole, by a synced `MANIFEST.new` renamed into place, so a directory holds
//! either the record or only that leftover.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use kvik_files::appender::{Appender, Opened};
use kvik_files::desc::FileDescription;
use kvik_files::image;
use kvik_files::record::{Body, Record};
use kvik_files::restore::RestoreRecord;
use kvik_files::{MANIFEST, MANIFEST_NEW};
use kvik_log::fs::OpenMode;
use kvik_log::header::{FileHeader, kind};
use kvik_log::manifest::LostRange;
use kvik_log::name::SegmentName;
use kvik_log::store::{LOG_DIR, STORE};

use super::{RestoreError, codec};
use crate::LocalStore;

const STORE_TMP: &str = "STORE.tmp";

pub(crate) fn invalid(what: impl Into<String>) -> RestoreError {
    RestoreError::Io(io::Error::new(io::ErrorKind::InvalidData, what.into()))
}

/// What the directory holds.
pub(crate) enum Found {
    /// Nothing.
    Empty,
    /// Only what a restore writes before its record: removed, and the restore starts again (Q-39 (f)).
    Leftovers,
    /// A restore record: the restore resumes.
    Resume {
        store_id: [u8; 16],
        /// The manifest record holding it, as written.
        written: Box<Record>,
        record: Box<RestoreRecord>,
        /// The lost ranges recorded since.
        lost: Vec<LostRange>,
    },
    /// Files and no restore record.
    NotEmpty,
}

fn read(local: &LocalStore, name: &str) -> io::Result<Vec<u8>> {
    let f = local.fs.open(&local.dir.join(name), OpenMode::Read)?;
    let mut b = vec![0u8; usize::try_from(f.len()?).map_err(io::Error::other)?];
    let mut at = 0;
    while at < b.len() {
        match f.read_at(&mut b[at..], at as u64)? {
            0 => b.truncate(at),
            n => at += n,
        }
    }
    Ok(b)
}

fn names(local: &LocalStore, dir: &Path) -> io::Result<BTreeSet<String>> {
    match local.fs.read_dir(dir) {
        Ok(es) => Ok(es.into_iter().map(|e| e.name).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e),
    }
}

pub(crate) fn inspect(local: &LocalStore) -> Result<Found, RestoreError> {
    let present = names(local, &local.dir)?;
    if present.is_empty() {
        return Ok(Found::Empty);
    }
    if present.len() == 1 && present.contains(MANIFEST_NEW) {
        return Ok(Found::Leftovers);
    }
    if !present.contains(MANIFEST) {
        return Ok(Found::NotEmpty);
    }
    let bytes = read(local, MANIFEST)?;
    let Some(h) = FileHeader::decode(&bytes).filter(|h| h.kind == kind::MANIFEST) else {
        return Ok(Found::NotEmpty);
    };
    let Ok(img) = image::read(&bytes, &h.store_id) else {
        return Ok(Found::NotEmpty);
    };
    let mut found = None;
    let mut lost = Vec::new();
    for r in img.records() {
        if let Some(rec) = codec::from_body(&r.body) {
            if found.is_none() {
                found = Some((Box::new(r.clone()), Box::new(rec?)));
            }
        } else if let Body::LostRange(l) = &r.body {
            lost.push(l.clone());
        }
    }
    Ok(match found {
        Some((written, record)) => Found::Resume {
            store_id: h.store_id,
            written,
            record,
            lost,
        },
        None => Found::NotEmpty,
    })
}

pub(crate) fn clear_leftovers(local: &LocalStore) -> io::Result<()> {
    local.fs.remove_file(&local.dir.join(MANIFEST_NEW))?;
    local.fs.sync_dir(&local.dir)
}

fn write_whole(local: &LocalStore, tmp: &Path, to: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = to.parent().map_or_else(|| local.dir.clone(), PathBuf::from);
    match local.fs.remove_file(tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let f = local.fs.open(tmp, OpenMode::CreateNew)?;
    f.write_at(bytes, 0)?;
    f.sync_all()?;
    drop(f);
    local.fs.rename(tmp, to)?;
    local.fs.sync_dir(&dir)
}

/// The manifest of generation `generation` whose first record is `record`.
pub(crate) fn write_manifest(
    local: &LocalStore,
    store_id: [u8; 16],
    record: &Record,
    created_at: i64,
) -> Result<(), RestoreError> {
    let mut bytes = FileHeader::new(kind::MANIFEST, record.generation, 0, created_at, store_id)
        .encode()
        .to_vec();
    bytes.extend(
        record
            .encode()
            .map_err(|e| RestoreError::Append(e.into()))?,
    );
    local.fs.create_dir_all(&local.dir)?;
    write_whole(
        local,
        &local.dir.join(MANIFEST_NEW),
        &local.dir.join(MANIFEST),
        &bytes,
    )?;
    Ok(())
}

pub(crate) fn open_appender(
    local: &LocalStore,
    store_id: [u8; 16],
) -> Result<Appender, RestoreError> {
    match Appender::open(&*local.fs, &local.dir, store_id, 0) {
        Ok(Opened::Ready(a)) => Ok(*a),
        Ok(Opened::Corrupt(_)) => Err(invalid("the restore's manifest holds a corrupt record")),
        Err(e) => Err(invalid(format!("the restore's manifest: {e}"))),
    }
}

/// Writes `STORE` unchanged (`format.md` A.12) unless it is there.
pub(crate) fn write_store(local: &LocalStore, bytes: &[u8]) -> io::Result<()> {
    if names(local, &local.dir)?.contains(STORE) {
        return Ok(());
    }
    write_whole(
        local,
        &local.dir.join(STORE_TMP),
        &local.dir.join(STORE),
        bytes,
    )
}

fn segment_name(d: &FileDescription) -> String {
    SegmentName {
        generation: d.generation,
        file_id: d.file_id,
        sealed: true,
    }
    .to_string()
}

/// The names under `log/`.
pub(crate) fn installed(local: &LocalStore) -> io::Result<BTreeSet<String>> {
    names(local, &local.dir.join(LOG_DIR))
}

pub(crate) fn is_installed(names: &BTreeSet<String>, d: &FileDescription) -> bool {
    names.contains(&segment_name(d))
}

/// Installs a verified log segment under its name (`format.md` A.12): synced, renamed into place, directory synced.
pub(crate) fn install(local: &LocalStore, d: &FileDescription, plain: &[u8]) -> io::Result<()> {
    let dir = local.dir.join(LOG_DIR);
    local.fs.create_dir_all(&dir)?;
    let name = segment_name(d);
    write_whole(
        local,
        &dir.join(format!("{name}.tmp")),
        &dir.join(name),
        plain,
    )
}
