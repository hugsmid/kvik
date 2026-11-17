//! Derived files (`format.md` A.18): the log directory (kind 15), the series directory (kind 16) and the status file
//! (kind 17). Each is an A.1 header and one block, checksummed, written under a new `file_id` to a temporary name and
//! renamed, never synced; the previous generation of each family is kept, and a reader takes the newest valid one.
//!
//! - [`logdir`]: the root and its groups of 256 sealed segments.
//! - [`catalogue`]: what a log directory is checked against, and loading and rebuilding one.
//! - [`series`]: one entry per series, supplied by the crate that owns their meaning through
//!   [`crate::writer::SealHook`].
//! - [`status`]: the writer's counters, health and pressure flag for other processes.
//! - [`cadence`]: when the series directory is written, bounded in uncovered bytes.

pub mod cadence;
pub mod catalogue;
pub mod logdir;
pub mod series;
pub mod status;

use std::io;
use std::path::{Path, PathBuf};

use crate::block::{BLOCK_HEADER_LEN, BlockHeader, encode_block};
use crate::crc::crc32c;
use crate::error::{Element, EncodeError, FormatError};
use crate::fs::{Fs, OpenMode};
use crate::header::{FileHeader, HEADER_LEN};
use crate::mark::ResetStamp;
use crate::store::read_all;
use crate::writer::Position;

/// The directory derived files live in (`format.md` A.12).
pub const DERIVED_DIR: &str = "derived";

/// Block types of derived files (A.18).
pub mod block_type {
    /// The log directory's root.
    pub const ROOT: u8 = 0x50;
    /// A log directory group.
    pub const GROUP: u8 = 0x51;
    /// The series directory.
    pub const SERIES: u8 = 0x52;
    /// The status file.
    pub const STATUS: u8 = 0x53;
}

/// Why a derived file cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DerivedError {
    /// It fails a check of A.18: absent for every reader, rebuilt by the writer.
    #[error("invalid derived file: {0}")]
    Invalid(&'static str),
    /// A newer major version or an unknown critical element (common rules): a version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

/// A family of derived files: one name stem, generations told apart by `file_id` (A.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// `logdir-<file_id>.root`.
    Root,
    /// `logdir-<group>-<file_id>.grp`.
    Group(u32),
    /// `series-<file_id>.sdir`.
    Series,
    /// `status-<file_id>.stat`.
    Status,
}

impl Family {
    /// The file name of generation `file_id`.
    pub fn file_name(self, file_id: u64) -> String {
        match self {
            Family::Root => format!("logdir-{file_id:016x}.root"),
            Family::Group(g) => format!("logdir-{g:08x}-{file_id:016x}.grp"),
            Family::Series => format!("series-{file_id:016x}.sdir"),
            Family::Status => format!("status-{file_id:016x}.stat"),
        }
    }

    /// The family and generation a name gives, if it is one; names are matched exactly.
    pub fn parse(name: &str) -> Option<(Family, u64)> {
        let hex = |s: &str, n: usize| {
            s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let id = |s: &str| {
            hex(s, 16)
                .then(|| u64::from_str_radix(s, 16).ok())
                .flatten()
        };
        if let Some(s) = name.strip_prefix("logdir-") {
            if let Some(s) = s.strip_suffix(".root") {
                return Some((Family::Root, id(s)?));
            }
            let (g, f) = s.strip_suffix(".grp")?.split_once('-')?;
            if !hex(g, 8) {
                return None;
            }
            return Some((Family::Group(u32::from_str_radix(g, 16).ok()?), id(f)?));
        }
        if let Some(s) = name.strip_prefix("series-") {
            return Some((Family::Series, id(s.strip_suffix(".sdir")?)?));
        }
        let s = name.strip_prefix("status-")?.strip_suffix(".stat")?;
        Some((Family::Status, id(s)?))
    }

    /// The A.1 kind of the family.
    pub fn kind(self) -> u8 {
        match self {
            Family::Root | Family::Group(_) => crate::header::kind::LOG_DIRECTORY,
            Family::Series => crate::header::kind::SERIES_DIRECTORY,
            Family::Status => crate::header::kind::STATUS,
        }
    }

    /// The generations kept: two, so a power cut that leaves the newest invalid falls back to the previous; one
    /// status file.
    fn kept(self) -> usize {
        if self == Family::Status { 1 } else { 2 }
    }
}

/// The identity a derived file's header carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The store generation its positions belong to.
    pub generation: u32,
    /// Its generation within its family.
    pub file_id: u64,
    /// When it was written.
    pub written_at: i64,
}

