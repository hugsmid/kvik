//! Synthetic rows: decimals (no backend stores them yet), every flag, the Parquet types other tools see, and the
//! errors that stand in for a silent null.

mod common;

use common::{at, bits, export_to_vec, f64_series, open, read_back, row};
use kvik::{Position, Quality, SeriesId, Value, View};
use kvik_export::{
    ChangedAt, ExportError, ExportOptions, ExportRow, Flags, KindName, MAX_BATCH_ROWS,
    SeriesSchema, ValueKind, column, export_rows, read_rows,
};
use parquet::basic::{LogicalType, Repetition, TimeUnit, Type as PhysicalType};
use proptest::prelude::*;

fn decimal_series(scale: u8) -> SeriesSchema {
    SeriesSchema {
        value: ValueKind::Decimal { scale },
        kind: KindName::Counter,
        unit: "kWh".into(),
        ..f64_series(3)
    }
}

fn dec(units: i64, scale: u8) -> Value {
    Value::Decimal { units, scale }
}

#[test]
fn decimals_round_trip_with_their_stored_scale() {
    let series = decimal_series(6);
    let mut rows = vec![
        row(3, 0, 1, dec(12_000, 4)),
        row(3, 1, 2, dec(120_000, 5)),
        row(3, 2, 3, dec(-7, 0)),
        row(3, 3, 4, dec(i64::MAX, 6)),
        row(3, 4, 5, dec(-i64::MAX, 6)),
        row(3, 5, 6, dec(0, 2)),
        row(3, 6, 7, Value::Absent),
    ];
    rows[1].flags.rounded = true;
    rows[6].flags.no_value = true;
    rows[6].quality = Quality::Bad;
    rows[6].supplied_quality = Quality::Estimated;
    let file = export_to_vec(&series, &rows, &ExportOptions::default()).unwrap();
    assert_eq!(bits(&read_back(&file)), bits(&rows));
    assert_eq!(open(&file).metadata("kvik.value_type"), Some("decimal(6)"));

    let descr = parquet_columns(&file);
    let value = &descr[column::VALUE];
    assert_eq!(value.0, Some(LogicalType::decimal(6, 19)));
    assert_eq!(value.1, PhysicalType::FIXED_LEN_BYTE_ARRAY);
    assert_eq!(descr[column::STORED_SCALE].1, PhysicalType::INT32);
    assert_eq!(
        descr[column::STORED_SCALE].0,
        Some(LogicalType::integer(8, false))
    );
}

