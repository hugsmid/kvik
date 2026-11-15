//! Byte forms the restore driver reads and writes before their owners land them: the restore record's body (Q-300,
//! provisional until roadmap 8.3a's `RestoreRecord::to_body` and `from_body`) and an uploaded manifest record (the
//! A.14 block alone, `design` S3: sealed `record.encode()`).

use kvik_files::catalogue::Tier;
use kvik_files::desc::FileDescription;
use kvik_files::record::{AckedLoss, Body, Record, RecordError, record_type};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
use kvik_log::crc::crc32c;
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::RestoredEnd;

const GENERATION: u16 = 1;
const STAMP: u16 = 2;
const SOURCE_GENERATION: u16 = 3;
const RESTORED_END: u16 = 4;
const CARRIED_END: u16 = 5;
const LOSS: u16 = 6;
const ACKNOWLEDGEMENT: u16 = 7;
const ENTRY_LOSS: u16 = 8;
const FILE: u16 = 9;
const DRILL: u16 = 10;

const LOSS_LEN: usize = 100;
const FILE_PREFIX: usize = 44;

fn invalid(what: &'static str) -> RecordError {
    RecordError::Invalid(what)
}

fn position(p: RecordPosition) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..4].copy_from_slice(&p.0.to_le_bytes());
    b[4..].copy_from_slice(&p.1.to_le_bytes());
    b
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RecordError> {
        if self.0.len() < n {
            return Err(invalid("a restore record field shorter than its layout"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], RecordError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.array::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, RecordError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    fn position(&mut self) -> Result<RecordPosition, RecordError> {
        Ok((self.u32()?, self.u64()?))
    }
    fn end(self) -> Result<(), RecordError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid("a restore record field longer than its layout"))
        }
    }
}

fn loss_bytes(l: &LostRange) -> Vec<u8> {
    let mut v = Vec::with_capacity(LOSS_LEN);
    for p in [l.identity, l.first, l.last] {
        v.extend_from_slice(&position(p));
    }
    for n in [l.file_id, l.offset, l.length] {
        v.extend_from_slice(&n.to_le_bytes());
    }
    let flags = u8::from(l.time_before.is_some())
        | u8::from(l.time_after.is_some()) << 1
        | u8::from(l.extends) << 2;
    v.extend_from_slice(&[l.cause as u8, flags, 0, 0]);
    v.extend_from_slice(&l.boot_id);
    v.extend_from_slice(&l.id_floor.to_le_bytes());
    v.extend_from_slice(&l.time_before.unwrap_or(0).to_le_bytes());
    v.extend_from_slice(&l.time_after.unwrap_or(0).to_le_bytes());
    v
}

fn cause(c: u8) -> Result<LossCause, RecordError> {
    Ok(match c {
        1 => LossCause::Corruption,
        2 => LossCause::TruncatedTail,
        3 => LossCause::LostInBucket,
        4 => LossCause::InvalidContent,
        5 => LossCause::Unknown,
        _ => return Err(invalid("an unknown loss cause in a restore record")),
    })
}

fn loss_of(v: &[u8]) -> Result<LostRange, RecordError> {
    let mut r = Reader(v);
    let (identity, first, last) = (r.position()?, r.position()?, r.position()?);
    let (file_id, offset, length) = (r.u64()?, r.u64()?, r.u64()?);
    let c = cause(r.u8()?)?;
    let flags = r.u8()?;
    r.take(2)?;
    let boot_id = r.array()?;
    let id_floor = r.u32()?;
    let (before, after) = (r.i64()?, r.i64()?);
    r.end()?;
    if flags & !0b111 != 0 {
        return Err(invalid("reserved loss flags set in a restore record"));
    }
    if first > last {
        return Err(invalid(
            "a restore record loss whose first position is above its last",
        ));
    }
    Ok(LostRange {
        identity,
        first,
        last,
        file_id,
        offset,
        length,
        cause: c,
        boot_id,
        id_floor,
        time_before: (flags & 1 != 0).then_some(before),
        time_after: (flags & 2 != 0).then_some(after),
        extends: flags & 4 != 0,
        stamp: None,
    })
}

fn tier_code(t: Tier) -> u8 {
    match t {
        Tier::Hot => 0,
        Tier::Cold => 1,
        Tier::Deep => 2,
    }
}

fn tier_of(c: u8) -> Result<Tier, RecordError> {
    Ok(match c {
        0 => Tier::Hot,
        1 => Tier::Cold,
        2 => Tier::Deep,
        _ => return Err(invalid("an unknown tier in a restore record")),
    })
}

fn file_bytes(f: &RestoredFile) -> Vec<u8> {
    let mut v = Vec::with_capacity(FILE_PREFIX + 80);
    v.extend_from_slice(&[tier_code(f.tier), 0, 0, 0]);
    v.extend_from_slice(&f.copy.size.to_le_bytes());
    v.extend_from_slice(&f.copy.sha256);
    v.extend_from_slice(&f.description.encode());
    v
}

