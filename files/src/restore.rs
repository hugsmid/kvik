//! The restore record (`0x25`, `format.md` A.14) as a typed view over [`Body::Other`] (`spec.md` §8 Restore,
//! Generations).

use std::collections::BTreeSet;

use kvik_log::Element;
use kvik_log::manifest::{LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::writer::GenerationStart;

use crate::catalogue::Tier;
use crate::desc::FileDescription;
use crate::fields::{self, position_bytes, position_of, u32_of, u64_at};
use crate::record::{
    AckedLoss, Body, FIELD_OLD_EPOCH, RecordError, decode_lost, lost_fields, optional, record_type,
    single,
};
use crate::tier::DeepCopy;

/// One file a restore lists, with the copy it is restored from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredFile {
    /// The file.
    pub description: FileDescription,
    /// The tier the restore places it on (`spec.md` §8 Restore step 3).
    pub tier: Tier,
    /// Its verified copy in the deep tier, which the restore fetches.
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
    /// The ends carried from the restore record it read, each with its generation, in descending generation.
    pub carried_ends: Vec<RestoredEnd>,
    /// The lost ranges carried, ascending by identity.
    pub losses: Vec<LostRange>,
    /// The acknowledged ranges carried, ascending.
    pub acknowledgements: Vec<AckedLoss>,
    /// The recovery-entry loss reports carried (Q-291).
    pub entry_losses: Vec<LostRange>,
    /// The files, ascending by (kind, generation, id); set-aside segments included.
    pub files: Vec<RestoredFile>,
    /// A drill's record, which is never uploaded.
    pub drill: bool,
}

mod field {
    pub const GENERATION: u16 = 1;
    pub const STAMP: u16 = 2;
    pub const SOURCE: u16 = 3;
    pub const END: u16 = 4;
    pub const CARRIED: u16 = 5;
    pub const LOSS: u16 = 6;
    pub const ACK: u16 = 7;
    pub const ENTRY_LOSS: u16 = 8;
    pub const FILE: u16 = 9;
    pub const DRILL: u16 = 10;
}

/// Bytes of a file field before its description: tier, 3 reserved, copy size, copy SHA-256.
const FILE_FIXED: usize = 44;

fn tier_code(t: Tier) -> u8 {
    match t {
        Tier::Hot => 1,
        Tier::Cold => 2,
        Tier::Deep => 3,
    }
}

fn tier_of(c: u8) -> Option<Tier> {
    Some(match c {
        1 => Tier::Hot,
        2 => Tier::Cold,
        3 => Tier::Deep,
        _ => return None,
    })
}

fn nested_loss(l: &LostRange) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    for (id, v) in lost_fields(l) {
        fields::push(&mut out, id, &v).expect("a lost-range field holds at most 16 bytes");
    }
    out
}

fn decode_nested_loss(v: &[u8], field: u16) -> Result<LostRange, RecordError> {
    let list = fields::parse(v).ok_or(RecordError::Invalid(
        "a carried lost range whose fields do not fit",
    ))?;
    if list.iter().any(|(id, _)| !(1..=13).contains(id)) {
        return Err(RecordError::Unsupported(Element::Field {
            entry_type: record_type::RESTORE,
            field,
        }));
    }
    decode_lost(&list)
}