/// The bytes of a derived file: the header and one block holding `body`.
pub(crate) fn encode_frame(
    kind: u8,
    block: u8,
    frame: Frame,
    store_id: [u8; 16],
    body: &[u8],
) -> Result<Vec<u8>, EncodeError> {
    let mut out = FileHeader::new(
        kind,
        frame.generation,
        frame.file_id,
        frame.written_at,
        store_id,
    )
    .encode()
    .to_vec();
    encode_block(block, 0, body, &mut out)?;
    Ok(out)
}

/// The header's identity and the body of a derived file of `kind` whose block is `block` (A.18 checks: a valid header
/// of this store and kind, one valid block of the expected type, nothing after it).
pub(crate) fn decode_frame<'a>(
    bytes: &'a [u8],
    store_id: &[u8; 16],
    kind: u8,
    block: u8,
) -> Result<(Frame, &'a [u8]), DerivedError> {
    let h = FileHeader::decode(bytes).ok_or(DerivedError::Invalid("header"))?;
    if h.format_major > crate::FORMAT_MAJOR {
        return Err(DerivedError::Unsupported(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(h.format_major),
        }));
    }
    if h.kind != kind || &h.store_id != store_id {
        return Err(DerivedError::Invalid("kind or store"));
    }
    let head: &[u8; BLOCK_HEADER_LEN] = bytes
        .get(HEADER_LEN..HEADER_LEN + BLOCK_HEADER_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or(DerivedError::Invalid("block header"))?;
    let bh = BlockHeader::decode(head).ok_or(DerivedError::Invalid("block header"))?;
    let start = HEADER_LEN + BLOCK_HEADER_LEN;
    let body = usize::try_from(bh.body_len)
        .ok()
        .and_then(|n| bytes.get(start..start.checked_add(n)?))
        .ok_or(DerivedError::Invalid("body length"))?;
    if crc32c(body) != bh.body_crc32c {
        return Err(DerivedError::Invalid("body checksum"));
    }
    if bh.block_type != block {
        let known = (block_type::ROOT..=block_type::STATUS).contains(&bh.block_type);
        return Err(if known || bh.skippable() {
            DerivedError::Invalid("block type")
        } else {
            DerivedError::Unsupported(FormatError::Unsupported {
                offset: HEADER_LEN as u64,
                element: Element::BlockType(bh.block_type),
            })
        });
    }
    if u64::try_from(bytes.len()).ok() != Some(HEADER_LEN as u64 + bh.block_len()) {
        return Err(DerivedError::Invalid("bytes after the block"));
    }
    Ok((
        Frame {
            generation: h.generation,
            file_id: h.file_id,
            written_at: h.created_at,
        },
        body,
    ))
}

/// A fixed-size record array inside a body (A.8's rule): `count` records of `size` bytes from `at`, each at least
/// `min` bytes; a record's tail beyond `min` is ignored.
pub(crate) fn records(
    body: &[u8],
    at: usize,
    count: u32,
    size: u16,
    min: usize,
) -> Result<impl Iterator<Item = &[u8]>, DerivedError> {
    let size = usize::from(size);
    let count = usize::try_from(count).map_err(|_| DerivedError::Invalid("record count"))?;
    if count > 0 && (size < min || size == 0) {
        return Err(DerivedError::Invalid("record size"));
    }
    let len = count
        .checked_mul(size)
        .ok_or(DerivedError::Invalid("record count"))?;
    let all = at
        .checked_add(len)
        .and_then(|end| body.get(at..end))
        .ok_or(DerivedError::Invalid("records past the body"))?;
    Ok((0..count).map(move |i| &all[i * size..(i + 1) * size]))
}

pub(crate) fn put_position(b: &mut Vec<u8>, p: Position) {
    b.extend_from_slice(&p.generation.to_le_bytes());
    b.extend_from_slice(&p.sequence.to_le_bytes());
}

pub(crate) fn position_at(b: &[u8], at: usize) -> Position {
    Position {
        generation: crate::bytes::u32_at(b, at),
        sequence: crate::bytes::u64_at(b, at + 4),
    }
}

pub(crate) fn put_stamp(b: &mut Vec<u8>, s: Option<ResetStamp>) {
    let s = s.unwrap_or(ResetStamp {
        sequence: 0,
        nonce: 0,
    });
    b.extend_from_slice(&s.sequence.to_le_bytes());
    b.extend_from_slice(&s.nonce.to_le_bytes());
}