fn file_of(v: &[u8]) -> Result<RestoredFile, RecordError> {
    let mut r = Reader(v);
    let tier = tier_of(r.u8()?)?;
    r.take(3)?;
    let size = r.u64()?;
    let sha256 = r.array()?;
    let description = FileDescription::decode(r.0).map_err(invalid)?;
    Ok(RestoredFile {
        description,
        tier,
        copy: DeepCopy { size, sha256 },
    })
}

/// The `0x25` body of `r` (Q-300).
pub(crate) fn to_body(r: &RestoreRecord) -> Body {
    let mut f: Vec<(u16, Vec<u8>)> = vec![
        (GENERATION, r.generation.to_le_bytes().to_vec()),
        (STAMP, r.stamp.encode().to_vec()),
        (
            SOURCE_GENERATION,
            r.source_generation.to_le_bytes().to_vec(),
        ),
        (RESTORED_END, position(r.restored_end).to_vec()),
    ];
    for e in &r.carried_ends {
        let mut v = e.generation.to_le_bytes().to_vec();
        v.extend_from_slice(&position(e.end));
        f.push((CARRIED_END, v));
    }
    f.extend(r.losses.iter().map(|l| (LOSS, loss_bytes(l))));
    for a in &r.acknowledgements {
        let mut v = Vec::with_capacity(36);
        for p in [a.identity, a.first, a.last] {
            v.extend_from_slice(&position(p));
        }
        f.push((ACKNOWLEDGEMENT, v));
    }
    f.extend(r.entry_losses.iter().map(|l| (ENTRY_LOSS, loss_bytes(l))));
    f.extend(r.files.iter().map(|x| (FILE, file_bytes(x))));
    f.push((DRILL, vec![u8::from(r.drill)]));
    Body::Other {
        record_type: record_type::RESTORE,
        skippable: false,
        fields: f,
    }
}

fn one(fields: &[(u16, Vec<u8>)], id: u16) -> Result<&[u8], RecordError> {
    let mut it = fields.iter().filter(|(i, _)| *i == id);
    match (it.next(), it.next()) {
        (Some((_, v)), None) => Ok(v),
        _ => Err(invalid("a restore record field missing or repeated")),
    }
}

/// The restore record a body holds: `None` for any other record (Q-300).
pub(crate) fn from_body(b: &Body) -> Option<Result<RestoreRecord, RecordError>> {
    match b {
        Body::Other {
            record_type: record_type::RESTORE,
            fields,
            ..
        } => Some(decode(fields)),
        _ => None,
    }
}

fn decode(fields: &[(u16, Vec<u8>)]) -> Result<RestoreRecord, RecordError> {
    if fields
        .iter()
        .any(|(id, _)| !(GENERATION..=DRILL).contains(id))
    {
        return Err(invalid("an unknown field in a restore record"));
    }
    let mut r = Reader(one(fields, GENERATION)?);
    let generation = r.u32()?;
    r.end()?;
    let mut r = Reader(one(fields, STAMP)?);
    let stamp = ResetStamp {
        sequence: r.u64()?,
        nonce: r.u64()?,
    };
    r.end()?;
    let mut r = Reader(one(fields, SOURCE_GENERATION)?);
    let source_generation = r.u32()?;
    r.end()?;
    let mut r = Reader(one(fields, RESTORED_END)?);
    let restored_end = r.position()?;
    r.end()?;
    let drill = match one(fields, DRILL)? {
        [0] => false,
        [1] => true,
        _ => return Err(invalid("a restore record drill flag other than 0 or 1")),
    };
    let all = |id: u16| fields.iter().filter(move |(i, _)| *i == id).map(|(_, v)| v);
    let carried_ends = all(CARRIED_END)
        .map(|v| {
            let mut r = Reader(v);
            let e = RestoredEnd {
                generation: r.u32()?,
                end: r.position()?,
            };
            r.end()?;
            Ok(e)
        })
        .collect::<Result<_, RecordError>>()?;
    let acknowledgements = all(ACKNOWLEDGEMENT)
        .map(|v| {
            let mut r = Reader(v);
            let a = AckedLoss {
                identity: r.position()?,
                first: r.position()?,
                last: r.position()?,
            };
            r.end()?;
            Ok(a)
        })
        .collect::<Result<_, RecordError>>()?;
    Ok(RestoreRecord {
        generation,
        stamp,
        source_generation,
        restored_end,
        carried_ends,
        losses: all(LOSS).map(|v| loss_of(v)).collect::<Result<_, _>>()?,
        acknowledgements,
        entry_losses: all(ENTRY_LOSS)
            .map(|v| loss_of(v))
            .collect::<Result<_, _>>()?,
        files: all(FILE).map(|v| file_of(v)).collect::<Result<_, _>>()?,
        drill,
    })
}

