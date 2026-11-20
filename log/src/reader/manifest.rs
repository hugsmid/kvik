//! The manifest as a reader holds it between calls (Q-852): checked by path at every call, by its
//! length, its file identity where the platform gives one and the bytes of the last record the reader holds; a longer
//! manifest is parsed from the offset last parsed to, a shorter or changed one from its start.

use std::collections::{BTreeMap, BTreeSet};

use super::files::read_at_full;
use super::{LogReader, ReaderError, io_err};
use crate::fs::{FileIdentity, FsFile, OpenMode};
use crate::manifest::{
    LostRange, ManifestRead, Record, listed_segments, read_manifest, read_records,
};
use crate::store::MANIFEST;

/// What the reader parsed of the manifest. `valid_end` is 0 before the first read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HeldManifest {
    identity: Option<FileIdentity>,
    valid_end: u64,
    /// The last valid record: its offset and bytes.
    last: Option<(u64, Vec<u8>)>,
    /// The last record's epoch.
    pub(super) epoch: u64,
    pub(super) lost: Vec<LostRange>,
    /// The lowest file id the records name in each generation (`format.md` A.14).
    pub(super) firsts: BTreeMap<u32, u64>,
}

impl HeldManifest {
    fn name(&mut self, listed: BTreeSet<(u32, u64)>) {
        for (g, id) in listed {
            let e = self.firsts.entry(g).or_insert(id);
            *e = (*e).min(id);
        }
    }
}

impl LogReader {
    /// The manifest as the files now hold it; `None` when it is what the reader holds.
    pub(super) fn check_manifest(&self) -> Result<Option<HeldManifest>, ReaderError> {
        let path = self.cfg.dir.join(MANIFEST);
        let f = self
            .cfg
            .fs
            .open(&path, OpenMode::Read)
            .map_err(io_err(&path))?;
        let len = f.len().map_err(io_err(&path))?;
        let identity = f.identity().map_err(io_err(&path))?;
        let held = &self.manifest;
        let same_file = held.valid_end > 0
            && len >= held.valid_end
            && match (held.identity, identity) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
            && match &held.last {
                Some((offset, bytes)) => {
                    let mut b = vec![0u8; bytes.len()];
                    read_at_full(&*f, &mut b, *offset).map_err(io_err(&path))? == b.len()
                        && b == *bytes
                }
                None => true,
            };
        if same_file && len == held.valid_end {
            return Ok(None);
        }
        let mut next = if same_file {
            held.clone()
        } else {
            HeldManifest::default()
        };
        if !same_file {
            let bytes = read_whole(&*f, len).map_err(io_err(&path))?;
            let m = read_manifest(&bytes, &self.store_id).map_err(ReaderError::Manifest)?;
            next.lost = m.lost_ranges().cloned().collect();
            next.name(listed_segments(&bytes, &m));
            next.epoch = m.last_epoch();
            next.valid_end = m.valid_end;
            next.last = m
                .records
                .last()
                .map(|r| (r.offset, record_bytes(&bytes, 0, r)));
        } else {
            let tail_len = usize::try_from(len - held.valid_end)
                .map_err(|e| io_err(&path)(std::io::Error::other(e)))?;
            let mut tail = vec![0u8; tail_len];
            let got = read_at_full(&*f, &mut tail, held.valid_end).map_err(io_err(&path))?;
            tail.truncate(got);
            let (records, end) =
                read_records(&tail, held.valid_end, held.epoch).map_err(ReaderError::Manifest)?;
            let shifted = ManifestRead {
                records: records
                    .iter()
                    .map(|r| Record {
                        offset: r.offset - held.valid_end,
                        end: r.end - held.valid_end,
                        ..r.clone()
                    })
                    .collect(),
                valid_end: end - held.valid_end,
                torn: false,
            };
            next.name(listed_segments(&tail, &shifted));
            if let Some(r) = records.last() {
                next.last = Some((r.offset, record_bytes(&tail, held.valid_end, r)));
                next.epoch = r.epoch;
            }
            next.lost.extend(records.into_iter().filter_map(|r| r.lost));
            next.valid_end = end;
        }
        next.identity = identity;
        Ok(Some(next))
    }
}

/// The bytes of record `r` in `bytes`, which start at file offset `base`; its padding past the end of the file is left
/// out.
fn record_bytes(bytes: &[u8], base: u64, r: &Record) -> Vec<u8> {
    let at = |o: u64| {
        usize::try_from(o.saturating_sub(base)).map_or(bytes.len(), |o| o.min(bytes.len()))
    };
    bytes[at(r.offset)..at(r.end)].to_vec()
}

fn read_whole(f: &dyn FsFile, len: u64) -> std::io::Result<Vec<u8>> {
    let mut b = vec![0u8; usize::try_from(len).map_err(std::io::Error::other)?];
    let got = read_at_full(f, &mut b, 0)?;
    b.truncate(got);
    Ok(b)
}