/// A zero nonce is no stamp (A.10).
pub(crate) fn stamp_at(b: &[u8], at: usize) -> Option<ResetStamp> {
    let nonce = crate::bytes::u64_at(b, at + 8);
    (nonce != 0).then(|| ResetStamp {
        sequence: crate::bytes::u64_at(b, at),
        nonce,
    })
}

/// Every derived file in `store_dir`, newest generation first within each family; other names are ignored.
pub fn list(fs: &dyn Fs, store_dir: &Path) -> io::Result<Vec<(Family, u64, PathBuf)>> {
    let dir = store_dir.join(DERIVED_DIR);
    let mut out: Vec<_> = match fs.read_dir(&dir) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    .into_iter()
    .filter_map(|e| Family::parse(&e.name).map(|(f, id)| (f, id, dir.join(&e.name))))
    .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    Ok(out)
}

/// The generations of `family`, newest first.
pub fn generations(fs: &dyn Fs, store_dir: &Path, family: Family) -> Vec<(u64, PathBuf)> {
    list(fs, store_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|(f, _, _)| *f == family)
        .map(|(_, id, p)| (id, p))
        .collect()
}

/// The newest generation of `family` that `decode` accepts and whose header's `file_id` is its name's, with its
/// `file_id`; `None` when none is. A version refusal is returned at once.
pub fn read_latest<T>(
    fs: &dyn Fs,
    store_dir: &Path,
    family: Family,
    decode: impl Fn(&[u8]) -> Result<(Frame, T), DerivedError>,
) -> Result<Option<(Frame, T)>, FormatError> {
    for (id, path) in generations(fs, store_dir, family) {
        let Ok(bytes) = read_all(fs, &path) else {
            continue;
        };
        match decode(&bytes) {
            Ok((frame, v)) if frame.file_id == id => return Ok(Some((frame, v))),
            Ok(_) | Err(DerivedError::Invalid(_)) => continue,
            Err(DerivedError::Unsupported(e)) => return Err(e),
        }
    }
    Ok(None)
}

/// Writes generation `file_id` of `family` (A.18 Writing): to `<name>.tmp`, renamed into place, never synced; then
/// removes the generations older than the ones the family keeps and any temporary file of the family. The
/// `derived/` directory is created on first use.
pub fn write_file(
    fs: &dyn Fs,
    store_dir: &Path,
    family: Family,
    file_id: u64,
    bytes: &[u8],
) -> io::Result<()> {
    let dir = store_dir.join(DERIVED_DIR);
    fs.create_dir_all(&dir)?;
    let name = family.file_name(file_id);
    let tmp = dir.join(format!("{name}.tmp"));
    match fs.remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let f = fs.open(&tmp, OpenMode::CreateNew)?;
    let written = f.write_at(bytes, 0);
    drop(f);
    if let Err(e) = written.and_then(|()| fs.rename(&tmp, &dir.join(&name))) {
        let _ = fs.remove_file(&tmp);
        return Err(e);
    }
    for (id, path) in generations(fs, store_dir, family)
        .into_iter()
        .skip(family.kept())
    {
        if id < file_id {
            fs.remove_file(&path)?;
        }
    }
    Ok(())
}

/// Removes temporary files a crash left in `derived/` (A.18): a writer does this at its start.
pub fn remove_temporaries(fs: &dyn Fs, store_dir: &Path) -> io::Result<()> {
    let dir = store_dir.join(DERIVED_DIR);
    let Ok(entries) = fs.read_dir(&dir) else {
        return Ok(());
    };
    for e in entries {
        if e.name
            .strip_suffix(".tmp")
            .and_then(Family::parse)
            .is_some()
        {
            fs.remove_file(&dir.join(&e.name))?;
        }
    }
    Ok(())
}

/// Removes every generation of `family`.
pub fn remove_family(fs: &dyn Fs, store_dir: &Path, family: Family) -> io::Result<()> {
    for (_, path) in generations(fs, store_dir, family) {
        fs.remove_file(&path)?;
    }
    Ok(())
}

/// The derived files as a source of F (`spec.md` §3 Recovery rule 3): the highest position the valid root and series
/// directory of the current generation name (the status file's synced mark is diagnostic and names none). A file of a newer version is passed over: the writer
/// writes none of its family, and none names a position above the last synced mark.
#[derive(Clone, Copy, Debug, Default)]
pub struct DerivedSource;

