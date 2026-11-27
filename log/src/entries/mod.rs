//! Entries block bodies (`format.md` A.6): typed entries, the canonical encoder and the decoder with every check.

use crate::block::block_type;
use crate::body::{DATA_PREFIX_LEN, DataPrefix};
use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::content_error::{BodyError, ContentError};
use crate::error::{Element, EncodeError};
use crate::facts::{MAX_SCALE, ValueType};
use crate::mark::ResetStamp;

mod decode;
mod definition;
mod encode;

pub use decode::consumer_name_ok;
use decode::decode;
pub(crate) use decode::{check, holds_recovery};
use encode::encode_entry;

/// The largest entry, header excluded: 8 MiB.
pub const MAX_ENTRY_LEN: usize = 8 << 20;
/// The most state names of one series.
pub const MAX_STATE_NAMES: usize = 65_536;
/// Most boundaries of a levels gate (field 73).
pub const GATE_LEVELS: usize = 16;
/// Most members of a set gate (field 74).
pub const GATE_MEMBERS: usize = 64;
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
    /// Routing change (laid out by roadmap 5.2).
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
    /// 41: the reset stamp this entry starts, its sequence the entry's position; every writer writes it, and an
    /// entry without it is read as having none (`format.md` A.6).
    pub stamp: Option<crate::mark::ResetStamp>,
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

/// A series selection in a routing change: a funnel's (fields 53 to 55) or a consumer's `emits` (fields 60 to 62).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selection {
    /// Kind 0: no series.
    None,
    /// Kind 1: the listed ids (the id field repeated, possibly none).
    Ids(Vec<u32>),
    /// Kind 2: a name pattern as declared, 1 to 255 bytes. For a condition funnel, `resolved` holds the ids the
    /// pattern was resolved to when it was routed (the id field repeated); empty otherwise, and always for `emits`.
    Pattern {
        /// The pattern.
        pattern: String,
        /// Ids resolved once, for a condition funnel.
        resolved: Vec<u32>,
    },
    /// Kind 3: every series.
    All,
}

/// What a routing change does (field 51).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteOp {
    /// 1: adds a funnel.
    Add = 1,
    /// 2: removes a funnel.
    Remove = 2,
    /// 3: declares the consumer's `emits` only.
    Emits = 3,
    /// 4: watches a gate.
    Watch = 4,
    /// 5: unwatches a gate.
    Unwatch = 5,
}

/// The series a gate reads (field 69, with 54 or 55).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GateSelector {
    /// 1: one series (field 54, one id).
    One(u32),
    /// 2: each series a name pattern selects (field 55).
    Each(String),
}

/// One boundary of a levels gate (field 73, 32 bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateLevel {
    /// Crossed upwards above this, finite.
    pub at: f64,
    /// Crossed back below this, finite, at most `at`.
    pub clear: f64,
    /// Hold before the upward crossing, nanoseconds ≥ 0.
    pub hold_up: i64,
    /// Hold before the downward crossing, nanoseconds ≥ 0.
    pub hold_down: i64,
}

/// A set member (field 74): a tag byte, then the value.
#[derive(Clone, Debug, PartialEq)]
pub enum GateValue {
    /// 1: `f64`, finite.
    F64(f64),
    /// 2: decimal, `i64` units then `u8` scale.
    Decimal {
        /// Units.
        units: i64,
        /// Scale.
        scale: u8,
    },
    /// 3: a named state's index, `u32`.
    State(u32),
    /// 4: a boolean byte.
    Bool(bool),
    /// 5: UTF-8 text, 1 to 255 bytes.
    Text(String),
}

/// How a gate turns its input into a state (field 72, with 73 to 76).
#[derive(Clone, Debug, PartialEq)]
pub enum GateKind {
    /// 1: levels, 1 to 16 boundaries (73, repeated), rising.
    Levels(Vec<GateLevel>),
    /// 2: set, 1 to 64 members (74, repeated) and holds (75: `hold_on`, `hold_off`).
    Set {
        /// Members.
        members: Vec<GateValue>,
        /// Hold on, nanoseconds ≥ 0.
        hold_on: i64,
        /// Hold off, nanoseconds ≥ 0.
        hold_off: i64,
    },
    /// 3: change, with a dead-band (76).
    Change {
        /// Dead-band, finite, ≥ 0.
        deadband: f64,
    },
}

