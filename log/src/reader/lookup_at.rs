//! A lookup of facts at known positions: each read from the one segment that holds its position, found by the
//! segments' header base sequences, without a listing of the log directory or any sealed segment's index.

use std::collections::{BTreeMap, HashMap};

use super::files::read_at_full;
use super::lookup::Query;
use super::{LogReader, ReaderError};
use crate::header::HEADER_LEN;
use crate::name::SegmentName;
use crate::scan::ScanLimit;
use crate::writer::Position;

use super::lookup::Lookup;

/// One fact wanted at a known position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WantedAt {
    /// The series.
    pub series_id: u32,
    /// Its event time.
    pub t: i64,
    /// Its position.
    pub position: Position,
}

impl LogReader {
    /// The facts at the positions `wanted` names, in the local log up to the durable mark, as [`LogReader::lookup`]
    /// reads them but from one segment each: the one of the mark's generation with the highest file id whose base
    /// sequence is at or below the position. `complete[i]` says whether that segment was found and read with only
    /// lost regions met; a complete result without the fact means the segment does not hold it.
    pub fn lookup_at(
        &mut self,
        wanted: &[WantedAt],
        lost: &dyn Fn(u64, u64, u64) -> bool,
    ) -> Result<Lookup, ReaderError> {
        let mark = self.read_mark()?;
        let lease_kept = self.keep_alive().is_ok();
        let mut out = Lookup {
            facts: Vec::new(),
            complete: vec![true; wanted.len()],
            lease_kept,
            durable: Position {
                generation: mark.generation,
                sequence: mark.last_sequence,
            },
            stamp: mark.stamp,
            segments_read: 0,
            overflowed: false,
            listed: Vec::new(),
        };
        let unbounded = super::lookup::Budget::new(u64::MAX);
        let mut bases = HashMap::new();
        let mut by_segment: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (i, w) in wanted.iter().enumerate() {
            let at = w.position;
            let held = if at.generation == mark.generation
                && at.sequence <= mark.last_sequence
                && mark.file_id > 0
            {
                self.segment_holding(at.sequence, mark.generation, mark.file_id, &mut bases)?
            } else {
                None
            };
            match held {
                Some(file_id) => by_segment.entry(file_id).or_default().push(i),
                None => out.complete[i] = false,
            }
        }
        for (file_id, at) in by_segment {
            let name = SegmentName {
                generation: mark.generation,
                file_id,
                sealed: file_id < mark.file_id,
            };
            let limit = if file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let mut by_series: BTreeMap<u32, Vec<(i64, i64)>> = BTreeMap::new();
            for &i in &at {
                by_series
                    .entry(wanted[i].series_id)
                    .or_default()
                    .push((wanted[i].t, wanted[i].t));
            }
            let query = Query {
                by_series,
                ranges: at.iter().map(|&i| (wanted[i].t, wanted[i].t)).collect(),
                generation: mark.generation,
                last_sequence: mark.last_sequence,
                lost,
                budget: &unbounded,
                above: None,
                upto: None,
            };
            let read = self.lookup_segment(name, limit, &query, &mut out.facts);
            out.segments_read += 1;
            if !read {
                for i in at {
                    out.complete[i] = false;
                }
            }
            self.caches.trim();
        }
        Ok(out)
    }

    /// The file id, at most `last`, of the segment of `generation` that holds `sequence`: a binary search over file
    /// ids by header base sequence, passing over ids with no readable segment.
    pub(super) fn segment_holding(
        &self,
        sequence: u64,
        generation: u32,
        last: u64,
        bases: &mut HashMap<u64, Option<u64>>,
    ) -> Result<Option<u64>, ReaderError> {
        let (mut lo, mut hi) = (1u64, last);
        let mut best = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let mut found = None;
            for k in mid..=hi {
                if let Some(b) = self.base_of(generation, k, last, bases)? {
                    found = Some((k, b));
                    break;
                }
            }
            match found {
                Some((k, base)) if base <= sequence => {
                    best = Some(k);
                    lo = k + 1;
                }
                _ => match mid.checked_sub(1) {
                    Some(h) => hi = h,
                    None => break,
                },
            }
        }
        Ok(best)
    }

    /// The base sequence of segment `file_id` from its header, remembered for the call; `None` when it has no
    /// readable header.
    pub(super) fn base_of(
        &self,
        generation: u32,
        file_id: u64,
        last: u64,
        bases: &mut HashMap<u64, Option<u64>>,
    ) -> Result<Option<u64>, ReaderError> {
        if let Some(b) = bases.get(&file_id) {
            return Ok(*b);
        }
        let name = SegmentName {
            generation,
            file_id,
            sealed: file_id < last,
        };
        let base = match self.open_segment(name)? {
            Some((_, f)) => {
                let mut head = [0u8; HEADER_LEN];
                let read = read_at_full(&*f, &mut head, 0).unwrap_or(0);
                (read == HEADER_LEN)
                    .then(|| crate::header::FileHeader::decode(&head))
                    .flatten()
                    .map(|h| h.base_sequence)
                    .filter(|b| *b > 0)
            }
            None => None,
        };
        bases.insert(file_id, base);
        Ok(base)
    }
}
