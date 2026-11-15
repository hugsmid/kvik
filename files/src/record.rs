//! Typed manifest records (`format.md` A.14): their canonical encoding and their strict decoding.
//!
//! A record is an A.2 block whose body is `epoch: u64`, `generation: u32`, reserved `u32`, `written_at: i64`, then a
//! field list. Canonical writing puts a record's own fields in ascending id order, then field 100 (the old epoch of a
//! record carried into a rebuilt manifest), then the ignorable fields it was read with, in their order.

use kvik_log::Element;
use kvik_log::block::{FLAG_SKIPPABLE, MAX_BODY_LEN, encode_block};
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;

use crate::desc::{FileDescription, kind};
use crate::fields::{self, position_bytes, position_of, u32_of, u64_of};

/// Record types (`format.md` A.14).
pub mod record_type {
    /// An empty record.
    pub const EMPTY: u8 = 0x20;
    /// Publish and retire.
    pub const PUBLISH: u8 = 0x21;
    /// A sealed log segment.
    pub const SEALED: u8 = 0x22;
    /// A lost range.
    pub const LOST_RANGE: u8 = 0x23;
    /// An acknowledgement.
    pub const ACKNOWLEDGEMENT: u8 = 0x24;
    /// A restore record, carried as a field list and checked by its typed view (`crate::restore`).
    pub const RESTORE: u8 = 0x25;
    /// A quarantine (laid out by roadmap 8.1).
    pub const QUARANTINE: u8 = 0x26;
    /// A copy-verified record (`crate::tier::CopyVerified`; carried as a field list, decoded strictly).
    pub const TIER_MOVE: u8 = 0x27;
    /// A rebuild record.
    pub const REBUILD: u8 = 0x28;

    /// Whether `t` is carried as a field list: a restore record, or a record whose layout a later roadmap item gives.
    pub fn is_reserved(t: u8) -> bool {
        matches!(t, RESTORE | QUARANTINE | TIER_MOVE)
    }
}

/// Field 100: a carried record's old epoch.
pub const FIELD_OLD_EPOCH: u16 = 100;
/// Bytes of a record body before its field list.
pub const RECORD_PREFIX: usize = 24;

/// A publish-and-retire record (`0x21`): one record publishes a set of files and retires another, atomically.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Publish {
    /// Field 1, repeated: the files published, in ascending order of kind and id.
    pub published: Vec<FileDescription>,
    /// Field 2, repeated: the files retired, in ascending order of kind and id.
    pub retired: Vec<FileDescription>,
}

/// One loss an acknowledgement names: its identity and its range as reported then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AckedLoss {
    /// The loss's identity.
    pub identity: RecordPosition,
    /// The first position, as reported.
    pub first: RecordPosition,
    /// The last position, as reported.
    pub last: RecordPosition,
}

/// An acknowledgement record (`0x24`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Acknowledgement {
    /// Field 1, repeated: the losses acknowledged, in ascending order of identity.
    pub losses: Vec<AckedLoss>,
    /// Field 2: the epoch of a rebuild record acknowledged (matched against field 100 where that record has it).
    pub rebuild: Option<u64>,
}

/// A rebuild record (`0x28`), the first record of a rebuilt manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rebuild {
    /// Field 1: the corrupt record's epoch as presumed, the last valid epoch before it + 1 (for the first corrupt
    /// record where there are several; 0 for a rebuild of a manifest with none).
    pub presumed_epoch: u64,
    /// Field 2: the epoch of the last valid record before it (0 for none).
    pub last_valid_before: u64,
    /// Field 3: files found.
    pub files_found: u64,
    /// Field 4: lost ranges recorded again.
    pub losses_restated: u32,
    /// Field 5, repeated: a `file_id` floor per file kind, in ascending order of kind.
    pub floors: Vec<(u8, u64)>,
    /// Field 6: losses listed under their first missing position because no valid record names their identity.
    pub identities_rebuilt: u32,
    /// Field 7, repeated: every corrupt span the rebuild stood in for, as the epochs of the valid records around it in
    /// the manifest it rebuilt (`after`, `before`; 0 for no record before), in ascending order. A start asks the copy
    /// sources about each of them again while the rebuild is unacknowledged.
    pub gaps: Vec<(u64, u64)>,
}

