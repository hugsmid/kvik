//! The reference scan for the differential tests of the scan bound (Q-110): every body checksummed directly and
//! every boundary read walked in full, candidate by candidate, as before the prefix table.

use super::{
    Chains, HEADER_LEN, Resume, ScanLimit, ScanReport, ScanStats, Scanner, collect, ignoring_seal,
};
use crate::batch::SegmentIdentity;
use crate::error::FormatError;

/// [`super::scan`] by the reference scan.
pub fn scan<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    let from = Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    };
    collect(Scanner::resume_inner(bytes, id, limit, from, true, true))
}

/// The scan recovery runs on a segment whose seal it ignores: by the reference scan when `legacy`, else as recovery
/// runs it.
pub fn scan_ignoring_seal<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    legacy: bool,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    ignoring_seal(bytes, id, legacy)
}

/// [`super::scan`] by the walked chains, each candidate's chain walked and remembered in an ordered map: the oracle of
/// the chain ends read ahead.
pub fn scan_with_walked_links<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    with_chains(bytes, id, limit, Chains::walked())
}

/// [`super::scan`] with the chain ends found `span` bytes of starts at a time instead of 4 MiB (a multiple of 8), so
/// that small files cross the blocks.
pub fn scan_with_span<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
    span: u64,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    with_chains(bytes, id, limit, Chains::ends(span))
}

fn with_chains<'a>(
    bytes: &'a [u8],
    id: &SegmentIdentity,
    limit: ScanLimit,
    chains: Chains,
) -> (Result<ScanReport<'a>, FormatError>, ScanStats) {
    let from = Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    };
    collect(
        Scanner::resume_inner(bytes, id, limit, from, true, false).map(|mut s| {
            s.chains = chains;
            s
        }),
    )
}

/// The starts from `from` to `to` (exclusive, steps of 8) that resynchronisation reads further with chain ends found
/// `span` bytes of starts at a time, below `limit`: those whose chain of block headers ends in a commit block naming
/// them (Q-873).
pub fn chain_candidates(bytes: &[u8], from: u64, to: u64, limit: u64, span: u64) -> Vec<u64> {
    let mut reads = super::Reads::new(bytes);
    let mut ends = super::ends::Ends::new(span);
    let mut out = Vec::new();
    let mut o = from;
    while o < to {
        ends.move_to(o);
        let stop = (o + (1 << 16)).min(to);
        o = ends.next_candidate(&mut reads, o, stop, limit.min(bytes.len() as u64));
        if o < stop {
            out.push(o);
            o += 8;
        }
    }
    out
}