/// The record an uploaded object's plaintext holds: exactly one A.14 block.
pub(crate) fn decode_block(b: &[u8]) -> Result<Record, RecordError> {
    let raw: &[u8; BLOCK_HEADER_LEN] = b
        .get(..BLOCK_HEADER_LEN)
        .and_then(|h| h.try_into().ok())
        .ok_or(invalid("an uploaded record shorter than a block header"))?;
    let h = BlockHeader::decode(raw)
        .filter(|h| h.block_type != 0)
        .ok_or(invalid("an uploaded record whose block header is invalid"))?;
    if u64::try_from(b.len()).ok() != Some(h.block_len()) {
        return Err(invalid("an uploaded record that is not exactly one block"));
    }
    let len = usize::try_from(h.body_len).map_err(|_| invalid("an uploaded record too long"))?;
    let body = b
        .get(BLOCK_HEADER_LEN..BLOCK_HEADER_LEN.saturating_add(len))
        .ok_or(invalid("an uploaded record shorter than its body"))?;
    if crc32c(body) != h.body_crc32c {
        return Err(invalid("an uploaded record whose body checksum fails"));
    }
    Record::decode(h.block_type, h.skippable(), body)
}

#[cfg(test)]
mod tests {
    use kvik_files::desc::{FileClass, kind};

    use super::*;

    fn loss(n: u64, times: bool) -> LostRange {
        LostRange {
            identity: (1, n),
            first: (1, n),
            last: (1, n + 4),
            file_id: 9,
            offset: 3,
            length: 70,
            cause: LossCause::LostInBucket,
            boot_id: [5; 16],
            id_floor: 2,
            time_before: times.then_some(-4),
            time_after: times.then_some(11),
            extends: times,
            stamp: None,
        }
    }

    fn sample() -> RestoreRecord {
        RestoreRecord {
            generation: 3,
            stamp: ResetStamp {
                sequence: 1,
                nonce: 0xfeed,
            },
            source_generation: 2,
            restored_end: (2, 40),
            carried_ends: vec![RestoredEnd {
                generation: 2,
                end: (1, 30),
            }],
            losses: vec![loss(5, true), loss(20, false)],
            acknowledgements: vec![AckedLoss {
                identity: (1, 5),
                first: (1, 5),
                last: (1, 9),
            }],
            entry_losses: vec![loss(7, false)],
            files: vec![RestoredFile {
                description: FileDescription {
                    kind: kind::LOG_SEGMENT,
                    class: FileClass::None,
                    generation: 2,
                    file_id: 4,
                    range: Some(((2, 1), (2, 40))),
                    hash: [8; 32],
                    inputs: Vec::new(),
                },
                tier: Tier::Deep,
                copy: DeepCopy {
                    size: 1000,
                    sha256: [6; 32],
                },
            }],
            drill: true,
        }
    }

    #[test]
    fn a_restore_record_round_trips_through_its_body_and_a_manifest_block() {
        let r = sample();
        assert_eq!(from_body(&to_body(&r)), Some(Ok(r.clone())));
        let block = Record::new(1, 3, 77, to_body(&r)).encode().unwrap();
        let back = decode_block(&block).unwrap();
        assert_eq!(back.generation, 3);
        assert_eq!(from_body(&back.body), Some(Ok(r)));
    }

    #[test]
    fn other_bodies_are_not_restore_records() {
        assert_eq!(from_body(&Body::Empty), None);
    }

    #[test]
    fn damaged_restore_record_fields_are_refused() {
        let Body::Other { fields, .. } = to_body(&sample()) else {
            unreachable!()
        };
        let with = |f: Vec<(u16, Vec<u8>)>| {
            from_body(&Body::Other {
                record_type: record_type::RESTORE,
                skippable: false,
                fields: f,
            })
            .unwrap()
        };
        let mut short = fields.clone();
        short[0].1.pop();
        assert!(with(short).is_err());
        let mut repeated = fields.clone();
        repeated.push((GENERATION, 3u32.to_le_bytes().to_vec()));
        assert!(with(repeated).is_err());
        let mut unknown = fields.clone();
        unknown.push((11, Vec::new()));
        assert!(with(unknown).is_err());
        let mut drill = fields.clone();
        drill.last_mut().unwrap().1 = vec![2];
        assert!(with(drill).is_err());
        let mut tier = fields.clone();
        let f = tier.iter_mut().find(|(id, _)| *id == FILE).unwrap();
        f.1[0] = 3;
        assert!(with(tier).is_err());
        let mut cause = fields;
        let l = cause.iter_mut().find(|(id, _)| *id == LOSS).unwrap();
        l.1[60] = 0;
        assert!(with(cause).is_err());
    }

    #[test]
    fn an_uploaded_record_is_exactly_one_checksummed_block() {
        let block = Record::new(4, 0, 1, Body::Empty).encode().unwrap();
        assert_eq!(decode_block(&block).unwrap().epoch, 4);
        assert!(decode_block(&block[..block.len() - 1]).is_err());
        assert!(decode_block(&[block.as_slice(), &[0; 8]].concat()).is_err());
        let mut bad = block.clone();
        bad[BLOCK_HEADER_LEN] ^= 1;
        assert!(decode_block(&bad).is_err());
        assert!(decode_block(&block[..4]).is_err());
    }
}