#[test]
fn a_decimal_stored_above_the_export_scale_is_an_error_not_a_rounding() {
    let err = export_to_vec(
        &decimal_series(4),
        &[row(3, 0, 1, dec(1, 5))],
        &ExportOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::ScaleAboveExport {
                stored: 5,
                export: 4,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn a_decimal_beyond_nineteen_digits_after_rescaling_is_an_error() {
    let err = export_to_vec(
        &decimal_series(6),
        &[row(3, 0, 1, dec(i64::MAX, 4))],
        &ExportOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExportError::DecimalOutOfRange { .. }),
        "{err}"
    );
}

/// A row whose only flags are `flags`, without a value exactly when `no_value` is among them.
fn flagged(secs: i64, flags: [bool; 7]) -> ExportRow {
    let flags = Flags::from_array(flags);
    let value = if flags.no_value {
        Value::Absent
    } else {
        Value::F64(secs as f64)
    };
    ExportRow {
        flags,
        ..row(1, secs, secs as u64 + 1, value)
    }
}

/// Rows 0 to 6 each set one flag, in `column::FLAGS` order; row 7 sets all, row 8 none.
fn flag_rows() -> Vec<ExportRow> {
    let mut rows: Vec<ExportRow> = (0..7)
        .map(|i| {
            let mut a = [false; 7];
            a[i] = true;
            flagged(i as i64, a)
        })
        .collect();
    rows.push(flagged(7, [true; 7]));
    rows.push(flagged(8, [false; 7]));
    rows
}

#[test]
fn every_flag_round_trips_alone_and_together() {
    let rows = flag_rows();
    let file = export_to_vec(&f64_series(1), &rows, &ExportOptions::default()).unwrap();
    assert_eq!(read_back(&file), rows);
}

type SetFlag = fn(&mut Flags);

#[test]
fn each_flag_lands_in_the_column_of_its_name() {
    let mut rows: Vec<ExportRow> = Vec::new();
    let named: [(&str, SetFlag); 7] = [
        ("future", |f| f.future = true),
        ("late", |f| f.late = true),
        ("revise", |f| f.revise = true),
        ("host_time", |f| f.host_time = true),
        ("unsynced_clock", |f| f.unsynced_clock = true),
        ("no_value", |f| f.no_value = true),
        ("rounded", |f| f.rounded = true),
    ];
    for (i, (_, set)) in named.iter().enumerate() {
        let mut r = row(1, i as i64, i as u64 + 1, Value::F64(1.0));
        set(&mut r.flags);
        if r.flags.no_value {
            r.value = Value::Absent;
        }
        rows.push(r);
    }
    let file = export_to_vec(&f64_series(1), &rows, &ExportOptions::default()).unwrap();
    for (i, (name, _)) in named.iter().enumerate() {
        let expected: Vec<bool> = (0..named.len()).map(|j| j == i).collect();
        assert_eq!(column_bools(&file, name), expected, "{name}");
    }
}

#[test]
fn quality_columns_hold_the_documented_strings() {
    let qualities = [
        (Quality::Good, "good"),
        (Quality::Estimated, "estimated"),
        (Quality::Suspect, "suspect"),
        (Quality::Bad, "bad"),
    ];
    let rows: Vec<ExportRow> = qualities
        .iter()
        .enumerate()
        .map(|(i, (q, _))| ExportRow {
            quality: *q,
            supplied_quality: qualities[3 - i].0,
            ..row(1, i as i64, i as u64 + 1, Value::F64(1.0))
        })
        .collect();
    let file = export_to_vec(&f64_series(1), &rows, &ExportOptions::default()).unwrap();
    let names: Vec<&str> = qualities.iter().map(|(_, n)| *n).collect();
    let reversed: Vec<&str> = names.iter().rev().copied().collect();
    assert_eq!(column_strings(&file, column::QUALITY), names);
    assert_eq!(column_strings(&file, column::SUPPLIED_QUALITY), reversed);
}

#[test]
fn a_value_against_its_no_value_flag_is_an_error_not_a_null() {
    let mut absent = row(1, 0, 1, Value::Absent);
    absent.flags.no_value = false;
    let mut present = row(1, 0, 1, Value::F64(1.0));
    present.flags.no_value = true;
    for r in [absent, present] {
        let err = export_to_vec(&f64_series(1), &[r], &ExportOptions::default()).unwrap_err();
        assert!(matches!(err, ExportError::NoValueMismatch { .. }), "{err}");
    }
}

#[test]
fn the_parquet_types_alone_carry_the_schema() {
    let series = SeriesSchema {
        changed_at: Some(ChangedAt::Version),
        interval: Some(900_000_000_000),
        kind: KindName::Interval,
        ..f64_series(1)
    };
    let mut r = row(1, 0, 1, Value::F64(1.0));
    r.changed_at = Some(7);
    let file = export_to_vec(&series, &[r], &ExportOptions::default()).unwrap();
    let c = parquet_columns(&file);
    let utc_nanos = Some(LogicalType::timestamp(true, TimeUnit::NANOS));
    let unsigned = |bit_width| Some(LogicalType::integer(bit_width, false));
    for name in [column::T, column::RECEIVED_AT, column::INTERVAL_END] {
        assert_eq!(c[name].0, utc_nanos, "{name}");
        assert_eq!(c[name].1, PhysicalType::INT64, "{name}");
    }
    assert_eq!(c[column::SERIES_ID].0, unsigned(32));
    assert_eq!(c[column::GENERATION].0, unsigned(32));
    assert_eq!(c[column::SEQUENCE].0, unsigned(64));
    assert_eq!(c[column::CHANGED_AT].0, unsigned(64));
    assert_eq!(c[column::VALUE].1, PhysicalType::DOUBLE);
    for name in [
        column::SERIES_NAME,
        column::QUALITY,
        column::SUPPLIED_QUALITY,
    ] {
        assert_eq!(c[name].0, Some(LogicalType::String), "{name}");
    }
    for name in column::FLAGS {
        assert_eq!(c[name].1, PhysicalType::BOOLEAN, "{name}");
    }
    let names: Vec<&str> = c.keys().map(String::as_str).collect();
    assert_eq!(names.len(), 6 + 1 + 2 + 7 + 2);
}

/// A column name and whether it is optional.
type Column = (&'static str, bool);

#[test]
fn columns_come_in_the_documented_order_with_the_documented_nullability() {
    let head = [
        ("series_id", false),
        ("series_name", false),
        ("t", false),
        ("generation", false),
        ("sequence", false),
        ("received_at", false),
    ];
    let tail = [
        ("quality", false),
        ("supplied_quality", false),
        ("future", false),
        ("late", false),
        ("revise", false),
        ("host_time", false),
        ("unsynced_clock", false),
        ("no_value", false),
        ("rounded", false),
    ];
    let cases: [(SeriesSchema, &[Column], &[Column]); 3] = [
        (
            SeriesSchema {
                changed_at: Some(ChangedAt::Version),
                interval: Some(900_000_000_000),
                kind: KindName::Interval,
                ..f64_series(1)
            },
            &[("value", true)],
            &[("changed_at", false), ("interval_end", false)],
        ),
        (
            decimal_series(4),
            &[("value", true), ("stored_scale", true)],
            &[],
        ),
        (
            SeriesSchema {
                value: ValueKind::State(vec!["off".into()]),
                kind: KindName::State,
                changed_at: Some(ChangedAt::Time),
                ..f64_series(1)
            },
            &[("value", true), ("state_name", true)],
            &[("changed_at", false)],
        ),
    ];
    for (series, value, extra) in cases {
        let file = export_to_vec(&series, &[], &ExportOptions::default()).unwrap();
        let expected: Vec<(String, bool)> = head
            .iter()
            .chain(value)
            .chain(&tail)
            .chain(extra)
            .map(|(n, o)| (n.to_string(), *o))
            .collect();
        assert_eq!(column_order(&file), expected, "{:?}", series.value);
    }
}

#[test]
fn every_column_chunk_is_zstd_compressed() {
    let rows = [row(1, 0, 1, Value::F64(1.0))];
    let file = export_to_vec(&f64_series(1), &rows, &ExportOptions::default()).unwrap();
    let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(&file),
    )
    .unwrap();
    let group = b.metadata().row_group(0);
    for c in group.columns() {
        assert!(
            matches!(c.compression(), parquet::basic::Compression::ZSTD(_)),
            "{}",
            c.column_path()
        );
    }
}

#[test]
fn quality_columns_are_dictionary_encoded() {
    let rows: Vec<ExportRow> = (0..64)
        .map(|i| row(1, i, i as u64 + 1, Value::F64(1.0)))
        .collect();
    let file = export_to_vec(&f64_series(1), &rows, &ExportOptions::default()).unwrap();
    let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(&file),
    )
    .unwrap();
    for c in b.metadata().row_group(0).columns() {
        let name = c.column_path().string();
        if name == column::QUALITY || name == column::SUPPLIED_QUALITY {
            assert!(
                c.encodings()
                    .any(|e| e == parquet::basic::Encoding::RLE_DICTIONARY),
                "{name}"
            );
        }
    }
}

#[test]
fn fact_flags_count_matches_the_flag_columns() {
    assert_eq!(size_of::<kvik::FactFlags>(), column::FLAGS.len());
}

#[test]
fn a_decimal_scale_above_eighteen_is_refused() {
    let err = export_to_vec(&decimal_series(19), &[], &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::UnsupportedSeries { .. }),
        "{err}"
    );
}

