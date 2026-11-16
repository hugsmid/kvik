//! Site and store identity in the metadata, and the partitioned layout: site labels, the series-name escaping,
//! UTC month cuts and one file per month on both backends (Q-196).

mod common;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{BACKENDS, bits, block_on, export_to_vec, f64_series, open, read_back, store_on};
use kvik::{
    IntervalLength, Kind, Position, Reader, Reading, SeriesId, SeriesSpec, TimeRange, Timestamp,
    Value, ValueType, View,
};
use kvik_export::{
    ExportError, ExportOptions, ExportRow, MAX_SITE_LEN, Month, check_site, escape_segment,
    export_partitioned, file_name, partition_dir, unescape_segment,
};
use proptest::prelude::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

#[test]
fn a_site_label_is_lower_case_letters_digits_hyphens_and_underscores() {
    let long = "a".repeat(MAX_SITE_LEN);
    for ok in ["a", "0", "site-1", "north_2", "a-", long.as_str()] {
        assert!(check_site(ok).is_ok(), "{ok:?}");
    }
    let too_long = "a".repeat(MAX_SITE_LEN + 1);
    for bad in [
        "",
        "-a",
        "_a",
        "Site",
        "a.b",
        "..",
        "a/b",
        "a\\b",
        "a b",
        "a=b",
        "a%41",
        "sité",
        too_long.as_str(),
    ] {
        assert!(
            matches!(check_site(bad), Err(ExportError::InvalidOptions { .. })),
            "{bad:?}"
        );
        let err = export_to_vec(&f64_series(1), &[], &ExportOptions::default().site(bad));
        assert!(
            matches!(err, Err(ExportError::InvalidOptions { .. })),
            "{bad:?}"
        );
    }
}

#[test]
fn site_and_store_id_are_in_the_metadata_when_given() {
    let mut id = [0u8; 16];
    id[0] = 0xAB;
    id[15] = 0x07;
    let options = ExportOptions::default().site("site-1").store_id(id);
    let file = export_to_vec(&f64_series(1), &[], &options).unwrap();
    let f = open(&file);
    assert_eq!(f.metadata("kvik.site"), Some("site-1"));
    assert_eq!(
        f.metadata("kvik.store_id"),
        Some("ab000000000000000000000000000007")
    );
    assert_eq!(f.metadata("kvik.export_version"), Some("1"));
    let plain = export_to_vec(&f64_series(1), &[], &ExportOptions::default()).unwrap();
    let f = open(&plain);
    assert_eq!(f.metadata("kvik.site"), None);
    assert_eq!(f.metadata("kvik.store_id"), None);
}

#[test]
fn series_names_escape_to_one_case_safe_segment() {
    for (name, segment) in [
        ("plant/line-1/temperature", "plant%2Fline-1%2Ftemperature"),
        ("a_b-c9", "a_b-c9"),
        ("A", "%41"),
        ("a.b", "a%2Eb"),
        ("%", "%25"),
        ("=", "%3D"),
        (" ", "%20"),
        ("å", "%C3%A5"),
        ("", ""),
    ] {
        assert_eq!(escape_segment(name), segment, "{name:?}");
        assert_eq!(
            unescape_segment(segment).as_deref(),
            Some(name),
            "{segment:?}"
        );
    }
    for never in ["%2f", "%41%", "%4", "%61", "a.b", "A", "%C3", "%zz"] {
        assert_eq!(unescape_segment(never), None, "{never:?}");
    }
}

/// The escape with every letter lower-cased and the hex digits after `%` upper-cased again: what a file system
/// that folds case gives back.
fn fold(segment: &str) -> String {
    let lower = segment.to_ascii_lowercase();
    let mut out = String::new();
    let mut hex = 0;
    for c in lower.chars() {
        if hex > 0 {
            out.push(c.to_ascii_uppercase());
            hex -= 1;
        } else {
            if c == '%' {
                hex = 2;
            }
            out.push(c);
        }
    }
    out
}

