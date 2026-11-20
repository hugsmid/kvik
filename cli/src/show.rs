//! How times, values, positions and kinds are shown.

use jiff::tz::TimeZone;
use kvik::{Fact, Kind, Live, Position, SeriesInfo, StateValues, Timestamp, Value, ValueType};

use crate::Error;
use crate::out::{Cell, num};

/// Times in UTC or in a zone.
#[derive(Clone, Debug)]
pub struct Clock {
    zone: Option<TimeZone>,
}

impl Clock {
    /// Times in `zone`, UTC when `None`.
    pub fn new(zone: Option<&str>) -> Result<Clock, Error> {
        let zone = zone
            .map(|z| TimeZone::get(z).map_err(|e| Error::Usage(format!("zone {z:?}: {e}"))))
            .transpose()?;
        Ok(Clock { zone })
    }

    /// RFC 3339, with the fraction of a second when it is not zero.
    pub fn time(&self, t: Timestamp) -> String {
        let Ok(ts) = jiff::Timestamp::from_nanosecond(i128::from(t.0)) else {
            return t.0.to_string();
        };
        match &self.zone {
            None => ts.to_string(),
            Some(z) => ts.to_zoned(z.clone()).strftime("%Y-%m-%dT%H:%M:%S%.f%:z").to_string(),
        }
    }

    /// [`Clock::time`] as a cell.
    pub fn cell(&self, t: Option<Timestamp>) -> Cell {
        t.map_or(Cell::Null, |t| Cell::Text(self.time(t)))
    }
}

/// `generation:sequence`.
pub fn position(p: Position) -> String {
    format!("{}:{}", p.generation, p.sequence)
}

/// A value as text: a named state by its name.
pub fn value(v: &Value, kind: Option<&Kind>) -> Cell {
    match v {
        Value::F64(x) => num(x),
        Value::Decimal { units, scale } => Cell::Num(decimal(*units, *scale)),
        Value::State(k) => match kind {
            Some(Kind::State {
                values: StateValues::Named(names),
            }) => names
                .get(usize::from(*k))
                .map_or_else(|| Cell::Text(format!("#{k}")), |n| Cell::Text(n.clone())),
            _ => Cell::Text(format!("#{k}")),
        },
        Value::Bool(b) => Cell::Text(b.to_string()),
        Value::Text(t) => Cell::Text(t.clone()),
        Value::Absent => Cell::Null,
        _ => Cell::Text(format!("{v:?}")),
    }
}

fn decimal(units: i64, scale: u8) -> String {
    let digits = units.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let sign = if units < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{frac}")
}

/// A kind and its value type, in lower case.
pub fn kind(k: &Kind) -> String {
    let vt = |v: &ValueType| match v {
        ValueType::F64 => "f64".to_owned(),
        ValueType::Decimal { scale } => format!("decimal({scale})"),
        _ => format!("{v:?}").to_lowercase(),
    };
    match k {
        Kind::Gauge { value } => format!("gauge {}", vt(value)),
        Kind::Counter { value } => format!("counter {}", vt(value)),
        Kind::State {
            values: StateValues::Bool,
        } => "state bool".to_owned(),
        Kind::State { .. } => "state".to_owned(),
        Kind::Text => "text".to_owned(),
        Kind::Interval { value, .. } => format!("interval {}", vt(value)),
        _ => format!("{k:?}").to_lowercase(),
    }
}

/// A lower-case name from a `Debug` form.
pub fn lower(d: impl std::fmt::Debug) -> String {
    let s = format!("{d:?}");
    let mut out = String::new();
    for (k, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if k > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The latest-value columns.
pub const LATEST: &[&str] = &["id", "name", "state", "t", "value", "quality", "stale", "position"];

/// A row of [`LATEST`].
pub fn latest(info: Option<&SeriesInfo>, id: kvik::SeriesId, live: &Live, clock: &Clock) -> Vec<Cell> {
    let name = info.map(|i| Cell::Text(i.name.clone())).unwrap_or(Cell::Null);
    let kind = info.map(|i| &i.spec.kind);
    match live {
        Live::Value(v) => vec![
            num(id.0),
            name,
            "value".into(),
            Cell::Text(clock.time(v.fact.t)),
            value(&v.fact.value, kind),
            Cell::Text(lower(v.fact.quality)),
            Cell::Text(v.stale.to_string()),
            Cell::Text(position(v.fact.position)),
        ],
        other => vec![
            num(id.0),
            name,
            Cell::Text(match other {
                Live::Unknown => "unknown".into(),
                _ => "none".into(),
            }),
            Cell::Null,
            Cell::Null,
            Cell::Null,
            Cell::Null,
            Cell::Null,
        ],
    }
}

/// The fact cells of a tail row: position, id, name, t, value, quality.
pub fn fact(f: &Fact, info: Option<&SeriesInfo>, clock: &Clock) -> Vec<Cell> {
    vec![
        Cell::Text(position(f.position)),
        num(f.series.0),
        info.map(|i| Cell::Text(i.name.clone())).unwrap_or(Cell::Null),
        Cell::Text(clock.time(f.t)),
        value(&f.value, info.map(|i| &i.spec.kind)),
        Cell::Text(lower(f.quality)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_keep_their_scale_and_sign() {
        assert_eq!(decimal(12_345, 2), "123.45");
        assert_eq!(decimal(-5, 3), "-0.005");
        assert_eq!(decimal(7, 0), "7");
        assert_eq!(decimal(i64::MIN, 1), "-922337203685477580.8");
    }

    #[test]
    fn times_are_utc_unless_a_zone_is_given() {
        let t = Timestamp(1_767_225_600_500_000_000);
        assert_eq!(Clock::new(None).unwrap().time(t), "2026-01-01T00:00:00.5Z");
        assert_eq!(
            Clock::new(Some("Etc/GMT-2")).unwrap().time(t),
            "2026-01-01T02:00:00.5+02:00"
        );
        assert!(Clock::new(Some("Nowhere/Never")).is_err());
    }

    #[test]
    fn debug_names_become_snake_case() {
        assert_eq!(lower(kvik::LossCause::TruncatedTail), "truncated_tail");
        assert_eq!(lower(kvik::Quality::Good), "good");
    }
}