#[test]
fn a_named_state_is_an_unsigned_16_bit_index_with_its_name() {
    let series = SeriesSchema {
        value: ValueKind::State(vec!["off".into(), "on".into()]),
        kind: KindName::State,
        ..f64_series(1)
    };
    let mut rows = [row(1, 0, 1, Value::State(1)), row(1, 1, 2, Value::Absent)];
    rows[1].flags.no_value = true;
    let file = export_to_vec(&series, &rows, &ExportOptions::default()).unwrap();
    assert_eq!(read_back(&file), rows);
    let c = parquet_columns(&file);
    assert_eq!(c[column::VALUE].0, Some(LogicalType::integer(16, false)));
    assert_eq!(c[column::VALUE].1, PhysicalType::INT32);
    assert_eq!(c[column::STATE_NAME].0, Some(LogicalType::String));
}

#[test]
fn file_metadata_names_the_series_view_and_range() {
    let options = ExportOptions::default()
        .view(View::AsOf(Position::new(2, 40)))
        .range(at(0)..at(60));
    let file = export_to_vec(&f64_series(1), &[], &options).unwrap();
    let f = open(&file);
    for (k, v) in [
        ("kvik.export_version", "1"),
        ("kvik.series_id", "1"),
        ("kvik.series_name", "site/meter/power"),
        ("kvik.unit", "W"),
        ("kvik.kind", "gauge"),
        ("kvik.value_type", "f64"),
        ("kvik.timing", "observed"),
        ("kvik.view", "as_of 2:40"),
    ] {
        assert_eq!(f.metadata(k), Some(v), "{k}");
    }
    assert_eq!(
        f.metadata("kvik.range_from_ns"),
        Some(at(0).0.to_string().as_str())
    );
    assert_eq!(f.metadata("kvik.changed_at_kind"), None);
}

