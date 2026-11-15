//! Exports from a store on each backend, read back and compared with `Reader::facts`.

mod common;

use std::time::Duration;

use common::{BACKENDS, at, bits, block_on, open, read_back, store_on};
use kvik::{
    Backend, ChangedAtKind, Intent, IntervalLength, Kind, Quality, Reader, Reading, SeriesId,
    SeriesSpec, StateValues, Store, Timestamp, Timing, Value, ValueType, View,
};
use kvik_export::{
    ExportError, ExportOptions, ExportRow, SeriesSchema, ValueKind, column, export_series,
};

fn facts(r: &Reader, id: SeriesId, view: View) -> Vec<ExportRow> {
    r.facts(id, at(-1_000_000)..at(1_000_000), view)
        .unwrap()
        .map(|f| ExportRow::from(f.unwrap()))
        .collect()
}

fn export(r: &Reader, id: SeriesId, view: View) -> Vec<u8> {
    let mut out = Vec::new();
    export_series(r, id, &ExportOptions::default().view(view), &mut out).unwrap();
    out
}

fn assert_round_trip(r: &Reader, id: SeriesId, view: View) -> Vec<u8> {
    let file = export(r, id, view);
    let expected = facts(r, id, view);
    assert!(!expected.is_empty());
    assert_eq!(bits(&read_back(&file)), bits(&expected), "{view:?}");
    file
}

fn define(store: &Store, spec: SeriesSpec) -> SeriesId {
    block_on(store.define_series(spec)).unwrap()
}

fn emit(store: &Store, readings: &[Reading]) {
    let ack = block_on(store.emit(readings)).unwrap();
    assert_eq!(ack.recorded(), readings.len(), "{ack:?}");
}

fn gauge(name: &str) -> SeriesSpec {
    SeriesSpec::new(
        name,
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    )
    .expected_interval(Duration::from_secs(1))
}

