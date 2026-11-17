//! The description block (`0x36`, `docs/format-compacted.md` B.1): a field list (`format.md` A.6 framing).

use super::MAX_INPUTS;
use super::error::{Bad, FileElement};

/// A compacted file's class (`format.md` A.14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// Current facts: one per key.
    Current = 1,
    /// Revisions, and facts set aside by a restore.
    Revision = 2,
}

/// The merge level a file was written at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// A compaction of log segments.
    L0 = 0,
    /// An hourly merge of one partition's L0 files.
    Hour = 1,
    /// A day.
    Day = 2,
    /// A week.
    Week = 3,
    /// A sparse period of 1, 13 or 52 weeks.
    Sparse = 4,
}

/// An input file: kind and `file_id`, as in `format.md` A.14's file description.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Input {
    /// 1 log segment, 2 compacted segment.
    pub kind: u8,
    /// Its `file_id`.
    pub file_id: u64,
}

/// What the description block says about the file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Description {
    /// Field 1.
    pub class: Class,
    /// Field 2.
    pub level: Level,
    /// Field 3: UTC nanoseconds.
    pub period_start: i64,
    /// Field 4: nanoseconds, above 0.
    pub period_len: i64,
    /// Field 5: the lowest `file_id` among the outputs published together.
    pub run_id: u64,
    /// Field 6: how many outputs were published together.
    pub run_count: u32,
    /// Field 7: strictly ascending, 1 to 1,024.
    pub inputs: Vec<Input>,
    /// Fields 8 and 9: the first and last series name of a split part.
    pub name_range: Option<(String, String)>,
    /// Field 10: the positions held per generation, generations strictly ascending, each count at least 1.
    pub positions: Vec<(u32, u64)>,
    /// Field 11: the file holds entries.
    pub has_entries: bool,
    /// Field 12: the file holds set-aside chunks.
    pub has_set_aside: bool,
    /// Field 13: every element of every input was known and carried (Q-809, Q-810).
    pub complete: bool,
}

const CLASS: u16 = 1;
const LEVEL: u16 = 2;
const PERIOD_START: u16 = 3;
const PERIOD_LEN: u16 = 4;
const RUN_ID: u16 = 5;
const RUN_COUNT: u16 = 6;
const INPUTS: u16 = 7;
const FIRST_NAME: u16 = 8;
const LAST_NAME: u16 = 9;
const POSITIONS: u16 = 10;
const HAS_ENTRIES: u16 = 11;
const HAS_SET_ASIDE: u16 = 12;
const COMPLETE: u16 = 13;
const INPUT_LEN: usize = 12;
const GENERATION_LEN: usize = 16;
/// The most generations field 10 holds: its value fits a field.
const MAX_GENERATIONS: usize = u16::MAX as usize / GENERATION_LEN;

pub(crate) fn push_field(out: &mut Vec<u8>, id: u16, value: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

/// The fields of a field list in order, or `None` if one does not fit. Padding is not read.
pub(crate) fn parse_fields(body: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let h = body.get(at..at.checked_add(4)?)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
        let start = at + 4;
        out.push((id, body.get(start..start.checked_add(len)?)?));
        at = (start + len).div_ceil(4) * 4;
    }
    Some(out)
}

fn name_ok(s: &str) -> bool {
    (1..=255).contains(&s.len())
}

