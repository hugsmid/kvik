//! The status file (`format.md` A.18, kind 17): the writer's counters, health and pressure flag with their time, for
//! processes that share no memory with it. It names no position above the last synced mark.

use super::{
    DerivedError, Frame, block_type, decode_frame, encode_frame, position_at, put_position, records,
};
use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::error::EncodeError;
use crate::header::kind;
use crate::writer::Position;

const PREFIX: usize = 48;
/// A reason record's size in v1.
pub const REASON_LEN: u16 = 4;
/// A counter record's size in v1.
pub const COUNTER_LEN: u16 = 16;

/// Health levels (A.18).
pub mod health {
    /// Healthy.
    pub const OK: u8 = 0;
    /// Degraded.
    pub const DEGRADED: u8 = 1;
    /// Critical.
    pub const CRITICAL: u8 = 2;
    /// Fatal: the store is read-only.
    pub const FATAL: u8 = 3;
}

/// Counter ids (A.18). Every id is ignorable: a reader skips one it does not know.
pub mod counter {
    /// Readings queued now.
    pub const QUEUE_DEPTH: u16 = 1;
    /// Total time emits have waited, nanoseconds.
    pub const WAITED_NS: u16 = 2;
    /// Batches committed.
    pub const BATCHES: u16 = 3;
    /// Log syncs.
    pub const SYNCS: u16 = 4;
    /// Commit latency, median, nanoseconds.
    pub const COMMIT_P50_NS: u16 = 5;
    /// Commit latency, 99th percentile, nanoseconds.
    pub const COMMIT_P99_NS: u16 = 6;
    /// Commit latency, largest, nanoseconds.
    pub const COMMIT_MAX_NS: u16 = 7;
    /// Readings the duplicate rule decided by the slow path.
    pub const SLOW_PATH: u16 = 8;
    /// Segments sealed by this writer.
    pub const SEALED_SEGMENTS: u16 = 9;
    /// Free bytes.
    pub const DISK_FREE: u16 = 10;
    /// The batch-sync interval in force, nanoseconds.
    pub const SYNC_INTERVAL_NS: u16 = 11;
    /// Reads of the log for delivery that failed and were retried.
    pub const REFRESH_ERRORS: u16 = 12;
    /// Invalid regions below the mark that delivery passed over.
    pub const CORRUPTIONS: u16 = 13;
}

/// Health reason codes (A.18). A reader shows a code it does not know as "other".
pub mod reason {
    /// A lost range, held until acknowledged.
    pub const DATA_LOST: u16 = 1;
    /// An emit is waiting.
    pub const INGEST_WAITING: u16 = 2;
    /// Free space below the low mark.
    pub const DISK_LOW: u16 = 3;
    /// Free space below the critical mark.
    pub const DISK_CRITICAL: u16 = 4;
    /// A segment cannot be sealed.
    pub const SEAL_BLOCKED: u16 = 5;
    /// A manifest record was rebuilt.
    pub const MANIFEST_REBUILT: u16 = 6;
    /// Read-only after a write or sync failure.
    pub const READ_ONLY: u16 = 7;
    /// Any other reason.
    pub const OTHER: u16 = 0xffff;
}

/// The status file's content; its frame's `written_at` is the time it states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusFile {
    /// The periodic write interval; `None` when only changes are written. A reader ignores a pressure flag older
    /// than twice this.
    pub interval_ns: Option<u64>,
    /// The last synced mark.
    pub mark_synced: Position,
    /// The health level ([`health`]).
    pub health: u8,
    /// The pressure flag (section 3.4 of the query design; set by roadmap 9.0g, clear before it).
    pub pressure: bool,
    /// When the flag was last set.
    pub pressure_since: Option<i64>,
    /// The store's query slot count (roadmap 9.0g; 0 before it).
    pub slot_count: u32,
    /// Health reasons ([`reason`]).
    pub reasons: Vec<u16>,
    /// Counters ([`counter`]), in ascending id.
    pub counters: Vec<(u16, u64)>,
}