impl crate::writer::sources::PositionSource for DerivedSource {
    fn kind(&self) -> &'static str {
        "derived file"
    }

    fn read(
        &self,
        ctx: &crate::writer::sources::SourceContext<'_>,
    ) -> Result<crate::writer::sources::SourceReport, FormatError> {
        let id = ctx.store_id;
        let at = |p: Position| (p.generation == ctx.generation).then_some(p.sequence);
        let root = read_latest(ctx.fs, ctx.store_dir, Family::Root, |b| {
            logdir::Root::decode(b, &id)
        })
        .ok()
        .flatten()
        .and_then(|(_, r)| at(r.through));
        let series = read_latest(ctx.fs, ctx.store_dir, Family::Series, |b| {
            series::SeriesDirectory::decode(b, &id)
        })
        .ok()
        .flatten()
        .and_then(|(_, d)| at(d.through));
        Ok(crate::writer::sources::SourceReport {
            highest: root.max(series).unwrap_or(0),
            named: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MemFs;

    #[test]
    fn a_huge_count_of_empty_records_is_invalid_and_allocates_nothing() {
        assert!(records(&[0; 8], 0, u32::MAX, 0, 0).is_err());
        assert!(records(&[0; 8], 0, u32::MAX, 8, 8).is_err());
        assert_eq!(records(&[0; 8], 0, 0, 0, 0).unwrap().count(), 0);
        assert_eq!(records(&[0; 16], 0, 2, 8, 4).unwrap().count(), 2);
    }

    #[test]
    fn names_round_trip_and_are_matched_exactly() {
        for f in [
            Family::Root,
            Family::Group(7),
            Family::Series,
            Family::Status,
        ] {
            assert_eq!(Family::parse(&f.file_name(0x1a)), Some((f, 0x1a)));
        }
        for bad in [
            "logdir-1a.root",
            "logdir-000000000000001A.root",
            "logdir-0000007-000000000000001a.grp",
            "series-000000000000001a.sdir.tmp",
            "status-000000000000001a",
        ] {
            assert_eq!(Family::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn writing_keeps_the_previous_generation_and_one_status_file() {
        let fs = MemFs::new();
        let dir = Path::new("s");
        for id in 1..=4 {
            write_file(&fs, dir, Family::Series, id, &[id as u8]).unwrap();
            write_file(&fs, dir, Family::Status, id, &[id as u8]).unwrap();
        }
        let ids = |f| {
            generations(&fs, dir, f)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Family::Series), vec![4, 3]);
        assert_eq!(ids(Family::Status), vec![4]);
        assert!(
            fs.ops()
                .iter()
                .all(|o| !matches!(o.op, crate::fs::FaultOp::Sync | crate::fs::FaultOp::SyncDir)),
            "derived files are never synced"
        );
    }

    #[test]
    fn a_frame_refuses_trailing_bytes_other_kinds_and_unknown_critical_blocks() {
        let frame = Frame {
            generation: 0,
            file_id: 1,
            written_at: 5,
        };
        let kind = crate::header::kind::STATUS;
        let ok = encode_frame(kind, block_type::STATUS, frame, [1; 16], &[0; 8]).unwrap();
        assert!(decode_frame(&ok, &[1; 16], kind, block_type::STATUS).is_ok());
        let mut long = ok.clone();
        long.extend_from_slice(&[0; 8]);
        assert!(matches!(
            decode_frame(&long, &[1; 16], kind, block_type::STATUS),
            Err(DerivedError::Invalid(_))
        ));
        assert!(matches!(
            decode_frame(&ok, &[2; 16], kind, block_type::STATUS),
            Err(DerivedError::Invalid(_))
        ));
        let other = encode_frame(kind, 0x5f, frame, [1; 16], &[0; 8]).unwrap();
        assert!(matches!(
            decode_frame(&other, &[1; 16], kind, block_type::STATUS),
            Err(DerivedError::Unsupported(_))
        ));
        let mut skippable = Vec::from(FileHeader::new(kind, 0, 1, 5, [1; 16]).encode());
        encode_block(0x5f, crate::block::FLAG_SKIPPABLE, &[0; 8], &mut skippable).unwrap();
        assert!(matches!(
            decode_frame(&skippable, &[1; 16], kind, block_type::STATUS),
            Err(DerivedError::Invalid(_))
        ));
    }
}
