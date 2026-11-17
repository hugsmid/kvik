//! The series directory (`format.md` A.18, kind 16): one fixed-size entry per series as of a position at or below
//! the last synced mark. The entries mean what the crate that supplies them says (`spec.md` §2.2 latest recorded `t`
//! and lateness reference, §2.1 orphaned facts, §2.3 live state); this module only lays them out.

use super::{
    DerivedError, Frame, block_type, decode_frame, encode_frame, position_at, put_position,
    put_stamp, records, stamp_at,
};
use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::error::EncodeError;
use crate::facts::Quality;
use crate::header::kind;
use crate::mark::ResetStamp;
use crate::writer::Position;

/// An entry's size in v1.
pub const ENTRY_LEN: u16 = 96;
const PREFIX: usize = 48;

/// Entry flag bits (A.18).
pub mod flag {
    /// `latest_t` holds the latest recorded `t`.
    pub const LATEST: u16 = 1 << 0;
    /// `lateness_reference` holds one.
    pub const REFERENCE: u16 = 1 << 1;
    /// The live candidate is present.
    pub const CANDIDATE: u16 = 1 << 2;
    /// The candidate may not be the live fact (a later fact fell due unseen, or it was corrupted to bad): a reader
    /// finds the fact in force instead.
    pub const CANDIDATE_UNSURE: u16 = 1 << 3;
    /// The next pending fact is present.
    pub const PENDING: u16 = 1 << 4;
    /// Pending facts may exist that the entry does not name: once the named one falls due, a reader finds the next.
    pub const PENDING_MORE: u16 = 1 << 5;
}

/// A candidate's value: inline for fixed-width types; text is read at the candidate's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateValue {
    /// Kind 0: value and quality are read from the fact at the candidate's position (a candidate continued from a
    /// pending fact, whose value the entry did not hold).
    AtPosition,
    /// Kind 1: an `f64`'s bits.
    F64(u64),
    /// Kind 2: a decimal at its stored scale.
    Decimal {
        /// The scaled integer.
        units: i64,
        /// The scale.
        scale: u8,
    },
    /// Kind 3: a state index.
    State(u16),
    /// Kind 4: a boolean.
    Bool(bool),
    /// Kind 5: text, located by the position.
    Text,
}

/// The live candidate: the latest fact that is not bad and not ahead of the file's write time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// Its event time.
    pub t: i64,
    /// Its position.
    pub position: Position,
    /// Its adapter-supplied quality.
    pub quality: Quality,
    /// Its value.
    pub value: CandidateValue,
}

/// A pending fact: not bad and ahead of the write time, applied by a reader once it falls due.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pending {
    /// Its event time.
    pub t: i64,
    /// Its position.
    pub position: Position,
}

/// One series' entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SeriesEntry {
    /// The series.
    pub series_id: u32,
    /// The latest recorded `t`.
    pub latest_t: Option<i64>,
    /// The lateness reference.
    pub lateness_reference: Option<i64>,
    /// Kept facts of the series.
    pub fact_count: u64,
    /// Of those, facts whose series has no definition (orphaned).
    pub orphan_count: u64,
    /// The live candidate.
    pub candidate: Option<Candidate>,
    /// The candidate may be wrong: see [`flag::CANDIDATE_UNSURE`].
    pub candidate_unsure: bool,
    /// The next pending fact.
    pub pending: Option<Pending>,
    /// See [`flag::PENDING_MORE`].
    pub pending_more: bool,
}

fn quality_code(q: Quality) -> u8 {
    match q {
        Quality::Good => 0,
        Quality::Estimated => 1,
        Quality::Suspect => 2,
        Quality::Bad => 3,
    }
}

/// A candidate's quality: never bad.
fn quality_of(c: u8) -> Option<Quality> {
    Some(match c {
        0 => Quality::Good,
        1 => Quality::Estimated,
        2 => Quality::Suspect,
        _ => return None,
    })
}

