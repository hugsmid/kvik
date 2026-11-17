//! Differential check against the reference model (`kvik-model`): model facts and registry events, converted to
//! log entries, encoded in a segment, read back and converted again, are the same model values. The model is the
//! reference for what each field means. Decimal values, scale raises and the excess-precision fields are not
//! covered: the model has no decimal type. Recovery entries are not registry events in the model either; they are
//! covered by the round trips in `batches.rs`.

mod convert;
mod support;

use convert::*;
use kvik_log::content::{BatchContent, Item};
use kvik_log::facts::{FactFlags, FactRow, Value as LogValue};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::segment::SegmentImage;
use kvik_model::registry::{ChangedAtKind, RegistryEvent, RetentionClass, SeriesOptions, Timing};
use kvik_model::summary::CounterRules;
use kvik_model::types::{Fact, IntervalSpec, Kind, Position, Quality, SeriesId, Value, ValueType};
use proptest::prelude::*;
use support::{ID, T0};

/// The value type of each series of this test.
fn value_type_of(series: u32) -> ValueType {
    [
        ValueType::Number,
        ValueType::Boolean,
        ValueType::Name,
        ValueType::Text,
    ][(series % 4) as usize]
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
                durability: kvik_model::registry::Durability::Synced,
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
                        log_items.push(Item::Fact(fact_to_row(&f, value_type_of(f.series.0))));
                        expected.push(ModelItem::Fact(f));
                    }
                    ModelItem::Event(e) => {
                        log_items.push(Item::from(event_to_entry(&e)));
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
