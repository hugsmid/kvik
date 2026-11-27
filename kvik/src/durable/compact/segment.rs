//! One input segment read for a run a step at a time (compaction design §4): 128 KiB reads, then the scan and the
//! seal's checks as one step (Q-1534), then one batch decoded a step.

use std::collections::BTreeMap;

use kvik_log::BodyError;
use kvik_log::FormatError;
use kvik_log::batch::{Batch, DataBlock, SegmentIdentity};
use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader, RawBlock};
use kvik_log::body::{Commit, DataPrefix};
use kvik_log::content::BatchContent;
use kvik_log::fs::{Fs, FsFile, OpenMode};
use kvik_log::scan::{HeaderState, ScanLimit, SealState, scan};

use super::plan::Candidate;
use super::{Cause, RunError, Stall};

/// The size of one device read or write of the task (compaction design §4).
pub(crate) const PIECE: usize = 128 * 1024;

/// A batch's blocks by offset into the segment's bytes, so a step can decode it without the scan's borrow.
struct Shape {
    offset: u64,
    blocks: Vec<(u64, BlockHeader, [u8; BLOCK_HEADER_LEN], Option<DataPrefix>)>,
    commit: Commit,
}

/// What the sealer wrote about a segment: positions and per-series fact counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Sealed {
    pub(super) first: u64,
    pub(super) last: u64,
    pub(super) positions: u64,
    pub(super) facts: BTreeMap<u32, u64>,
}

/// One decoded batch of a segment.
pub(super) struct Decoded {
    pub(super) content: BatchContent,
    pub(super) commit: Commit,
    pub(super) set_aside: bool,
}

/// What one step of a segment did.
pub(super) enum Event {
    /// A read or the scan; more follows.
    Pending,
    /// A batch, in file order.
    Batch(Decoded),
    /// The last batch was decoded.
    End,
}

/// A segment being read.
pub(super) struct Segment {
    candidate: Candidate,
    file: Box<dyn FsFile>,
    len: usize,
    bytes: Vec<u8>,
    shapes: Option<Vec<Shape>>,
    next: usize,
    pub(super) sealed: Sealed,
}

impl Segment {
    pub(super) fn open(fs: &dyn Fs, c: &Candidate) -> Result<Self, RunError> {
        let file = fs.open(&c.path, OpenMode::Read)?;
        let len =
            usize::try_from(file.len()?).map_err(|_| std::io::Error::other("segment too large"))?;
        Ok(Self {
            candidate: c.clone(),
            file,
            len,
            bytes: Vec::new(),
            shapes: None,
            next: 0,
            sealed: Sealed::default(),
        })
    }

    pub(super) fn stall(&self, cause: Cause) -> RunError {
        RunError::Stalled(Stall {
            generation: self.candidate.generation,
            file_id: self.candidate.file_id,
            cause,
        })
    }

    /// Bytes this segment holds in memory.
    pub(super) fn held(&self) -> u64 {
        let shapes = self.shapes.as_ref().map_or(0, |s| {
            s.iter()
                .map(|b| 64 + 48 * b.blocks.len() as u64)
                .sum::<u64>()
        });
        self.bytes.capacity() as u64 + shapes
    }

    /// Whether the next step issues a device read of this many bytes.
    pub(super) fn reading(&self) -> Option<u64> {
        (self.bytes.len() < self.len).then(|| (self.len - self.bytes.len()).min(PIECE) as u64)
    }

    pub(super) fn step(
        &mut self,
        store_id: [u8; 16],
        set_aside: &kvik_log::setaside::SetAside,
    ) -> Result<Event, RunError> {
        if self.bytes.len() < self.len {
            if self.bytes.capacity() == 0 {
                self.bytes.reserve_exact(self.len);
            }
            let at = self.bytes.len();
            let n = (self.len - at).min(PIECE);
            self.bytes.resize(at + n, 0);
            let mut done = 0;
            while done < n {
                let got = self
                    .file
                    .read_at(&mut self.bytes[at + done..at + n], (at + done) as u64)?;
                if got == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                }
                done += got;
            }
            return Ok(Event::Pending);
        }
        let Some(shapes) = &self.shapes else {
            self.scan(store_id)?;
            return Ok(Event::Pending);
        };
        let Some(s) = shapes.get(self.next) else {
            self.bytes = Vec::new();
            self.shapes = Some(Vec::new());
            return Ok(Event::End);
        };
        self.next += 1;
        let batch = Batch {
            offset: s.offset,
            blocks: s
                .blocks
                .iter()
                .map(|&(offset, header, header_bytes, prefix)| {
                    let start = offset as usize + BLOCK_HEADER_LEN;
                    DataBlock {
                        block: RawBlock {
                            offset,
                            header,
                            header_bytes,
                            body: &self.bytes[start..start + header.body_len as usize],
                        },
                        prefix,
                    }
                })
                .collect(),
            commit: s.commit,
        };
        let content = BatchContent::decode_canonical(&batch).map_err(|e| match e {
            BodyError::NonCanonical => self.stall(Cause::Unknown),
            _ => self.stall(Cause::Unreadable("a batch body")),
        })?;
        if content.first_sequence != s.commit.first_sequence
            || content.last_sequence() != s.commit.last_sequence
        {
            return Err(self.stall(Cause::Unreadable("a batch disagrees with its commit")));
        }
        Ok(Event::Batch(Decoded {
            content,
            commit: s.commit,
            set_aside: kvik_log::scan::set_aside(&batch, set_aside),
        }))
    }

    fn scan(&mut self, store_id: [u8; 16]) -> Result<(), RunError> {
        let c = &self.candidate;
        let id = SegmentIdentity {
            store_id,
            file_id: c.file_id,
            generation: c.generation,
        };
        let bytes = &self.bytes;
        let report = scan(bytes, &id, ScanLimit::End).map_err(|e| match e {
            FormatError::Unsupported { .. } => self.stall(Cause::Unknown),
            _ => self.stall(Cause::Unreadable("refused by the scan")),
        })?;
        let seal = report
            .seal
            .as_ref()
            .filter(|s| s.state == SealState::Sealed && s.file_crc_matches(bytes));
        let (Some(seal), HeaderState::Valid(_), true) = (
            seal,
            report.header,
            report.regions.is_empty() && report.tail.is_none(),
        ) else {
            return Err(self.stall(Cause::Unreadable("not sealed whole")));
        };
        let Some(Some(index)) = &seal.index else {
            return Err(self.stall(Cause::Unreadable("no segment index")));
        };
        self.sealed = Sealed {
            first: seal.trailer.first_sequence,
            last: seal.trailer.last_sequence,
            positions: seal.trailer.entry_count,
            facts: index
                .records
                .iter()
                .map(|r| (r.series_id, u64::from(r.fact_count)))
                .filter(|&(_, n)| n > 0)
                .collect(),
        };
        let shapes = report
            .batches
            .iter()
            .map(|b| Shape {
                offset: b.offset,
                blocks: b
                    .blocks
                    .iter()
                    .map(|d| {
                        (
                            d.block.offset,
                            d.block.header,
                            d.block.header_bytes,
                            d.prefix,
                        )
                    })
                    .collect(),
                commit: b.commit,
            })
            .collect();
        self.shapes = Some(shapes);
        Ok(())
    }
}
