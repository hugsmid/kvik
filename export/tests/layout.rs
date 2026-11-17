//! Site and store identity in the metadata, and the partitioned layout: site labels, the split of a series name
//! into area, device and metric, the per-segment escaping, UTC month cuts and one file per month on both backends
//! (Q-196 as amended, Q-670).

mod common;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{BACKENDS, bits, block_on, export_to_vec, f64_series, open, read_back, store_on};
use kvik::{
    DefineError, IntervalLength, Kind, Position, Reader, Reading, SeriesId, SeriesSpec, TimeRange,
    Timestamp, Value, ValueType, View,
};
use kvik_export::{
    ExportError, ExportOptions, ExportRow, MAX_SITE_LEN, Month, check_site, escape_segment,
    export_partitioned, file_name, name_segments, partial_file_name, partition_dir,
    unescape_segment,
};
use proptest::prelude::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

#[test]
fn a_site_label_is_a_lower_case_letter_then_letters_digits_hyphens_and_underscores() {
    let long = "a".repeat(MAX_SITE_LEN);
    for ok in ["a", "a0", "site-1", "north_2", "a-", long.as_str()] {
        assert!(check_site(ok).is_ok(), "{ok:?}");
    }
    let too_long = "a".repeat(MAX_SITE_LEN + 1);
    for bad in [
        "",
        "0",
        "0042",
        "9a",
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
fn name_segments_escape_to_case_safe_path_segments() {
    for (name, segment) in [
        ("Temperature", "%54emperature"),
        ("0042", "0042"),
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
    fn each_name_segment_round_trips_through_its_own_path_segment(
        segments in prop::array::uniform3("[^/]{1,12}"),
    ) {
        let name = segments.join("/");
        prop_assert_eq!(name_segments(&name), Some([segments[0].as_str(), segments[1].as_str(), segments[2].as_str()]));
        let dir = partition_dir("s", &name, Month::of(Timestamp(0))).unwrap();
        let parts: Vec<&str> = dir.split('/').collect();
        prop_assert_eq!(parts.len(), 5, "{}", dir);
        prop_assert_eq!(parts[0], "site=s");
        prop_assert_eq!(parts[4], "month=1970-01");
        for ((part, key), segment) in parts[1..4].iter().zip(["area=", "device=", "metric="]).zip(&segments) {
            let escaped = part.strip_prefix(key).unwrap();
            prop_assert_eq!(escaped, escape_segment(segment));
            prop_assert_eq!(unescape_segment(escaped), Some(segment.clone()));
            prop_assert_eq!(unescape_segment(&fold(escaped)), Some(segment.clone()));
        }
    }

    #[test]
    fn a_name_of_other_than_three_segments_or_with_an_empty_one_is_refused(name in misshapen_name()) {
        prop_assert_eq!(name_segments(&name), None, "{:?}", name);
        let refused = matches!(
            partition_dir("s", &name, Month::of(Timestamp(0))),
            Err(ExportError::UnsupportedSeries { .. })
        );
        prop_assert!(refused, "{:?}", name);
    }

    #[test]
    fn a_segment_built_from_valid_tokens_is_the_escape_of_its_name((segment, name) in valid_segment()) {
        prop_assert_eq!(unescape_segment(&segment), Some(name.clone()));
        prop_assert_eq!(escape_segment(&name), segment);
    }

    #[test]
    fn a_segment_with_one_invalid_token_does_not_unescape(
        (tokens, at, bad) in (valid_tokens(), any::<prop::sample::Index>(), prop::sample::select(INVALID.to_vec())),
    ) {
        let mut tokens: Vec<String> = tokens.into_iter().map(|(t, _)| t).collect();
        // A cut escape is only invalid at the end: before a digit token it would complete itself.
        let at = if bad.starts_with('%') && bad.len() < 3 { tokens.len() } else { at.index(tokens.len() + 1) };
        tokens.insert(at, bad.to_string());
        let broken = tokens.concat();
        prop_assert_eq!(unescape_segment(&broken), None, "{}", broken);
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

/// A name of one, two, four or five segments, or of three with at least one empty.
fn misshapen_name() -> impl Strategy<Value = String> {
    let other_count = prop::sample::select(vec![1usize, 2, 4, 5])
        .prop_flat_map(|n| prop::collection::vec("[^/]{0,6}", n));
    let three_with_an_empty =
        (prop::array::uniform3("[^/]{0,6}"), 1u8..8).prop_map(|(mut s, empties)| {
            for (i, segment) in s.iter_mut().enumerate() {
                if empties & (1 << i) != 0 {
                    segment.clear();
                }
            }
            s.to_vec()
        });
    prop_oneof![other_count, three_with_an_empty].prop_map(|s| s.join("/"))
}

/// Tokens no escape contains: a byte that is escaped, lower-case hex, an escape of a kept byte, a cut escape, a
/// bare `%` at the end, and an escape that starts invalid UTF-8.
const INVALID: [&str; 7] = [".", "A", "%2f", "%61", "%4", "%", "%FF"];

/// Tokens of a canonical segment (a kept byte as itself, any other character as the `%XX` of each of its UTF-8
/// bytes), each with the part of the name it stands for.
fn valid_tokens() -> impl Strategy<Value = Vec<(String, String)>> {
    prop::collection::vec(
        prop_oneof![
            prop::sample::select(b"abcxyz0189-_".to_vec()).prop_map(char::from),
            any::<char>(),
        ],
        0..24,
    )
    .prop_map(|chars| {
        chars
            .into_iter()
            .map(|c| {
                let mut buf = [0u8; 4];
                let mut token = String::new();
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' {
                        token.push(char::from(b));
                    } else {
                        token.push_str(&format!("%{b:02X}"));
                    }
                }
                (token, c.to_string())
            })
            .collect()
    })
}

/// A canonical segment built from [`valid_tokens`], with the name it stands for.
fn valid_segment() -> impl Strategy<Value = (String, String)> {
    valid_tokens().prop_map(|tokens| {
        let segment = tokens.iter().map(|(t, _)| t.as_str()).collect();
        let name = tokens.iter().map(|(_, n)| n.as_str()).collect();
        (segment, name)
    })
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
        partial_file_name(View::Current, TimeRange::new(Timestamp(-5), Timestamp(7))).unwrap(),
        "current.partial--5_7.parquet"
    );
}

#[test]
fn a_partition_folder_splits_the_name_into_area_device_and_metric_each_escaped_alone() {
    let feb = Month::of(ts("2026-02-01T00:00:00Z"));
    for (name, dir) in [
        (
            "plant/line-1/power",
            "site=site-1/area=plant/device=line-1/metric=power/month=2026-02",
        ),
        (
            "Hall A/Pump.2/flow %",
            "site=site-1/area=%48all%20%41/device=%50ump%2E2/metric=flow%20%25/month=2026-02",
        ),
        (
            "0042/å/=",
            "site=site-1/area=0042/device=%C3%A5/metric=%3D/month=2026-02",
        ),
        (
            " / / ",
            "site=site-1/area=%20/device=%20/metric=%20/month=2026-02",
        ),
    ] {
        assert_eq!(partition_dir("site-1", name, feb).unwrap(), dir, "{name:?}");
    }
}

#[test]
fn a_name_without_exactly_three_non_empty_segments_has_no_partition_folder() {
    let feb = Month::of(ts("2026-02-01T00:00:00Z"));
    assert_eq!(name_segments("a/b/c"), Some(["a", "b", "c"]));
    for bad in [
        "", "a", "a/b", "a/b/c/d", "a/b/c/", "/a/b/c", "/b/c", "a//c", "a/b/", "//", "///",
        "a/b//c",
    ] {
        assert_eq!(name_segments(bad), None, "{bad:?}");
        assert!(
            matches!(
                partition_dir("s", bad, feb),
                Err(ExportError::UnsupportedSeries { .. })
            ),
            "{bad:?}"
        );
    }
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
        let hex: String = r.store_id().iter().map(|b| format!("{b:02x}")).collect();
        let options = ExportOptions::default().site("site-1");
        let files = partitioned(&r, id, &options).unwrap();
        let dir = "site=site-1/area=plant/device=line-1/metric=%50ower";
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
            assert_eq!(f.metadata("kvik.store_id"), Some(hex.as_str()));
            assert_eq!(f.metadata("kvik.series_name"), Some("plant/line-1/Power"));
        }
        assert_eq!(read_back(&files[0].1).len(), 2);
        assert_eq!(read_back(&files[1].1).len(), 2);

        let clipped = options
            .clone()
            .view(View::History)
            .range(ts("2026-01-20T00:00:00Z")..ts("2026-02-10T00:00:00Z"));
        let files = partitioned(&r, id, &clipped).unwrap();
        let (from, to) = (ts("2026-01-20T00:00:00Z").0, ts("2026-02-10T00:00:00Z").0);
        let (feb, mar) = (ts("2026-02-01T00:00:00Z").0, ts("2026-03-01T00:00:00Z").0);
        assert_eq!(
            files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            vec![
                format!("{dir}/month=2026-01/history.partial-{from}_{feb}.parquet"),
                format!("{dir}/month=2026-02/history.partial-{feb}_{to}.parquet"),
            ],
            "{backend:?}"
        );
        let to_march = options.clone().range(Timestamp(from)..Timestamp(mar));
        assert_eq!(
            partitioned(&r, id, &to_march)
                .unwrap()
                .iter()
                .map(|f| f.0.as_str())
                .collect::<Vec<_>>(),
            vec![
                format!("{dir}/month=2026-01/current.partial-{from}_{feb}.parquet"),
                format!("{dir}/month=2026-02/current.parquet"),
            ],
            "{backend:?}"
        );
        let other = options.clone().store_id([0xEE; 16]);
        assert!(matches!(
            partitioned(&r, id, &other),
            Err(ExportError::InvalidOptions { .. })
        ));
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
fn a_partitioned_export_as_of_a_point_writes_only_the_months_with_a_fact_then_on_both_backends() {
    for backend in BACKENDS {
        let (store, clock) = store_on(backend, "partitioned-as-of");
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let id = block_on(store.define_series(spec)).unwrap();
        let january = block_on(store.emit(&[Reading::new(
            id,
            ts("2026-01-10T00:00:00Z"),
            Value::F64(1.0),
        )]))
        .unwrap();
        let point = january.last().unwrap();
        let between = Timestamp(clock.wall().0 + 5_000_000_000);
        clock.advance(Duration::from_secs(10));
        block_on(store.emit(&[Reading::new(
            id,
            ts("2026-03-10T00:00:00Z"),
            Value::F64(2.0),
        )]))
        .unwrap();
        let r = store.reader();
        let options = ExportOptions::default().site("s");
        assert_eq!(
            partitioned(&r, id, &options).unwrap().len(),
            2,
            "{backend:?}"
        );
        for view in [View::AsOf(point), View::AsOfTime(between)] {
            let files = partitioned(&r, id, &options.clone().view(view)).unwrap();
            let name = file_name(view).unwrap();
            assert_eq!(
                files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
                vec![format!(
                    "site=s/area=a/device=b/metric=c/month=2026-01/{name}"
                )],
                "{backend:?} {view:?}"
            );
            assert_eq!(read_back(&files[0].1).len(), 1);
        }
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
        matches!(&err, ExportError::Open { path, .. } if path == "site=s/area=a/device=b/metric=c/month=2026-01/current.parquet"),
        "{err}"
    );
    block_on(store.emit(&[Reading::new(
        id,
        ts("2026-02-05T12:00:00Z"),
        Value::F64(2.0),
    )]))
    .unwrap();
    let files: Files = Arc::default();
    let sink = files.clone();
    let err = export_partitioned(
        &r,
        id,
        &ExportOptions::default().site("s"),
        move |path: &str| {
            if path.contains("month=2026-02") {
                return Err(io::Error::other("full"));
            }
            let buf = Arc::new(Mutex::new(Vec::new()));
            sink.lock().unwrap().push((path.to_string(), buf.clone()));
            Ok(Shared(buf))
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, ExportError::Open { path, .. } if path.contains("month=2026-02")),
        "{err}"
    );
    let first = files.lock().unwrap()[0].1.lock().unwrap().clone();
    assert_eq!(
        read_back(&first).len(),
        1,
        "the file before the failure is complete"
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

#[test]
fn a_name_without_three_non_empty_segments_is_refused_at_definition_and_by_the_partitioned_layout_on_both_backends()
 {
    let names = [
        "power",
        "line-1/power",
        "plant/line-1/power/peak",
        "plant//power",
        "plant/line-1/",
    ];
    for backend in BACKENDS {
        let (store, _clock) = store_on(backend, "partitioned-shape");
        for name in names {
            let spec = SeriesSpec::new(
                name,
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            );
            assert!(
                matches!(
                    block_on(store.define_series(spec)),
                    Err(DefineError::Invalid { field: "name", .. })
                ),
                "{backend:?} {name:?}"
            );
        }
    }
    // A log written before Q-670 (a) may still hold such a name; the layout refuses it before any file is named.
    let month = Month::of(ts("2026-01-05T12:00:00Z"));
    for name in names {
        let err = partition_dir("s", name, month).unwrap_err();
        assert!(
            matches!(err, ExportError::UnsupportedSeries { .. }),
            "{name:?}: {err}"
        );
        assert!(
            err.to_string().contains("three non-empty segments"),
            "{err}"
        );
    }
}
