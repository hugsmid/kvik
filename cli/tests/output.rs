//! The tool's output, pinned (the query design's section 6).

mod common;

use std::time::Duration;

use common::{Fixture, S, SEGMENT, T0, block_on};
use kvik::{Kind, Reading, SeriesSpec, Timestamp, Value, ValueType};
use kvik_log::directory::status::{StatusFile, counter, health, reason};
use kvik_log::directory::{self, Family, Frame};

#[test]
fn series_lists_names_kinds_units_aliases_and_latest_times() {
    let f = Fixture::new();
    insta::assert_snapshot!(f.run("series").unwrap());
    insta::assert_snapshot!(
        "series_pattern_csv",
        f.run("series site/hall/* --format csv").unwrap()
    );
}

#[test]
fn latest_shows_each_series_live_fact_at_one_cut_in_every_format() {
    let f = Fixture::new();
    insta::assert_snapshot!(f.run("latest").unwrap());
    insta::assert_snapshot!("latest_csv", f.run("latest --format csv").unwrap());
    insta::assert_snapshot!("latest_jsonl", f.run("latest --format jsonl").unwrap());
    insta::assert_snapshot!(
        "latest_zone",
        f.run("latest site/meter/* --zone Etc/GMT-2").unwrap()
    );
}

#[test]
fn latest_live_opens_with_the_static_answer() {
    let f = Fixture::new();
    insta::assert_snapshot!(f.run("latest --live --for 0ms --format csv").unwrap());
}