proptest! {
    #[test]
    fn any_name_round_trips_through_its_segment(name in any::<String>()) {
        let segment = escape_segment(&name);
        prop_assert!(segment
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_%ABCDEF".contains(&c)));
        prop_assert_eq!(unescape_segment(&segment), Some(name.clone()));
        prop_assert_eq!(unescape_segment(&fold(&segment)), Some(name));
    }

    #[test]
    fn a_segment_that_unescapes_is_the_escape_of_its_name(segment in "[a-z0-9_%A-Fa-f.-]{0,24}") {
        if let Some(name) = unescape_segment(&segment) {
            prop_assert_eq!(escape_segment(&name), segment);
        }
    }

    #[test]
    fn a_month_holds_its_timestamps_and_starts_where_the_last_ends(t in any::<i64>()) {
        let m = Month::of(Timestamp(t));
        let r = m.range();
        prop_assert!(r.from.0 <= t);
        prop_assert!(t < r.to.0 || (t == i64::MAX && r.to.0 == i64::MAX));
        prop_assert_eq!(Month::of(r.from), m);
        prop_assert_eq!(Month::of(Timestamp(r.to.0 - 1)), m);
        prop_assert_eq!((m.year(), m.month()), civil_month(t));
        if r.from.0 > i64::MIN {
            let before = Month::of(Timestamp(r.from.0 - 1));
            prop_assert!(before < m);
            prop_assert_eq!(before.range().to, r.from);
        }
    }
}

/// The UTC year and month of `t` by days-from-civil arithmetic, independent of jiff.
fn civil_month(t: i64) -> (i16, i8) {
    let days = t.div_euclid(86_400_000_000_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i16, month as i8)
}

#[test]
fn months_cut_at_utc_midnight_on_the_first() {
    for (t, month) in [
        ("2026-01-31T23:59:59.999999999Z", "2026-01"),
        ("2026-02-01T00:00:00Z", "2026-02"),
        ("2024-02-29T23:59:59.999999999Z", "2024-02"),
        ("2024-03-01T00:00:00Z", "2024-03"),
        ("2025-12-31T23:59:59.999999999Z", "2025-12"),
        ("2026-01-01T00:00:00Z", "2026-01"),
        ("1969-12-31T23:59:59.999999999Z", "1969-12"),
        ("1970-01-01T00:00:00Z", "1970-01"),
        ("2026-03-29T00:30:00+01:00", "2026-03"),
        ("2026-04-01T00:30:00+02:00", "2026-03"),
    ] {
        assert_eq!(Month::of(ts(t)).to_string(), month, "{t}");
    }
    let feb = Month::of(ts("2026-02-15T12:00:00Z")).range();
    assert_eq!(
        (feb.from, feb.to),
        (ts("2026-02-01T00:00:00Z"), ts("2026-03-01T00:00:00Z"))
    );
    let first = Month::of(Timestamp(i64::MIN));
    assert_eq!(first.to_string(), "1677-09");
    assert_eq!(first.range().from, Timestamp(i64::MIN));
    let last = Month::of(Timestamp(i64::MAX));
    assert_eq!(last.to_string(), "2262-04");
    assert_eq!(last.range().to, Timestamp(i64::MAX));
}

#[test]
fn file_names_follow_the_view() {
    assert_eq!(file_name(View::Current).unwrap(), "current.parquet");
    assert_eq!(file_name(View::History).unwrap(), "history.parquet");
    assert_eq!(
        file_name(View::AsOf(Position::new(2, 40))).unwrap(),
        "as_of-2-40.parquet"
    );
    assert_eq!(
        file_name(View::AsOfTime(Timestamp(-5))).unwrap(),
        "as_of_time--5.parquet"
    );
    assert_eq!(
        partition_dir(
            "site-1",
            "plant/line-1/power",
            Month::of(ts("2026-02-01T00:00:00Z"))
        ),
        "site=site-1/series=plant%2Fline-1%2Fpower/month=2026-02"
    );
}

/// Files written to memory by path.
type Files = Arc<Mutex<Vec<(String, Arc<Mutex<Vec<u8>>>)>>>;

struct Shared(Arc<Mutex<Vec<u8>>>);

impl io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn partitioned(
    r: &Reader,
    id: SeriesId,
    options: &ExportOptions,
) -> Result<Vec<(String, Vec<u8>)>, ExportError> {
    let files: Files = Arc::default();
    let sink = files.clone();
    let written = export_partitioned(r, id, options, move |path: &str| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        sink.lock().unwrap().push((path.to_string(), buf.clone()));
        Ok(Shared(buf))
    })?;
    let files = files.lock().unwrap();
    assert_eq!(
        written.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        files.iter().map(|f| f.0.clone()).collect::<Vec<_>>()
    );
    Ok(files
        .iter()
        .map(|(p, b)| (p.clone(), b.lock().unwrap().clone()))
        .collect())
}

fn facts_in(r: &Reader, id: SeriesId, range: TimeRange, view: View) -> Vec<ExportRow> {
    r.facts(id, range, view)
        .unwrap()
        .map(|f| ExportRow::from(f.unwrap()))
        .collect()
}

