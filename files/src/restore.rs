//! The restore record (`0x25`, `format.md` A.14) as a typed view (`spec.md` §8 Restore, Generations). Its codec over
//! `Body::Other` and its fold arrive with roadmap 8.3a.

use kvik_log::manifest::{LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::writer::GenerationStart;

use crate::catalogue::Tier;
use crate::desc::FileDescription;
use crate::record::AckedLoss;
use crate::tier::DeepCopy;

/// One file a restore lists, with the copy it is restored from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredFile {
    /// The file.
    pub description: FileDescription,
    /// The tier it is restored from.
    pub tier: Tier,
    /// The copy.
    pub copy: DeepCopy,
}

/// A restore record (`0x25`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreRecord {
    /// The generation the restore claimed.
    pub generation: u32,
    /// Its reset stamp: the sequence is the generation's first position; the nonce is fresh.
    pub stamp: ResetStamp,
    /// The generation of the records it read.
    pub source_generation: u32,
    /// The last position it restored.
    pub restored_end: RecordPosition,
    /// The ends carried from the restore record it read, each with its generation.
    pub carried_ends: Vec<RestoredEnd>,
    /// The losses carried.
    pub losses: Vec<LostRange>,
    /// The acknowledgements carried.
    pub acknowledgements: Vec<AckedLoss>,
    /// Losses of registry entries.
    pub entry_losses: Vec<LostRange>,
    /// The files, ascending by (kind, generation, id); set-aside segments included.
    pub files: Vec<RestoredFile>,
    /// A drill's record, which is never uploaded.
    pub drill: bool,
}

impl RestoreRecord {
    /// What it sets aside: its own end plus the carried ones (`spec.md` §8 Restore step 1).
    pub fn set_aside(&self) -> SetAside {
        let own = RestoredEnd {
            generation: self.generation,
            end: self.restored_end,
        };
        SetAside::new(
            std::iter::once(own)
                .chain(self.carried_ends.iter().cloned())
                .collect(),
        )
    }

    /// The start of its generation.
    pub fn generation_start(&self) -> GenerationStart {
        GenerationStart {
            generation: self.generation,
            stamp: self.stamp,
            set_aside: self.set_aside(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RestoreRecord {
        RestoreRecord {
            generation: 2,
            stamp: ResetStamp {
                sequence: 31,
                nonce: 9,
            },
            source_generation: 1,
            restored_end: (1, 30),
            carried_ends: vec![RestoredEnd {
                generation: 1,
                end: (0, 12),
            }],
            losses: Vec::new(),
            acknowledgements: Vec::new(),
            entry_losses: Vec::new(),
            files: Vec::new(),
            drill: false,
        }
    }

    #[test]
    fn set_aside_is_its_own_end_then_the_carried_ones() {
        let s = record().set_aside();
        assert_eq!(
            s.ends(),
            &[
                RestoredEnd {
                    generation: 2,
                    end: (1, 30)
                },
                RestoredEnd {
                    generation: 1,
                    end: (0, 12)
                }
            ]
        );
        assert!(s.holds((1, 31)));
        assert!(s.holds((0, 13)));
        assert!(!s.holds((0, 12)));
    }

    #[test]
    fn generation_start_carries_generation_stamp_and_set_aside() {
        let r = record();
        let g = r.generation_start();
        assert_eq!(g.generation, 2);
        assert_eq!(g.stamp, r.stamp);
        assert_eq!(g.set_aside, r.set_aside());
    }
}