#[test]
fn tail_shows_facts_as_they_become_durable() {
    let mut f = Fixture::new();
    let env = f.env.clone();
    let dir = f.dir.clone();
    let run = std::thread::spawn(move || {
        let args: Vec<String> = "tail --format jsonl --interval 20ms --for 3s site/meter/*"
            .split_whitespace()
            .map(String::from)
            .collect();
        let mut out = Vec::new();
        kvik_cli::run(&args, Some(dir), env, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    });
    std::thread::sleep(Duration::from_millis(500));
    for k in 0..3 {
        f.emit(&[
            Reading::new(f.ids[0], f.at(0), Value::F64(430.0 + f64::from(k))),
            Reading::new(f.ids[3], f.at(0), Value::Text("not selected".into())),
        ]);
        f.clock.advance(Duration::from_millis(500));
        std::thread::sleep(Duration::from_millis(400));
    }
    let out = run.join().unwrap();
    let facts: Vec<String> = out
        .lines()
        .filter(|l| l.contains("\"event\":\"fact\""))
        .map(|l| l.split_once(',').map(|(_, rest)| rest.to_owned()).unwrap())
        .collect();
    insta::assert_snapshot!(facts.join("\n"));
    assert!(
        out.lines().next().unwrap().contains("\"event\":\"opened\""),
        "{out}"
    );
}

/// Output without what the writer's own timing decides: its status file's content and size, and the bytes held.
fn steady(out: &str) -> String {
    assert!(
        out.lines()
            .any(|l| l.starts_with("status ") && l.contains(" health "))
            || out.lines().any(|l| l.contains("derived/*.stat")),
        "{out}"
    );
    out.lines()
        .filter(|l| {
            !l.starts_with("status ") && !l.contains("local_bytes") && !l.contains("*.stat")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn health_reports_position_losses_segments_leases_and_derived_files() {
    let mut f = Fixture::new();
    f.await_status();
    f.await_leases(3);
    f.await_segments();
    insta::assert_snapshot!("health_open", steady(&f.run("health").unwrap()));
    f.shut();
    insta::assert_snapshot!("health_shut", steady(&f.run("health").unwrap()));
}

#[test]
fn info_reports_the_descriptor_zone_tzdata_and_files_by_kind() {
    let mut f = Fixture::new();
    f.shut();
    insta::assert_snapshot!(steady(&f.run("info").unwrap()));
}

#[test]
fn a_store_that_is_not_there_is_an_error_not_an_empty_answer() {
    let f = Fixture::new();
    let v: Vec<String> = ["series", "--store", "elsewhere"].map(String::from).into();
    let mut out = Vec::new();
    let e = kvik_cli::run(&v, None, f.env.clone(), &mut out).unwrap_err();
    assert!(matches!(e, kvik_cli::Error::Open(_)), "{e}");
    assert!(out.is_empty());
}

/// `out` without the bytes held, which the writer's file sizes decide.
fn without_bytes(out: &str) -> String {
    out.lines()
        .filter(|l| !l.contains("local_bytes"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_loss_found_then_recorded_degrades_health_and_fails_static_answers_after_writing_them() {
    let mut f = Fixture::new();
    f.damage(1_204.25);
    f.await_status();
    f.await_segments();
    insta::assert_snapshot!("health_found_loss", steady(&f.run("health").unwrap()));
    for cmd in ["latest", "series"] {
        let (out, done) = f.run_both(cmd, f.env.clone());
        let e = done.unwrap_err();
        assert!(matches!(e, kvik_cli::Error::Losses(_)), "{e}");
        insta::assert_snapshot!(format!("{cmd}_with_a_loss"), format!("{out}\nerror: {e}"));
    }
    // The store's own process finds it too; its next start records it (spec §3 Recovery rule 2).
    f.store.as_ref().unwrap().reader().snapshot().unwrap();
    f.shut();
    f.reopen();
    f.await_segments();
    f.shut();
    insta::assert_snapshot!("health_recorded_loss", steady(&f.run("health").unwrap()));
}

/// The `log bytes` figure of a `health` answer.
fn log_bytes(out: &str) -> u64 {
    let line = out
        .lines()
        .find(|l| l.starts_with("log") && l.contains(" bytes "))
        .unwrap();
    line.split_whitespace().last().unwrap().parse().unwrap()
}

#[test]
fn health_log_bytes_count_whole_segments_once_the_segment_created_ahead_is_filled() {
    let f = Fixture::with_fill_pace(Duration::from_millis(10));
    f.await_status();
    let during = log_bytes(&f.run("health").unwrap());
    f.await_segments();
    let after = log_bytes(&f.run("health").unwrap());
    assert!(during < after, "{during} {after}");
    assert!(
        (2 * SEGMENT..2 * SEGMENT + 65_536).contains(&after),
        "{after}"
    );
}

#[test]
fn health_shows_the_series_directory_and_every_status_field_at_a_fixed_clock() {
    let mut f = Fixture::with_seal_interval(Some(Duration::from_secs(1)));
    let id = f.store_id();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while directory::generations(&f.fs, &f.dir, Family::Series).is_empty() {
        assert!(std::time::Instant::now() < deadline, "no series directory");
        std::thread::sleep(Duration::from_millis(50));
    }
    f.shut();
    directory::remove_family(&f.fs, &f.dir, Family::Status).unwrap();
    let st = StatusFile {
        interval_ns: Some(60 * S as u64),
        health: health::DEGRADED,
        pressure: true,
        pressure_since: Some(T0 - 20 * S),
        reasons: vec![reason::DISK_LOW, reason::SEAL_BLOCKED],
        counters: vec![(counter::BATCHES, 7), (counter::DISK_FREE, 123_456)],
        ..StatusFile::default()
    };
    let frame = Frame {
        generation: 0,
        file_id: 40,
        written_at: T0 - 5 * S,
    };
    let bytes = st.encode(frame, id).unwrap();
    directory::write_file(&f.fs, &f.dir, Family::Status, 40, &bytes).unwrap();
    insta::assert_snapshot!(without_bytes(&f.run("health").unwrap()));
}

#[test]
fn latest_live_writes_updates_new_series_and_retired_ones_under_one_header() {
    let mut f = Fixture::new();
    let env = f.env.clone();
    let dir = f.dir.clone();
    let run = std::thread::spawn(move || {
        let args: Vec<String> = "latest --live --format csv --interval 20ms --for 3s"
            .split_whitespace()
            .map(String::from)
            .collect();
        let mut out = Vec::new();
        kvik_cli::run(&args, Some(dir), env, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    });
    std::thread::sleep(Duration::from_millis(500));
    f.emit(&[Reading::new(f.ids[1], f.at(0), Value::F64(1_205.5))]);
    f.clock.advance(Duration::from_millis(500));
    std::thread::sleep(Duration::from_millis(400));
    let store = f.store.as_ref().unwrap();
    let extra = SeriesSpec::new(
        "site/meter/voltage",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "V",
    );
    block_on(store.define_series(extra)).unwrap();
    f.clock.advance(Duration::from_millis(500));
    std::thread::sleep(Duration::from_millis(400));
    block_on(store.retire(f.ids[3], Timestamp(T0))).unwrap();
    f.clock.advance(Duration::from_millis(500));
    std::thread::sleep(Duration::from_millis(400));
    let out = run.join().unwrap();
    assert_eq!(
        out.lines().filter(|l| l.starts_with("cut,")).count(),
        1,
        "{out}"
    );
    let rows: Vec<&str> = out
        .lines()
        .filter(|l| !l.contains(",opened,"))
        .map(|l| l.split_once(',').unwrap().1)
        .collect();
    insta::assert_snapshot!(rows.join("\n"));
}

#[test]
fn info_takes_the_zone_in_force_from_the_newest_registry_file() {
    let mut f = Fixture::new();
    let store = f.store.as_ref().unwrap();
    block_on(store.set_site_zone(&kvik::ZoneName::new("Etc/GMT-2"), Timestamp(T0))).unwrap();
    f.shut();
    let out = f.run("info").unwrap();
    let zones: Vec<&str> = out.lines().filter(|l| l.contains("site_zone")).collect();
    insta::assert_snapshot!(zones.join("\n"));
}
