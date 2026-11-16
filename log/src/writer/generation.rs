//! A start in a claimed generation (`spec.md` §8 Generations, Restore step 1): the generation's first position and
//! reset stamp from its restore record, its first mark, and the sealed log of earlier generations read past what
//! the restores set aside.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use crate::batch::SegmentIdentity;
use crate::content::{BatchContent, Item};
use crate::entries::{Entry, IntervalLength};
use crate::fs::OpenMode;
use crate::manifest::{LostRange, RecordPosition};
use crate::mark::{MarkSlot, ResetStamp, encode_mark_file, read_mark};
use crate::name::SegmentName;
use crate::scan::{ScanLimit, scan, set_aside};
use crate::setaside::SetAside;
use crate::store::{MARK, StoreDir, StoreError};

use super::GenerationStart;

/// The generation a start runs in.
#[derive(Clone, Debug)]
pub(super) struct Gen {
    pub g: u32,
    /// The generation's first sequence.
    pub first: u64,
    /// The reset stamp the generation began with: zero in generation 0.
    pub stamp: ResetStamp,
    pub set_aside: SetAside,
}

impl Gen {
    pub(super) fn of(start: Option<&GenerationStart>) -> Result<Self, StoreError> {
        let Some(s) = start else {
            return Ok(Self {
                g: 0,
                first: 1,
                stamp: ResetStamp::default(),
                set_aside: SetAside::default(),
            });
        };
        if s.generation == 0 {
            return Err(StoreError::Corrupt("generation 0 is never a restore's"));
        }
        if s.stamp.sequence == 0 {
            return Err(StoreError::Corrupt("a generation's first position is 0"));
        }
        Ok(Self {
            g: s.generation,
            first: s.stamp.sequence,
            stamp: s.stamp,
            set_aside: s.set_aside.clone(),
        })
    }

    /// The mark a generation starts with: its stamp and no position (Q-260).
    pub(super) fn first_mark(&self, store_id: [u8; 16], boot_id: [u8; 16], floor: u32) -> MarkSlot {
        MarkSlot {
            generation: self.g,
            store_id,
            boot_id,
            counter: 1,
            file_id: 0,
            offset: 0,
            last_sequence: self.first - 1,
            max_series_id: floor,
            stamp: self.stamp,
        }
    }

    /// Whether `m` is a restored generation's first mark, which a start reads as no mark (Q-260).
    pub(super) fn is_first_mark(&self, m: &MarkSlot) -> bool {
        self.g > 0
            && m.generation == self.g
            && m.file_id == 0
            && m.offset == 0
            && m.last_sequence == self.first - 1
            && m.stamp == self.stamp
    }

    /// Refuses a file of a generation this start cannot run beside (Q-261).
    pub(super) fn check(&self, found: u32, sealed: bool) -> Result<(), StoreError> {
        if found > self.g || (found < self.g && !sealed) {
            return Err(StoreError::Generation {
                found,
                start: self.g,
            });
        }
        Ok(())
    }

    /// `l`, found by this start, in this generation.
    pub(super) fn place(&self, l: &mut LostRange) {
        l.identity.0 = self.g;
        l.first.0 = self.g;
        l.last.0 = self.g;
    }
}

/// What the sealed log above the registry file's `through` holds that a start needs (Q-130, Q-262).
#[derive(Debug, Default)]
pub(super) struct SealedScan {
    /// Consumers named by routing changes.
    pub consumers: BTreeSet<String>,
    /// The highest series id defined or used, set-aside batches included (`spec.md` §8 Restore step 1).
    pub id_floor: u32,
    /// Interval lengths by definition, in log order.
    pub intervals: Vec<(u32, IntervalLength)>,
}

/// Reads the sealed segments of this generation and earlier ones above `through`, newest first, skipping what is
/// set aside except for the ids it reserves. `None`: no registry file of this generation, so every one is read.
pub(super) fn scan_sealed(
    store: &StoreDir,
    sealed: &[(SegmentName, PathBuf)],
    through: Option<RecordPosition>,
    generation: &Gen,
) -> SealedScan {
    let store_id = store.descriptor().store_id;
    let mut out = SealedScan::default();
    let mut found: Vec<(RecordPosition, u32, IntervalLength)> = Vec::new();
    let mut by_id: Vec<&(SegmentName, PathBuf)> = sealed
        .iter()
        .filter(|(n, _)| n.generation <= generation.g)
        .collect();
    by_id.sort_by_key(|(n, _)| n.file_id);
    for (name, path) in by_id.into_iter().rev() {
        let Ok(bytes) = store.read_file(path) else {
            continue;
        };
        let id = SegmentIdentity {
            store_id,
            file_id: name.file_id,
            generation: name.generation,
        };
        let Ok(r) = scan(&bytes, &id, ScanLimit::End) else {
            continue;
        };
        let above = |p: RecordPosition| through.is_none_or(|t| p > t);
        for b in r
            .batches
            .iter()
            .filter(|b| above((b.commit.generation, b.commit.last_sequence)))
        {
            let Ok(content) = BatchContent::decode(b) else {
                continue;
            };
            let aside = set_aside(b, &generation.set_aside);
            let at = (b.commit.generation, b.commit.first_sequence);
            for i in &content.items {
                match i {
                    Item::Fact(f) => out.id_floor = out.id_floor.max(f.series_id),
                    Item::Entry(Entry::Definition(d)) => {
                        out.id_floor = out.id_floor.max(d.id);
                        if !aside && let Some(iv) = &d.interval {
                            found.push((at, d.id, iv.clone()));
                        }
                    }
                    Item::Entry(Entry::Routing(rc)) if !aside => {
                        out.consumers.insert(rc.consumer.clone());
                    }
                    _ => {}
                }
            }
        }
        if let Some(t) = through
            && r.batches
                .first()
                .is_none_or(|b| (b.commit.generation, b.commit.first_sequence) <= t)
        {
            break;
        }
    }
    found.sort_by_key(|f| f.0);
    out.intervals = found.into_iter().map(|(_, id, iv)| (id, iv)).collect();
    out
}

/// Writes the first mark of a restored generation, its stamp and no position, unless the directory holds it
/// already (`spec.md` §8 Restore step 1; Q-260). A restore calls it once, after its files are installed.
pub fn init_generation(
    store: &StoreDir,
    start: &GenerationStart,
    _wall: i64,
) -> Result<(), StoreError> {
    let generation = Gen::of(Some(start))?;
    let fs = store.fs().clone();
    let log_dir = store.log_dir();
    let store_id = store.descriptor().store_id;
    let path = log_dir.join(MARK);
    let existing = match store.read_file(&path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(b) = &existing
        && let Some((_, m)) = read_mark(b, &store_id).map_err(StoreError::Unsupported)?
    {
        return if m.generation == generation.g && m.stamp == generation.stamp {
            Ok(())
        } else {
            Err(StoreError::Generation {
                found: m.generation,
                start: generation.g,
            })
        };
    }
    let image = encode_mark_file([Some(generation.first_mark(store_id, [0; 16], 0)), None]);
    let f = match existing {
        None => {
            let f = fs.open(&path, OpenMode::CreateNew)?;
            f.write_at(&image, 0)?;
            f.sync_all()?;
            f
        }
        Some(_) => {
            let f = fs.open(&path, OpenMode::ReadWrite)?;
            f.write_at(&image, 0)?;
            f.sync_data()?;
            f
        }
    };
    drop(f);
    fs.sync_dir(&log_dir)?;
    Ok(())
}
