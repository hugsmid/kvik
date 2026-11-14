//! Differential check against the reference model (`kvik-model`): model facts and registry events, converted to
//! log entries, encoded in a segment, read back and converted again, are the same model values. The model is the
//! reference for what each field means.

mod support;

use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{
    self, Definition, Entry, IntervalLength, ParameterChange, Retention, Versioning,
};
use kvik_log::facts::{
    FactFlags, FactRow, Quality as LogQuality, Value as LogValue, ValueType as LogValueType,
};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::segment::SegmentImage;
use kvik_model::registry::{ChangedAtKind, RegistryEvent, RetentionClass, SeriesOptions, Timing};
use kvik_model::summary::CounterRules;
use kvik_model::types::{
    Fact, Flags, IntervalSpec, Kind, Position, Quality, SeriesId, Value, ValueType,
};
use proptest::prelude::*;
use support::{ID, T0};

/// The state names of the named-state series of this test; the log stores a state's index (A.5).
const STATES: [&str; 3] = ["off", "on", "standby"];

/// The value type of each series of this test.
fn value_type_of(series: u32) -> ValueType {
    [
        ValueType::Number,
        ValueType::Boolean,
        ValueType::Name,
        ValueType::Text,
    ][(series % 4) as usize]
}

fn log_value_type(vt: ValueType) -> LogValueType {
    match vt {
        ValueType::Number => LogValueType::F64,
        ValueType::Boolean => LogValueType::Bool,
        ValueType::Name => LogValueType::State,
        ValueType::Text => LogValueType::Text,
    }
}

fn quality_to_log(q: Quality) -> LogQuality {
    match q {
        Quality::Good => LogQuality::Good,
        Quality::Estimated => LogQuality::Estimated,
        Quality::Suspect => LogQuality::Suspect,
        Quality::Bad => LogQuality::Bad,
    }
}

fn quality_from_log(q: LogQuality) -> Quality {
    match q {
        LogQuality::Good => Quality::Good,
        LogQuality::Estimated => Quality::Estimated,
        LogQuality::Suspect => Quality::Suspect,
        LogQuality::Bad => Quality::Bad,
    }
}