#[test]
fn a_partitioned_export_writes_one_file_per_utc_month_with_facts_on_both_backends() {
    for backend in BACKENDS {
        let (store, _clock) = store_on(backend, "partitioned");
        let spec = SeriesSpec::new(
            "plant/line-1/Power",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        )
        .expected_interval(Duration::from_secs(1));
        let id = block_on(store.define_series(spec)).unwrap();
        let times = [
            "2026-01-05T12:00:00Z",
            "2026-01-31T23:59:59.999999999Z",
            "2026-02-01T00:00:00Z",
            "2026-02-28T23:00:00Z",
            "2026-04-01T00:00:00Z",
        ];
        let readings: Vec<Reading> = times
            .iter()
            .enumerate()
            .map(|(i, t)| Reading::new(id, ts(t), Value::F64(i as f64)))
            .collect();
        assert_eq!(block_on(store.emit(&readings)).unwrap().recorded(), 5);
        let r = store.reader();
        let store_id = [7u8; 16];
        let options = ExportOptions::default().site("site-1").store_id(store_id);
        let files = partitioned(&r, id, &options).unwrap();
        let dir = "site=site-1/series=plant%2Fline-1%2F%50ower";
        assert_eq!(
            files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            vec![
                format!("{dir}/month=2026-01/current.parquet"),
                format!("{dir}/month=2026-02/current.parquet"),
                format!("{dir}/month=2026-04/current.parquet"),
            ],
            "{backend:?}"
        );
        for ((_, file), month) in files.iter().zip(["2026-01", "2026-02", "2026-04"]) {
            let range = Month::of(ts(&format!("{month}-01T00:00:00Z"))).range();
            let expected = facts_in(&r, id, range, View::Current);
            assert_eq!(
                bits(&read_back(file)),
                bits(&expected),
                "{backend:?} {month}"
            );
            let f = open(file);
            assert_eq!(
                f.metadata("kvik.range_from_ns"),
                Some(range.from.0.to_string().as_str())
            );
            assert_eq!(
                f.metadata("kvik.range_to_ns"),
                Some(range.to.0.to_string().as_str())
            );
            assert_eq!(f.metadata("kvik.site"), Some("site-1"));
            assert_eq!(f.metadata("kvik.store_id"), Some("07".repeat(16).as_str()));
            assert_eq!(f.metadata("kvik.series_name"), Some("plant/line-1/Power"));
        }
        assert_eq!(read_back(&files[0].1).len(), 2);
        assert_eq!(read_back(&files[1].1).len(), 2);

        let clipped = options
            .clone()
            .view(View::History)
            .range(ts("2026-01-20T00:00:00Z")..ts("2026-02-10T00:00:00Z"));
        let files = partitioned(&r, id, &clipped).unwrap();
        assert_eq!(files.len(), 2, "{backend:?}");
        assert!(files[0].0.ends_with("month=2026-01/history.parquet"));
        let f = open(&files[0].1);
        assert_eq!(
            f.metadata("kvik.range_from_ns"),
            Some(ts("2026-01-20T00:00:00Z").0.to_string().as_str())
        );
        let f = open(&files[1].1);
        assert_eq!(
            f.metadata("kvik.range_to_ns"),
            Some(ts("2026-02-10T00:00:00Z").0.to_string().as_str())
        );
        assert_eq!(read_back(&files[0].1).len(), 1);
        assert_eq!(read_back(&files[1].1).len(), 1);
    }
}

#[test]
fn an_interval_is_filed_under_the_utc_month_of_its_start() {
    for backend in BACKENDS {
        let (store, _clock) = store_on(backend, "partitioned-interval");
        let spec = SeriesSpec::new(
            "grid/import/energy",
            Kind::Interval {
                value: ValueType::F64,
                length: IntervalLength::Fixed {
                    d: Duration::from_secs(1_800),
                    phase: Duration::ZERO,
                },
            },
            "kWh",
        );
        let id = block_on(store.define_series(spec)).unwrap();
        let readings = [
            Reading::new(id, ts("2026-01-31T23:30:00Z"), Value::F64(1.0)),
            Reading::new(id, ts("2026-02-01T00:00:00Z"), Value::F64(2.0)),
        ];
        assert_eq!(block_on(store.emit(&readings)).unwrap().recorded(), 2);
        let files = partitioned(&store.reader(), id, &ExportOptions::default().site("s")).unwrap();
        assert_eq!(files.len(), 2, "{backend:?}");
        assert!(files[0].0.contains("/month=2026-01/"));
        assert_eq!(read_back(&files[0].1)[0].t, ts("2026-01-31T23:30:00Z"));
        assert!(files[1].0.contains("/month=2026-02/"));
    }
}

#[test]
fn a_partitioned_export_needs_a_valid_site_and_reports_a_file_it_cannot_open() {
    let (store, _clock) = store_on(BACKENDS[0], "partitioned-errors");
    let spec = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let id = block_on(store.define_series(spec)).unwrap();
    block_on(store.emit(&[Reading::new(
        id,
        ts("2026-01-05T12:00:00Z"),
        Value::F64(1.0),
    )]))
    .unwrap();
    let r = store.reader();
    for options in [ExportOptions::default(), ExportOptions::default().site("A")] {
        let err = partitioned(&r, id, &options).unwrap_err();
        assert!(matches!(err, ExportError::InvalidOptions { .. }), "{err}");
    }
    let err = export_partitioned(
        &r,
        id,
        &ExportOptions::default().site("s"),
        |_: &str| -> io::Result<Vec<u8>> { Err(io::Error::other("full")) },
    )
    .unwrap_err();
    assert!(
        matches!(&err, ExportError::Open { path, .. } if path == "site=s/series=a%2Fb%2Fc/month=2026-01/current.parquet"),
        "{err}"
    );
    let empty = r.facts(
        id,
        TimeRange::new(Timestamp(0), Timestamp(1)),
        View::Current,
    );
    assert_eq!(empty.unwrap().count(), 0);
    let none = partitioned(
        &r,
        id,
        &ExportOptions::default()
            .site("s")
            .range(Timestamp(0)..Timestamp(1)),
    )
    .unwrap();
    assert!(none.is_empty());
}
