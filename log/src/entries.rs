//! Entries block bodies (`format.md` A.6): typed entries, the canonical encoder and the decoder with every check.

use crate::block::block_type;
use crate::body::{DATA_PREFIX_LEN, DataPrefix};
use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::content_error::{BodyError, ContentError};
use crate::error::{Element, EncodeError};
use crate::facts::{MAX_SCALE, ValueType};

/// The largest entry, header excluded: 8 MiB.
pub const MAX_ENTRY_LEN: usize = 8 << 20;
/// The most state names of one series.
pub const MAX_STATE_NAMES: usize = 65_536;
/// One day in nanoseconds (the bound of a day start).
pub const DAY: i64 = 86_400_000_000_000;

/// Entry types (A.6).
pub mod entry_type {
    /// Series definition.
    pub const DEFINITION: u8 = 1;
    /// Parameter change.
    pub const PARAMETER_CHANGE: u8 = 2;
    /// Alias.
    pub const ALIAS: u8 = 3;
    /// Retirement.
    pub const RETIREMENT: u8 = 4;
    /// Routing change: reserved, laid out by roadmap 5.2; refused until then (Q-106 (f)).
    pub const ROUTING: u8 = 5;
    /// Site zone.
    pub const SITE_ZONE: u8 = 6;
    /// Tzdata version.
    pub const TZDATA: u8 = 7;
    /// Recovery entry.
    pub const RECOVERY: u8 = 8;
    /// Scale raise.
    pub const SCALE_RAISE: u8 = 9;
}

/// A series kind (A.6 field 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// 1.
    Gauge = 1,
    /// 2.
    Counter = 2,
    /// 3.
    State = 3,
    /// 4.
    Text = 4,
    /// 5.
    Interval = 5,
}

/// What `changed_at` holds (field 14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Versioning {
    /// 0: not versioned.
    #[default]
    None = 0,
    /// 1: `changed_at` is a time.
    Time = 1,
    /// 2: `changed_at` is a version.
    Version = 2,
}

/// When values apply (field 15).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Timing {
    /// 0.
    #[default]
    Observed = 0,
    /// 1.
    Scheduled = 1,
    /// 2.
    Forecast = 2,
}

/// The length of an interval series (fields 16 to 19 and 22).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IntervalLength {
    /// Tag 0: a fixed length `d > 0` with `0 ≤ phase < d`.
    Fixed {
        /// d, nanoseconds.
        length: i64,
        /// Phase, nanoseconds.
        phase: i64,
    },
    /// Tag 1: a day in `zone`, starting at `day_start` (0 ≤ x < 24 h).
    Day {
        /// IANA zone name as given.
        zone: String,
        /// Day start, nanoseconds after local midnight.
        day_start: i64,
    },
    /// Tag 2: a month in `zone`.
    Month {
        /// IANA zone name as given.
        zone: String,
    },
}

/// Retention class (fields 23 and 24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Retention {
    /// Class 0.
    #[default]
    LongTerm,
    /// Class 1, with its period.
    Diagnostic {
        /// Retention period, nanoseconds.
        period: i64,
    },
}

/// Excess precision policy of a decimal series (fields 25 and 26).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExcessPrecision {
    /// 0.
    Round,
    /// 1.
    Refuse,
    /// 2, up to a maximum scale.
    AutoRaise {
        /// Field 26: 0 to 18, at least the series' scale.
        max_scale: u8,
    },
}

/// Counter fields (10 and 11).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CounterRules {
    /// `wrap_at`: 8 bytes in the series' value type, as a little-endian `u64`; `None` for none.
    pub wrap_at: Option<u64>,
    /// `reset_ceiling` in parts per million.
    pub reset_ceiling_ppm: u32,
}

