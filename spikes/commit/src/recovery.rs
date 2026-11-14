//! The decision of `spec.md` §3 Recovery rules 1, 3 and 4 for one open segment. Rule 2 (corruption) is not
//! covered by the spike.

use crate::format::{self, BatchExtent, Mark};

/// Which rule applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Rule 1: same boot as the mark; truncate after the mark.
    SameBoot,
    /// Rule 3: a different boot, or the mark is missing, unreadable or half-updated; keep complete batches,
    /// then (rule 4) sync and write a new mark.
    DifferentBoot,
}

/// What recovery keeps.
#[derive(Debug)]
pub struct Decision {
    /// The rule that applied.
    pub rule: Rule,
    /// Batches kept.
    pub kept: Vec<BatchExtent>,
    /// Everything from this offset on is truncated (zeroed in a preallocated segment).
    pub truncate_from: u64,
    /// Rule 4: the segment and directory must be synced before the new mark is written.
    pub sync_before_resuming: bool,
}

/// Decides recovery for the segment `bytes` (whose first entry has `base_sequence`) given the mark as read.
pub fn decide(
    bytes: &[u8],
    base_sequence: u64,
    mark: Option<Mark>,
    boot_now: [u8; 16],
) -> Decision {
    let scan = format::scan(bytes, base_sequence);
    match mark {
        Some(m) if m.boot_id == boot_now => {
            let kept: Vec<BatchExtent> = scan
                .batches
                .into_iter()
                .filter(|b| b.end <= m.offset)
                .collect();
            Decision {
                rule: Rule::SameBoot,
                kept,
                truncate_from: m.offset,
                sync_before_resuming: false,
            }
        }
        _ => Decision {
            rule: Rule::DifferentBoot,
            kept: scan.batches,
            truncate_from: scan.valid_end,
            sync_before_resuming: true,
        },
    }
}