impl SeriesEntry {
    fn flags(&self) -> u16 {
        let mut f = 0;
        let mut set = |b: bool, bit| {
            if b {
                f |= bit;
            }
        };
        set(self.latest_t.is_some(), flag::LATEST);
        set(self.lateness_reference.is_some(), flag::REFERENCE);
        set(self.candidate.is_some(), flag::CANDIDATE);
        set(self.candidate_unsure, flag::CANDIDATE_UNSURE);
        set(self.pending.is_some(), flag::PENDING);
        set(self.pending_more, flag::PENDING_MORE);
        f
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let c = self.candidate;
        let (kind, scale, value) = match c.map(|c| c.value) {
            None | Some(CandidateValue::AtPosition) => (0u8, 0u8, 0u64),
            Some(CandidateValue::F64(b)) => (1, 0, b),
            Some(CandidateValue::Decimal { units, scale }) => (2, scale, units as u64),
            Some(CandidateValue::State(s)) => (3, 0, u64::from(s)),
            Some(CandidateValue::Bool(b)) => (4, 0, u64::from(b)),
            Some(CandidateValue::Text) => (5, 0, 0),
        };
        out.extend_from_slice(&self.series_id.to_le_bytes());
        out.extend_from_slice(&self.flags().to_le_bytes());
        out.push(kind);
        out.push(c.map_or(0, |c| {
            if c.value == CandidateValue::AtPosition {
                0
            } else {
                quality_code(c.quality)
            }
        }));
        out.extend_from_slice(&self.latest_t.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.lateness_reference.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.fact_count.to_le_bytes());
        out.extend_from_slice(&self.orphan_count.to_le_bytes());
        out.extend_from_slice(&c.map_or(0, |c| c.t).to_le_bytes());
        put_position(out, c.map_or(Position::default(), |c| c.position));
        out.push(scale);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&value.to_le_bytes());
        let p = self.pending;
        out.extend_from_slice(&p.map_or(0, |p| p.t).to_le_bytes());
        put_position(out, p.map_or(Position::default(), |p| p.position));
        out.extend_from_slice(&[0; 4]);
    }

    fn decode(b: &[u8]) -> Result<Self, DerivedError> {
        let f = u16_at(b, 4);
        let has = |bit| f & bit != 0;
        let candidate = if has(flag::CANDIDATE) {
            let value = match b[6] {
                0 if b[7] == 0 => CandidateValue::AtPosition,
                1 => CandidateValue::F64(u64_at(b, 64)),
                2 if b[60] <= 18 => CandidateValue::Decimal {
                    units: i64_at(b, 64),
                    scale: b[60],
                },
                3 => CandidateValue::State(
                    u16::try_from(u64_at(b, 64))
                        .map_err(|_| DerivedError::Invalid("state index"))?,
                ),
                4 if u64_at(b, 64) <= 1 => CandidateValue::Bool(u64_at(b, 64) == 1),
                5 => CandidateValue::Text,
                _ => return Err(DerivedError::Invalid("candidate value")),
            };
            Some(Candidate {
                t: i64_at(b, 40),
                position: position_at(b, 48),
                quality: quality_of(b[7]).ok_or(DerivedError::Invalid("candidate quality"))?,
                value,
            })
        } else {
            None
        };
        Ok(SeriesEntry {
            series_id: u32_at(b, 0),
            latest_t: has(flag::LATEST).then(|| i64_at(b, 8)),
            lateness_reference: has(flag::REFERENCE).then(|| i64_at(b, 16)),
            fact_count: u64_at(b, 24),
            orphan_count: u64_at(b, 32),
            candidate,
            candidate_unsure: has(flag::CANDIDATE_UNSURE),
            pending: has(flag::PENDING).then(|| Pending {
                t: i64_at(b, 72),
                position: position_at(b, 80),
            }),
            pending_more: has(flag::PENDING_MORE),
        })
    }
}

/// The series directory's content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeriesDirectory {
    /// Complete through this position, at or below the last synced mark: the entries are as of it.
    pub through: Position,
    /// The lost ranges at or below `through` that the manifest held when it was written: one recorded since makes
    /// the file no longer a function of the kept log.
    pub lost: u32,
    /// The mark's reset stamp when it was written.
    pub stamp: Option<ResetStamp>,
    /// The time the candidates and pending facts are judged at: the later of the write time and every receive time
    /// the entries cover.
    pub judged_at: i64,
    /// The entries, in strictly ascending series id.
    pub entries: Vec<SeriesEntry>,
}

impl SeriesDirectory {
    /// The bytes of the file; the frame's `written_at` is the write time the candidates are judged against.
    pub fn encode(&self, frame: Frame, store_id: [u8; 16]) -> Result<Vec<u8>, EncodeError> {
        let mut b = Vec::with_capacity(PREFIX + self.entries.len() * usize::from(ENTRY_LEN));
        put_position(&mut b, self.through);
        b.extend_from_slice(&self.lost.to_le_bytes());
        put_stamp(&mut b, self.stamp);
        b.extend_from_slice(&self.judged_at.to_le_bytes());
        b.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        b.extend_from_slice(&ENTRY_LEN.to_le_bytes());
        b.extend_from_slice(&[0; 2]);
        for e in &self.entries {
            e.encode(&mut b);
        }
        encode_frame(
            kind::SERIES_DIRECTORY,
            block_type::SERIES,
            frame,
            store_id,
            &b,
        )
    }