/// A series definition (entry type 1). Every applicable field is carried; `None` where a field does not apply.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Definition {
    /// 1: the series id.
    pub id: u32,
    /// 2: the name, 1 to 255 bytes.
    pub name: String,
    /// 3.
    pub kind: Kind,
    /// 4.
    pub value_type: ValueType,
    /// 5: decimal scale, for value type 2.
    pub scale: Option<u8>,
    /// 6: unit, 0 to 63 bytes.
    pub unit: String,
    /// 7: expected interval, > 0, or none.
    pub expected_interval: Option<i64>,
    /// 8: staleness limit S, > 0, or none.
    pub staleness: Option<i64>,
    /// 9.
    pub record_on_change: bool,
    /// 10 and 11, for counters.
    pub counter: Option<CounterRules>,
    /// 12.
    pub lateness_bound: i64,
    /// 13: state names in index order, for value type 3 (1 to 65,536 names of 1 to 63 bytes).
    pub state_names: Vec<String>,
    /// 14.
    pub versioning: Versioning,
    /// 15.
    pub timing: Timing,
    /// 16 to 19 and 22, for kind 5.
    pub interval: Option<IntervalLength>,
    /// 23 and 24.
    pub retention: Retention,
    /// 25 and 26, for value type 2.
    pub excess_precision: Option<ExcessPrecision>,
    /// 27: S was set by the caller.
    pub staleness_set: bool,
}

/// A parameter change (entry type 2). `None` means the field is not carried.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParameterChange {
    /// 1.
    pub id: u32,
    /// 20: effective time; carried exactly when one of fields 7 to 12 is.
    pub effective: Option<i64>,
    /// 7: the new expected interval (`Some(None)`: none). Carried only with field 8.
    pub expected_interval: Option<Option<i64>>,
    /// 8 and 27: the staleness limit in force after the change and whether the caller set it.
    pub staleness: Option<(Option<i64>, bool)>,
    /// 9.
    pub record_on_change: Option<bool>,
    /// 10 (`Some(None)`: none).
    pub wrap_at: Option<Option<u64>>,
    /// 11.
    pub reset_ceiling_ppm: Option<u32>,
    /// 12.
    pub lateness_bound: Option<i64>,
    /// 23: 0 long-term, 1 diagnostic; takes effect at the entry's position.
    pub retention_class: Option<u8>,
    /// 24: retention period; takes effect at the entry's position.
    pub retention_period: Option<i64>,
    /// 25 and 26: the excess-precision policy of a decimal series; takes effect at the entry's position
    /// (`spec.md` §2.1). Field 26 is checked against 0 to 18 here; that it is at least the scale in force needs the
    /// registry.
    pub excess_precision: Option<ExcessPrecision>,
}

/// Mark state in a recovery entry (field 45).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkState {
    /// 0.
    Readable = 0,
    /// 1.
    Missing = 1,
    /// 2: no valid slot.
    Unreadable = 2,
    /// 3.
    Stale = 3,
}

/// A recovery entry (entry type 8), the only entry of its batch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Recovery {
    /// 40: K, the last kept position's sequence.
    pub after: u64,
    /// 42: boot id; zero where the platform has none.
    pub boot_id: [u8; 16],
    /// 43: the tail's length in bytes.
    pub tail_bytes: u64,
    /// 44: readable commit blocks in the tail.
    pub readable_commits: u32,
    /// 45.
    pub mark_state: MarkState,
    /// 46: id floor.
    pub id_floor: u32,
    /// 47: consumers whose cursor file was missing or unreadable.
    pub consumers: Vec<String>,
}

/// One entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    /// 1.
    Definition(Box<Definition>),
    /// 2.
    ParameterChange(ParameterChange),
    /// 3.
    Alias {
        /// 1.
        id: u32,
        /// 2: 1 to 255 bytes.
        name: String,
    },
    /// 4.
    Retirement {
        /// 1.
        id: u32,
        /// 21: `t_r`.
        t_r: i64,
    },
    /// 6.
    SiteZone {
        /// 30: IANA name, 1 to 63 bytes.
        zone: String,
        /// 20.
        effective: i64,
    },
    /// 7.
    Tzdata {
        /// 31: ASCII release name, 1 to 15 bytes.
        version: String,
    },
    /// 8.
    Recovery(Recovery),
    /// 9.
    ScaleRaise {
        /// 1.
        id: u32,
        /// 5: the new scale.
        scale: u8,
    },
    /// Consecutive entries of unknown, ignorable types (`≥ 0x80`): skipped, but each takes its sequence. Never
    /// written.
    Ignored {
        /// How many.
        count: u32,
    },
}

/// An entries block body.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntriesBlock {
    /// Sequence of the first entry.
    pub first_sequence: u64,
    /// The batch's receive time.
    pub received_at: i64,
    /// The entries.
    pub entries: Vec<Entry>,
}

