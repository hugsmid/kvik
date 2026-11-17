//! The export schema (`docs/export.md`, Q-190): columns, their Arrow types and the file's key-value metadata.

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use kvik::{
    ChangedAtKind, IntervalLength, Kind, SeriesId, SeriesInfo, StateValues, Timing, ValueType,
};

use crate::ExportError;

/// The schema version written to every file (Q-190, `docs/export.md` Compatibility).
pub const EXPORT_VERSION: &str = "1";

/// Precision of the decimal value column: every `i64` units value fits (Q-192).
pub const DECIMAL_PRECISION: u8 = 19;

/// The highest decimal scale a series may have (spec §2.1 `decimal scale`).
pub const MAX_DECIMAL_SCALE: u8 = 18;

/// The Arrow scale of a decimal column; a scale above [`MAX_DECIMAL_SCALE`] is refused.
pub(crate) fn decimal_scale(scale: u8) -> Result<i8, ExportError> {
    if scale > MAX_DECIMAL_SCALE {
        return Err(ExportError::UnsupportedSeries {
            reason: "a decimal scale above 18",
        });
    }
    Ok(i8::try_from(scale).expect("at most 18"))
}

/// Column names (`docs/export.md` Columns).
pub mod column {
    /// Series id.
    pub const SERIES_ID: &str = "series_id";
    /// Series display name.
    pub const SERIES_NAME: &str = "series_name";
    /// Event time.
    pub const T: &str = "t";
    /// Position: generation.
    pub const GENERATION: &str = "generation";
    /// Position: sequence.
    pub const SEQUENCE: &str = "sequence";
    /// Host clock at commit.
    pub const RECEIVED_AT: &str = "received_at";
    /// The value, typed by the series.
    pub const VALUE: &str = "value";
    /// Decimal series: the scale each fact was stored at.
    pub const STORED_SCALE: &str = "stored_scale";
    /// Named-state series: the declared name of the value's index.
    pub const STATE_NAME: &str = "state_name";
    /// Quality after stamping.
    pub const QUALITY: &str = "quality";
    /// Quality as supplied.
    pub const SUPPLIED_QUALITY: &str = "supplied_quality";
    /// Flag columns, in spec §2.2 order, then `no_value` and `rounded`.
    pub const FLAGS: [&str; 7] = [
        "future",
        "late",
        "revise",
        "host_time",
        "unsynced_clock",
        "no_value",
        "rounded",
    ];
    /// Versioned series only.
    pub const CHANGED_AT: &str = "changed_at";
    /// Interval series only.
    pub const INTERVAL_END: &str = "interval_end";
}

/// What a series' value column holds.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueKind {
    /// `f64`.
    F64,
    /// Decimal, exported at `scale` (Q-192).
    Decimal {
        /// Scale of the decimal column.
        scale: u8,
    },
    /// Boolean state.
    Bool,
    /// Named state, with its declared names.
    State(Vec<String>),
    /// Text.
    Text,
}

/// A series' kind (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KindName {
    /// Gauge.
    Gauge,
    /// Counter.
    Counter,
    /// State.
    State,
    /// Text.
    Text,
    /// Interval.
    Interval,
}

/// What `changed_at` holds in a versioned series (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChangedAt {
    /// UTC nanoseconds, exported as a timestamp.
    Time,
    /// An opaque counter, exported as `UINT_64`.
    Version,
}

/// A series' timing (spec §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimingName {
    /// Observed.
    Observed,
    /// Scheduled.
    Scheduled,
    /// Forecast: `t` is the valid time, `changed_at` the issue time.
    Forecast,
}

/// What the export needs to know about a series.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesSchema {
    /// Id.
    pub id: SeriesId,
    /// Display name.
    pub name: String,
    /// Unit.
    pub unit: String,
    /// Kind.
    pub kind: KindName,
    /// Value column type.
    pub value: ValueKind,
    /// `Some` for a versioned series.
    pub changed_at: Option<ChangedAt>,
    /// Timing.
    pub timing: TimingName,
    /// Fixed interval length in nanoseconds, for an interval series.
    pub interval: Option<i64>,
}

impl SeriesSchema {
    /// The schema of a series as the store describes it. A decimal series takes the definition's scale (Q-192).
    pub fn from_info(info: &SeriesInfo) -> Result<Self, ExportError> {
        let unsupported = |reason| ExportError::UnsupportedSeries { reason };
        let numeric = |v: &ValueType| match v {
            ValueType::F64 => Ok(ValueKind::F64),
            ValueType::Decimal { scale } => {
                decimal_scale(*scale)?;
                Ok(ValueKind::Decimal { scale: *scale })
            }
            _ => Err(unsupported("an unknown value type")),
        };
        let (kind, value, interval) = match &info.spec.kind {
            Kind::Gauge { value } => (KindName::Gauge, numeric(value)?, None),
            Kind::Counter { value } => (KindName::Counter, numeric(value)?, None),
            Kind::State {
                values: StateValues::Bool,
            } => (KindName::State, ValueKind::Bool, None),
            Kind::State {
                values: StateValues::Named(names),
            } => (KindName::State, ValueKind::State(names.clone()), None),
            Kind::Text => (KindName::Text, ValueKind::Text, None),
            Kind::Interval {
                value,
                length: IntervalLength::Fixed { d, .. },
            } => {
                let d = i64::try_from(d.as_nanos())
                    .map_err(|_| unsupported("an interval length beyond i64 nanoseconds"))?;
                (KindName::Interval, numeric(value)?, Some(d))
            }
            Kind::Interval { .. } => return Err(unsupported("a calendar-length interval series")),
            _ => return Err(unsupported("an unknown kind")),
        };
        let changed_at = match info.spec.versioned {
            None => None,
            Some(ChangedAtKind::Time) => Some(ChangedAt::Time),
            Some(ChangedAtKind::Version) => Some(ChangedAt::Version),
            Some(_) => return Err(unsupported("an unknown changed_at kind")),
        };
        let timing = match info.spec.timing {
            Timing::Observed => TimingName::Observed,
            Timing::Scheduled => TimingName::Scheduled,
            Timing::Forecast => TimingName::Forecast,
            _ => return Err(unsupported("an unknown timing")),
        };
        Ok(SeriesSchema {
            id: info.id,
            name: info.name.clone(),
            unit: info.spec.unit.clone(),
            kind,
            value,
            changed_at,
            timing,
            interval,
        })
    }