/// A model fact as a fact row: the supplied quality is stored, the engine quality is derived (A.5).
fn fact_to_row(f: &Fact) -> FactRow {
    let vt = value_type_of(f.series.0);
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

fn row_to_fact(r: &FactRow, position: Position, received_at: i64) -> Fact {
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

fn kind_to_log(k: Kind) -> entries::Kind {
    match k {
        Kind::Gauge => entries::Kind::Gauge,
        Kind::Counter => entries::Kind::Counter,
        Kind::State => entries::Kind::State,
        Kind::Text => entries::Kind::Text,
        Kind::Interval => entries::Kind::Interval,
    }
}

fn kind_from_log(k: entries::Kind) -> Kind {
    match k {
        entries::Kind::Gauge => Kind::Gauge,
        entries::Kind::Counter => Kind::Counter,
        entries::Kind::State => Kind::State,
        entries::Kind::Text => Kind::Text,
        entries::Kind::Interval => Kind::Interval,
    }
}

fn vt_from_log(vt: LogValueType) -> ValueType {
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
fn event_to_entry(e: &RegistryEvent) -> Entry {
    match e {
        RegistryEvent::Define {
            id,
            name,
            kind,
            value_type,
            unit,
            staleness,
            options,
        } => Entry::Definition(Definition {
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
        }),
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

fn entry_to_event(e: &Entry) -> RegistryEvent {
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

fn model_value(series: u32) -> BoxedStrategy<Value> {
    match value_type_of(series) {
        ValueType::Number => prop_oneof![
            (-1e300f64..1e300).prop_map(Value::Num),
            Just(Value::Num(-0.0))
        ]
        .boxed(),
        ValueType::Boolean => any::<bool>().prop_map(Value::Bool).boxed(),
        ValueType::Name => prop::sample::select(STATES.to_vec())
            .prop_map(|s| Value::Str(s.to_owned()))
            .boxed(),
        ValueType::Text => "[ -~]{0,40}".prop_map(Value::Str).boxed(),
    }
}

fn quality() -> impl Strategy<Value = Quality> {
    prop::sample::select(vec![
        Quality::Good,
        Quality::Estimated,
        Quality::Suspect,
        Quality::Bad,
    ])
}

/// A model fact as the engine records it: the engine quality follows from the supplied one (A.5).
fn model_fact() -> impl Strategy<Value = Fact> {
    (1u32..9)
        .prop_flat_map(|series| {
            (
                Just(series),
                any::<i64>(),
                prop::option::weighted(0.85, model_value(series)),
                quality(),
                any::<[bool; 5]>(),
                prop::option::weighted(0.2, any::<u64>()),
            )
        })
        .prop_map(|(series, t, value, supplied, f, changed_at)| {
            let value = value.unwrap_or(Value::Absent);
            let r = FactRow {
                series_id: series,
                t,
                quality: quality_to_log(supplied),
                flags: FactFlags {
                    future: f[0],
                    late: f[1],
                    revise: f[2],
                    host_time: f[3],
                    unsynced_clock: f[4],
                    rounded: false,
                },
                value: LogValue::Bool(false),
                changed_at,
            };
            let mut fact = row_to_fact(&r, Position::new(0, 0), 0);
            fact.flags.no_value = matches!(value, Value::Absent);
            if fact.flags.no_value {
                fact.quality = Quality::Bad;
            }
            fact.value = value;
            fact
        })
}

fn model_event() -> impl Strategy<Value = RegistryEvent> {
    let define = (
        1u32..9,
        "[a-z]{1,8}/[a-z]{1,8}/[a-z]{1,8}",
        "[a-zA-Z%]{0,6}",
        any::<bool>(),
        1i64..1 << 40,
        0usize..3,
        0usize..3,
        any::<bool>(),
        0u32..=1_000_000,
    )
        .prop_map(|(id, name, unit, roc, interval, ver, timing, wraps, ppm)| {
            let vt = value_type_of(id);
            let kind = match vt {
                ValueType::Number => {
                    [Kind::Gauge, Kind::Counter, Kind::Interval][(id as usize / 4) % 3]
                }
                ValueType::Boolean | ValueType::Name => Kind::State,
                ValueType::Text => Kind::Text,
            };
            let mut options = SeriesOptions {
                versioned: [
                    None,
                    Some(ChangedAtKind::Time),
                    Some(ChangedAtKind::Version),
                ][ver],
                timing: [Timing::Observed, Timing::Scheduled, Timing::Forecast][timing],
                expected_interval: Some(interval),
                record_on_change: roc,
                counter: CounterRules::default(),
                lateness_bound: Some(interval / 2),
                interval: None,
            };
            if kind == Kind::Counter {
                options.counter = CounterRules {
                    wrap_at: wraps.then_some(65_536.0),
                    reset_ceiling: f64::from(ppm) / 1e6,
                };
            }
            if kind == Kind::Interval {
                options.interval = Some(IntervalSpec {
                    length: interval,
                    phase: interval / 3,
                });
            }
            RegistryEvent::Define {
                id: SeriesId(id),
                name,
                kind,
                value_type: vt,
                unit,
                staleness: Some(3 * interval),
                options,
            }
        });
    prop_oneof![
        define,
        (1u32..9, "[a-z/]{1,20}").prop_map(|(id, name)| RegistryEvent::Alias {
            id: SeriesId(id),
            name
        }),
        (1u32..9, prop::option::of(1i64..1 << 50)).prop_map(|(id, p)| RegistryEvent::Retention {
            id: SeriesId(id),
            class: p.map_or(RetentionClass::LongTerm, |period| {
                RetentionClass::Diagnostic { period }
            }),
        }),
        (1u32..9, any::<i64>()).prop_map(|(id, at)| RegistryEvent::Retire {
            id: SeriesId(id),
            at
        }),
        ("[A-Z][a-z]{2,8}/[A-Z][a-z]{2,8}", any::<i64>())
            .prop_map(|(zone, effective)| RegistryEvent::SiteZone { zone, effective }),
        "20[0-9]{2}[a-z]".prop_map(|version| RegistryEvent::Tzdata { version }),
    ]
}

#[derive(Clone, Debug, PartialEq)]
enum ModelItem {
    Fact(Fact),
    Event(RegistryEvent),
}

proptest! {
    #[test]
    fn model_values_survive_the_log(
        batches in prop::collection::vec(
            (prop::collection::vec(prop_oneof![3 => model_fact().prop_map(ModelItem::Fact), 1 => model_event().prop_map(ModelItem::Event)], 1..20), any::<i64>()),
            1..4,
        ),
    ) {
        let mut seg = SegmentImage::create_ahead(ID, T0, 1 << 20).unwrap();
        seg.take_into_use(1).unwrap();
        let mut expected = Vec::new();
        let mut next = 1u64;
        for (items, received_at) in batches {
            let mut log_items = Vec::new();
            for (k, item) in items.into_iter().enumerate() {
                let position = Position::new(ID.generation, next + k as u64);
                match item {
                    ModelItem::Fact(mut f) => {
                        f.position = position;
                        f.received_at = received_at;
                        log_items.push(Item::Fact(fact_to_row(&f)));
                        expected.push(ModelItem::Fact(f));
                    }
                    ModelItem::Event(e) => {
                        log_items.push(Item::Entry(event_to_entry(&e)));
                        expected.push(ModelItem::Event(e));
                    }
                }
            }
            let content = BatchContent { first_sequence: next, received_at, items: log_items };
            next = content.last_sequence() + 1;
            seg.append(&content.builder().unwrap(), received_at).unwrap();
        }
        let r = scan(seg.bytes(), &ID, ScanLimit::End).unwrap();
        let mut got = Vec::new();
        for b in &r.batches {
            let c = BatchContent::decode_canonical(b).unwrap();
            for (k, item) in c.items.iter().enumerate() {
                let position = Position::new(ID.generation, c.first_sequence + k as u64);
                got.push(match item {
                    Item::Fact(row) => ModelItem::Fact(row_to_fact(row, position, c.received_at)),
                    Item::Entry(e) => ModelItem::Event(entry_to_event(e)),
                });
            }
        }
        prop_assert_eq!(got, expected);
    }
}