/// Writes a field list (A.6 framing).
struct Fields(Vec<u8>);

impl Fields {
    fn raw(&mut self, id: u16, v: &[u8]) {
        self.0.extend_from_slice(&id.to_le_bytes());
        self.0
            .extend_from_slice(&(v.len().min(u16::MAX as usize) as u16).to_le_bytes());
        self.0.extend_from_slice(v);
        self.0.resize(self.0.len().div_ceil(4) * 4, 0);
    }
    fn u8(&mut self, id: u16, v: u8) {
        self.raw(id, &[v]);
    }
    fn u32(&mut self, id: u16, v: u32) {
        self.raw(id, &v.to_le_bytes());
    }
    fn u64(&mut self, id: u16, v: u64) {
        self.raw(id, &v.to_le_bytes());
    }
    fn i64(&mut self, id: u16, v: i64) {
        self.raw(id, &v.to_le_bytes());
    }
    fn opt_i64(&mut self, id: u16, v: Option<i64>) {
        self.raw(
            id,
            &v.map(i64::to_le_bytes).unwrap_or_default()[..v.map_or(0, |_| 8)],
        );
    }
    fn opt_u64(&mut self, id: u16, v: Option<u64>) {
        self.raw(
            id,
            &v.map(u64::to_le_bytes).unwrap_or_default()[..v.map_or(0, |_| 8)],
        );
    }
    fn str(&mut self, id: u16, v: &str) {
        self.raw(id, v.as_bytes());
    }
}

fn encode_entry(e: &Entry) -> (u8, Vec<u8>) {
    let mut f = Fields(Vec::new());
    let t = match e {
        Entry::Definition(d) => {
            f.u32(1, d.id);
            f.str(2, &d.name);
            f.u8(3, d.kind as u8);
            f.u8(4, d.value_type.code());
            if let Some(s) = d.scale {
                f.u8(5, s);
            }
            f.str(6, &d.unit);
            f.opt_i64(7, d.expected_interval);
            f.opt_i64(8, d.staleness);
            f.u8(9, u8::from(d.record_on_change));
            if let Some(c) = d.counter {
                f.opt_u64(10, c.wrap_at);
                f.u32(11, c.reset_ceiling_ppm);
            }
            f.i64(12, d.lateness_bound);
            for n in &d.state_names {
                f.str(13, n);
            }
            f.u8(14, d.versioning as u8);
            f.u8(15, d.timing as u8);
            match &d.interval {
                None => {}
                Some(IntervalLength::Fixed { length, phase }) => {
                    f.u8(16, 0);
                    f.i64(17, *length);
                    f.i64(18, *phase);
                }
                Some(IntervalLength::Day { zone, day_start }) => {
                    f.u8(16, 1);
                    f.str(19, zone);
                    f.i64(22, *day_start);
                }
                Some(IntervalLength::Month { zone }) => {
                    f.u8(16, 2);
                    f.str(19, zone);
                }
            }
            match d.retention {
                Retention::LongTerm => f.u8(23, 0),
                Retention::Diagnostic { period } => {
                    f.u8(23, 1);
                    f.i64(24, period);
                }
            }
            match d.excess_precision {
                None => {}
                Some(ExcessPrecision::Round) => f.u8(25, 0),
                Some(ExcessPrecision::Refuse) => f.u8(25, 1),
                Some(ExcessPrecision::AutoRaise { max_scale }) => {
                    f.u8(25, 2);
                    f.u8(26, max_scale);
                }
            }
            f.u8(27, u8::from(d.staleness_set));
            entry_type::DEFINITION
        }
        Entry::ParameterChange(p) => {
            f.u32(1, p.id);
            if let Some(v) = p.expected_interval {
                f.opt_i64(7, v);
            }
            if let Some((s, _)) = p.staleness {
                f.opt_i64(8, s);
            }
            if let Some(v) = p.record_on_change {
                f.u8(9, u8::from(v));
            }
            if let Some(v) = p.wrap_at {
                f.opt_u64(10, v);
            }
            if let Some(v) = p.reset_ceiling_ppm {
                f.u32(11, v);
            }
            if let Some(v) = p.lateness_bound {
                f.i64(12, v);
            }
            if let Some(v) = p.effective {
                f.i64(20, v);
            }
            if let Some(v) = p.retention_class {
                f.u8(23, v);
            }
            if let Some(v) = p.retention_period {
                f.i64(24, v);
            }
            match p.excess_precision {
                None => {}
                Some(ExcessPrecision::Round) => f.u8(25, 0),
                Some(ExcessPrecision::Refuse) => f.u8(25, 1),
                Some(ExcessPrecision::AutoRaise { max_scale }) => {
                    f.u8(25, 2);
                    f.u8(26, max_scale);
                }
            }
            if let Some((_, set)) = p.staleness {
                f.u8(27, u8::from(set));
            }
            entry_type::PARAMETER_CHANGE
        }
        Entry::Alias { id, name } => {
            f.u32(1, *id);
            f.str(2, name);
            entry_type::ALIAS
        }
        Entry::Retirement { id, t_r } => {
            f.u32(1, *id);
            f.i64(21, *t_r);
            entry_type::RETIREMENT
        }
        Entry::SiteZone { zone, effective } => {
            f.i64(20, *effective);
            f.str(30, zone);
            entry_type::SITE_ZONE
        }
        Entry::Tzdata { version } => {
            f.str(31, version);
            entry_type::TZDATA
        }
        Entry::Recovery(r) => {
            f.u64(40, r.after);
            f.raw(42, &r.boot_id);
            f.u64(43, r.tail_bytes);
            f.u32(44, r.readable_commits);
            f.u8(45, r.mark_state as u8);
            f.u32(46, r.id_floor);
            for c in &r.consumers {
                f.str(47, c);
            }
            entry_type::RECOVERY
        }
        Entry::ScaleRaise { id, scale } => {
            f.u32(1, *id);
            f.u8(5, *scale);
            entry_type::SCALE_RAISE
        }
        Entry::Ignored { .. } => unreachable!("ignored entries are never encoded"),
    };
    (t, f.0)
}