    /// The Arrow schema of the series' export file (`docs/export.md` Columns).
    pub fn arrow_schema(&self) -> Result<SchemaRef, ExportError> {
        let ts = timestamp();
        let mut fields = vec![
            Field::new(column::SERIES_ID, DataType::UInt32, false),
            Field::new(column::SERIES_NAME, DataType::Utf8, false),
            Field::new(column::T, ts.clone(), false),
            Field::new(column::GENERATION, DataType::UInt32, false),
            Field::new(column::SEQUENCE, DataType::UInt64, false),
            Field::new(column::RECEIVED_AT, ts.clone(), false),
        ];
        match &self.value {
            ValueKind::F64 => fields.push(Field::new(column::VALUE, DataType::Float64, true)),
            ValueKind::Decimal { scale } => {
                let scale = decimal_scale(*scale)?;
                fields.push(Field::new(
                    column::VALUE,
                    DataType::Decimal128(DECIMAL_PRECISION, scale),
                    true,
                ));
                fields.push(Field::new(column::STORED_SCALE, DataType::UInt8, true));
            }
            ValueKind::Bool => fields.push(Field::new(column::VALUE, DataType::Boolean, true)),
            ValueKind::State(_) => {
                fields.push(Field::new(column::VALUE, DataType::UInt16, true));
                fields.push(Field::new(column::STATE_NAME, DataType::Utf8, true));
            }
            ValueKind::Text => fields.push(Field::new(column::VALUE, DataType::Utf8, true)),
        }
        fields.push(Field::new(column::QUALITY, DataType::Utf8, false));
        fields.push(Field::new(column::SUPPLIED_QUALITY, DataType::Utf8, false));
        for f in column::FLAGS {
            fields.push(Field::new(f, DataType::Boolean, false));
        }
        match self.changed_at {
            Some(ChangedAt::Time) => fields.push(Field::new(column::CHANGED_AT, ts.clone(), false)),
            Some(ChangedAt::Version) => {
                fields.push(Field::new(column::CHANGED_AT, DataType::UInt64, false));
            }
            None => {}
        }
        if self.interval.is_some() {
            fields.push(Field::new(column::INTERVAL_END, ts, false));
        }
        Ok(Arc::new(Schema::new(fields)))
    }

    /// The series' entries of the file's key-value metadata (`docs/export.md` Metadata).
    pub(crate) fn metadata(&self) -> Vec<(String, String)> {
        let mut kv = vec![
            ("kvik.export_version", EXPORT_VERSION.to_string()),
            ("kvik.series_id", self.id.0.to_string()),
            ("kvik.series_name", self.name.clone()),
            ("kvik.unit", self.unit.clone()),
            ("kvik.kind", kind_name(self.kind).to_string()),
            ("kvik.value_type", value_type_name(&self.value)),
            ("kvik.timing", timing_name(self.timing).to_string()),
        ];
        if let ValueKind::State(names) = &self.value {
            kv.push(("kvik.state_names", json_strings(names)));
        }
        if let Some(c) = self.changed_at {
            let c = match c {
                ChangedAt::Time => "time",
                ChangedAt::Version => "version",
            };
            kv.push(("kvik.changed_at_kind", c.to_string()));
        }
        if let Some(d) = self.interval {
            kv.push(("kvik.interval_length_ns", d.to_string()));
        }
        kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }
}

/// `TIMESTAMP(NANOS, UTC)`.
pub(crate) fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
}

fn kind_name(k: KindName) -> &'static str {
    match k {
        KindName::Gauge => "gauge",
        KindName::Counter => "counter",
        KindName::State => "state",
        KindName::Text => "text",
        KindName::Interval => "interval",
    }
}

fn timing_name(t: TimingName) -> &'static str {
    match t {
        TimingName::Observed => "observed",
        TimingName::Scheduled => "scheduled",
        TimingName::Forecast => "forecast",
    }
}

fn value_type_name(v: &ValueKind) -> String {
    match v {
        ValueKind::F64 => "f64".into(),
        ValueKind::Decimal { scale } => format!("decimal({scale})"),
        ValueKind::Bool => "bool".into(),
        ValueKind::State(_) => "state".into(),
        ValueKind::Text => "text".into(),
    }
}

/// A JSON array of strings.
pub(crate) fn json_strings(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escapes_quotes_backslashes_and_controls() {
        let s = json_strings(&["a\"b".into(), "c\\d".into(), "e\nf".into(), "på".into()]);
        assert_eq!(s, r#"["a\"b","c\\d","e\u000af","på"]"#);
        assert_eq!(json_strings(&[]), "[]");
    }
}
