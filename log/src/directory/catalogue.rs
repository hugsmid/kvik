//! What a log directory is checked against (`format.md` A.18 Validation), and how a reader loads one and a writer
//! rebuilds one: the catalogue's sealed segments and manifest records, the newest matching root with its groups,
//! and the records a rebuild reuses.

use std::collections::BTreeMap;
use std::path::Path;

use super::logdir::{GROUP_SEGMENTS, Group, Root, SegmentRecord};
use super::{DerivedError, Family, Frame, read_latest};
use crate::crc::crc32c;
use crate::error::FormatError;
use crate::fs::Fs;
use crate::manifest::ManifestRead;
use crate::store::read_all;

/// A sealed segment as the catalogue describes it, for the check of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    /// The generation it was created in.
    pub generation: u32,
    /// Its last sequence.
    pub last_sequence: u64,
    /// Its plaintext SHA-256, where the catalogue carries it (a manifest file description).
    pub sha256: Option<[u8; 32]>,
    /// Its length and trailer `file_crc32c`, where the check reads them from the segment instead.
    pub seal: Option<(u64, u32)>,
}

/// What a log directory is checked against: the store generation, the live sealed segments and the manifest's
/// valid records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// The store's current generation.
    pub generation: u32,
    /// Live sealed log segments by `file_id`.
    pub segments: BTreeMap<u64, Listed>,
    /// The manifest's valid records: the offset just past each, its epoch and the CRC32C of its block bytes.
    pub records: BTreeMap<u64, (u64, u32)>,
}

impl Catalogue {
    /// The manifest's valid records, from a read of `bytes`.
    pub fn with_manifest(mut self, m: &ManifestRead, bytes: &[u8]) -> Self {
        self.records = m
            .records
            .iter()
            .map(|r| {
                let block = crate::bytes::slice(bytes, r.offset, r.end - r.offset).unwrap_or(&[]);
                (r.end, (r.epoch, crc32c(block)))
            })
            .collect();
        self
    }

    /// Whether a group written in store generation `generation` still describes the catalogue's segments (A.18
    /// Validation): every record's id, generation and last sequence, and its hash or its length and
    /// `file_crc32c`, as the catalogue gives them.
    pub fn group_matches(&self, generation: u32, g: &Group) -> bool {
        generation == self.generation
            && g.segments.iter().all(|s| {
                self.segments.get(&s.file_id).is_some_and(|l| {
                    l.generation == s.generation
                        && l.last_sequence == s.last_sequence
                        && l.sha256.is_none_or(|h| h == s.sha256)
                        && l.seal
                            .is_none_or(|(len, crc)| len == s.len && crc == s.file_crc32c)
                })
            })
    }

    /// Whether a root written in store generation `generation` still names a valid manifest record by its offset,
    /// epoch and content.
    pub fn root_matches(&self, generation: u32, r: &Root) -> bool {
        generation == self.generation
            && self.records.get(&r.manifest_end) == Some(&(r.epoch, r.manifest_crc32c))
    }
}

/// A log directory as a reader finds it: the newest root that matches the catalogue, and each of its groups whose
/// file is the generation the root names and still matches; `None` for a group to be read from the segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogDirectory {
    /// The root's frame.
    pub frame: Frame,
    /// The root.
    pub root: Root,
    /// Its groups, by number.
    pub groups: Vec<Option<Group>>,
}

impl LogDirectory {
    /// The segments it lists, of the groups found.
    pub fn segments(&self) -> impl Iterator<Item = &SegmentRecord> {
        self.groups.iter().flatten().flat_map(|g| &g.segments)
    }
}

/// Loads the log directory (A.18 Reading): the newest root that decodes and matches `cat`, falling back to the
/// previous generation, with its groups.
pub fn load(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
    cat: &Catalogue,
) -> Result<Option<LogDirectory>, FormatError> {
    let decode = |b: &[u8]| {
        let (f, r) = Root::decode(b, store_id)?;
        if cat.root_matches(f.generation, &r) {
            Ok((f, r))
        } else {
            Err(DerivedError::Invalid("root does not match the catalogue"))
        }
    };
    let Some((frame, root)) = read_latest(fs, store_dir, Family::Root, decode)? else {
        return Ok(None);
    };
    let mut groups = Vec::with_capacity(root.groups.len());
    for e in &root.groups {
        let mut found = None;
        for (id, path) in super::generations(fs, store_dir, Family::Group(e.number)) {
            let Ok(bytes) = read_all(fs, &path) else {
                continue;
            };
            match Group::decode(&bytes, store_id) {
                Ok((f, (g, crc)))
                    if f.file_id == id
                        && crc == e.body_crc32c
                        && g.entry() == *e
                        && super::logdir::contiguous(&g, &root.lost)
                        && cat.group_matches(f.generation, &g) =>
                {
                    found = Some(g);
                    break;
                }
                Ok(_) | Err(DerivedError::Invalid(_)) => {}
                Err(DerivedError::Unsupported(err)) => return Err(err),
            }
        }
        groups.push(found);
    }
    Ok(Some(LogDirectory {
        frame,
        root,
        groups,
    }))
}

