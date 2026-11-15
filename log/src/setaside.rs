//! Positions a restore set aside (`spec.md` §8 Restore step 1): the batches of an earlier generation past the end
//! a restore record restored. Readers and the writer's scans skip them.

use crate::manifest::RecordPosition;

/// A restore record's generation and the end it restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredEnd {
    /// The generation the restore claimed.
    pub generation: u32,
    /// The last position it restored.
    pub end: RecordPosition,
}

/// The restored ends in force: a position is set aside when it lies past an end, in a generation below that end's
/// restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetAside {
    ends: Vec<RestoredEnd>,
}

impl SetAside {
    /// The set of `ends`.
    pub fn new(ends: Vec<RestoredEnd>) -> Self {
        Self { ends }
    }

    /// The ends, as given.
    pub fn ends(&self) -> &[RestoredEnd] {
        &self.ends
    }

    /// Whether `p` is set aside: some end has `p.0 < generation` and `p > end` (`spec.md` §8 Restore step 1).
    pub fn holds(&self, p: RecordPosition) -> bool {
        self.ends.iter().any(|e| p.0 < e.generation && p > e.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end(generation: u32, end: RecordPosition) -> RestoredEnd {
        RestoredEnd { generation, end }
    }

    #[test]
    fn nothing_is_set_aside_by_default() {
        let s = SetAside::default();
        assert!(s.ends().is_empty());
        assert!(!s.holds((0, 1)));
        assert!(!s.holds((u32::MAX, u64::MAX)));
    }

    #[test]
    fn positions_past_the_end_below_the_restores_generation_are_set_aside() {
        let s = SetAside::new(vec![end(1, (0, 10))]);
        assert!(!s.holds((0, 9)));
        assert!(!s.holds((0, 10)));
        assert!(s.holds((0, 11)));
        assert!(s.holds((0, u64::MAX)));
    }

    #[test]
    fn the_restores_own_generation_and_later_ones_are_never_set_aside() {
        let s = SetAside::new(vec![end(1, (0, 10))]);
        assert!(!s.holds((1, 1)));
        assert!(!s.holds((1, 11)));
        assert!(!s.holds((2, 0)));
    }

    #[test]
    fn a_carried_end_sets_aside_its_own_older_generation() {
        let s = SetAside::new(vec![end(2, (1, 5)), end(1, (0, 10))]);
        assert!(s.holds((0, 11)));
        assert!(!s.holds((0, 10)));
        assert!(!s.holds((1, 5)));
        assert!(s.holds((1, 6)));
        assert!(!s.holds((2, 6)));
        assert_eq!(s.ends().len(), 2);
    }
}
