//! The tool's output, pinned (the query design's section 6).

mod common;

use std::time::Duration;

use common::Fixture;
use kvik::{Reading, Value};

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