impl Description {
    /// Checks the rules a v1 writer keeps: a positive period that does not overflow, a run count of at least 1,
    /// 1 to 1,024 strictly ascending inputs of kind 1 or 2, names of 1 to 255 bytes with the first at or below the
    /// last, entries only in a current file and set-aside chunks only in a revision file.
    pub(crate) fn check(&self) -> Result<(), &'static str> {
        if self.period_len <= 0 || self.period_start.checked_add(self.period_len).is_none() {
            return Err("description period");
        }
        if self.run_count == 0 {
            return Err("description run count");
        }
        if self.inputs.is_empty()
            || self.inputs.len() > MAX_INPUTS
            || !self.inputs.windows(2).all(|w| w[0] < w[1])
            || self.inputs.iter().any(|i| !matches!(i.kind, 1 | 2))
        {
            return Err("description inputs");
        }
        if let Some((a, b)) = &self.name_range
            && (!name_ok(a) || !name_ok(b) || a.as_bytes() > b.as_bytes())
        {
            return Err("description name range");
        }
        if self.positions.is_empty()
            || self.positions.len() > MAX_GENERATIONS
            || !self.positions.windows(2).all(|w| w[0].0 < w[1].0)
            || self.positions.iter().any(|&(_, n)| n == 0)
        {
            return Err("description positions per generation");
        }
        if (self.has_entries && self.class != Class::Current)
            || (self.has_set_aside && self.class != Class::Revision)
        {
            return Err("description class flags");
        }
        Ok(())
    }

    /// The canonical body: fields in ascending id order.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(96 + INPUT_LEN * self.inputs.len());
        push_field(&mut b, CLASS, &[self.class as u8]);
        push_field(&mut b, LEVEL, &[self.level as u8]);
        push_field(&mut b, PERIOD_START, &self.period_start.to_le_bytes());
        push_field(&mut b, PERIOD_LEN, &self.period_len.to_le_bytes());
        push_field(&mut b, RUN_ID, &self.run_id.to_le_bytes());
        push_field(&mut b, RUN_COUNT, &self.run_count.to_le_bytes());
        let mut inputs = Vec::with_capacity(INPUT_LEN * self.inputs.len());
        for i in &self.inputs {
            inputs.extend_from_slice(&[i.kind, 0, 0, 0]);
            inputs.extend_from_slice(&i.file_id.to_le_bytes());
        }
        push_field(&mut b, INPUTS, &inputs);
        if let Some((first, last)) = &self.name_range {
            push_field(&mut b, FIRST_NAME, first.as_bytes());
            push_field(&mut b, LAST_NAME, last.as_bytes());
        }
        let mut positions = Vec::with_capacity(GENERATION_LEN * self.positions.len());
        for &(g, n) in &self.positions {
            positions.extend_from_slice(&g.to_le_bytes());
            positions.extend_from_slice(&[0; 4]);
            positions.extend_from_slice(&n.to_le_bytes());
        }
        push_field(&mut b, POSITIONS, &positions);
        for (flag, id) in [
            (self.has_entries, HAS_ENTRIES),
            (self.has_set_aside, HAS_SET_ASIDE),
            (self.complete, COMPLETE),
        ] {
            if flag {
                push_field(&mut b, id, &[]);
            }
        }
        b
    }

    /// Decodes a body: critical fields in ascending order and once each, an unknown one a version refusal, an
    /// ignorable one (`≥ 0x8000`) skipped; then [`Description::check`].
    pub(crate) fn decode(body: &[u8]) -> Result<Self, Bad> {
        let fields = parse_fields(body).ok_or(Bad::body("description field list"))?;
        if let Some(&(id, _)) = fields
            .iter()
            .find(|&&(id, _)| id < 0x8000 && !(CLASS..=COMPLETE).contains(&id))
        {
            return Err(Bad::Unsupported(FileElement::DescriptionField(id)));
        }
        let mut last = 0u16;
        let mut got: [Option<&[u8]>; 14] = [None; 14];
        for &(id, v) in &fields {
            if id >= 0x8000 {
                continue;
            }
            if id <= last {
                return Err(Bad::body("description field order"));
            }
            last = id;
            got[usize::from(id)] = Some(v);
        }
        let fixed = |id: u16, n: usize| {
            got[usize::from(id)]
                .filter(|v| v.len() == n)
                .ok_or(Bad::body(
                    "description field missing or of the wrong length",
                ))
        };
        let class = match fixed(CLASS, 1)?[0] {
            1 => Class::Current,
            2 => Class::Revision,
            c => return Err(Bad::Unsupported(FileElement::Class(c))),
        };
        let level = match fixed(LEVEL, 1)?[0] {
            0 => Level::L0,
            1 => Level::Hour,
            2 => Level::Day,
            3 => Level::Week,
            4 => Level::Sparse,
            l => return Err(Bad::Unsupported(FileElement::Level(l))),
        };
        let i64_of = |v: &[u8]| i64::from_le_bytes(v.try_into().unwrap_or_default());
        let raw = got[usize::from(INPUTS)].ok_or(Bad::body("description inputs missing"))?;
        if raw.len() % INPUT_LEN != 0 || raw.len() / INPUT_LEN > MAX_INPUTS {
            return Err(Bad::body("description inputs"));
        }
        let inputs = raw
            .as_chunks::<INPUT_LEN>()
            .0
            .iter()
            .map(|c| Input {
                kind: c[0],
                file_id: u64::from_le_bytes(c[4..12].try_into().unwrap_or_default()),
            })
            .collect();
        let name = |id: u16| {
            got[usize::from(id)]
                .map(|v| std::str::from_utf8(v).map(str::to_owned))
                .transpose()
                .map_err(|_| Bad::body("description name not UTF-8"))
        };
        let name_range = match (name(FIRST_NAME)?, name(LAST_NAME)?) {
            (Some(a), Some(b)) => Some((a, b)),
            (None, None) => None,
            _ => return Err(Bad::body("description name range")),
        };
        let raw = got[usize::from(POSITIONS)].ok_or(Bad::body("description positions missing"))?;
        if raw.len() % GENERATION_LEN != 0 {
            return Err(Bad::body("description positions per generation"));
        }
        let positions = raw
            .as_chunks::<GENERATION_LEN>()
            .0
            .iter()
            .map(|c| {
                (
                    u32::from_le_bytes(c[..4].try_into().unwrap_or_default()),
                    u64::from_le_bytes(c[8..].try_into().unwrap_or_default()),
                )
            })
            .collect();
        let flag = |id: u16| match got[usize::from(id)] {
            None => Ok(false),
            Some([]) => Ok(true),
            Some(_) => Err(Bad::body("description flag field with a value")),
        };
        let d = Self {
            class,
            level,
            period_start: i64_of(fixed(PERIOD_START, 8)?),
            period_len: i64_of(fixed(PERIOD_LEN, 8)?),
            run_id: u64::from_le_bytes(fixed(RUN_ID, 8)?.try_into().unwrap_or_default()),
            run_count: u32::from_le_bytes(fixed(RUN_COUNT, 4)?.try_into().unwrap_or_default()),
            inputs,
            name_range,
            positions,
            has_entries: flag(HAS_ENTRIES)?,
            has_set_aside: flag(HAS_SET_ASIDE)?,
            complete: flag(COMPLETE)?,
        };
        d.check().map_err(Bad::body)?;
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::error::Fault;

    fn sample() -> Description {
        Description {
            class: Class::Revision,
            level: Level::Sparse,
            period_start: -5,
            period_len: 7,
            run_id: 3,
            run_count: 2,
            inputs: vec![
                Input {
                    kind: 1,
                    file_id: 4,
                },
                Input {
                    kind: 2,
                    file_id: 1,
                },
            ],
            name_range: Some(("a".into(), "b/c".into())),
            positions: vec![(1, 5), (3, 2)],
            has_entries: false,
            has_set_aside: true,
            complete: true,
        }
    }

    #[test]
    fn a_description_round_trips_with_its_fields_in_place() {
        let d = sample();
        let b = d.encode();
        assert_eq!(&b[..6], &[1, 0, 1, 0, 2, 0]);
        assert_eq!(Description::decode(&b), Ok(d));
    }

    #[test]
    fn unknown_fields_class_and_level_follow_the_critical_rule() {
        let base = sample().encode();
        let mut ignorable = base.clone();
        push_field(&mut ignorable, 0x8001, &[9, 9]);
        assert_eq!(Description::decode(&ignorable), Ok(sample()));
        let mut critical = base.clone();
        push_field(&mut critical, 14, &[]);
        assert_eq!(
            Description::decode(&critical),
            Err(Bad::Unsupported(FileElement::DescriptionField(14)))
        );
        let mut first = vec![];
        push_field(&mut first, 0, &[]);
        first.extend_from_slice(&base);
        assert_eq!(
            Description::decode(&first),
            Err(Bad::Unsupported(FileElement::DescriptionField(0))),
            "an unknown critical field is refused before the order is judged"
        );
        let mut class = base.clone();
        class[4] = 3;
        assert_eq!(
            Description::decode(&class),
            Err(Bad::Unsupported(FileElement::Class(3)))
        );
        let mut level = base;
        level[12] = 5;
        assert_eq!(
            Description::decode(&level),
            Err(Bad::Unsupported(FileElement::Level(5)))
        );
    }

    #[test]
    fn broken_rules_are_invalid() {
        for d in [
            Description {
                period_len: 0,
                ..sample()
            },
            Description {
                inputs: vec![],
                ..sample()
            },
            Description {
                has_entries: true,
                ..sample()
            },
            Description {
                name_range: Some(("b".into(), "a".into())),
                ..sample()
            },
            Description {
                positions: vec![(3, 2), (1, 5)],
                ..sample()
            },
            Description {
                positions: vec![(1, 0)],
                ..sample()
            },
            Description {
                positions: vec![],
                ..sample()
            },
        ] {
            assert!(matches!(
                Description::decode(&d.encode()),
                Err(Bad::Invalid(Fault::Body(_)))
            ));
        }
    }
}