/// What a record says.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Body {
    /// `0x20`.
    Empty,
    /// `0x21`.
    Publish(Publish),
    /// `0x22`, field 1 repeated: the sealed log segments, in ascending order of id.
    Sealed(Vec<FileDescription>),
    /// `0x23`.
    LostRange(LostRange),
    /// `0x24`.
    Acknowledgement(Acknowledgement),
    /// `0x28`.
    Rebuild(Rebuild),
    /// A record type reserved for a later roadmap item (`0x25` to `0x27`), or an unknown type with `skippable` set:
    /// its fields as read, field 100 excepted, so that it can be carried into a rebuilt manifest.
    Other {
        /// The record type.
        record_type: u8,
        /// The block's `skippable` flag.
        skippable: bool,
        /// Its fields, in order.
        fields: Vec<(u16, Vec<u8>)>,
    },
}

impl Body {
    /// The record type.
    pub fn record_type(&self) -> u8 {
        match self {
            Body::Empty => record_type::EMPTY,
            Body::Publish(_) => record_type::PUBLISH,
            Body::Sealed(_) => record_type::SEALED,
            Body::LostRange(_) => record_type::LOST_RANGE,
            Body::Acknowledgement(_) => record_type::ACKNOWLEDGEMENT,
            Body::Rebuild(_) => record_type::REBUILD,
            Body::Other { record_type, .. } => *record_type,
        }
    }
}

/// One manifest record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Record {
    /// Its epoch, strictly above the previous record's.
    pub epoch: u64,
    /// The generation it was written in.
    pub generation: u32,
    /// The wall time it was written.
    pub written_at: i64,
    /// Field 100: its epoch before it was carried into a rebuilt manifest.
    pub old_epoch: Option<u64>,
    /// What it says.
    pub body: Body,
    /// Ignorable fields (ids `0x8000` and above) it was read with, kept so that carrying it loses nothing.
    pub ignorable: Vec<(u16, Vec<u8>)>,
}

/// Why a record cannot be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A field value longer than 65,535 bytes (a publish record naming a file with too many inputs).
    #[error("field {0} is longer than a field can hold")]
    FieldTooLong(u16),
    /// A body longer than a block may be.
    #[error("the record is longer than a block may be")]
    BodyTooLong,
    /// Content a reader would refuse.
    #[error("invalid record: {0}")]
    Invalid(&'static str),
}

/// Why a block is not a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// A checksum-valid block whose content fails the record's checks: the record is invalid.
    #[error("invalid record: {0}")]
    Invalid(&'static str),
    /// An unknown critical record type or field: a version refusal.
    #[error("unsupported: {0}")]
    Unsupported(Element),
}

/// The identity epoch of a record: the epoch other records refer to it by, its field 100 where it has one.
pub fn identity_epoch(r: &Record) -> u64 {
    r.old_epoch.unwrap_or(r.epoch)
}

impl Record {
    /// A record of epoch `epoch` holding `body`, with no old epoch and no ignorable fields.
    pub fn new(epoch: u64, generation: u32, written_at: i64, body: Body) -> Self {
        Self {
            epoch,
            generation,
            written_at,
            old_epoch: None,
            body,
            ignorable: Vec::new(),
        }
    }

    /// The epoch other records refer to it by ([`identity_epoch`]).
    pub fn identity_epoch(&self) -> u64 {
        identity_epoch(self)
    }

    /// Every epoch the record names: its own, its old epoch, and those it refers to (`format.md` A.14: new epochs
    /// continue above every epoch the last valid record names).
    pub fn epochs_named(&self) -> Vec<u64> {
        let mut out = vec![self.epoch];
        out.extend(self.old_epoch);
        match &self.body {
            Body::Rebuild(r) => out.extend([r.presumed_epoch, r.last_valid_before]),
            Body::Acknowledgement(a) => out.extend(a.rebuild),
            _ => {}
        }
        out
    }

