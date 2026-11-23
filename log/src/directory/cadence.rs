//! When the series directory is written (`format.md` A.18 Cadence): once the sealed bytes it does not cover, plus
//! the largest of the recent seals, would pass [`SERIES_BYTES`], and otherwise at least every [`SERIES_INTERVAL`].
//! So between writes the uncovered sealed bytes stay at most `SERIES_BYTES`, and while a due write waits for the
//! mark sync at most `SERIES_BYTES` plus one seal; a reader beyond its [`READER_ALLOWANCE`] waits one mark-sync
//! interval and tries again (Q-773).

use std::collections::VecDeque;
use std::time::Duration;

use crate::writer::Position;

/// The uncovered sealed bytes the series directory is kept under: half of [`READER_ALLOWANCE`].
pub const SERIES_BYTES: u64 = 32 << 20;
/// Sealed bytes no directory covers that a reader reads directly before it fails with `DirectoryUnavailable`.
pub const READER_ALLOWANCE: u64 = 64 << 20;
/// Positions after the last series directory's `through` above which a clean shutdown writes one through the mark.
pub const SHUTDOWN_POSITIONS: u64 = 1 << 16;
/// The longest time between two series directory writes while segments seal.
pub const SERIES_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Whether the series directory is due: `uncovered` sealed bytes, of which none may be left once `next` more (the
/// largest recent seal) could pass `bound` ([`SERIES_BYTES`] outside tests), or `since` the last write (`None`:
/// never) at least [`SERIES_INTERVAL`].
pub fn series_due(uncovered: u64, next: u64, bound: u64, since: Option<Duration>) -> bool {
    uncovered > 0
        && (uncovered.saturating_add(next) > bound || since.is_none_or(|d| d >= SERIES_INTERVAL))
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

    const MIB: u64 = 1 << 20;

    #[test]
    fn due_by_bytes_by_time_and_never_with_nothing_uncovered() {
        let recent = Some(Duration::from_secs(1));
        let b = SERIES_BYTES;
        assert!(!series_due(0, 64 * MIB, b, None));
        assert!(!series_due(8 * MIB, 16 * MIB, b, recent));
        assert!(series_due(17 * MIB, 16 * MIB, b, recent));
        assert!(series_due(MIB, 64 * MIB, b, recent));
        assert!(series_due(MIB, MIB, b, Some(SERIES_INTERVAL)));
        let just = Some(SERIES_INTERVAL - Duration::from_nanos(1));
        assert!(!series_due(MIB, MIB, b, just));
        assert!(series_due(MIB, MIB, b, None));
    }
}