/// A watched gate as its entry carries it (`format.md` A.6, fields 68 to 77).
#[derive(Clone, Debug, PartialEq)]
pub struct GateSpec {
    /// 68: 1 to 64 bytes.
    pub name: String,
    /// 69 with 54 or 55.
    pub selector: GateSelector,
    /// 71: the input; 1 value (2, window, reserved).
    pub input: u8,
    /// 72 to 76.
    pub kind: GateKind,
    /// 77: staleness, nanoseconds > 0; `None` never.
    pub stale_after: Option<i64>,
}

/// The gate part of a watch or unwatch change.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteGate {
    /// 67: the gate id, never reused by its consumer.
    pub id: u32,
    /// The gate, for [`RouteOp::Watch`] only.
    pub spec: Option<GateSpec>,
}

impl Eq for RouteGate {}

impl std::hash::Hash for RouteGate {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.id.hash(h);
        if let Some(s) = &self.spec {
            (&s.name, &s.selector, s.input, s.stale_after).hash(h);
        }
    }
}

/// The argument of a time-in-state operator (field 58).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateArg {
    /// A boolean state.
    Bool(bool),
    /// A named state's index.
    State(u32),
}

/// A funnel's rolling-window operator (fields 56 to 58).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteOperator {
    /// 56: 1 avg, 2 sum, 3 integral, 4 min, 5 max, 6 count, 7 increase, 8 rate, 9 last, 10 time in state.
    pub aggregate: u8,
    /// 57: window width, nanoseconds, > 0.
    pub width: i64,
    /// 58: the state counted, for aggregate 10 only.
    pub state: Option<StateArg>,
}

/// A funnel's condition (fields 63 to 66). Levels are finite `f64`s, durations nanoseconds ≥ 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RouteCondition {
    /// Kind 1: 64 activation level, 65 clear level (at most the activation level).
    Threshold {
        /// Activation level.
        on: f64,
        /// Clear level.
        clear: f64,
    },
    /// Kind 2: 64 dead-band (≥ 0).
    OnChange {
        /// Dead-band.
        deadband: f64,
    },
    /// Kind 3: 64 level, 66 duration.
    HeldFor {
        /// Level.
        above: f64,
        /// Duration.
        n: i64,
    },
    /// Kind 4: 66 duration beyond the series' staleness limit in force at the entry's position.
    StaleFor {
        /// Duration.
        n: i64,
    },
}

impl Eq for RouteCondition {}

impl std::hash::Hash for RouteCondition {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        match *self {
            RouteCondition::Threshold { on, clear } => {
                (1u8, on.to_bits(), clear.to_bits(), 0i64).hash(h)
            }
            RouteCondition::OnChange { deadband } => (2u8, deadband.to_bits(), 0u64, 0i64).hash(h),
            RouteCondition::HeldFor { above, n } => (3u8, above.to_bits(), 0u64, n).hash(h),
            RouteCondition::StaleFor { n } => (4u8, 0u64, 0u64, n).hash(h),
        }
    }
}

/// A routing change (entry type 5, `spec.md` §4 Routing): a funnel added to or removed from a consumer, or the
/// consumer's `emits` declared; every change carries the consumer's `emits` in force from its position.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Routing {
    /// 50: the consumer's name (A.12).
    pub consumer: String,
    /// 51.
    pub op: RouteOp,
    /// 52: the funnel id, for [`RouteOp::Add`] and [`RouteOp::Remove`].
    pub funnel: Option<u32>,
    /// 53 to 55: the funnel's selection, for [`RouteOp::Add`].
    pub selection: Option<Selection>,
    /// 56 to 58: the funnel's operator, for [`RouteOp::Add`].
    pub operator: Option<RouteOperator>,
    /// 60 to 62: the consumer's `emits`.
    pub emits: Selection,
    /// 63 to 66: the funnel's condition, for [`RouteOp::Add`].
    pub condition: Option<RouteCondition>,
    /// 67 to 77: the gate, for [`RouteOp::Watch`] and [`RouteOp::Unwatch`].
    pub gate: Option<Box<RouteGate>>,
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
    /// 5.
    Routing(Box<Routing>),
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

    /// The checks of [`EntriesBlock::decode`], with its verdict, keeping no entry and copying no text (Q-651).
    pub fn check(body: &[u8]) -> Result<(), BodyError> {
        check(body).map(|_| ())
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

/// The bytes `e` takes in an entries body: its header, fields and padding to 8.
pub(crate) fn entry_len(e: &Entry) -> u64 {
    (8 + encode_entry(e).1.len() as u64).div_ceil(8) * 8
}

impl Entry {
    /// The bytes it takes in an entries body: its header, fields and padding to 8.
    pub fn encoded_len(&self) -> u64 {
        entry_len(self)
    }

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