fn position_field(v: &[u8]) -> Result<RecordPosition, RecordError> {
    position_of(v).ok_or(RecordError::Invalid("a position not 12 bytes"))
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

    /// Its body: the fields in ascending id order, each repeated field in its stated order (`format.md` A.14).
    pub fn to_body(&self) -> Body {
        let mut f: Vec<(u16, Vec<u8>)> = vec![
            (field::GENERATION, self.generation.to_le_bytes().to_vec()),
            (field::STAMP, self.stamp.encode().to_vec()),
            (field::SOURCE, self.source_generation.to_le_bytes().to_vec()),
            (field::END, position_bytes(self.restored_end).to_vec()),
        ];
        for e in &self.carried_ends {
            let mut v = e.generation.to_le_bytes().to_vec();
            v.extend_from_slice(&position_bytes(e.end));
            f.push((field::CARRIED, v));
        }
        f.extend(self.losses.iter().map(|l| (field::LOSS, nested_loss(l))));
        for a in &self.acknowledgements {
            let mut v = Vec::with_capacity(36);
            v.extend_from_slice(&position_bytes(a.identity));
            v.extend_from_slice(&position_bytes(a.first));
            v.extend_from_slice(&position_bytes(a.last));
            f.push((field::ACK, v));
        }
        f.extend(
            self.entry_losses
                .iter()
                .map(|l| (field::ENTRY_LOSS, nested_loss(l))),
        );
        for rf in &self.files {
            let mut v = vec![tier_code(rf.tier), 0, 0, 0];
            v.extend_from_slice(&rf.copy.size.to_le_bytes());
            v.extend_from_slice(&rf.copy.sha256);
            v.extend_from_slice(&rf.description.encode());
            f.push((field::FILE, v));
        }
        if self.drill {
            f.push((field::DRILL, Vec::new()));
        }
        Body::Other {
            record_type: record_type::RESTORE,
            skippable: false,
            fields: f,
        }
    }

    /// The record a body holds, checked; `None` for a body that is not a `0x25`. Ignorable fields are skipped and an
    /// unknown critical field is a version refusal.
    pub fn from_body(b: &Body) -> Option<Result<Self, RecordError>> {
        match b {
            Body::Other {
                record_type: record_type::RESTORE,
                fields,
                ..
            } => Some(decode(fields)),
            _ => None,
        }
    }

    /// The rules of `format.md` A.14 for a restore record.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.generation == 0 {
            return Err("a restore record of generation 0");
        }
        if self.source_generation >= self.generation {
            return Err("a source generation not below the restore's");
        }
        if self.stamp.sequence != 1 || self.stamp.nonce == 0 {
            return Err("a reset stamp not at sequence 1 or with a zero nonce");
        }
        if self.restored_end.0 >= self.generation {
            return Err("a restored end not below the restore's generation");
        }
        if self
            .carried_ends
            .iter()
            .any(|e| e.generation > self.source_generation || e.end.0 >= e.generation)
            || !self
                .carried_ends
                .windows(2)
                .all(|w| w[0].generation > w[1].generation)
        {
            return Err("carried ends out of order or not below their generations");
        }
        let range_ok = |l: &LostRange| l.first <= l.last;
        if !self.losses.iter().all(range_ok)
            || !self.entry_losses.iter().all(range_ok)
            || self.acknowledgements.iter().any(|a| a.first > a.last)
        {
            return Err("a range whose first position is above its last");
        }
        if !self
            .losses
            .windows(2)
            .all(|w| w[0].identity < w[1].identity)
        {
            return Err("carried losses not in strictly ascending order of identity");
        }
        if !self.acknowledgements.windows(2).all(|w| w[0] < w[1]) {
            return Err("carried acknowledgements not in strictly ascending order");
        }
        for rf in &self.files {
            rf.description.check()?;
        }
        let key = |rf: &RestoredFile| {
            let d = &rf.description;
            (d.kind, d.generation, d.file_id)
        };
        if !self.files.windows(2).all(|w| key(&w[0]) < key(&w[1])) {
            return Err("files not in strictly ascending order of kind, generation and id");
        }
        let refs: BTreeSet<_> = self
            .files
            .iter()
            .map(|rf| rf.description.file_ref())
            .collect();
        if refs.len() != self.files.len() {
            return Err("a file listed twice");
        }
        Ok(())
    }
}