pub(crate) fn read_exact_at(f: &dyn crate::fs::FsFile, buf: &mut [u8], at: u64) -> Option<()> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], at.checked_add(done as u64)?) {
            Ok(0) | Err(_) => return None,
            Ok(n) => done += n,
        }
    }
    Some(())
}

/// A sealed segment's length and trailer, read positionally (end marker, then trailer block): `None` if it is not
/// sealed or cannot be read.
pub fn read_trailer(fs: &dyn Fs, path: &Path) -> Option<(u64, crate::body::Trailer)> {
    use crate::block::BLOCK_HEADER_LEN;
    let f = fs.open(path, crate::fs::OpenMode::Read).ok()?;
    let len = f.len().ok()?;
    let mut end = [0u8; 16];
    read_exact_at(&*f, &mut end, len.checked_sub(16)?)?;
    let at = crate::body::decode_end_marker(&end)?;
    let mut block = [0u8; BLOCK_HEADER_LEN + crate::body::TRAILER_LEN];
    read_exact_at(&*f, &mut block, at)?;
    let head: &[u8; BLOCK_HEADER_LEN] = block[..BLOCK_HEADER_LEN].try_into().ok()?;
    let h = crate::block::BlockHeader::decode(head)?;
    let body = &block[BLOCK_HEADER_LEN..];
    if h.block_type != crate::block::block_type::TRAILER
        || h.body_len as usize != body.len()
        || crc32c(body) != h.body_crc32c
    {
        return None;
    }
    Some((len, crate::body::Trailer::decode(body).ok()?))
}

/// The catalogue as the writer sees it before the manifest lists sealed log segments (roadmap 6.3b): every sealed
/// segment under `log/` with its trailer's last sequence, length and `file_crc32c`, and the manifest's valid
/// records. Two small reads per segment, no listing of anything else.
pub fn catalogue_from_disk(
    fs: &dyn Fs,
    store_dir: &Path,
    generation: u32,
    manifest: Option<(&ManifestRead, &[u8])>,
) -> std::io::Result<Catalogue> {
    let log = store_dir.join(crate::store::LOG_DIR);
    let mut segments = BTreeMap::new();
    for e in fs.read_dir(&log)? {
        let Some(n) = crate::name::SegmentName::parse(&e.name).filter(|n| n.sealed) else {
            continue;
        };
        let Some((len, t)) = read_trailer(fs, &log.join(&e.name)) else {
            continue;
        };
        if t.batch_count == 0 {
            continue;
        }
        segments.insert(
            n.file_id,
            Listed {
                generation: n.generation,
                last_sequence: t.last_sequence,
                sha256: None,
                seal: Some((len, t.file_crc32c)),
            },
        );
    }
    let cat = Catalogue {
        generation,
        segments,
        records: BTreeMap::new(),
    };
    Ok(match manifest {
        Some((m, bytes)) => cat.with_manifest(m, bytes),
        None => cat,
    })
}

/// The catalogue's sealed segments in position order.
pub fn ordered(cat: &Catalogue) -> Vec<(u64, Listed)> {
    let mut v: Vec<_> = cat.segments.iter().map(|(id, l)| (*id, *l)).collect();
    v.sort_by_key(|(_, l)| (l.generation, l.last_sequence));
    v
}

/// Every record of every valid group file that still matches the catalogue, by `file_id`, whatever root names
/// it: what a rebuild reuses instead of reading the segment again (A.18 Crash and rebuild).
pub fn known_records(
    fs: &dyn Fs,
    store_dir: &Path,
    store_id: &[u8; 16],
    cat: &Catalogue,
) -> BTreeMap<u64, SegmentRecord> {
    let mut out = BTreeMap::new();
    for (family, id, path) in super::list(fs, store_dir).unwrap_or_default() {
        if !matches!(family, Family::Group(_)) {
            continue;
        }
        let Ok(bytes) = read_all(fs, &path) else {
            continue;
        };
        let Ok((f, (g, _))) = Group::decode(&bytes, store_id) else {
            continue;
        };
        if f.file_id != id || f.generation != cat.generation {
            continue;
        }
        for s in g.segments {
            let one = Group {
                number: 0,
                segments: vec![s],
            };
            if cat.group_matches(f.generation, &one) {
                let s = one.segments.into_iter().next().expect("one record");
                out.entry(s.file_id).or_insert(s);
            }
        }
    }
    out
}

