//! Conversions between the reference model's values (`kvik-model`) and the log's facts and entries, shared by the
//! differential tests. The model is the reference for what each field means; where the two represent a value
//! differently, the conversion says how.

#![allow(dead_code)]

use kvik_log::entries::{
    self, Definition, Entry, IntervalLength, ParameterChange, Retention, Versioning,
};
use kvik_log::facts::{
    FactFlags, FactRow, Quality as LogQuality, Value as LogValue, ValueType as LogValueType,
};
use kvik_model::registry::{ChangedAtKind, RegistryEvent, RetentionClass, SeriesOptions, Timing};
use kvik_model::summary::CounterRules;
use kvik_model::types::{
    Fact, Flags, IntervalSpec, Kind, Position, Quality, SeriesId, Value, ValueType,
};

/// The state names of every named-state series; the log stores a state's index (A.5).
pub const STATES: [&str; 3] = ["off", "on", "standby"];

pub fn log_value_type(vt: ValueType) -> LogValueType {
    match vt {
        ValueType::Number => LogValueType::F64,
        ValueType::Boolean => LogValueType::Bool,
        ValueType::Name => LogValueType::State,
        ValueType::Text => LogValueType::Text,
    }
}

pub fn quality_to_log(q: Quality) -> LogQuality {
    match q {
        Quality::Good => LogQuality::Good,
        Quality::Estimated => LogQuality::Estimated,
        Quality::Suspect => LogQuality::Suspect,
        Quality::Bad => LogQuality::Bad,
    }
}

pub fn quality_from_log(q: LogQuality) -> Quality {
    match q {
        LogQuality::Good => Quality::Good,
        LogQuality::Estimated => Quality::Estimated,
        LogQuality::Suspect => Quality::Suspect,
        LogQuality::Bad => Quality::Bad,
    }
}

/// A model fact as a fact row: the supplied quality is stored, the engine quality is derived (A.5).
pub fn fact_to_row(f: &Fact, vt: ValueType) -> FactRow {
    let value = match (&f.value, vt) {
        (Value::Absent, _) => LogValue::Absent {
            value_type: log_value_type(vt),
            scale: 0,
        },
        (Value::Num(v), _) => LogValue::F64(v.to_bits()),
        (Value::Bool(b), _) => LogValue::Bool(*b),
        (Value::Str(s), ValueType::Name) => {
            LogValue::State(STATES.iter().position(|x| x == s).unwrap() as u16)
        }
        (Value::Str(s), _) => LogValue::Text(s.clone()),
    };
    FactRow {
        series_id: f.series.0,
        t: f.t,
        quality: quality_to_log(f.supplied_quality),
        flags: FactFlags {
            future: f.flags.future,
            late: f.flags.late,
            revise: f.flags.revise,
            host_time: f.flags.host_time,
            unsynced_clock: f.flags.unsynced_clock,
            rounded: false,
        },
        value,
        changed_at: f.changed_at,
    }
}

pub fn row_to_fact(r: &FactRow, position: Position, received_at: i64) -> Fact {
    let no_value = matches!(r.value, LogValue::Absent { .. });
    let value = match &r.value {
        LogValue::Absent { .. } => Value::Absent,
        LogValue::F64(bits) => Value::Num(f64::from_bits(*bits)),
        LogValue::Bool(b) => Value::Bool(*b),
        LogValue::State(i) => Value::Str(STATES[usize::from(*i)].to_owned()),
        LogValue::Text(s) => Value::Str(s.clone()),
        LogValue::Decimal { .. } => unreachable!("the model has no decimal series"),
    };
    let supplied = quality_from_log(r.quality);
    let quality = if no_value {
        Quality::Bad
    } else if r.flags.future {
        supplied.worst(Quality::Suspect)
    } else {
        supplied
    };
    Fact {
        series: SeriesId(r.series_id),
        t: r.t,
        position,
        received_at,
        value,
        quality,
        supplied_quality: supplied,
        changed_at: r.changed_at,
        flags: Flags {
            future: r.flags.future,
            late: r.flags.late,
            revise: r.flags.revise,
            host_time: r.flags.host_time,
            unsynced_clock: r.flags.unsynced_clock,
            no_value,
        },
    }
}

pub fn kind_to_log(k: Kind) -> entries::Kind {
    match k {
        Kind::Gauge => entries::Kind::Gauge,
        Kind::Counter => entries::Kind::Counter,
        Kind::State => entries::Kind::State,
        Kind::Text => entries::Kind::Text,
        Kind::Interval => entries::Kind::Interval,
    }
}

pub fn kind_from_log(k: entries::Kind) -> Kind {
    match k {
        entries::Kind::Gauge => Kind::Gauge,
        entries::Kind::Counter => Kind::Counter,
        entries::Kind::State => Kind::State,
        entries::Kind::Text => Kind::Text,
        entries::Kind::Interval => Kind::Interval,
    }
}

pub fn vt_from_log(vt: LogValueType) -> ValueType {
    match vt {
        LogValueType::F64 => ValueType::Number,
        LogValueType::Bool => ValueType::Boolean,
        LogValueType::State => ValueType::Name,
        LogValueType::Text => ValueType::Text,
        LogValueType::Decimal => unreachable!("the model has no decimal series"),
    }
}