#[test]
fn every_value_type_and_store_flag_round_trips_on_both_backends() {
    for backend in BACKENDS {
        let (store, clock) = store_on(backend, "every-type");
        let g = define(&store, gauge("site/meter/power"));
        let c = define(
            &store,
            SeriesSpec::new(
                "site/meter/energy",
                Kind::Counter {
                    value: ValueType::F64,
                },
                "Wh",
            ),
        );
        let b = define(
            &store,
            SeriesSpec::new(
                "site/door/open",
                Kind::State {
                    values: StateValues::Bool,
                },
                "",
            ),
        );
        let names = vec!["off".to_string(), "heat \"eco\"".into(), "cool".into()];
        let s = define(
            &store,
            SeriesSpec::new(
                "site/pump/mode",
                Kind::State {
                    values: StateValues::Named(names.clone()),
                },
                "",
            ),
        );
        let x = define(&store, SeriesSpec::new("site/pump/note", Kind::Text, ""));
        let quarter = Duration::from_secs(900);
        let i = define(
            &store,
            SeriesSpec::new(
                "site/meter/import",
                Kind::Interval {
                    value: ValueType::F64,
                    length: IntervalLength::Fixed {
                        d: quarter,
                        phase: Duration::ZERO,
                    },
                },
                "kWh",
            ),
        );

        clock.advance(Duration::from_secs(3_600));
        emit(
            &store,
            &[
                Reading::new(g, at(0), Value::F64(1.5)),
                Reading::new(g, at(1), Value::F64(-0.0)),
                Reading::new(g, at(2), Value::F64(0.0)),
                Reading::new(g, at(3), Value::F64(f64::NAN)),
                Reading::new(g, at(4), Value::Absent).with_quality(Quality::Suspect),
                Reading::new(g, at(3_700), Value::F64(9.0)),
                Reading::new(g, at(3_600), Value::F64(f64::MIN_POSITIVE))
                    .with_quality(Quality::Estimated)
                    .host_stamped(),
                Reading::new(c, at(0), Value::F64(10.0)),
                Reading::new(c, at(60), Value::F64(-1.0)),
                Reading::new(b, at(0), Value::Bool(true)),
                Reading::new(b, at(5), Value::Bool(false)),
                Reading::new(s, at(0), Value::State(1)),
                Reading::new(s, at(5), Value::State(2)),
                Reading::new(s, at(9), Value::Absent),
                Reading::new(x, at(0), Value::Text("ready, \"steady\"\n".into())),
                Reading::new(x, at(5), Value::Text(String::new())),
                Reading::new(i, at(0), Value::F64(0.25)),
                Reading::new(i, at(900), Value::F64(0.5)),
            ],
        );
        let ack =
            block_on(store.emit_with(&[Reading::new(g, at(0), Value::F64(1.5))], Intent::Revise))
                .unwrap();
        assert_eq!(ack.recorded(), 1);
        clock.set_synchronised(false);
        emit(&store, &[Reading::new(g, at(10), Value::F64(2.0))]);
        clock.set_synchronised(true);

        let r = store.reader();
        let file = assert_round_trip(&r, g, View::History);
        let rows = read_back(&file);
        for (flag, want) in [
            ("future", rows.iter().any(|r| r.flags.future)),
            ("late", rows.iter().any(|r| r.flags.late)),
            ("revise", rows.iter().any(|r| r.flags.revise)),
            ("host_time", rows.iter().any(|r| r.flags.host_time)),
            (
                "unsynced_clock",
                rows.iter().any(|r| r.flags.unsynced_clock),
            ),
            ("no_value", rows.iter().any(|r| r.flags.no_value)),
        ] {
            assert!(want, "{backend:?}: no fact flagged {flag}");
        }
        assert!(rows.iter().any(|r| r.supplied_quality == Quality::Suspect
            && r.quality == Quality::Bad
            && r.value == Value::Absent));
        assert_round_trip(&r, g, View::Current);
        for id in [c, b, s, x, i] {
            assert_round_trip(&r, id, View::History);
        }

        let file = assert_round_trip(&r, s, View::History);
        let f = open(&file);
        assert_eq!(
            f.metadata("kvik.state_names"),
            Some(r#"["off","heat \"eco\"","cool"]"#)
        );
        assert_eq!(f.metadata("kvik.kind"), Some("state"));
        assert_eq!(f.metadata("kvik.view"), Some("history"));
        let state_names = column_strings(&file, column::STATE_NAME);
        assert_eq!(
            state_names,
            vec![Some("heat \"eco\"".into()), Some("cool".into()), None]
        );

        let file = assert_round_trip(&r, i, View::Current);
        assert_eq!(
            open(&file).metadata("kvik.interval_length_ns"),
            Some("900000000000")
        );
        let ends = column_i64(&file, column::INTERVAL_END);
        assert_eq!(ends, vec![at(900).0, at(1_800).0]);
        assert_eq!(
            column_strings(&file, column::SERIES_NAME),
            vec![Some("site/meter/import".into()); 2]
        );
        block_on(store.shutdown()).unwrap();
    }
}

#[test]
fn versioned_series_export_current_and_history_with_typed_changed_at() {
    for backend in BACKENDS {
        let (store, clock) = store_on(backend, "versioned");
        let f = define(
            &store,
            gauge("site/forecast/price")
                .versioned(ChangedAtKind::Time)
                .timing(Timing::Forecast),
        );
        let v = define(
            &store,
            gauge("site/meter/revised").versioned(ChangedAtKind::Version),
        );
        clock.advance(Duration::from_secs(60));
        let issued = at(0).0 as u64;
        emit(
            &store,
            &[
                Reading::new(f, at(3_600), Value::F64(0.30)).changed_at(issued),
                Reading::new(f, at(3_600), Value::F64(0.32)).changed_at(issued + 2_000),
                Reading::new(f, at(3_600), Value::F64(0.31)).changed_at(issued + 1_000),
                Reading::new(f, at(7_200), Value::F64(0.28)).changed_at(issued),
                Reading::new(v, at(0), Value::F64(5.0)).changed_at(1),
                Reading::new(v, at(0), Value::F64(6.0)).changed_at(u64::MAX),
                Reading::new(v, at(0), Value::F64(5.5)).changed_at(2),
            ],
        );
        let r = store.reader();
        for id in [f, v] {
            let current = assert_round_trip(&r, id, View::Current);
            let history = assert_round_trip(&r, id, View::History);
            assert!(read_back(&current).len() < read_back(&history).len());
        }
        let current = read_back(&export(&r, f, View::Current));
        assert_eq!(current[0].changed_at, Some(issued + 2_000));
        assert_eq!(current[0].value, Value::F64(0.32));
        let file = export(&r, f, View::Current);
        assert_eq!(open(&file).metadata("kvik.changed_at_kind"), Some("time"));
        assert_eq!(open(&file).metadata("kvik.timing"), Some("forecast"));
        assert_eq!(open(&file).metadata("kvik.view"), Some("current"));
        assert_eq!(
            column_i64(&file, column::CHANGED_AT)[0],
            (issued + 2_000) as i64
        );
        let file = export(&r, v, View::Current);
        assert_eq!(
            open(&file).metadata("kvik.changed_at_kind"),
            Some("version")
        );
        assert_eq!(read_back(&file)[0].changed_at, Some(u64::MAX));
        block_on(store.shutdown()).unwrap();
    }
}

#[test]
fn as_of_position_and_as_of_time_exports_match_the_store_on_both_backends() {
    for backend in BACKENDS {
        let (store, clock) = store_on(backend, "as-of");
        let g = define(&store, gauge("site/meter/power"));
        clock.advance(Duration::from_secs(60));
        let first = block_on(store.emit(&[
            Reading::new(g, at(0), Value::F64(1.0)),
            Reading::new(g, at(1), Value::F64(2.0)),
        ]))
        .unwrap();
        let before = clock.wall();
        clock.advance(Duration::from_secs(10));
        emit(
            &store,
            &[
                Reading::new(g, at(0), Value::F64(1.5)),
                Reading::new(g, at(2), Value::F64(3.0)),
            ],
        );
        let r = store.reader();
        let as_of = View::AsOf(first.last().unwrap());
        let as_of_time = View::AsOfTime(Timestamp(before.0 + 5_000_000_000));
        for view in [as_of, as_of_time] {
            let rows = read_back(&assert_round_trip(&r, g, view));
            let values: Vec<_> = rows.iter().map(|r| r.value.clone()).collect();
            assert_eq!(
                values,
                [Value::F64(1.0), Value::F64(2.0)],
                "{backend:?} {view:?}"
            );
        }
        assert_eq!(read_back(&export(&r, g, View::Current)).len(), 3);
        let file = export(&r, g, as_of);
        let p = first.last().unwrap();
        let want = format!("as_of {}:{}", p.generation, p.sequence);
        assert_eq!(open(&file).metadata("kvik.view"), Some(want.as_str()));
        block_on(store.shutdown()).unwrap();
    }
}

#[test]
fn a_series_schema_refuses_a_decimal_scale_above_eighteen() {
    let (store, _clock) = store_on(Backend::Durable, "scale");
    let g = define(&store, gauge("site/meter/power"));
    let mut info = store.reader().series(g).unwrap();
    info.spec.kind = Kind::Gauge {
        value: ValueType::Decimal { scale: 18 },
    };
    let schema = SeriesSchema::from_info(&info).unwrap();
    assert_eq!(schema.value, ValueKind::Decimal { scale: 18 });
    info.spec.kind = Kind::Gauge {
        value: ValueType::Decimal { scale: 19 },
    };
    let err = SeriesSchema::from_info(&info).unwrap_err();
    assert!(
        matches!(err, ExportError::UnsupportedSeries { .. }),
        "{err}"
    );
    block_on(store.shutdown()).unwrap();
}

#[test]
fn an_empty_series_exports_a_file_with_its_schema_and_no_rows() {
    for backend in BACKENDS {
        let (store, _clock) = store_on(backend, "empty");
        let id = define(&store, gauge("site/meter/idle"));
        let mut file = Vec::new();
        let summary =
            export_series(&store.reader(), id, &ExportOptions::default(), &mut file).unwrap();
        assert_eq!((summary.rows, summary.row_groups), (0, 0));
        let back = open(&file);
        assert_eq!(back.row_groups(), 0);
        assert_eq!(back.metadata("kvik.series_name"), Some("site/meter/idle"));
        assert_eq!(back.count(), 0);
        let schema = arrow_schema(&file);
        assert!(schema.field_with_name(column::VALUE).is_ok());
        assert!(schema.field_with_name(column::CHANGED_AT).is_err());
        block_on(store.shutdown()).unwrap();
    }
}

#[test]
fn an_unknown_series_is_a_query_error() {
    let (store, _clock) = store_on(Backend::Durable, "unknown");
    let err = export_series(
        &store.reader(),
        SeriesId(42),
        &ExportOptions::default(),
        Vec::new(),
    )
    .unwrap_err();
    assert!(matches!(err, kvik_export::ExportError::Query(_)), "{err}");
    block_on(store.shutdown()).unwrap();
}

fn batches(file: &[u8]) -> Vec<arrow_array::RecordBatch> {
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(file),
    )
    .unwrap()
    .build()
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn arrow_schema(file: &[u8]) -> arrow_schema::SchemaRef {
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(file),
    )
    .unwrap()
    .schema()
    .clone()
}

fn column_strings(file: &[u8], name: &str) -> Vec<Option<String>> {
    use arrow_array::cast::AsArray;
    batches(file)
        .iter()
        .flat_map(|b| {
            b.column_by_name(name)
                .unwrap()
                .as_string::<i32>()
                .iter()
                .map(|s| s.map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn column_i64(file: &[u8], name: &str) -> Vec<i64> {
    use arrow_array::cast::AsArray;
    batches(file)
        .iter()
        .flat_map(|b| {
            b.column_by_name(name)
                .unwrap()
                .as_primitive::<arrow_array::types::TimestampNanosecondType>()
                .values()
                .to_vec()
        })
        .collect()
}