impl EntriesBlock {
    /// The canonical body. Every entry is checked by decoding it again, so only valid bodies are produced.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let count = u32::try_from(self.entries.len()).map_err(|_| EncodeError::TooLarge)?;
        let mut body = vec![0u8; DATA_PREFIX_LEN];
        DataPrefix {
            first_sequence: self.first_sequence,
            received_at: self.received_at,
            count,
        }
        .write(&mut body);
        for (i, e) in self.entries.iter().enumerate() {
            if matches!(e, Entry::Ignored { .. }) {
                return Err(EncodeError::Content(ContentError::FieldPresence {
                    entry: i as u32,
                    field: 0,
                }));
            }
            let (t, fields) = encode_entry(e);
            if fields.len() > MAX_ENTRY_LEN {
                return Err(EncodeError::TooLarge);
            }
            body.extend_from_slice(&[t, 0, 0, 0]);
            body.extend_from_slice(&(fields.len() as u32).to_le_bytes());
            body.extend_from_slice(&fields);
            body.resize(body.len().div_ceil(8) * 8, 0);
        }
        match decode(&body) {
            Ok(back) if back == *self => Ok(body),
            Ok(_) => Err(EncodeError::Content(ContentError::Field {
                entry: 0,
                field: 0,
            })),
            Err(BodyError::Invalid(e)) => Err(EncodeError::Content(e)),
            Err(_) => Err(EncodeError::BlockType(block_type::ENTRIES)),
        }
    }

    /// Decodes a body, applying every check of A.6. Unknown ignorable entries and fields are skipped.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        decode(body)
    }

    /// Decodes a body and also requires it to be exactly what [`EntriesBlock::encode`] writes for its content.
    pub fn decode_canonical(body: &[u8]) -> Result<Self, BodyError> {
        let b = decode(body)?;
        if b.entries.iter().any(|e| matches!(e, Entry::Ignored { .. })) {
            return Err(BodyError::NonCanonical);
        }
        match b.encode() {
            Ok(again) if again == body => Ok(b),
            _ => Err(BodyError::NonCanonical),
        }
    }
}

