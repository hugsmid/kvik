//! The partitioned layout's exports on both backends: one file per UTC month with facts, as-of views, interval
//! starts, option and open failures, and several series from one read (Q-196 as amended, Q-670, Q-1131).

mod common;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{BACKENDS, bits, block_on, open, read_back, store_on};
use kvik::{
    DefineError, IntervalLength, Kind, Reader, Reading, SeriesId, SeriesSpec, TimeRange, Timestamp,
    Value, ValueType, View,
};
use kvik_export::{
    ExportError, ExportOptions, ExportRow, Month, export_partitioned, export_partitioned_of,
    file_name, partition_dir,
};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
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
    partitioned_of(r, &[id], options, false)
}

/// The files of a partitioned export of `ids`, through `export_partitioned_of` when `many`.
fn partitioned_of(
    r: &Reader,
    ids: &[SeriesId],
    options: &ExportOptions,
    many: bool,
) -> Result<Vec<(String, Vec<u8>)>, ExportError> {
    let files: Files = Arc::default();
    let sink = files.clone();
    let open = move |path: &str| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        sink.lock().unwrap().push((path.to_string(), buf.clone()));
        Ok(Shared(buf))
    };
    let written = if many {
        export_partitioned_of(r, ids, options, open)?
    } else {
        export_partitioned(r, ids[0], options, open)?
    };
    let files = files.lock().unwrap();
    let mut by_path: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
    if many {
        by_path.sort_by_key(|p| written.iter().position(|w| &w.path == p));
    }
    assert_eq!(
        written.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        by_path
    );
    Ok(written
        .iter()
        .map(|w| {
            let b = &files.iter().find(|f| f.0 == w.path).unwrap().1;
            (w.path.clone(), b.lock().unwrap().clone())
        })
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

#[test]
fn a_partitioned_export_of_several_series_writes_each_series_files_from_one_read_on_both_backends()
{
    for backend in BACKENDS {
        let (store, _clock) = store_on(backend, "partitioned-many");
        let gauge = |name: &str| {
            SeriesSpec::new(
                name,
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            )
        };
        let a = block_on(store.define_series(gauge("plant/line-1/power"))).unwrap();
        let b = block_on(store.define_series(gauge("plant/line-2/power"))).unwrap();
        let readings: Vec<Reading> = [
            (a, "2026-01-05T12:00:00Z"),
            (b, "2026-01-07T12:00:00Z"),
            (a, "2026-02-01T00:00:00Z"),
            (b, "2026-03-03T00:00:00Z"),
            (a, "2026-03-04T00:00:00Z"),
            (b, "2026-03-31T23:00:00Z"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (id, t))| Reading::new(*id, ts(t), Value::F64(i as f64)))
        .collect();
        assert_eq!(block_on(store.emit(&readings)).unwrap().recorded(), 6);
        let r = store.reader();
        let options = ExportOptions::default()
            .site("site-1")
            .range(ts("2026-01-06T00:00:00Z")..ts("2026-12-01T00:00:00Z"));
        let many = partitioned_of(&r, &[b, a, b], &options, true).unwrap();
        let mut want = Vec::new();
        let (fa, fb) = (
            partitioned(&r, a, &options).unwrap(),
            partitioned(&r, b, &options).unwrap(),
        );
        for month in ["2026-01", "2026-02", "2026-03"] {
            for f in fb.iter().chain(&fa) {
                if f.0.contains(&format!("month={month}/")) {
                    want.push(f.clone());
                }
            }
        }
        assert_eq!(
            many.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            want.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            "{backend:?}"
        );
        for (got, want) in many.iter().zip(&want) {
            assert_eq!(
                bits(&read_back(&got.1)),
                bits(&read_back(&want.1)),
                "{backend:?} {}",
                got.0
            );
        }
        let read = Arc::new(Mutex::new(false));
        let opened = read.clone();
        let err = export_partitioned_of(&r, &[a, SeriesId(9_999)], &options, move |_: &str| {
            *opened.lock().unwrap() = true;
            Ok(Vec::new())
        });
        assert!(err.is_err(), "{backend:?}");
        assert!(
            !*read.lock().unwrap(),
            "{backend:?}: nothing is opened before every series is checked"
        );
    }
}