#[test]
fn a_value_of_another_type_is_an_error_not_a_null() {
    let text = SeriesSchema {
        value: ValueKind::Text,
        kind: KindName::Text,
        ..f64_series(1)
    };
    let named = SeriesSchema {
        value: ValueKind::State(vec!["off".into(), "on".into()]),
        kind: KindName::State,
        ..f64_series(1)
    };
    for (series, value) in [
        (f64_series(1), Value::Text("1".into())),
        (f64_series(1), dec(1, 0)),
        (text.clone(), Value::F64(1.0)),
        (named.clone(), Value::State(2)),
        (named, Value::Bool(true)),
        (decimal_series(2), Value::F64(1.0)),
    ] {
        let mut r = row(1, 0, 1, value.clone());
        r.series = series.id;
        let err = export_to_vec(&series, &[r], &ExportOptions::default()).unwrap_err();
        assert!(
            matches!(err, ExportError::WrongValue { .. }),
            "{value:?}: {err}"
        );
    }
}

#[test]
fn changed_at_must_match_the_series_versioning_and_fit_a_timestamp() {
    let time = SeriesSchema {
        changed_at: Some(ChangedAt::Time),
        ..f64_series(1)
    };
    let mut r = row(1, 0, 1, Value::F64(1.0));
    let err =
        export_to_vec(&time, std::slice::from_ref(&r), &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::ChangedAtMismatch { .. }),
        "{err}"
    );
    r.changed_at = Some(i64::MAX as u64 + 1);
    let err =
        export_to_vec(&time, std::slice::from_ref(&r), &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::ChangedAtOutOfRange { changed_at, .. } if changed_at == i64::MAX as u64 + 1),
        "{err}"
    );
    r.changed_at = Some(i64::MAX as u64);
    let file = export_to_vec(&time, std::slice::from_ref(&r), &ExportOptions::default()).unwrap();
    assert_eq!(read_back(&file), vec![r.clone()]);
    let err = export_to_vec(&f64_series(1), &[r], &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::ChangedAtMismatch { .. }),
        "{err}"
    );
}