/// The fields an entry type allows, the ones a parameter change forbids, and the repeated ones.
fn allowed(t: u8, id: u16) -> Allowed {
    use Allowed::*;
    let ok = match t {
        entry_type::DEFINITION => matches!(id, 1..=19 | 22..=27),
        entry_type::PARAMETER_CHANGE => {
            if matches!(id, 2..=6 | 13..=19 | 22) {
                return Forbidden;
            }
            matches!(id, 1 | 7..=12 | 20 | 23..=27)
        }
        entry_type::ALIAS => matches!(id, 1 | 2),
        entry_type::RETIREMENT => matches!(id, 1 | 21),
        entry_type::SITE_ZONE => matches!(id, 20 | 30),
        entry_type::TZDATA => id == 31,
        entry_type::RECOVERY => matches!(id, 40..=47),
        entry_type::SCALE_RAISE => matches!(id, 1 | 5),
        _ => false,
    };
    match (ok, id) {
        (true, _) => Yes,
        (false, 0x8000..) => Ignorable,
        (false, _) => Unknown,
    }
}

enum Allowed {
    Yes,
    Forbidden,
    Ignorable,
    Unknown,
}

fn repeated(t: u8, id: u16) -> bool {
    matches!(
        (t, id),
        (entry_type::DEFINITION, 13) | (entry_type::RECOVERY, 47)
    )
}

/// The fields of one entry, framing and order checked.
struct Parsed<'a> {
    entry: u32,
    fields: Vec<(u16, &'a [u8])>,
}

impl<'a> Parsed<'a> {
    fn get(&self, id: u16) -> Option<&'a [u8]> {
        self.fields.iter().find(|f| f.0 == id).map(|f| f.1)
    }
    fn all(&self, id: u16) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.fields.iter().filter(move |f| f.0 == id).map(|f| f.1)
    }
    fn bad(&self, field: u16) -> BodyError {
        ContentError::Field {
            entry: self.entry,
            field,
        }
        .into()
    }
    fn presence(&self, field: u16) -> BodyError {
        ContentError::FieldPresence {
            entry: self.entry,
            field,
        }
        .into()
    }
    /// A field that must be carried.
    fn need(&self, id: u16) -> Result<&'a [u8], BodyError> {
        self.get(id).ok_or(self.presence(id))
    }
    /// A field that must not be carried.
    fn none(&self, id: u16) -> Result<(), BodyError> {
        if self.get(id).is_some() {
            Err(self.presence(id))
        } else {
            Ok(())
        }
    }
    fn u8_in(&self, id: u16, max: u8) -> Result<u8, BodyError> {
        let v = self.need(id)?;
        if v.len() != 1 || v[0] > max {
            return Err(self.bad(id));
        }
        Ok(v[0])
    }
    fn u32(&self, id: u16) -> Result<u32, BodyError> {
        let v = self.need(id)?;
        if v.len() != 4 {
            return Err(self.bad(id));
        }
        Ok(u32_at(v, 0))
    }
    fn u64(&self, id: u16) -> Result<u64, BodyError> {
        let v = self.need(id)?;
        if v.len() != 8 {
            return Err(self.bad(id));
        }
        Ok(u64_at(v, 0))
    }
    fn i64(&self, id: u16) -> Result<i64, BodyError> {
        self.u64(id).map(|v| v as i64)
    }
    /// An `i64` or none (zero length); a present value must be positive when `positive`.
    fn opt_i64_of(&self, id: u16, v: &[u8], positive: bool) -> Result<Option<i64>, BodyError> {
        match v.len() {
            0 => Ok(None),
            8 if !positive || i64_at(v, 0) > 0 => Ok(Some(i64_at(v, 0))),
            _ => Err(self.bad(id)),
        }
    }
    fn opt_u64_of(&self, id: u16, v: &[u8]) -> Result<Option<u64>, BodyError> {
        match v.len() {
            0 => Ok(None),
            8 => Ok(Some(u64_at(v, 0))),
            _ => Err(self.bad(id)),
        }
    }
    fn text(&self, id: u16, v: &[u8], min: usize, max: usize) -> Result<String, BodyError> {
        match std::str::from_utf8(v) {
            Ok(s) if (min..=max).contains(&s.len()) => Ok(s.to_owned()),
            _ => Err(self.bad(id)),
        }
    }
    fn need_text(&self, id: u16, min: usize, max: usize) -> Result<String, BodyError> {
        self.text(id, self.need(id)?, min, max)
    }
}