    /// The canonical bytes of the record's block.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&self.epoch.to_le_bytes());
        body.extend_from_slice(&self.generation.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&self.written_at.to_le_bytes());
        let mut list: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut late: Vec<(u16, Vec<u8>)> = Vec::new();
        match &self.body {
            Body::Empty => {}
            Body::Publish(p) => {
                check_publish(p).map_err(EncodeError::Invalid)?;
                list.extend(p.published.iter().map(|d| (1, d.encode())));
                list.extend(p.retired.iter().map(|d| (2, d.encode())));
            }
            Body::Sealed(s) => {
                check_sealed(s).map_err(EncodeError::Invalid)?;
                list.extend(s.iter().map(|d| (1, d.encode())));
            }
            Body::LostRange(l) => {
                check_lost(l).map_err(EncodeError::Invalid)?;
                list.extend(lost_fields(l));
            }
            Body::Acknowledgement(a) => {
                check_ack(a).map_err(EncodeError::Invalid)?;
                for l in &a.losses {
                    let mut v = Vec::with_capacity(36);
                    v.extend_from_slice(&position_bytes(l.identity));
                    v.extend_from_slice(&position_bytes(l.first));
                    v.extend_from_slice(&position_bytes(l.last));
                    list.push((1, v));
                }
                if let Some(e) = a.rebuild {
                    list.push((2, e.to_le_bytes().to_vec()));
                }
            }
            Body::Rebuild(r) => {
                check_rebuild(r).map_err(EncodeError::Invalid)?;
                list.push((1, r.presumed_epoch.to_le_bytes().to_vec()));
                list.push((2, r.last_valid_before.to_le_bytes().to_vec()));
                list.push((3, r.files_found.to_le_bytes().to_vec()));
                list.push((4, r.losses_restated.to_le_bytes().to_vec()));
                for (k, id) in &r.floors {
                    let mut v = vec![*k];
                    v.extend_from_slice(&id.to_le_bytes());
                    list.push((5, v));
                }
                list.push((6, r.identities_rebuilt.to_le_bytes().to_vec()));
                for (a, b) in &r.gaps {
                    let mut v = a.to_le_bytes().to_vec();
                    v.extend_from_slice(&b.to_le_bytes());
                    list.push((7, v));
                }
            }
            Body::Other { fields, .. } => {
                crate::tier::CopyVerified::from_body(&self.body)
                    .transpose()
                    .map_err(|_| EncodeError::Invalid("an invalid copy-verified record"))?;
                if let Some(Err(e)) = crate::restore::RestoreRecord::from_body(&self.body) {
                    return Err(EncodeError::Invalid(match e {
                        RecordError::Invalid(why) => why,
                        RecordError::Unsupported(_) => {
                            "a restore record field this version does not define"
                        }
                    }));
                }
                if fields.iter().any(|(id, _)| *id == FIELD_OLD_EPOCH) {
                    return Err(EncodeError::Invalid(
                        "field 100 held among a record's fields",
                    ));
                }
                // Field 100 goes before the first field with a higher id.
                let split = fields
                    .iter()
                    .position(|(id, _)| *id > FIELD_OLD_EPOCH)
                    .unwrap_or(fields.len());
                list.extend(fields[..split].iter().cloned());
                late.extend(fields[split..].iter().cloned());
            }
        }
        if let Some(e) = self.old_epoch {
            list.push((FIELD_OLD_EPOCH, e.to_le_bytes().to_vec()));
        }
        list.extend(late);
        if self.ignorable.iter().any(|(id, _)| *id < 0x8000) {
            return Err(EncodeError::Invalid("an ignorable field below 0x8000"));
        }
        list.extend(self.ignorable.iter().cloned());
        for (id, v) in &list {
            fields::push(&mut body, *id, v).ok_or(EncodeError::FieldTooLong(*id))?;
        }
        if body.len() > MAX_BODY_LEN as usize {
            return Err(EncodeError::BodyTooLong);
        }
        let flags = match &self.body {
            Body::Other {
                skippable: true, ..
            } => FLAG_SKIPPABLE,
            _ => 0,
        };
        let mut out = Vec::with_capacity(body.len() + 24);
        encode_block(self.body.record_type(), flags, &body, &mut out)
            .map_err(|_| EncodeError::BodyTooLong)?;
        Ok(out)
    }

    /// Decodes the body of a checksum-valid block of type `t` (`skippable` as its flag gives) and applies the
    /// record's checks.
    pub fn decode(t: u8, skippable: bool, body: &[u8]) -> Result<Self, RecordError> {
        let known = matches!(
            t,
            record_type::EMPTY
                | record_type::PUBLISH
                | record_type::SEALED
                | record_type::LOST_RANGE
                | record_type::ACKNOWLEDGEMENT
                | record_type::REBUILD
        );
        if !known && !record_type::is_reserved(t) && !skippable {
            return Err(RecordError::Unsupported(Element::BlockType(t)));
        }
        if body.len() < RECORD_PREFIX {
            return Err(RecordError::Invalid(
                "a record body shorter than its prefix",
            ));
        }
        let epoch = fields::u64_at(body, 0);
        let generation = fields::u32_at(body, 8);
        let written_at = fields::u64_at(body, 16) as i64;
        let list = fields::parse(&body[RECORD_PREFIX..])
            .ok_or(RecordError::Invalid("a field that does not fit"))?;
        let mut old_epoch = None;
        let mut own: Vec<(u16, &[u8])> = Vec::new();
        let mut ignorable = Vec::new();
        let mut other = Vec::new();
        for (id, v) in list {
            if id == FIELD_OLD_EPOCH {
                if old_epoch.is_some() {
                    return Err(RecordError::Invalid("field 100 twice"));
                }
                old_epoch = Some(u64_of(v).ok_or(RecordError::Invalid("field 100 not 8 bytes"))?);
            } else if !known {
                other.push((id, v.to_vec()));
            } else if id >= 0x8000 {
                ignorable.push((id, v.to_vec()));
            } else {
                own.push((id, v));
            }
        }
        let unknown = |field: u16| {
            RecordError::Unsupported(Element::Field {
                entry_type: t,
                field,
            })
        };
        let only = |defined: &[u16]| -> Result<(), RecordError> {
            match own.iter().find(|(id, _)| !defined.contains(id)) {
                Some((id, _)) => Err(unknown(*id)),
                None => Ok(()),
            }
        };
        let body = match t {
            record_type::EMPTY => {
                only(&[])?;
                Body::Empty
            }
            record_type::PUBLISH => {
                only(&[1, 2])?;
                let mut p = Publish::default();
                for (id, v) in &own {
                    let d = FileDescription::decode(v).map_err(RecordError::Invalid)?;
                    if *id == 1 {
                        p.published.push(d);
                    } else {
                        p.retired.push(d);
                    }
                }
                check_publish(&p).map_err(RecordError::Invalid)?;
                Body::Publish(p)
            }
            record_type::SEALED => {
                only(&[1])?;
                let s = own
                    .iter()
                    .map(|(_, v)| FileDescription::decode(v))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(RecordError::Invalid)?;
                check_sealed(&s).map_err(RecordError::Invalid)?;
                Body::Sealed(s)
            }
            record_type::LOST_RANGE => {
                only(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13])?;
                let l = decode_lost(&own)?;
                check_lost(&l).map_err(RecordError::Invalid)?;
                Body::LostRange(l)
            }
            record_type::ACKNOWLEDGEMENT => {
                only(&[1, 2])?;
                let mut a = Acknowledgement::default();
                for (id, v) in &own {
                    if *id == 1 {
                        if v.len() != 36 {
                            return Err(RecordError::Invalid("an acknowledged loss not 36 bytes"));
                        }
                        a.losses.push(AckedLoss {
                            identity: position_of(&v[..12]).unwrap_or_default(),
                            first: position_of(&v[12..24]).unwrap_or_default(),
                            last: position_of(&v[24..]).unwrap_or_default(),
                        });
                    } else {
                        if a.rebuild.is_some() {
                            return Err(RecordError::Invalid("field 2 twice"));
                        }
                        a.rebuild =
                            Some(u64_of(v).ok_or(RecordError::Invalid("field 2 not 8 bytes"))?);
                    }
                }
                check_ack(&a).map_err(RecordError::Invalid)?;
                Body::Acknowledgement(a)
            }
            record_type::REBUILD => {
                only(&[1, 2, 3, 4, 5, 6, 7])?;
                let r = decode_rebuild(&own)?;
                check_rebuild(&r).map_err(RecordError::Invalid)?;
                Body::Rebuild(r)
            }
            _ => Body::Other {
                record_type: t,
                skippable,
                fields: other,
            },
        };
        crate::tier::CopyVerified::from_body(&body).transpose()?;
        crate::restore::RestoreRecord::from_body(&body).transpose()?;
        Ok(Self {
            epoch,
            generation,
            written_at,
            old_epoch,
            body,
            ignorable,
        })
    }
}