/// The groups of `segments` (in position order): 256 to a group, numbered from 0.
pub fn group(segments: Vec<SegmentRecord>) -> Vec<Group> {
    let mut out = Vec::new();
    let mut it = segments.into_iter().peekable();
    while it.peek().is_some() {
        out.push(Group {
            number: out.len() as u32,
            segments: it.by_ref().take(GROUP_SEGMENTS).collect(),
        });
    }
    out
}

/// Bytes a [`Describe`] hashes per call at most.
pub const DESCRIBE_STEP: u64 = 4 << 20;

/// A sealed segment described a part at a time: its trailer and segment index first, then its SHA-256 over at most
/// a given number of bytes a call, so that a long segment holds neither the maintenance thread nor memory.
pub struct Describe {
    file: Box<dyn crate::fs::FsFile>,
    record: SegmentRecord,
    sha: sha2::Sha256,
    at: u64,
}

impl std::fmt::Debug for Describe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Describe")
            .field("file_id", &self.record.file_id)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl Describe {
    /// Reads the segment's trailer and index; `None` if it is not a sealed segment holding a batch.
    pub fn open(fs: &dyn Fs, path: &Path, id: &crate::batch::SegmentIdentity) -> Option<Self> {
        use crate::block::{BLOCK_HEADER_LEN, BlockHeader, block_type};
        use sha2::Digest;
        let (len, t) = read_trailer(fs, path)?;
        if t.batch_count == 0 {
            return None;
        }
        let file = fs.open(path, crate::fs::OpenMode::Read).ok()?;
        let mut index = None;
        if t.index_offset != 0 {
            let mut head = [0u8; BLOCK_HEADER_LEN];
            read_exact_at(&*file, &mut head, t.index_offset)?;
            let h = BlockHeader::decode(&head)?;
            let body_len = u64::from(h.body_len);
            if h.block_type != block_type::SEGMENT_INDEX
                || t.index_offset
                    .checked_add(BLOCK_HEADER_LEN as u64 + body_len)?
                    > len
            {
                return None;
            }
            let mut body = vec![0u8; usize::try_from(body_len).ok()?];
            read_exact_at(&*file, &mut body, t.index_offset + BLOCK_HEADER_LEN as u64)?;
            if crc32c(&body) != h.body_crc32c {
                return None;
            }
            index = crate::body::SegmentIndex::decode(&body).ok();
        }
        let record = SegmentRecord {
            file_id: id.file_id,
            generation: id.generation,
            has_entries: index
                .as_ref()
                .is_some_and(|i| i.flags & crate::body::INDEX_HAS_ENTRIES != 0),
            first_sequence: t.first_sequence,
            last_sequence: t.last_sequence,
            first_commit_time: t.first_commit_time,
            last_commit_time: t.last_commit_time,
            fact_count: index.map_or(0, |i| {
                i.records.iter().map(|r| u64::from(r.fact_count)).sum()
            }),
            len,
            file_crc32c: t.file_crc32c,
            sha256: [0; 32],
        };
        Some(Describe {
            file,
            record,
            sha: sha2::Sha256::new(),
            at: 0,
        })
    }

    /// Hashes at most `budget` more bytes: `None` if a read failed, `Some(None)` while bytes remain, and the record
    /// once the whole file is hashed.
    pub fn step(&mut self, budget: u64) -> Option<Option<SegmentRecord>> {
        use sha2::Digest;
        let end = self.record.len.min(self.at.saturating_add(budget.max(1)));
        let mut buf = vec![0u8; usize::try_from((end - self.at).min(1 << 20)).ok()?];
        while self.at < end {
            let n = usize::try_from((end - self.at).min(buf.len() as u64)).ok()?;
            read_exact_at(&*self.file, &mut buf[..n], self.at)?;
            self.sha.update(&buf[..n]);
            self.at += n as u64;
        }
        if self.at < self.record.len {
            return Some(None);
        }
        let mut r = self.record.clone();
        r.sha256 = std::mem::take(&mut self.sha).finalize().into();
        Some(Some(r))
    }
}
