//! The registry file (`format.md` A.16, `spec.md` §2.1): the registry entries of the kept log, each with its position,
//! compacted into one file on the hot tier, with the id floor its writer knew.
//!
//! A registry file is written whole under a new name, synced with its directory, and only then are older ones
//! removed, so a crash at any point leaves the previous file or the new one. A reader takes the valid file with the
//! highest `file_id`; a file that is not valid is passed over, and a file of a newer major version, or with an unknown
//! critical element, is a version refusal that stops the start.
//!
//! ```
//! use kvik_log::entries::Entry;
//! use kvik_log::registry::{RegistryFile, decode, encode};
//!
//! let file = RegistryFile {
//!     generation: 0,
//!     through: 3,
//!     max_series_id: 1,
//!     entries: vec![(2, 10, Entry::Alias { id: 1, name: "a/b/c".into() })],
//! };
//! let bytes = encode(&file, [1; 16], 7, 10).unwrap();
//! assert_eq!(decode(&bytes, &[1; 16]).unwrap(), file);
//! ```

use std::io;
use std::path::{Path, PathBuf};

use crate::block::{BLOCK_HEADER_LEN, BlockHeader, MAX_BODY_LEN, block_type, encode_block};
use crate::bytes::{u32_at, u64_at};
use crate::content_error::BodyError;
use crate::crc::crc32c;
use crate::entries::{EntriesBlock, Entry, entry_len};
use crate::error::{Element, EncodeError, FormatError};
use crate::fs::{Fs, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::store::read_all;

/// The directory registry files live in (`format.md` A.12).
pub const REGISTRY_DIR: &str = "registry";
/// The extension of a registry file.
pub const EXTENSION: &str = "reg";
/// The block type of the summary that ends a registry file (A.16).
pub const SUMMARY: u8 = 0x40;
/// The summary body's length.
pub const SUMMARY_LEN: usize = 24;

/// A registry file's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryFile {
    /// The generation its entries' positions belong to (its header's).
    pub generation: u32,
    /// The file holds every registry entry of the kept log with a sequence up to this one.
    pub through: u64,
    /// The highest series id its writer knew, from the mark, the log, recovery entries and earlier registry files:
    /// an id floor (`spec.md` §2.1). 0 when none.
    pub max_series_id: u32,
    /// Registry entries in position order: sequence, the receive time of its batch, the entry.
    pub entries: Vec<(u64, i64, Entry)>,
}

impl RegistryFile {
    /// The highest series id the file names: its floor, and every id it defines.
    pub fn floor(&self) -> u32 {
        self.entries
            .iter()
            .filter_map(|(_, _, e)| match e {
                Entry::Definition(d) => Some(d.id),
                Entry::Recovery(r) => Some(r.id_floor),
                _ => None,
            })
            .fold(self.max_series_id, u32::max)
    }
}

/// Why a registry file cannot be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// It fails a check of A.16: not a registry file, torn or damaged. It is passed over.
    #[error("invalid registry file at offset {0}")]
    Invalid(u64),
    /// A newer major version or an unknown critical element: a version refusal.
    #[error(transparent)]
    Unsupported(FormatError),
}

/// The file name of a registry file.
pub fn file_name(generation: u32, file_id: u64) -> String {
    format!("{generation:08x}-{file_id:016x}.{EXTENSION}")
}