/// The one value of a field that must appear exactly once.
pub(crate) fn single<'a>(own: &[(u16, &'a [u8])], id: u16) -> Result<&'a [u8], RecordError> {
    let mut it = own.iter().filter(|(i, _)| *i == id);
    match (it.next(), it.next()) {
        (Some((_, v)), None) => Ok(v),
        (None, _) => Err(RecordError::Invalid("a required field is missing")),
        _ => Err(RecordError::Invalid("a field appears twice")),
    }
}

/// The value of a field that may appear at most once.
pub(crate) fn optional<'a>(
    own: &[(u16, &'a [u8])],
    id: u16,
) -> Result<Option<&'a [u8]>, RecordError> {
    let mut it = own.iter().filter(|(i, _)| *i == id);
    match (it.next(), it.next()) {
        (v, None) => Ok(v.map(|(_, v)| *v)),
        _ => Err(RecordError::Invalid("a field appears twice")),
    }
}

fn bad(what: &'static str) -> RecordError {
    RecordError::Invalid(what)
}

pub(crate) fn decode_lost(own: &[(u16, &[u8])]) -> Result<LostRange, RecordError> {
    let pos =
        |id| single(own, id).and_then(|v| position_of(v).ok_or(bad("a position not 12 bytes")));
    let u64f = |id| single(own, id).and_then(|v| u64_of(v).ok_or(bad("a u64 field not 8 bytes")));
    let time = |id| {
        optional(own, id)?
            .map(|v| u64_of(v).map(|t| t as i64).ok_or(bad("a time not 8 bytes")))
            .transpose()
    };
    let cause = match single(own, 7)? {
        [1] => LossCause::Corruption,
        [2] => LossCause::TruncatedTail,
        [3] => LossCause::LostInBucket,
        [4] => LossCause::InvalidContent,
        [5] => LossCause::Unknown,
        _ => return Err(bad("an unknown loss cause")),
    };
    let extends = match optional(own, 12)? {
        None => false,
        Some([]) => true,
        Some(_) => return Err(bad("field 12 holds a value")),
    };
    let stamp = optional(own, 13)?
        .map(|v| ResetStamp::decode(v).ok_or(bad("a reset stamp not 16 bytes")))
        .transpose()?;
    Ok(LostRange {
        identity: pos(1)?,
        first: pos(2)?,
        last: pos(3)?,
        file_id: u64f(4)?,
        offset: u64f(5)?,
        length: u64f(6)?,
        cause,
        boot_id: single(own, 8)?
            .try_into()
            .map_err(|_| bad("a boot id not 16 bytes"))?,
        id_floor: u32_of(single(own, 9)?).ok_or(bad("an id floor not 4 bytes"))?,
        time_before: time(10)?,
        time_after: time(11)?,
        extends,
        stamp,
    })
}

