//! When the series directory is written (`format.md` A.18 Writing): at a seal after which the sealed bytes it does
//! not cover could pass [`SERIES_BYTES`] at the next seal, and otherwise at a seal at least every
//! [`SERIES_INTERVAL`]. So between writes the uncovered sealed bytes stay at most `SERIES_BYTES − segment_size`,
//! and while a due write waits for the mark sync at most `max(SERIES_BYTES, segment_size)`, inside a reader's
//! [`READER_ALLOWANCE`] whenever `segment_size` is (Q-773).

use std::collections::VecDeque;
use std::time::Duration;

use crate::writer::Position;

/// The uncovered sealed bytes the series directory is kept under: half of [`READER_ALLOWANCE`].
pub const SERIES_BYTES: u64 = 32 << 20;
/// Sealed bytes no directory covers that a reader reads directly before it fails with `DirectoryUnavailable`.
pub const READER_ALLOWANCE: u64 = 64 << 20;
/// The longest time between two series directory writes while segments seal.
pub const SERIES_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Whether the series directory is due at a seal: `uncovered` sealed bytes (this seal's included), of which none
/// may be left once `segment_size` more could pass [`SERIES_BYTES`], or `since` the last write (`None`: never) at
/// least [`SERIES_INTERVAL`].
pub fn series_due(uncovered: u64, segment_size: u64, since: Option<Duration>) -> bool {
    uncovered > 0
        && (uncovered.saturating_add(segment_size) > SERIES_BYTES
            || since.is_none_or(|d| d >= SERIES_INTERVAL))
}

/// The sealed segments a directory does not yet cover, oldest first, with their bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Uncovered {
    segments: VecDeque<(Position, u64)>,
    bytes: u64,
}

impl Uncovered {
    /// A sealed segment ending at `last`, of `len` bytes.
    pub fn sealed(&mut self, last: Position, len: u64) {
        self.segments.push_back((last, len));
        self.bytes = self.bytes.saturating_add(len);
    }

    /// The directory now covers every position up to `through`.
    pub fn covered(&mut self, through: Position) {
        while let Some(&(last, len)) = self.segments.front() {
            if last > through {
                break;
            }
            self.segments.pop_front();
            self.bytes -= len;
        }
    }

    /// Uncovered sealed bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The last position of the newest uncovered segment.
    pub fn newest(&self) -> Option<Position> {
        self.segments.back().map(|(p, _)| *p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MIB: u64 = 1 << 20;

    fn at(s: u64) -> Position {
        Position {
            generation: 0,
            sequence: s,
        }
    }

    #[test]
    fn due_by_bytes_by_time_and_never_with_nothing_uncovered() {
        let recent = Some(Duration::from_secs(1));
        assert!(!series_due(0, 64 * MIB, None));
        assert!(!series_due(8 * MIB, 16 * MIB, recent));
        assert!(series_due(17 * MIB, 16 * MIB, recent));
        assert!(series_due(MIB, 64 * MIB, recent));
        assert!(series_due(MIB, MIB, Some(SERIES_INTERVAL)));
        assert!(!series_due(
            MIB,
            MIB,
            Some(SERIES_INTERVAL - Duration::from_nanos(1))
        ));
        assert!(series_due(MIB, MIB, None));
    }

    proptest! {
        /// Seals of any size up to `segment_size` at any rate: after each seal's due write the uncovered bytes are at
        /// most `SERIES_BYTES − segment_size` (under half the allowance), and before it at most
        /// `max(SERIES_BYTES, segment_size)`, inside the allowance for every `segment_size` up to it.
        #[test]
        fn uncovered_bytes_stay_inside_the_allowance(
            segment_mib in 16u64..=64,
            seals in prop::collection::vec((1u64..=1024, 0u64..=1_200), 1..400),
        ) {
            let segment_size = segment_mib * MIB;
            let mut u = Uncovered::default();
            let mut since: Option<Duration> = None;
            for (i, (len_kib, gap_s)) in seals.into_iter().enumerate() {
                let len = (len_kib * segment_size / 1024).max(4096);
                since = since.map(|d| d + Duration::from_secs(gap_s));
                u.sealed(at(i as u64 + 1), len);
                prop_assert!(u.bytes() <= SERIES_BYTES.max(segment_size));
                prop_assert!(u.bytes() <= READER_ALLOWANCE);
                if series_due(u.bytes(), segment_size, since) {
                    u.covered(at(i as u64 + 1));
                    since = Some(Duration::ZERO);
                }
                prop_assert!(u.bytes() <= SERIES_BYTES.saturating_sub(segment_size));
            }
        }
    }
}