fn decode(body: &[u8]) -> Result<EntriesBlock, BodyError> {
    let mut entries: Vec<Entry> = Vec::new();
    let p = walk(body, |e| match (entries.last_mut(), e) {
        // Consecutive ignored entries are counted, not stored one by one.
        (Some(Entry::Ignored { count }), Entry::Ignored { count: n }) => *count += n,
        (_, e) => entries.push(e),
    })?;
    Ok(EntriesBlock {
        first_sequence: p.first_sequence,
        received_at: p.received_at,
        entries,
    })
}

/// Applies every check of A.6 to a body without keeping its entries: its prefix, and whether it holds a recovery
/// entry. This is what a reader runs on every entries block it accepts.
pub(crate) fn check(body: &[u8]) -> Result<(DataPrefix, bool), BodyError> {
    let mut recovery = false;
    let p = walk(body, |e| recovery |= e.is_recovery())?;
    Ok((p, recovery))
}

/// Decodes the entries of a body one at a time, handing each to `sink`.
fn walk(body: &[u8], mut sink: impl FnMut(Entry)) -> Result<DataPrefix, BodyError> {
    let p = DataPrefix::decode(block_type::ENTRIES, body).map_err(|_| ContentError::Prefix)?;
    let mut at = DATA_PREFIX_LEN;
    for i in 0..p.count {
        let overrun = BodyError::from(ContentError::EntryOverrun { entry: i });
        let head = body.get(at..at + 8).ok_or(overrun)?;
        let (t, len) = (head[0], u32_at(head, 4) as usize);
        let fields_at = at + 8;
        let raw = fields_at
            .checked_add(len)
            .and_then(|end| body.get(fields_at..end))
            .ok_or(overrun)?;
        at = (fields_at + len).div_ceil(8) * 8;
        let known = matches!(t, 1..=4 | 6..=9);
        if !known {
            if t >= 0x80 {
                fields(raw, i, t, false)?;
                sink(Entry::Ignored { count: 1 });
                continue;
            }
            return Err(BodyError::Unsupported(Element::EntryType(t)));
        }
        let parsed = Parsed {
            entry: i,
            fields: fields(raw, i, t, true)?,
        };
        sink(entry(t, &parsed)?);
    }
    // Bytes after the last entry are not checked here (Q-106 (a)); the canonical decoder rejects them.
    Ok(p)
}

/// Splits a field list, checking framing and, for a known type, order and allowed ids.
fn fields(raw: &[u8], entry: u32, t: u8, known: bool) -> Result<Vec<(u16, &[u8])>, BodyError> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut prev: Option<u16> = None;
    while at < raw.len() {
        let overrun = BodyError::from(ContentError::EntryOverrun { entry });
        let h = raw.get(at..at + 4).ok_or(overrun)?;
        let (id, len) = (u16_at(h, 0), usize::from(u16_at(h, 2)));
        let v = raw.get(at + 4..at + 4 + len).ok_or(overrun)?;
        // The field's padding to 4 is part of the entry's `length`.
        at = (at + 4 + len).div_ceil(4) * 4;
        if at > raw.len() {
            return Err(overrun);
        }
        if !known {
            continue;
        }
        // A version refusal wins over invalid content: unknown ids are classified before order is tested.
        let kind = allowed(t, id);
        match kind {
            Allowed::Unknown => {
                return Err(BodyError::Unsupported(Element::Field {
                    entry_type: t,
                    field: id,
                }));
            }
            Allowed::Forbidden => {
                return Err(ContentError::FieldNotAllowed { entry, field: id }.into());
            }
            Allowed::Yes | Allowed::Ignorable => {}
        }
        if id < 0x8000 && prev.is_some_and(|p| id < p || (id == p && !repeated(t, id))) {
            return Err(ContentError::FieldOrder { entry, field: id }.into());
        }
        prev = Some(prev.map_or(id, |p| p.max(id)));
        if let Allowed::Yes = kind {
            out.push((id, v));
        }
    }
    Ok(out)
}

