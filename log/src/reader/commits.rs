//! The last position whose batch was committed at or before a time (`spec.md` §2.3 As-of time), read up to the
//! durable mark: from sealed segments' trailers where they decide it, otherwise from the segment's batches as a lookup
//! maps them.

use std::collections::{BTreeMap, HashSet};

use super::lookup::{Budget, Query};
use super::{LogReader, ReaderError};
use crate::scan::ScanLimit;
use crate::writer::Position;

/// What [`LogReader::position_at`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitFind {
    /// The last position whose batch's commit time is at or before the time asked, if any.
    pub position: Option<Position>,
    /// Whether every segment that may hold a later such position was read, with only lost regions met.
    pub complete: bool,
    /// The mark's position read up to.
    pub durable: Position,
    /// The mark's reset stamp.
    pub stamp: crate::mark::ResetStamp,
}

impl LogReader {
    /// The last position up to the durable mark whose batch's commit time is at or before `at`, in position order:
    /// commit times are non-decreasing within one segment only (a generation's), so segments are taken newest first
    /// and the first that holds such a batch decides. `lost` is as for [`LogReader::lookup`].
    pub fn position_at(
        &mut self,
        at: i64,
        lost: &dyn Fn(u64, u64, u64) -> bool,
    ) -> Result<CommitFind, ReaderError> {
        let mark = self.read_mark()?;
        let mut out = CommitFind {
            position: None,
            complete: true,
            durable: Position {
                generation: mark.generation,
                sequence: mark.last_sequence,
            },
            stamp: mark.stamp,
        };
        let mut names = self.segments()?;
        names.retain(|n| n.file_id > 0 && n.file_id <= mark.file_id);
        if mark.file_id > 0 && names.last().is_none_or(|n| n.file_id != mark.file_id) {
            match self.find_segment(mark.file_id)? {
                Some(n) => names.push(n),
                None => {
                    out.complete = false;
                    return Ok(out);
                }
            }
        }
        let listed: HashSet<u64> = names.iter().map(|n| n.file_id).collect();
        if self.caches.keep_listed(&listed) {
            out.complete = false;
            return Ok(out);
        }
        let none = Budget::new(0);
        for name in names.into_iter().rev() {
            if name.sealed
                && name.file_id != mark.file_id
                && let Some((_, t, _)) = self.read_trailer(name)
            {
                if t.first_commit_time > at {
                    continue;
                }
                if t.last_commit_time <= at {
                    out.position = Some(Position {
                        generation: name.generation,
                        sequence: t.last_sequence,
                    });
                    return Ok(out);
                }
            }
            let limit = if name.file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let query = Query {
                by_series: BTreeMap::new(),
                ranges: Vec::new(),
                generation: mark.generation,
                last_sequence: mark.last_sequence,
                lost,
                budget: &none,
                above: None,
                upto: None,
            };
            let read = self.lookup_segment(name, limit, &query, &mut Vec::new());
            let found = self.caches.last_committed(name.file_id, at, &query);
            self.caches.trim();
            if !read {
                out.complete = false;
                return Ok(out);
            }
            if let Some(p) = found {
                out.position = Some(p);
                return Ok(out);
            }
        }
        Ok(out)
    }
}
