//! The reference scan for the differential tests of the scan bound (Q-110): every body checksummed directly and
//! every boundary read walked in full, candidate by candidate, as before the prefix table.

use super::{
    HEADER_LEN, Resume, ScanLimit, ScanReport, ScanStats, Scanner, collect, ignoring_seal,
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
    collect(Scanner::resume_inner(
        bytes, id, limit, from, true, true, None,
    ))
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