fn decode(list: &[(u16, Vec<u8>)]) -> Result<RestoreRecord, RecordError> {
    let mut own: Vec<(u16, &[u8])> = Vec::with_capacity(list.len());
    for (id, v) in list {
        match *id {
            FIELD_OLD_EPOCH => {
                return Err(RecordError::Invalid(
                    "field 100 held among a record's fields",
                ));
            }
            0x8000.. => {}
            field::GENERATION..=field::DRILL => own.push((*id, v)),
            _ => {
                return Err(RecordError::Unsupported(Element::Field {
                    entry_type: record_type::RESTORE,
                    field: *id,
                }));
            }
        }
    }
    let u32f = |id| {
        single(&own, id)
            .and_then(|v| u32_of(v).ok_or(RecordError::Invalid("a u32 field not 4 bytes")))
    };
    let all = |id: u16| own.iter().filter(move |(i, _)| *i == id).map(|(_, v)| *v);
    let stamp = ResetStamp::decode(single(&own, field::STAMP)?)
        .ok_or(RecordError::Invalid("a reset stamp not 16 bytes"))?;
    let carried_ends = all(field::CARRIED)
        .map(|v| {
            if v.len() != 16 {
                return Err(RecordError::Invalid("a carried end not 16 bytes"));
            }
            Ok(RestoredEnd {
                generation: u32_of(&v[..4]).unwrap_or_default(),
                end: position_field(&v[4..])?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let losses = all(field::LOSS)
        .map(|v| decode_nested_loss(v, field::LOSS))
        .collect::<Result<Vec<_>, _>>()?;
    let acknowledgements = all(field::ACK)
        .map(|v| {
            if v.len() != 36 {
                return Err(RecordError::Invalid("an acknowledged range not 36 bytes"));
            }
            Ok(AckedLoss {
                identity: position_field(&v[..12])?,
                first: position_field(&v[12..24])?,
                last: position_field(&v[24..])?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let entry_losses = all(field::ENTRY_LOSS)
        .map(|v| decode_nested_loss(v, field::ENTRY_LOSS))
        .collect::<Result<Vec<_>, _>>()?;
    let files = all(field::FILE)
        .map(|v| {
            if v.len() < FILE_FIXED {
                return Err(RecordError::Invalid(
                    "a restored file shorter than 44 bytes",
                ));
            }
            let tier = tier_of(v[0]).ok_or(RecordError::Invalid("an unknown tier"))?;
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(&v[12..FILE_FIXED]);
            Ok(RestoredFile {
                description: FileDescription::decode(&v[FILE_FIXED..])
                    .map_err(RecordError::Invalid)?,
                tier,
                copy: DeepCopy {
                    size: u64_at(v, 4),
                    sha256,
                },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let drill = match optional(&own, field::DRILL)? {
        None => false,
        Some([]) => true,
        Some(_) => return Err(RecordError::Invalid("field 10 holds a value")),
    };
    let r = RestoreRecord {
        generation: u32f(field::GENERATION)?,
        stamp,
        source_generation: u32f(field::SOURCE)?,
        restored_end: position_field(single(&own, field::END)?)?,
        carried_ends,
        losses,
        acknowledgements,
        entry_losses,
        files,
        drill,
    };
    r.check().map_err(RecordError::Invalid)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{FileClass, kind};
    use kvik_log::manifest::LossCause;

    fn loss(identity: RecordPosition, last: RecordPosition) -> LostRange {
        LostRange {
            identity,
            first: identity,
            last,
            file_id: 4,
            offset: 64,
            length: 200,
            cause: LossCause::Corruption,
            boot_id: [5; 16],
            id_floor: 3,
            time_before: Some(-7),
            time_after: None,
            extends: true,
            stamp: Some(ResetStamp {
                sequence: 2,
                nonce: 9,
            }),
        }
    }

    fn segment(generation: u32, file_id: u64, first: u64, last: u64) -> RestoredFile {
        RestoredFile {
            description: FileDescription {
                kind: kind::LOG_SEGMENT,
                class: FileClass::None,
                generation,
                file_id,
                range: Some(((generation, first), (generation, last))),
                hash: [file_id as u8; 32],
                inputs: Vec::new(),
            },
            tier: Tier::Hot,
            copy: DeepCopy {
                size: 4096 + file_id,
                sha256: [9; 32],
            },
        }
    }

    fn record() -> RestoreRecord {
        RestoreRecord {
            generation: 2,
            stamp: ResetStamp {
                sequence: 1,
                nonce: 9,
            },
            source_generation: 1,
            restored_end: (1, 30),
            carried_ends: vec![RestoredEnd {
                generation: 1,
                end: (0, 12),
            }],
            losses: vec![loss((0, 3), (0, 4)), loss((1, 7), (1, 9))],
            acknowledgements: vec![AckedLoss {
                identity: (0, 3),
                first: (0, 3),
                last: (0, 4),
            }],
            entry_losses: vec![loss((1, 20), (1, 20))],
            files: vec![segment(0, 1, 1, 12), segment(1, 5, 1, 30)],
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

    #[test]
    fn a_restore_record_round_trips_through_its_body() {
        for drill in [false, true] {
            let r = RestoreRecord { drill, ..record() };
            assert_eq!(RestoreRecord::from_body(&r.to_body()), Some(Ok(r)));
        }
    }

    #[test]
    fn another_body_is_not_a_restore_record() {
        assert_eq!(RestoreRecord::from_body(&Body::Empty), None);
        let other = Body::Other {
            record_type: record_type::QUARANTINE,
            skippable: false,
            fields: Vec::new(),
        };
        assert_eq!(RestoreRecord::from_body(&other), None);
    }

    fn fields_of(b: Body) -> Vec<(u16, Vec<u8>)> {
        match b {
            Body::Other { fields, .. } => fields,
            _ => unreachable!(),
        }
    }

    fn decoded(fields: Vec<(u16, Vec<u8>)>) -> Result<RestoreRecord, RecordError> {
        RestoreRecord::from_body(&Body::Other {
            record_type: record_type::RESTORE,
            skippable: false,
            fields,
        })
        .unwrap()
    }

    #[test]
    fn ignorable_fields_are_skipped_and_unknown_critical_ones_refused() {
        let mut f = fields_of(record().to_body());
        f.push((0x8001, vec![1, 2, 3]));
        assert_eq!(decoded(f.clone()), Ok(record()));
        f.push((11, vec![]));
        assert_eq!(
            decoded(f),
            Err(RecordError::Unsupported(Element::Field {
                entry_type: record_type::RESTORE,
                field: 11
            }))
        );
    }

    #[test]
    fn a_missing_or_repeated_required_field_is_invalid() {
        for id in field::GENERATION..=field::END {
            let f: Vec<_> = fields_of(record().to_body())
                .into_iter()
                .filter(|(i, _)| *i != id)
                .collect();
            assert!(matches!(decoded(f), Err(RecordError::Invalid(_))), "{id}");
            let mut f = fields_of(record().to_body());
            let again = f.iter().find(|(i, _)| *i == id).unwrap().clone();
            f.insert(0, again);
            assert!(matches!(decoded(f), Err(RecordError::Invalid(_))), "{id}");
        }
    }

    #[test]
    fn each_rule_of_the_layout_is_checked() {
        let bad: Vec<fn(&mut RestoreRecord)> = vec![
            |r| r.generation = 0,
            |r| r.source_generation = 2,
            |r| r.stamp.sequence = 2,
            |r| r.stamp.nonce = 0,
            |r| r.restored_end = (2, 1),
            |r| r.carried_ends[0].generation = 2,
            |r| r.carried_ends[0].end = (1, 1),
            |r| {
                r.carried_ends.push(RestoredEnd {
                    generation: 1,
                    end: (0, 1),
                })
            },
            |r| r.losses.swap(0, 1),
            |r| r.losses[0].first = (0, 5),
            |r| r.entry_losses[0].last = (1, 19),
            |r| r.acknowledgements.push(r.acknowledgements[0]),
            |r| r.acknowledgements[0].last = (0, 2),
            |r| r.files.swap(0, 1),
            |r| r.files[1].description.file_id = 1,
            |r| r.files[0].description.class = FileClass::Current,
        ];
        for (i, f) in bad.into_iter().enumerate() {
            let mut r = record();
            f(&mut r);
            assert!(r.check().is_err(), "rule {i}");
            assert!(
                matches!(
                    decoded(fields_of(r.to_body())),
                    Err(RecordError::Invalid(_))
                ),
                "rule {i}"
            );
        }
        assert_eq!(record().check(), Ok(()));
    }

    #[test]
    fn malformed_values_are_invalid() {
        let set = |id: u16, v: Vec<u8>| {
            let mut f = fields_of(record().to_body());
            let at = f.iter().position(|(i, _)| *i == id).unwrap();
            f[at].1 = v;
            decoded(f)
        };
        assert!(set(field::GENERATION, vec![1]).is_err());
        assert!(set(field::STAMP, vec![0; 15]).is_err());
        assert!(set(field::END, vec![0; 13]).is_err());
        assert!(set(field::CARRIED, vec![0; 15]).is_err());
        assert!(set(field::ACK, vec![0; 35]).is_err());
        assert!(set(field::LOSS, vec![1, 0, 12]).is_err());
        assert!(set(field::FILE, vec![1; 43]).is_err());
        let mut file = fields_of(record().to_body())
            .into_iter()
            .find(|(i, _)| *i == field::FILE)
            .unwrap()
            .1;
        file[0] = 4;
        assert!(set(field::FILE, file).is_err());
        let mut f = fields_of(record().to_body());
        f.push((field::DRILL, vec![1]));
        assert!(decoded(f).is_err());
        let mut nested = nested_loss(&loss((0, 3), (0, 4)));
        fields::push(&mut nested, 14, &[]).unwrap();
        assert_eq!(
            set(field::LOSS, nested),
            Err(RecordError::Unsupported(Element::Field {
                entry_type: record_type::RESTORE,
                field: field::LOSS
            }))
        );
    }
}