    /// Decodes a series directory file (A.18 checks).
    pub fn decode(bytes: &[u8], store_id: &[u8; 16]) -> Result<(Frame, Self), DerivedError> {
        let (frame, body) =
            decode_frame(bytes, store_id, kind::SERIES_DIRECTORY, block_type::SERIES)?;
        if body.len() < PREFIX {
            return Err(DerivedError::Invalid("series body"));
        }
        let entries = records(
            body,
            PREFIX,
            u32_at(body, 40),
            u16_at(body, 44),
            usize::from(ENTRY_LEN),
        )?
        .map(SeriesEntry::decode)
        .collect::<Result<Vec<_>, _>>()?;
        let len = PREFIX + entries.len() * usize::from(u16_at(body, 44));
        if len != body.len() || !entries.windows(2).all(|w| w[0].series_id < w[1].series_id) {
            return Err(DerivedError::Invalid("series entries"));
        }
        Ok((
            frame,
            SeriesDirectory {
                through: position_at(body, 0),
                lost: u32_at(body, 12),
                stamp: stamp_at(body, 16),
                judged_at: i64_at(body, 32),
                entries,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_out_of_order_or_a_bad_candidate_are_invalid() {
        let e = |id| SeriesEntry {
            series_id: id,
            ..SeriesEntry::default()
        };
        let frame = Frame {
            generation: 0,
            file_id: 1,
            written_at: 0,
        };
        let d = SeriesDirectory {
            entries: vec![e(2), e(1)],
            ..SeriesDirectory::default()
        };
        assert!(SeriesDirectory::decode(&d.encode(frame, [1; 16]).unwrap(), &[1; 16]).is_err());
        let mut bad = e(1);
        bad.candidate = Some(Candidate {
            t: 0,
            position: Position::default(),
            quality: Quality::Bad,
            value: CandidateValue::Bool(true),
        });
        let d = SeriesDirectory {
            entries: vec![bad],
            ..SeriesDirectory::default()
        };
        assert!(
            SeriesDirectory::decode(&d.encode(frame, [1; 16]).unwrap(), &[1; 16]).is_err(),
            "a candidate is never bad"
        );
    }

    #[test]
    fn a_position_only_candidate_with_a_quality_is_invalid() {
        let frame = Frame {
            generation: 0,
            file_id: 1,
            written_at: 0,
        };
        let d = SeriesDirectory {
            entries: vec![SeriesEntry {
                series_id: 1,
                candidate: Some(Candidate {
                    t: 0,
                    position: Position::default(),
                    quality: Quality::Good,
                    value: CandidateValue::AtPosition,
                }),
                ..SeriesEntry::default()
            }],
            ..SeriesDirectory::default()
        };
        let mut file = d.encode(frame, [1; 16]).unwrap();
        assert_eq!(SeriesDirectory::decode(&file, &[1; 16]).unwrap().1, d);
        let at = 80 + 48 + 7;
        file[at] = 2;
        let len = u32::from_le_bytes(file[68..72].try_into().unwrap()) as usize;
        let body = crate::crc::crc32c(&file[80..80 + len]).to_le_bytes();
        file[72..76].copy_from_slice(&body);
        let head = crate::crc::crc32c(&file[64..76]).to_le_bytes();
        file[76..80].copy_from_slice(&head);
        assert!(SeriesDirectory::decode(&file, &[1; 16]).is_err());
    }

    #[test]
    fn every_value_kind_round_trips() {
        let at = |s| Position {
            generation: 0,
            sequence: s,
        };
        let values = [
            CandidateValue::F64(1.5f64.to_bits()),
            CandidateValue::Decimal {
                units: -12,
                scale: 3,
            },
            CandidateValue::State(9),
            CandidateValue::Bool(true),
            CandidateValue::Text,
        ];
        let entries: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(i, v)| SeriesEntry {
                series_id: i as u32 + 1,
                latest_t: Some(100),
                lateness_reference: (i % 2 == 0).then_some(90),
                fact_count: 4,
                orphan_count: 0,
                candidate: Some(Candidate {
                    t: 80,
                    position: at(3),
                    quality: Quality::Suspect,
                    value: *v,
                }),
                candidate_unsure: i == 2,
                pending: (i == 1).then_some(Pending {
                    t: 200,
                    position: at(4),
                }),
                pending_more: i == 1,
            })
            .collect();
        let d = SeriesDirectory {
            through: at(4),
            lost: 1,
            stamp: None,
            judged_at: 160,
            entries,
        };
        let frame = Frame {
            generation: 0,
            file_id: 1,
            written_at: 150,
        };
        let bytes = d.encode(frame, [1; 16]).unwrap();
        assert_eq!(
            SeriesDirectory::decode(&bytes, &[1; 16]).unwrap(),
            (frame, d)
        );
    }
}