impl StatusFile {
    /// The bytes of the file.
    pub fn encode(&self, frame: Frame, store_id: [u8; 16]) -> Result<Vec<u8>, EncodeError> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.interval_ns.unwrap_or(0).to_le_bytes());
        put_position(&mut b, self.mark_synced);
        b.push(self.health);
        b.push(u8::from(self.pressure));
        b.extend_from_slice(&[0; 2]);
        b.extend_from_slice(&self.pressure_since.unwrap_or(0).to_le_bytes());
        b.extend_from_slice(&self.slot_count.to_le_bytes());
        b.extend_from_slice(&(self.reasons.len() as u32).to_le_bytes());
        b.extend_from_slice(&(self.counters.len() as u32).to_le_bytes());
        b.extend_from_slice(&REASON_LEN.to_le_bytes());
        b.extend_from_slice(&COUNTER_LEN.to_le_bytes());
        for r in &self.reasons {
            b.extend_from_slice(&r.to_le_bytes());
            b.extend_from_slice(&[0; 2]);
        }
        for (id, v) in &self.counters {
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&[0; 6]);
            b.extend_from_slice(&v.to_le_bytes());
        }
        encode_frame(kind::STATUS, block_type::STATUS, frame, store_id, &b)
    }

    /// Decodes a status file (A.18 checks).
    pub fn decode(bytes: &[u8], store_id: &[u8; 16]) -> Result<(Frame, Self), DerivedError> {
        let (frame, body) = decode_frame(bytes, store_id, kind::STATUS, block_type::STATUS)?;
        if body.len() < PREFIX || body[20] > health::FATAL || body[21] > 1 {
            return Err(DerivedError::Invalid("status body"));
        }
        let (nr, nc) = (u32_at(body, 36), u32_at(body, 40));
        let (sr, sc) = (u16_at(body, 44), u16_at(body, 46));
        let reasons: Vec<u16> = records(body, PREFIX, nr, sr, usize::from(REASON_LEN))?
            .map(|r| u16_at(r, 0))
            .collect();
        let at = PREFIX + reasons.len() * usize::from(sr);
        let counters: Vec<(u16, u64)> = records(body, at, nc, sc, usize::from(COUNTER_LEN))?
            .map(|r| (u16_at(r, 0), u64_at(r, 8)))
            .collect();
        if at + counters.len() * usize::from(sc) != body.len()
            || !counters.windows(2).all(|w| w[0].0 < w[1].0)
        {
            return Err(DerivedError::Invalid("status records"));
        }
        let interval = u64_at(body, 0);
        let since = i64_at(body, 24);
        Ok((
            frame,
            StatusFile {
                interval_ns: (interval != 0).then_some(interval),
                mark_synced: position_at(body, 8),
                health: body[20],
                pressure: body[21] == 1,
                pressure_since: (since != 0).then_some(since),
                slot_count: u32_at(body, 32),
                reasons,
                counters,
            },
        ))
    }

    /// Whether a reader at wall time `now` takes the pressure flag: set, and the file no older than twice its
    /// interval, so a writer that stopped does not leave readers halved.
    pub fn pressure_in_force(&self, written_at: i64, now: i64) -> bool {
        self.pressure
            && self.interval_ns.is_some_and(|i| {
                now.saturating_sub(written_at)
                    <= i64::try_from(i.saturating_mul(2)).unwrap_or(i64::MAX)
            })
    }

    /// A counter's value, if the file has it.
    pub fn counter(&self, id: u16) -> Option<u64> {
        self.counters
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, v)| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_file_round_trips_and_a_stale_pressure_flag_is_ignored() {
        let s = StatusFile {
            interval_ns: Some(60_000_000_000),
            mark_synced: Position {
                generation: 0,
                sequence: 12,
            },
            health: health::DEGRADED,
            pressure: true,
            pressure_since: Some(5),
            slot_count: 1,
            reasons: vec![reason::DISK_LOW],
            counters: vec![(counter::QUEUE_DEPTH, 3), (counter::COMMIT_P99_NS, 9)],
        };
        let frame = Frame {
            generation: 0,
            file_id: 2,
            written_at: 1_000,
        };
        let bytes = s.encode(frame, [1; 16]).unwrap();
        assert_eq!(
            StatusFile::decode(&bytes, &[1; 16]).unwrap(),
            (frame, s.clone())
        );
        assert!(s.pressure_in_force(1_000, 1_000 + 120_000_000_000));
        assert!(!s.pressure_in_force(1_000, 1_001 + 120_000_000_000));
    }
}
