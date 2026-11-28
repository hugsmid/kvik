//! The layout of a published compacted file: publish record field 3 (`format.md` A.14, Q-792, Q-1850). It repeats the
//! file's own description and trailer, so the catalogue plans, checks counts (compaction design §3.4) and finds
//! C(g) without opening a file, and a rebuild restates it from the file.

use crate::desc::{FileRef, kind};
use crate::fields::{u32_at, u64_at};

/// Bytes of a layout before its per-generation counts.
const FIXED: usize = 88;
/// Bytes of one per-generation count.
const PER_GENERATION: usize = 12;
const ENTRIES: u8 = 1;
const SET_ASIDE: u8 = 2;
const COMPLETE: u8 = 4;
const NAME_RANGE: u8 = 8;
/// The greatest level a file may have (`format-compacted.md` B.1: sparse).
const MAX_LEVEL: u8 = 4;

/// What publish record field 3 says about one published compacted file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompactedLayout {
    /// The file's `file_id`; its kind is always a compacted segment.
    pub file_id: u64,
    /// Its merge level (`format-compacted.md` B.1).
    pub level: u8,
    /// Its period's start.
    pub period_start: i64,
    /// Its period's length.
    pub period_len: i64,
    /// The run id it shares with the files published with it.
    pub run_id: u64,
    /// How many files the run published.
    pub run_count: u32,
    /// The least `t` of a fact; 0 with none.
    pub min_t: i64,
    /// The greatest reach of a fact; 0 with none.
    pub max_reach: i64,
    /// The least commit time of its batch rows.
    pub first_commit_time: i64,
    /// The greatest commit time of its batch rows.
    pub last_commit_time: i64,
    /// Facts held.
    pub fact_count: u64,
    /// Positions held per generation, generations strictly ascending, each count at least 1.
    pub positions: Vec<(u32, u64)>,
    /// It holds entries.
    pub has_entries: bool,
    /// It holds set-aside chunks.
    pub has_set_aside: bool,
    /// Every element of every input was known and carried.
    pub complete: bool,
    /// A split part's first and last series name.
    pub name_range: Option<(String, String)>,
}

impl CompactedLayout {
    /// The file it describes.
    pub fn file_ref(&self) -> FileRef {
        FileRef {
            kind: kind::COMPACTED,
            file_id: self.file_id,
        }
    }

    /// Positions held, over every generation.
    pub fn entry_count(&self) -> u64 {
        self.positions
            .iter()
            .fold(0u64, |a, (_, n)| a.saturating_add(*n))
    }