fn entry(t: u8, p: &Parsed<'_>) -> Result<Entry, BodyError> {
    Ok(match t {
        entry_type::DEFINITION => Entry::Definition(Box::new(definition(p)?)),
        entry_type::PARAMETER_CHANGE => Entry::ParameterChange(parameter_change(p)?),
        entry_type::ALIAS => Entry::Alias {
            id: p.u32(1)?,
            name: p.need_text(2, 1, 255)?,
        },
        entry_type::RETIREMENT => Entry::Retirement {
            id: p.u32(1)?,
            t_r: p.i64(21)?,
        },
        entry_type::SITE_ZONE => Entry::SiteZone {
            zone: p.need_text(30, 1, 63)?,
            effective: p.i64(20)?,
        },
        entry_type::TZDATA => {
            let v = p.need_text(31, 1, 15)?;
            if !v.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(p.bad(31));
            }
            Entry::Tzdata { version: v }
        }
        entry_type::RECOVERY => {
            let boot = p.need(42)?;
            let boot_id: [u8; 16] = boot.try_into().map_err(|_| p.bad(42))?;
            let mark_state = match p.u8_in(45, 3)? {
                0 => MarkState::Readable,
                1 => MarkState::Missing,
                2 => MarkState::Unreadable,
                _ => MarkState::Stale,
            };
            let consumers = p
                .all(47)
                .map(|v| match std::str::from_utf8(v) {
                    Ok(s) if consumer_name_ok(s) => Ok(s.to_owned()),
                    _ => Err(p.bad(47)),
                })
                .collect::<Result<_, _>>()?;
            Entry::Recovery(Recovery {
                after: p.u64(40)?,
                boot_id,
                tail_bytes: p.u64(43)?,
                readable_commits: p.u32(44)?,
                mark_state,
                id_floor: p.u32(46)?,
                consumers,
            })
        }
        _ => Entry::ScaleRaise {
            id: p.u32(1)?,
            scale: p.u8_in(5, MAX_SCALE)?,
        },
    })
}

/// A consumer name: `[a-z0-9][a-z0-9._-]{0,63}` (A.12).
pub fn consumer_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let first = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 64
        && first(b[0])
        && b[1..]
            .iter()
            .all(|&c| first(c) || matches!(c, b'.' | b'_' | b'-'))
}

fn definition(p: &Parsed<'_>) -> Result<Definition, BodyError> {
    let kind = match p.u8_in(3, 5)? {
        1 => Kind::Gauge,
        2 => Kind::Counter,
        3 => Kind::State,
        4 => Kind::Text,
        5 => Kind::Interval,
        _ => return Err(p.bad(3)),
    };
    let value_type = ValueType::from_code(p.u8_in(4, 5)?).ok_or(p.bad(4))?;
    let decimal = value_type == ValueType::Decimal;
    let scale = if decimal {
        Some(p.u8_in(5, MAX_SCALE)?)
    } else {
        p.none(5).map(|_| None)?
    };
    let counter = if kind == Kind::Counter {
        Some(CounterRules {
            wrap_at: p.opt_u64_of(10, p.need(10)?)?,
            reset_ceiling_ppm: p.u32(11)?,
        })
    } else {
        p.none(10)?;
        p.none(11)?;
        None
    };
    let state_names: Vec<String> = p
        .all(13)
        .map(|v| p.text(13, v, 1, 63))
        .collect::<Result<_, _>>()?;
    if (value_type == ValueType::State) == state_names.is_empty()
        || state_names.len() > MAX_STATE_NAMES
    {
        return Err(if state_names.len() > MAX_STATE_NAMES {
            p.bad(13)
        } else {
            p.presence(13)
        });
    }
    let interval = if kind == Kind::Interval {
        Some(match p.u8_in(16, 2)? {
            0 => {
                let (length, phase) = (p.i64(17)?, p.i64(18)?);
                if length <= 0 {
                    return Err(p.bad(17));
                }
                if !(0..length).contains(&phase) {
                    return Err(p.bad(18));
                }
                p.none(19)?;
                p.none(22)?;
                IntervalLength::Fixed { length, phase }
            }
            tag => {
                p.none(17)?;
                p.none(18)?;
                let zone = p.need_text(19, 1, 63)?;
                if tag == 1 {
                    let day_start = p.i64(22)?;
                    if !(0..DAY).contains(&day_start) {
                        return Err(p.bad(22));
                    }
                    IntervalLength::Day { zone, day_start }
                } else {
                    p.none(22)?;
                    IntervalLength::Month { zone }
                }
            }
        })
    } else {
        for id in [16, 17, 18, 19, 22] {
            p.none(id)?;
        }
        None
    };
    let retention = match p.u8_in(23, 1)? {
        0 => p.none(24).map(|_| Retention::LongTerm)?,
        _ => Retention::Diagnostic { period: p.i64(24)? },
    };
    let excess_precision = if decimal {
        Some(match p.u8_in(25, 2)? {
            0 => p.none(26).map(|_| ExcessPrecision::Round)?,
            1 => p.none(26).map(|_| ExcessPrecision::Refuse)?,
            _ => {
                let max_scale = p.u8_in(26, MAX_SCALE)?;
                if Some(max_scale) < scale {
                    return Err(p.bad(26));
                }
                ExcessPrecision::AutoRaise { max_scale }
            }
        })
    } else {
        p.none(25)?;
        p.none(26)?;
        None
    };
    Ok(Definition {
        id: p.u32(1)?,
        name: p.need_text(2, 1, 255)?,
        kind,
        value_type,
        scale,
        unit: p.need_text(6, 0, 63)?,
        expected_interval: p.opt_i64_of(7, p.need(7)?, true)?,
        staleness: p.opt_i64_of(8, p.need(8)?, true)?,
        record_on_change: p.u8_in(9, 1)? == 1,
        counter,
        lateness_bound: p.i64(12)?,
        state_names,
        versioning: match p.u8_in(14, 2)? {
            0 => Versioning::None,
            1 => Versioning::Time,
            _ => Versioning::Version,
        },
        timing: match p.u8_in(15, 2)? {
            0 => Timing::Observed,
            1 => Timing::Scheduled,
            _ => Timing::Forecast,
        },
        interval,
        retention,
        excess_precision,
        staleness_set: p.u8_in(27, 1)? == 1,
    })
}