pub(crate) fn lost_fields(l: &LostRange) -> Vec<(u16, Vec<u8>)> {
    let mut v = vec![
        (1, position_bytes(l.identity).to_vec()),
        (2, position_bytes(l.first).to_vec()),
        (3, position_bytes(l.last).to_vec()),
        (4, l.file_id.to_le_bytes().to_vec()),
        (5, l.offset.to_le_bytes().to_vec()),
        (6, l.length.to_le_bytes().to_vec()),
        (7, vec![l.cause as u8]),
        (8, l.boot_id.to_vec()),
        (9, l.id_floor.to_le_bytes().to_vec()),
    ];
    if let Some(t) = l.time_before {
        v.push((10, t.to_le_bytes().to_vec()));
    }
    if let Some(t) = l.time_after {
        v.push((11, t.to_le_bytes().to_vec()));
    }
    if l.extends {
        v.push((12, Vec::new()));
    }
    if let Some(s) = l.stamp {
        v.push((13, s.encode().to_vec()));
    }
    v
}

fn decode_rebuild(own: &[(u16, &[u8])]) -> Result<Rebuild, RecordError> {
    let u64f = |id| single(own, id).and_then(|v| u64_of(v).ok_or(bad("a u64 field not 8 bytes")));
    let u32f = |id| single(own, id).and_then(|v| u32_of(v).ok_or(bad("a u32 field not 4 bytes")));
    let floors = own
        .iter()
        .filter(|(id, _)| *id == 5)
        .map(|(_, v)| {
            if v.len() != 9 {
                return Err(bad("a file id floor not 9 bytes"));
            }
            Ok((v[0], fields::u64_at(v, 1)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rebuild {
        presumed_epoch: u64f(1)?,
        last_valid_before: u64f(2)?,
        files_found: u64f(3)?,
        losses_restated: u32f(4)?,
        floors,
        identities_rebuilt: u32f(6)?,
        gaps: own
            .iter()
            .filter(|(id, _)| *id == 7)
            .map(|(_, v)| {
                if v.len() != 16 {
                    return Err(bad("a corrupt span not 16 bytes"));
                }
                Ok((fields::u64_at(v, 0), fields::u64_at(v, 8)))
            })
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn ascending(v: &[FileDescription]) -> bool {
    v.windows(2).all(|w| w[0].file_ref() < w[1].file_ref())
}

/// A publish record names at least one file, each a log segment or compacted file, each once, in ascending order.
fn check_publish(p: &Publish) -> Result<(), &'static str> {
    if p.published.is_empty() && p.retired.is_empty() {
        return Err("a publish record that names no file");
    }
    for d in p.published.iter().chain(&p.retired) {
        d.check()?;
    }
    if !ascending(&p.published) || !ascending(&p.retired) {
        return Err("files not in strictly ascending order");
    }
    if p.published
        .iter()
        .any(|d| p.retired.iter().any(|r| r.file_ref() == d.file_ref()))
    {
        return Err("a file both published and retired");
    }
    Ok(())
}

/// A sealed-segment record names at least one log segment, each once, in ascending order.
fn check_sealed(s: &[FileDescription]) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("a sealed-segment record that names no segment");
    }
    for d in s {
        d.check()?;
        if d.kind != kind::LOG_SEGMENT {
            return Err("a sealed-segment record naming a file that is not a log segment");
        }
    }
    if !ascending(s) {
        return Err("segments not in strictly ascending order");
    }
    Ok(())
}

fn check_lost(l: &LostRange) -> Result<(), &'static str> {
    if l.first > l.last {
        return Err("a lost range whose first position is above its last");
    }
    Ok(())
}

/// An acknowledgement names at least one loss or a rebuild; losses by ascending identity, each once, each range in
/// order.
fn check_ack(a: &Acknowledgement) -> Result<(), &'static str> {
    if a.losses.is_empty() && a.rebuild.is_none() {
        return Err("an acknowledgement that names nothing");
    }
    if !a.losses.windows(2).all(|w| w[0].identity < w[1].identity) {
        return Err("acknowledged losses not in strictly ascending order of identity");
    }
    if a.losses.iter().any(|l| l.first > l.last) {
        return Err("an acknowledged range whose first position is above its last");
    }
    Ok(())
}

fn check_rebuild(r: &Rebuild) -> Result<(), &'static str> {
    if !r.floors.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err("file id floors not in strictly ascending order of kind");
    }
    if r.gaps.iter().any(|(a, b)| a >= b) || !r.gaps.windows(2).all(|w| w[0].1 <= w[1].0) {
        return Err("corrupt spans out of order");
    }
    Ok(())
}

/// Whether `block` holds exactly one record in its canonical bytes (`format.md` A, canonical writing).
pub fn is_canonical(block: &[u8]) -> bool {
    let Some(r) = crate::image::record_at(block, 0, 0).ok().flatten() else {
        return false;
    };
    r.end as usize == block.len() && r.record.encode().is_ok_and(|b| b == block)
}