    /// Its canonical bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FIXED + PER_GENERATION * self.positions.len());
        let flags = [
            (self.has_entries, ENTRIES),
            (self.has_set_aside, SET_ASIDE),
            (self.complete, COMPLETE),
            (self.name_range.is_some(), NAME_RANGE),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |f, (_, bit)| f | bit);
        v.extend_from_slice(&[kind::COMPACTED, self.level, flags, 0]);
        v.extend_from_slice(&self.run_count.to_le_bytes());
        for w in [
            self.file_id,
            self.period_start as u64,
            self.period_len as u64,
            self.run_id,
            self.min_t as u64,
            self.max_reach as u64,
            self.first_commit_time as u64,
            self.last_commit_time as u64,
            self.fact_count,
        ] {
            v.extend_from_slice(&w.to_le_bytes());
        }
        v.extend_from_slice(&(self.positions.len() as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        for (g, n) in &self.positions {
            v.extend_from_slice(&g.to_le_bytes());
            v.extend_from_slice(&n.to_le_bytes());
        }
        if let Some((a, b)) = &self.name_range {
            for s in [a, b] {
                v.extend_from_slice(&(s.len() as u16).to_le_bytes());
                v.extend_from_slice(s.as_bytes());
            }
        }
        v
    }

    /// Strict decoding: every check of [`CompactedLayout::check`], exact lengths and zero reserved bytes.
    pub fn decode(v: &[u8]) -> Result<Self, &'static str> {
        if v.len() < FIXED {
            return Err("a layout shorter than 88 bytes");
        }
        if v[0] != kind::COMPACTED {
            return Err("a layout of a file that is not a compacted segment");
        }
        let flags = v[2];
        if flags & !(ENTRIES | SET_ASIDE | COMPLETE | NAME_RANGE) != 0 {
            return Err("a layout flag bit undefined");
        }
        if v[3] != 0 || u32_at(v, 84) != 0 {
            return Err("a layout's reserved bytes not zero");
        }
        let generations = u32_at(v, 80) as usize;
        let mut at = generations
            .checked_mul(PER_GENERATION)
            .and_then(|n| n.checked_add(FIXED))
            .filter(|end| *end <= v.len())
            .ok_or("a layout's counts per generation do not fit")?;
        let positions = (0..generations)
            .map(|k| {
                let o = FIXED + k * PER_GENERATION;
                (u32_at(v, o), u64_at(v, o + 4))
            })
            .collect();
        let name_range = if flags & NAME_RANGE != 0 {
            let mut name = || -> Result<String, &'static str> {
                let len = v
                    .get(at..at + 2)
                    .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
                    .ok_or("a layout's name range does not fit")?;
                let s = v
                    .get(at + 2..at + 2 + len)
                    .ok_or("a layout's name range does not fit")?;
                at += 2 + len;
                String::from_utf8(s.to_vec()).map_err(|_| "a layout's name not UTF-8")
            };
            let a = name()?;
            Some((a, name()?))
        } else {
            None
        };
        if at != v.len() {
            return Err("a layout longer than its fields");
        }
        let l = Self {
            file_id: u64_at(v, 8),
            level: v[1],
            period_start: u64_at(v, 16) as i64,
            period_len: u64_at(v, 24) as i64,
            run_id: u64_at(v, 32),
            run_count: u32_at(v, 4),
            min_t: u64_at(v, 40) as i64,
            max_reach: u64_at(v, 48) as i64,
            first_commit_time: u64_at(v, 56) as i64,
            last_commit_time: u64_at(v, 64) as i64,
            fact_count: u64_at(v, 72),
            positions,
            has_entries: flags & ENTRIES != 0,
            has_set_aside: flags & SET_ASIDE != 0,
            complete: flags & COMPLETE != 0,
            name_range,
        };
        l.check()?;
        Ok(l)
    }

    /// What a layout must say of itself: a level B.1 defines, a positive period, a run of at least one file, at least
    /// one generation in strictly ascending order with counts of at least 1 summing to at least `fact_count`, commit
    /// times ordered, and a name range ordered.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.level > MAX_LEVEL {
            return Err("a layout level undefined");
        }
        if self.period_len <= 0 || self.period_start.checked_add(self.period_len).is_none() {
            return Err("a layout period empty or out of range");
        }
        if self.run_count == 0 {
            return Err("a layout run of no file");
        }
        if self.positions.is_empty()
            || self.positions.iter().any(|(_, n)| *n == 0)
            || !self.positions.windows(2).all(|w| w[0].0 < w[1].0)
        {
            return Err("a layout's counts per generation not ascending or zero");
        }
        if self
            .positions
            .iter()
            .try_fold(0u64, |a, (_, n)| a.checked_add(*n))
            .is_none_or(|e| e < self.fact_count)
        {
            return Err("a layout holding fewer positions than facts");
        }
        if self.first_commit_time > self.last_commit_time {
            return Err("a layout's commit times not ordered");
        }
        if self.name_range.as_ref().is_some_and(|(a, b)| {
            a > b || a.len() > usize::from(u16::MAX) || b.len() > usize::from(u16::MAX)
        }) {
            return Err("a layout's name range not ordered");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> CompactedLayout {
        CompactedLayout {
            file_id: 9,
            level: 0,
            period_start: 86_400,
            period_len: 86_400,
            run_id: 3,
            run_count: 2,
            min_t: 86_401,
            max_reach: 86_500,
            first_commit_time: 10,
            last_commit_time: 20,
            fact_count: 5,
            positions: vec![(0, 6), (1, 1)],
            has_entries: true,
            has_set_aside: false,
            complete: true,
            name_range: Some(("a/b/c".into(), "a/b/d".into())),
        }
    }

    #[test]
    fn a_layout_decodes_to_itself_with_and_without_a_name_range() {
        let mut l = sample();
        assert_eq!(CompactedLayout::decode(&l.encode()), Ok(l.clone()));
        l.name_range = None;
        assert_eq!(CompactedLayout::decode(&l.encode()), Ok(l));
    }

    #[test]
    fn a_layout_refuses_trailing_bytes_reserved_bytes_and_undefined_flags() {
        let b = sample().encode();
        let mut t = b.clone();
        t.push(0);
        assert!(CompactedLayout::decode(&t).is_err());
        let mut r = b.clone();
        r[3] = 1;
        assert!(CompactedLayout::decode(&r).is_err());
        let mut f = b.clone();
        f[2] |= 0x10;
        assert!(CompactedLayout::decode(&f).is_err());
        let mut k = b;
        k[0] = kind::LOG_SEGMENT;
        assert!(CompactedLayout::decode(&k).is_err());
    }

    #[test]
    fn a_layout_refuses_fewer_positions_than_facts_and_unordered_generations() {
        let mut l = sample();
        l.fact_count = 8;
        assert!(l.check().is_err());
        let mut l = sample();
        l.positions = vec![(1, 1), (0, 6)];
        assert!(l.check().is_err());
        let mut l = sample();
        l.positions = vec![(0, 0)];
        assert!(l.check().is_err());
    }
}