/// A registry event as an entry. The model's definitions are taken with every default already resolved, as a
/// definition entry carries them (A.6).
pub fn event_to_entry(e: &RegistryEvent) -> Entry {
    match e {
        RegistryEvent::Define {
            id,
            name,
            kind,
            value_type,
            unit,
            staleness,
            options,
        } => Entry::Definition(Box::new(Definition {
            id: id.0,
            name: name.clone(),
            kind: kind_to_log(*kind),
            value_type: log_value_type(*value_type),
            scale: None,
            unit: unit.clone(),
            expected_interval: options.expected_interval,
            staleness: *staleness,
            record_on_change: options.record_on_change,
            counter: (*kind == Kind::Counter).then(|| entries::CounterRules {
                wrap_at: options.counter.wrap_at.map(f64::to_bits),
                reset_ceiling_ppm: (options.counter.reset_ceiling * 1e6).round() as u32,
            }),
            lateness_bound: options.lateness_bound.unwrap(),
            state_names: if *value_type == ValueType::Name {
                STATES.map(String::from).to_vec()
            } else {
                vec![]
            },
            versioning: match options.versioned {
                None => Versioning::None,
                Some(ChangedAtKind::Time) => Versioning::Time,
                Some(ChangedAtKind::Version) => Versioning::Version,
            },
            timing: match options.timing {
                Timing::Observed => entries::Timing::Observed,
                Timing::Scheduled => entries::Timing::Scheduled,
                Timing::Forecast => entries::Timing::Forecast,
            },
            interval: options.interval.map(|iv| IntervalLength::Fixed {
                length: iv.length,
                phase: iv.phase,
            }),
            retention: Retention::LongTerm,
            excess_precision: None,
            staleness_set: false,
        })),
        RegistryEvent::Alias { id, name } => Entry::Alias {
            id: id.0,
            name: name.clone(),
        },
        RegistryEvent::Retention { id, class } => Entry::ParameterChange(ParameterChange {
            id: id.0,
            retention_class: Some(match class {
                RetentionClass::LongTerm => 0,
                RetentionClass::Diagnostic { .. } => 1,
            }),
            retention_period: match class {
                RetentionClass::LongTerm => None,
                RetentionClass::Diagnostic { period } => Some(*period),
            },
            ..ParameterChange::default()
        }),
        RegistryEvent::Retire { id, at } => Entry::Retirement { id: id.0, t_r: *at },
        RegistryEvent::SiteZone { zone, effective } => Entry::SiteZone {
            zone: zone.clone(),
            effective: *effective,
        },
        RegistryEvent::Tzdata { version } => Entry::Tzdata {
            version: version.clone(),
        },
    }
}

pub fn entry_to_event(e: &Entry) -> RegistryEvent {
    match e {
        Entry::Definition(d) => RegistryEvent::Define {
            id: SeriesId(d.id),
            name: d.name.clone(),
            kind: kind_from_log(d.kind),
            value_type: vt_from_log(d.value_type),
            unit: d.unit.clone(),
            staleness: d.staleness,
            options: SeriesOptions {
                versioned: match d.versioning {
                    Versioning::None => None,
                    Versioning::Time => Some(ChangedAtKind::Time),
                    Versioning::Version => Some(ChangedAtKind::Version),
                },
                timing: match d.timing {
                    entries::Timing::Observed => Timing::Observed,
                    entries::Timing::Scheduled => Timing::Scheduled,
                    entries::Timing::Forecast => Timing::Forecast,
                },
                expected_interval: d.expected_interval,
                record_on_change: d.record_on_change,
                counter: d.counter.map_or(CounterRules::default(), |c| CounterRules {
                    wrap_at: c.wrap_at.map(f64::from_bits),
                    reset_ceiling: f64::from(c.reset_ceiling_ppm) / 1e6,
                }),
                lateness_bound: Some(d.lateness_bound),
                interval: match &d.interval {
                    Some(IntervalLength::Fixed { length, phase }) => Some(IntervalSpec {
                        length: *length,
                        phase: *phase,
                    }),
                    _ => None,
                },
                durability: kvik_model::registry::Durability::Synced,
            },
        },
        Entry::Alias { id, name } => RegistryEvent::Alias {
            id: SeriesId(*id),
            name: name.clone(),
        },
        Entry::ParameterChange(p) => RegistryEvent::Retention {
            id: SeriesId(p.id),
            class: match (p.retention_class, p.retention_period) {
                (Some(1), Some(period)) => RetentionClass::Diagnostic { period },
                _ => RetentionClass::LongTerm,
            },
        },
        Entry::Retirement { id, t_r } => RegistryEvent::Retire {
            id: SeriesId(*id),
            at: *t_r,
        },
        Entry::SiteZone { zone, effective } => RegistryEvent::SiteZone {
            zone: zone.clone(),
            effective: *effective,
        },
        Entry::Tzdata { version } => RegistryEvent::Tzdata {
            version: version.clone(),
        },
        other => panic!("not a registry event: {other:?}"),
    }
}