/// The generation and file id a registry file's name gives, if it is one.
pub fn parse_name(name: &str) -> Option<(u32, u64)> {
    let stem = name.strip_suffix(".reg")?;
    let (g, id) = stem.split_once('-')?;
    let hex = |s: &str, n: usize| {
        s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    if !hex(g, 8) || !hex(id, 16) {
        return None;
    }
    Some((
        u32::from_str_radix(g, 16).ok()?,
        u64::from_str_radix(id, 16).ok()?,
    ))
}

/// The canonical bytes of a registry file (A.16): its header, an entries block per run of entries with consecutive
/// sequences and one receive time (split as A.6 splits; a recovery entry alone), then the summary.
pub fn encode(
    file: &RegistryFile,
    store_id: [u8; 16],
    file_id: u64,
    created_at: i64,
) -> Result<Vec<u8>, EncodeError> {
    let mut out = FileHeader::new(
        kind::REGISTRY,
        file.generation,
        file_id,
        created_at,
        store_id,
    )
    .encode()
    .to_vec();
    let mut blocks: u32 = 0;
    let mut run: Option<EntriesBlock> = None;
    let mut body_len = 0usize;
    let flush = |run: &mut Option<EntriesBlock>,
                 out: &mut Vec<u8>,
                 blocks: &mut u32|
     -> Result<(), EncodeError> {
        if let Some(b) = run.take() {
            encode_block(block_type::ENTRIES, 0, &b.encode()?, out)?;
            *blocks += 1;
        }
        Ok(())
    };
    for (sequence, received_at, entry) in &file.entries {
        if matches!(entry, Entry::Ignored { .. }) {
            continue;
        }
        let len = entry_len(entry) as usize;
        // A recovery entry is alone in its batch, and so in its block (A.6).
        let continues = !entry.is_recovery()
            && run.as_ref().is_some_and(|b| {
                !b.entries.iter().any(Entry::is_recovery)
                    && b.received_at == *received_at
                    && b.first_sequence + b.entries.len() as u64 == *sequence
                    && body_len + len <= MAX_BODY_LEN as usize
            });
        if !continues {
            flush(&mut run, &mut out, &mut blocks)?;
            run = Some(EntriesBlock {
                first_sequence: *sequence,
                received_at: *received_at,
                entries: Vec::new(),
            });
            body_len = 24;
        }
        body_len += len;
        run.as_mut().expect("set above").entries.push(entry.clone());
    }
    flush(&mut run, &mut out, &mut blocks)?;
    let mut summary = [0u8; SUMMARY_LEN];
    summary[..8].copy_from_slice(&file.through.to_le_bytes());
    summary[8..12].copy_from_slice(&file.max_series_id.to_le_bytes());
    // The entries written: an ignored entry is not.
    let written = file
        .entries
        .iter()
        .filter(|(_, _, e)| !matches!(e, Entry::Ignored { .. }))
        .count();
    let count = u32::try_from(written).map_err(|_| EncodeError::TooLarge)?;
    summary[12..16].copy_from_slice(&count.to_le_bytes());
    summary[16..20].copy_from_slice(&blocks.to_le_bytes());
    encode_block(SUMMARY, 0, &summary, &mut out)?;
    Ok(out)
}

/// Reads a registry file, applying every check of A.16.
pub fn decode(bytes: &[u8], store_id: &[u8; 16]) -> Result<RegistryFile, RegistryError> {
    let h = FileHeader::decode(bytes).ok_or(RegistryError::Invalid(0))?;
    if h.format_major > crate::FORMAT_MAJOR {
        return Err(RegistryError::Unsupported(FormatError::Unsupported {
            offset: 0,
            element: Element::FormatMajor(h.format_major),
        }));
    }
    if h.kind != kind::REGISTRY || &h.store_id != store_id {
        return Err(RegistryError::Invalid(0));
    }
    let mut at = HEADER_LEN;
    let mut entries = Vec::new();
    let mut blocks: u32 = 0;
    let mut last: Option<u64> = None;
    loop {
        let invalid = RegistryError::Invalid(at as u64);
        let head: &[u8; BLOCK_HEADER_LEN] = bytes
            .get(at..at + BLOCK_HEADER_LEN)
            .and_then(|b| b.try_into().ok())
            .ok_or(RegistryError::Invalid(at as u64))?;
        let bh = BlockHeader::decode(head).ok_or(RegistryError::Invalid(at as u64))?;
        let start = at + BLOCK_HEADER_LEN;
        let body = bytes
            .get(start..start + bh.body_len as usize)
            .ok_or(RegistryError::Invalid(at as u64))?;
        if crc32c(body) != bh.body_crc32c {
            return Err(invalid);
        }
        let next = at + bh.block_len() as usize;
        match bh.block_type {
            block_type::ENTRIES => {
                let b = EntriesBlock::decode(body).map_err(|e| match e {
                    BodyError::Unsupported(element) => {
                        RegistryError::Unsupported(FormatError::Unsupported {
                            offset: at as u64,
                            element,
                        })
                    }
                    _ => RegistryError::Invalid(at as u64),
                })?;
                let mut sequence = b.first_sequence;
                for e in b.entries {
                    // Positions rise strictly through the file.
                    if last.is_some_and(|l| sequence <= l) {
                        return Err(RegistryError::Invalid(at as u64));
                    }
                    last = Some(sequence);
                    let n = e.sequences();
                    if !matches!(e, Entry::Ignored { .. }) {
                        entries.push((sequence, b.received_at, e));
                    }
                    sequence += n;
                }
                blocks += 1;
            }
            SUMMARY => {
                if body.len() < SUMMARY_LEN || next != bytes.len() {
                    return Err(invalid);
                }
                let file = RegistryFile {
                    generation: h.generation,
                    through: u64_at(body, 0),
                    max_series_id: u32_at(body, 8),
                    entries,
                };
                if u32_at(body, 12) as usize != file.entries.len()
                    || u32_at(body, 16) != blocks
                    || last.is_some_and(|l| l > file.through)
                {
                    return Err(invalid);
                }
                return Ok(file);
            }
            t if bh.skippable() => {
                let _ = t;
            }
            t => {
                return Err(RegistryError::Unsupported(FormatError::Unsupported {
                    offset: at as u64,
                    element: Element::BlockType(t),
                }));
            }
        }
        at = next;
    }
}

fn names(fs: &dyn Fs, dir: &Path) -> io::Result<Vec<(u32, u64, PathBuf)>> {
    match fs.read_dir(dir) {
        Ok(v) => Ok(v
            .into_iter()
            .filter_map(|e| parse_name(&e.name).map(|(g, id)| (g, id, dir.join(&e.name))))
            .collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The valid registry file of the store in `store_dir` with the highest `file_id`, with that id; `None` when there
/// is none. Files that are invalid or cannot be read, and a directory that cannot be listed, are passed over: the log
/// alone suffices (A.16). Only a version refusal is returned.
pub fn read_latest(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
) -> Result<Option<(u64, RegistryFile)>, FormatError> {
    read_latest_where(fs, store_dir, store_id, |_| true)
}

/// [`read_latest`] among the files `accept` takes: the newest valid one it takes, older generations tried in turn.
pub fn read_latest_where(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
    accept: impl Fn(&RegistryFile) -> bool,
) -> Result<Option<(u64, RegistryFile)>, FormatError> {
    let mut found = names(fs, &store_dir.join(REGISTRY_DIR)).unwrap_or_default();
    found.sort_by_key(|(_, id, _)| std::cmp::Reverse(*id));
    for (generation, id, path) in found {
        let Ok(bytes) = read_all(fs, &path) else {
            continue;
        };
        match decode(&bytes, store_id) {
            Ok(f)
                if f.generation == generation
                    && FileHeader::decode(&bytes).is_some_and(|h| h.file_id == id)
                    && accept(&f) =>
            {
                return Ok(Some((id, f)));
            }
            Ok(_) | Err(RegistryError::Invalid(_)) => continue,
            Err(RegistryError::Unsupported(e)) => return Err(e),
        }
    }
    Ok(None)
}

/// Writes `file` as the store's registry file (A.16): under the next `file_id`, synced with its directory, then
/// every other registry file removed and the directory synced again. Returns the new file's id.
///
/// The caller makes sure `file.through` names no position above the last synced mark (`spec.md` §3 Durable mark).
pub fn write(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: [u8; 16],
    created_at: i64,
    file: &RegistryFile,
) -> io::Result<u64> {
    let dir = store_dir.join(REGISTRY_DIR);
    let created = fs.read_dir(&dir).is_err();
    fs.create_dir_all(&dir)?;
    if created {
        fs.sync_dir(store_dir)?;
    }
    let old = names(fs, &dir)?;
    let id = old.iter().map(|(_, id, _)| *id).max().unwrap_or(0) + 1;
    let bytes = encode(file, store_id, id, created_at).map_err(io::Error::other)?;
    let path = dir.join(file_name(file.generation, id));
    let f = fs.open(&path, OpenMode::CreateNew)?;
    f.write_at(&bytes, 0)?;
    f.sync_all()?;
    drop(f);
    fs.sync_dir(&dir)?;
    for (_, _, p) in old {
        fs.remove_file(&p)?;
    }
    fs.sync_dir(&dir)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::{Definition, IntervalLength, Kind, Retention, Timing, Versioning};
    use crate::facts::ValueType;
    use crate::fs::MemFs;

    fn definition(id: u32) -> Entry {
        Entry::Definition(Box::new(Definition {
            id,
            name: format!("a/b/{id}"),
            kind: Kind::Interval,
            value_type: ValueType::F64,
            scale: None,
            unit: "kWh".into(),
            expected_interval: Some(1_800),
            staleness: Some(5_400),
            record_on_change: false,
            counter: None,
            lateness_bound: 300,
            state_names: Vec::new(),
            versioning: Versioning::None,
            timing: Timing::Observed,
            interval: Some(IntervalLength::Fixed {
                length: 1_800,
                phase: 0,
            }),
            retention: Retention::LongTerm,
            excess_precision: None,
            staleness_set: false,
        }))
    }

    fn sample() -> RegistryFile {
        RegistryFile {
            generation: 0,
            through: 40,
            max_series_id: 5,
            entries: vec![
                (1, 100, definition(1)),
                (
                    2,
                    100,
                    Entry::Alias {
                        id: 1,
                        name: "a/b/x".into(),
                    },
                ),
                (7, 200, definition(2)),
                (8, 300, Entry::Retirement { id: 1, t_r: 9 }),
                (
                    30,
                    400,
                    Entry::Tzdata {
                        version: "2026a".into(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn round_trip_and_blocks() {
        let f = sample();
        let bytes = encode(&f, [3; 16], 9, 1).unwrap();
        assert_eq!(decode(&bytes, &[3; 16]).unwrap(), f);
        // Runs: (1, 2) share a block; 7, 8 and 30 each start one (another batch); then the summary.
        let summary_at = bytes.len() - BLOCK_HEADER_LEN - SUMMARY_LEN;
        assert_eq!(bytes[summary_at], SUMMARY);
        assert_eq!(u32_at(&bytes[summary_at + BLOCK_HEADER_LEN..], 16), 4);
        assert_eq!(f.floor(), 5);
        // Encoding again gives the same bytes (canonical writing).
        assert_eq!(
            encode(&decode(&bytes, &[3; 16]).unwrap(), [3; 16], 9, 1).unwrap(),
            bytes
        );
    }

    #[test]
    fn every_damage_is_invalid_and_other_stores_are_refused() {
        let bytes = encode(&sample(), [3; 16], 9, 1).unwrap();
        assert!(matches!(
            decode(&bytes, &[4; 16]),
            Err(RegistryError::Invalid(0))
        ));
        for cut in [0, 10, HEADER_LEN, bytes.len() - 8] {
            assert!(
                matches!(
                    decode(&bytes[..cut], &[3; 16]),
                    Err(RegistryError::Invalid(_))
                ),
                "cut at {cut}"
            );
        }
        for at in (0..bytes.len()).step_by(7) {
            let mut b = bytes.clone();
            b[at] ^= 0x40;
            // Padding between blocks is not covered by a checksum (A, common rules): a flip there changes nothing.
            assert!(
                decode(&b, &[3; 16]).map_or(true, |f| f == sample()),
                "flip at {at}"
            );
        }
        let mut newer = bytes.clone();
        newer[5] = crate::FORMAT_MAJOR + 1;
        let crc = crc32c(&newer[..60]).to_le_bytes();
        newer[60..64].copy_from_slice(&crc);
        assert!(matches!(
            decode(&newer, &[3; 16]),
            Err(RegistryError::Unsupported(_))
        ));
    }

    #[test]
    fn the_latest_valid_file_wins_and_old_ones_go() {
        let fs = MemFs::new();
        let dir = Path::new("store");
        fs.create_dir_all(dir).unwrap();
        assert!(read_latest(&fs, dir, &[3; 16]).unwrap().is_none());
        let mut f = sample();
        assert_eq!(write(&fs, dir, [3; 16], 1, &f).unwrap(), 1);
        f.through = 50;
        assert_eq!(write(&fs, dir, [3; 16], 2, &f).unwrap(), 2);
        assert_eq!(names(&fs, &dir.join(REGISTRY_DIR)).unwrap().len(), 1);
        assert_eq!(
            read_latest(&fs, dir, &[3; 16]).unwrap().unwrap(),
            (2, f.clone())
        );
        // A torn newer file (a crash while it was written) is passed over; its id is not used again.
        let torn = dir.join(REGISTRY_DIR).join(file_name(0, 3));
        let w = fs.open(&torn, OpenMode::CreateNew).unwrap();
        w.write_at(&encode(&f, [3; 16], 3, 3).unwrap()[..100], 0)
            .unwrap();
        drop(w);
        assert_eq!(read_latest(&fs, dir, &[3; 16]).unwrap().unwrap().0, 2);
        assert_eq!(write(&fs, dir, [3; 16], 4, &f).unwrap(), 4);
        assert_eq!(names(&fs, &dir.join(REGISTRY_DIR)).unwrap().len(), 1);
    }

    #[test]
    fn ignored_entries_are_neither_written_nor_counted() {
        let mut f = sample();
        f.entries.insert(2, (3, 100, Entry::Ignored { count: 2 }));
        let bytes = encode(&f, [3; 16], 9, 1).unwrap();
        assert_eq!(decode(&bytes, &[3; 16]).unwrap(), sample());
    }

    #[test]
    fn names_are_exact() {
        assert_eq!(parse_name(&file_name(1, 0x2a)), Some((1, 0x2a)));
        assert_eq!(parse_name("00000001-000000000000002A.reg"), None);
        assert_eq!(parse_name("00000001-2a.reg"), None);
        assert_eq!(parse_name("00000001-000000000000002a.reg.tmp"), None);
    }
}