fn parameter_change(p: &Parsed<'_>) -> Result<ParameterChange, BodyError> {
    let opt = |id| p.get(id);
    let expected_interval = opt(7).map(|v| p.opt_i64_of(7, v, true)).transpose()?;
    let staleness = match (opt(8), opt(27)) {
        (Some(v), Some(_)) => Some((p.opt_i64_of(8, v, true)?, p.u8_in(27, 1)? == 1)),
        (None, None) => None,
        (Some(_), None) => return Err(p.presence(27)),
        (None, Some(_)) => return Err(p.presence(8)),
    };
    // A change of 7 is written with the staleness limit in force after it.
    if expected_interval.is_some() && staleness.is_none() {
        return Err(p.presence(8));
    }
    let change = ParameterChange {
        id: p.u32(1)?,
        effective: opt(20).map(|_| p.i64(20)).transpose()?,
        expected_interval,
        staleness,
        record_on_change: opt(9).map(|_| p.u8_in(9, 1).map(|v| v == 1)).transpose()?,
        wrap_at: opt(10).map(|v| p.opt_u64_of(10, v)).transpose()?,
        reset_ceiling_ppm: opt(11).map(|_| p.u32(11)).transpose()?,
        lateness_bound: opt(12).map(|_| p.i64(12)).transpose()?,
        retention_class: opt(23).map(|_| p.u8_in(23, 1)).transpose()?,
        retention_period: opt(24).map(|_| p.i64(24)).transpose()?,
        excess_precision: match opt(25) {
            None => p.none(26).map(|_| None)?,
            Some(_) => Some(match p.u8_in(25, 2)? {
                0 => p.none(26).map(|_| ExcessPrecision::Round)?,
                1 => p.none(26).map(|_| ExcessPrecision::Refuse)?,
                _ => ExcessPrecision::AutoRaise {
                    max_scale: p.u8_in(26, MAX_SCALE)?,
                },
            }),
        },
    };
    let timed = [7, 8, 9, 10, 11, 12].iter().any(|&id| opt(id).is_some());
    if timed != change.effective.is_some() {
        return Err(p.presence(20));
    }
    Ok(change)
}

/// The bytes `e` takes in an entries body: its header, fields and padding to 8.
pub(crate) fn entry_len(e: &Entry) -> u64 {
    (8 + encode_entry(e).1.len() as u64).div_ceil(8) * 8
}

impl Entry {
    /// The sequences this entry takes: 1, or the count of an [`Entry::Ignored`].
    pub fn sequences(&self) -> u64 {
        match self {
            Self::Ignored { count } => u64::from(*count),
            _ => 1,
        }
    }

    /// Whether this is a recovery entry.
    pub fn is_recovery(&self) -> bool {
        matches!(self, Self::Recovery(_))
    }
}