#[test]
fn a_fact_of_another_series_is_an_error() {
    let err = export_to_vec(
        &f64_series(1),
        &[row(2, 0, 1, Value::F64(1.0))],
        &ExportOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::WrongSeries {
                expected: SeriesId(1),
                found: SeriesId(2),
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn an_interval_end_past_the_timestamp_range_is_an_error() {
    let series = SeriesSchema {
        interval: Some(1),
        kind: KindName::Interval,
        ..f64_series(1)
    };
    let mut r = row(1, 0, 1, Value::F64(1.0));
    r.t = kvik::Timestamp(i64::MAX);
    let err = export_to_vec(&series, &[r], &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::IntervalEndOutOfRange { .. }),
        "{err}"
    );
}

#[test]
fn zero_or_unbounded_rows_per_group_or_batch_is_refused() {
    for options in [
        ExportOptions::default().row_group_rows(0),
        ExportOptions::default().batch_rows(0),
        ExportOptions::default().batch_rows(MAX_BATCH_ROWS + 1),
        ExportOptions::default().batch_rows(usize::MAX),
    ] {
        let err = export_to_vec(&f64_series(1), &[], &options).unwrap_err();
        assert!(matches!(err, ExportError::InvalidOptions { .. }), "{err}");
    }
}

#[test]
fn a_read_error_stops_the_export_and_leaves_a_file_without_a_footer() {
    let rows = [
        Ok(row(1, 0, 1, Value::F64(1.0))),
        Ok(row(1, 1, 2, Value::F64(2.0))),
        Err(ExportError::Query(kvik::QueryError::UnknownSeries)),
    ];
    let options = ExportOptions::default().row_group_rows(1).batch_rows(1);
    let mut out = Vec::new();
    let err = export_rows(&f64_series(1), rows, &options, &mut out).unwrap_err();
    assert!(matches!(err, ExportError::Query(_)), "{err}");
    assert!(out.len() > 4, "a row group was written before the error");
    assert!(!out.ends_with(b"PAR1"));
    assert!(read_rows(bytes::Bytes::from(out)).is_err());
}

fn quality() -> impl Strategy<Value = Quality> {
    prop_oneof![
        Just(Quality::Good),
        Just(Quality::Estimated),
        Just(Quality::Suspect),
        Just(Quality::Bad),
    ]
}

fn arb_row(value: BoxedStrategy<Value>) -> impl Strategy<Value = ExportRow> {
    (
        any::<i64>(),
        any::<(u32, u64)>(),
        any::<i64>(),
        value,
        quality(),
        quality(),
        any::<[bool; 7]>(),
    )
        .prop_map(|(t, (g, s), rx, value, q, sq, f)| {
            let mut flags = Flags::from_array(f);
            flags.no_value = matches!(value, Value::Absent);
            ExportRow {
                series: SeriesId(1),
                t: kvik::Timestamp(t),
                position: Position::new(g, s),
                received_at: kvik::Timestamp(rx),
                value,
                quality: q,
                supplied_quality: sq,
                flags,
                changed_at: None,
            }
        })
}

/// A decimal series' scale and rows stored at that scale or below, each fitting 19 digits at it.
fn decimal_rows() -> impl Strategy<Value = (u8, Vec<ExportRow>)> {
    (0u8..=18).prop_flat_map(|scale| {
        let value = (0..=scale)
            .prop_flat_map(move |s| {
                let room = (10i128.pow(19) - 1) / 10i128.pow(u32::from(scale - s));
                let lim = i64::try_from(room).unwrap_or(i64::MAX);
                (-lim..=lim).prop_map(move |u| dec(u, s))
            })
            .boxed();
        (Just(scale), prop::collection::vec(arb_row(value), 0..20))
    })
}

proptest! {
    #[test]
    fn any_f64_rows_round_trip_bitwise(
        rows in prop::collection::vec(
            arb_row(prop_oneof![
                any::<u64>().prop_map(|b| Value::F64(f64::from_bits(b))),
                Just(Value::Absent),
            ].boxed()),
            0..40,
        ),
        batch in 1usize..8,
        group in 1usize..16,
    ) {
        let options = ExportOptions::default().batch_rows(batch).row_group_rows(group);
        let file = export_to_vec(&f64_series(1), &rows, &options).unwrap();
        prop_assert_eq!(bits(&read_back(&file)), bits(&rows));
        prop_assert_eq!(open(&file).row_groups(), rows.len().div_ceil(group));
    }

    #[test]
    fn any_text_and_decimal_rows_round_trip(
        text in prop::collection::vec(
            arb_row(prop_oneof![
                "\\PC{0,64}".prop_map(Value::Text),
                Just(Value::Absent),
            ].boxed()),
            0..20,
        ),
        (scale, decimals) in decimal_rows(),
    ) {
        let series = SeriesSchema { value: ValueKind::Text, kind: KindName::Text, ..f64_series(1) };
        let file = export_to_vec(&series, &text, &ExportOptions::default()).unwrap();
        prop_assert_eq!(read_back(&file), text);
        let series = SeriesSchema { id: SeriesId(1), ..decimal_series(scale) };
        let file = export_to_vec(&series, &decimals, &ExportOptions::default()).unwrap();
        prop_assert_eq!(read_back(&file), decimals);
    }
}

/// Each leaf column's name and whether it is optional, in file order.
fn column_order(file: &[u8]) -> Vec<(String, bool)> {
    let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(file),
    )
    .unwrap();
    b.parquet_schema()
        .columns()
        .iter()
        .map(|c| {
            let optional = c.self_type().get_basic_info().repetition() == Repetition::OPTIONAL;
            (c.name().to_string(), optional)
        })
        .collect()
}

/// Each leaf column's logical and physical type, by name.
fn parquet_columns(
    file: &[u8],
) -> std::collections::BTreeMap<String, (Option<LogicalType>, PhysicalType)> {
    let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(file),
    )
    .unwrap();
    b.parquet_schema()
        .columns()
        .iter()
        .map(|c| {
            (
                c.name().to_string(),
                (c.logical_type_ref().cloned(), c.physical_type()),
            )
        })
        .collect()
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

/// A boolean column by name, from the Parquet file itself.
fn column_bools(file: &[u8], name: &str) -> Vec<bool> {
    use arrow_array::cast::AsArray;
    batches(file)
        .iter()
        .flat_map(|b| {
            let c = b.column_by_name(name).unwrap().as_boolean();
            (0..c.len()).map(|i| c.value(i)).collect::<Vec<_>>()
        })
        .collect()
}

/// A non-null string column by name, from the Parquet file itself.
fn column_strings(file: &[u8], name: &str) -> Vec<String> {
    use arrow_array::cast::AsArray;
    batches(file)
        .iter()
        .flat_map(|b| {
            b.column_by_name(name)
                .unwrap()
                .as_string::<i32>()
                .iter()
                .map(|s| s.unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}
