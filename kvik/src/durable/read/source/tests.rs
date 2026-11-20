use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use crate::config::LogConfig;
use crate::durable::read::cache::{LEAST_MEMORY, Sizes};
use crate::durable::read::latest::tests::{Rig, Step, steps};
use crate::durable::reader::DurableReader;
use crate::durable::reader::facts::{at, facts, facts_of};
use crate::env::Env;
use crate::error::QueryError;
use crate::read::View;
use crate::time::{Span, TimeRange, Timestamp};
use crate::types::{Fact, Position, Quality, Reading, SeriesId, Value};
use crate::{Intent, Reader};

const LONG: Duration = Duration::from_secs(1_800);

/// The bytes a found fact is counted as, without text.
fn fact_bytes() -> u64 {
    std::mem::size_of::<kvik_log::reader::FoundFact>() as u64
}

/// A cap whose passes keep about `facts` facts each, with no block reserve: small histories then read in many passes.
fn tiny(facts: u64) -> Sizes {
    let rows = facts * fact_bytes() * 2 / 3;
    Sizes {
        cap: rows * 4,
        block: 0,
        min_rows: 1,
        lookups: 0,
    }
}

fn reader_with(env: &Env, dir: &Path, sizes: Sizes) -> Arc<DurableReader> {
    Arc::new(
        DurableReader::open(dir, env, LONG, LONG)
            .unwrap()
            .with_memory(sizes),
    )
}

fn all() -> TimeRange {
    TimeRange::new(Timestamp(i64::MIN), Timestamp(i64::MAX))
}

fn collect(f: crate::read::Facts) -> Result<Vec<Fact>, QueryError> {
    f.collect()
}

/// Facts as both backends give them: positions and receive times set aside (the model writes no recovery or tzdata
/// entry, and its receive time is the call's, where a batch's may be later).
fn unpositioned(mut fs: Vec<Fact>) -> Vec<Fact> {
    for f in &mut fs {
        f.position = Position::ORIGIN;
        f.received_at = Timestamp(0);
    }
    fs
}

/// What `at` gives from a view's facts: the latest at or before `t` not of bad quality, the later position on a tie.
fn at_from(view: &[Fact], t: i64) -> Option<Fact> {
    view.iter()
        .filter(|f| f.t.0 <= t && f.quality != Quality::Bad)
        .max_by_key(|f| (f.t, f.position))
        .cloned()
}

/// Facts histories that read in passes and with every source: per series and view, what a reader at each cap gives
/// equals a fresh reader's fact index (positions included), and in history and the current view the model (positions
/// set aside) while no batch is damaged; `at` agrees at several times. A tiny cap may refuse only a key with more facts than it holds.
fn check(rig: &Rig, times: &[Timestamp], small: u64) -> Result<(), TestCaseError> {
    let oracle = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let readers = [
        reader_with(&rig.env, &rig.dir, tiny(small)),
        reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY)),
    ];
    let own = rig.store.as_ref().map(crate::Store::reader);
    let model = rig.model.reader();
    for id in &rig.ids {
        let history = oracle
            .with(|i, _| i.facts(*id, all(), View::History))
            .unwrap();
        let mut views = vec![View::History, View::Current];
        views.extend(
            [history.len() / 3, history.len() / 2]
                .iter()
                .filter_map(|k| history.get(*k))
                .map(|f| View::AsOf(f.position)),
        );
        views.extend(times.iter().map(|t| View::AsOfTime(*t)));
        let most = {
            let mut per_key = std::collections::HashMap::new();
            for f in &history {
                *per_key.entry(f.t).or_insert(0u64) += 1;
            }
            per_key.into_values().max().unwrap_or(0)
        };
        for view in &views {
            let want = oracle.with(|i, _| i.facts(*id, all(), *view)).unwrap();
            if !rig.faulted && matches!(view, View::History | View::Current) {
                let m = unpositioned(collect(model.facts(*id, all(), *view).unwrap()).unwrap());
                prop_assert_eq!(unpositioned(want.clone()), m, "{:?} {:?}", id, view);
            }
            for (k, r) in readers.iter().enumerate() {
                match collect(facts(r.clone(), *id, all(), *view).unwrap()) {
                    Ok(got) => prop_assert_eq!(&got, &want, "{:?} {:?} reader {}", id, view, k),
                    Err(QueryError::NotAvailable { .. }) if k == 0 => {
                        prop_assert!(most * fact_bytes() > small * fact_bytes() * 2 / 3);
                    }
                    Err(e) => return Err(TestCaseError::fail(format!("{e:?}"))),
                }
            }
            if let Some(own) = &own {
                let got = collect(own.facts(*id, all(), *view).unwrap()).unwrap();
                prop_assert_eq!(&got, &want);
            }
            for t in history
                .iter()
                .map(|f| f.t.0)
                .take(4)
                .chain([i64::MIN, i64::MAX])
            {
                let want = at_from(&want, t);
                match at(&readers[0], *id, Timestamp(t), *view) {
                    Ok(got) => prop_assert_eq!(got, want.clone()),
                    Err(QueryError::NotAvailable { .. }) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("{e:?}"))),
                }
                prop_assert_eq!(at(&readers[1], *id, Timestamp(t), *view).unwrap(), want);
            }
        }
    }
    Ok(())
}

static SEALED: AtomicU64 = AtomicU64::new(0);
static PASSES: AtomicU64 = AtomicU64::new(0);

fn sealed_segments(rig: &Rig) -> u64 {
    rig.fs
        .paths()
        .iter()
        .filter(|p| p.starts_with(rig.dir.join("log")) && p.to_string_lossy().ends_with(".kvl"))
        .count() as u64
}

#[test]
fn facts_and_at_equal_the_fact_index_and_the_model_at_every_cap() {
    // Each case opens two stores and several readers; a thirty-second of the configured cases keeps the deep run in
    // bounds.
    let mut runner = TestRunner::new(ProptestConfig {
        cases: (ProptestConfig::default().cases / 32).max(8),
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let log = LogConfig {
        seal_interval: Duration::from_millis(20),
        ..LogConfig::default()
    };
    runner
        .run(
            &(steps(), prop::sample::select(vec![6u64, 24])),
            |(history, small)| {
                let mut rig = Rig::with_log("sources", log.clone());
                let mut times = Vec::new();
                // The latest values' checks (`Check`, and the one `Directory` makes) are left to their own test: with
                // seals between steps a loss report's `commit_time_before` can differ between readers there, which
                // these reads do not touch.
                for s in &history {
                    match s {
                        Step::Check => check(&rig, &times, small)?,
                        Step::Directory(_) => {}
                        s => rig.step(s)?,
                    }
                    if let Step::Advance(_) = s {
                        times.push(rig.clock.wall());
                    }
                }
                check(&rig, &times, small)?;
                rig.shut();
                SEALED.fetch_add(sealed_segments(&rig), Ordering::Relaxed);
                PASSES.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap();
    assert!(PASSES.load(Ordering::Relaxed) > 0);
    assert!(
        SEALED.load(Ordering::Relaxed) > 0,
        "some history seals a segment"
    );
}

#[test]
fn several_series_read_in_one_pass_come_in_event_time_order_then_series() {
    let mut rig = Rig::new("several");
    let now = rig.clock.wall().0;
    let r = |i: usize, s: i64, v: f64| Reading::new(rig.ids[i], Timestamp(now + s), Value::F64(v));
    let rs = vec![
        r(0, 3, 1.0),
        r(1, 1, 2.0),
        r(0, 1, 3.0),
        r(1, 2, 4.0),
        r(0, 2, 5.0),
    ];
    rig.emit_readings(&rs, Intent::Normal);
    let reader = reader_with(&rig.env, &rig.dir, tiny(2));
    let got: Vec<(i64, SeriesId)> =
        facts_of(reader, &[rig.ids[1], rig.ids[0]], all(), View::History)
            .unwrap()
            .map(|f| f.map(|f| (f.t.0 - now, f.series)))
            .collect::<Result<_, _>>()
            .unwrap();
    let (a, b) = (rig.ids[0], rig.ids[1]);
    assert_eq!(got, vec![(1, a), (1, b), (2, a), (2, b), (3, a)]);
    rig.shut();
}

#[test]
fn a_maximal_batch_reads_in_several_passes_at_the_least_cap_with_the_fact_indexs_result() {
    let log = LogConfig {
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let mut rig = Rig::with_log("maximal-facts", log);
    let n = 26_000i64;
    let now = rig.clock.wall().0;
    let readings: Vec<Reading> = (0..n)
        .map(|k| {
            Reading::new(
                rig.ids[2],
                Timestamp(now - (n - k) * 1_000_000),
                Value::Text(format!("{k:0>255}")),
            )
        })
        .collect();
    rig.emit_readings(&readings, Intent::Normal);
    let reader = Reader::open(
        crate::ReaderConfig::new(&rig.dir)
            .env(rig.env.clone())
            .memory(LEAST_MEMORY),
    )
    .unwrap();
    let d = reader.durable().unwrap().unwrap();
    let want = d
        .with(|i, _| i.facts(rig.ids[2], all(), View::Current))
        .unwrap();
    let got = collect(reader.facts(rig.ids[2], all(), View::Current).unwrap()).unwrap();
    assert_eq!(got.len(), n as usize);
    assert_eq!(got, want);
    assert!(d.memory().peak() <= LEAST_MEMORY);
    let last = reader
        .at(rig.ids[2], Timestamp(now), View::Current)
        .unwrap();
    assert_eq!(last, want.last().cloned());
    rig.shut();
}

#[test]
fn a_reader_below_the_least_memory_cap_is_refused() {
    let rig = Rig::new("least");
    let r = Reader::open(
        crate::ReaderConfig::new(&rig.dir)
            .env(rig.env.clone())
            .memory(LEAST_MEMORY - 1),
    );
    assert!(matches!(
        r,
        Err(crate::OpenError::Config { key: "memory", .. })
    ));
}

#[test]
fn as_of_a_time_in_two_generations_is_the_later_position_committed_by_then() {
    let mut rig = Rig::new("as-of-restart");
    let id = rig.ids[0];
    let now = rig.clock.wall().0;
    let emit = |rig: &mut Rig, s: i64, v: f64| {
        rig.emit_readings(
            &[Reading::new(id, Timestamp(now + s), Value::F64(v))],
            Intent::Normal,
        );
    };
    emit(&mut rig, 0, 1.0);
    rig.clock.advance(Duration::from_secs(10));
    emit(&mut rig, 1, 2.0);
    rig.clock.advance(Duration::from_secs(10));
    emit(&mut rig, 2, 3.0);
    let t = rig.clock.wall();
    rig.shut();
    // The clock goes back across the restart: the writer keeps commit times from going back, so the next generation's
    // first batches commit at the time the first generation's last did.
    rig.clock.jump_wall(Span(-15_000_000_000));
    rig.reopen();
    emit(&mut rig, 3, 4.0);
    rig.clock.advance(Duration::from_secs(30));
    emit(&mut rig, 4, 5.0);
    let reader = reader_with(&rig.env, &rig.dir, Sizes::of(LEAST_MEMORY));
    let got: Vec<i64> = collect(facts(reader.clone(), id, all(), View::AsOfTime(t)).unwrap())
        .unwrap()
        .iter()
        .map(|f| f.t.0 - now)
        .collect();
    assert_eq!(
        got,
        vec![0, 1, 2, 3],
        "the second generation's first batch is the last committed by then"
    );
    let want = reader
        .with(|i, _| i.facts(id, all(), View::AsOfTime(t)))
        .unwrap();
    assert_eq!(
        collect(facts(reader, id, all(), View::AsOfTime(t)).unwrap()).unwrap(),
        want
    );
    rig.shut();
}

#[test]
fn a_stream_held_past_its_cuts_age_ends_with_cut_expired() {
    let mut rig = Rig::new("stream-age");
    let now = rig.clock.wall().0;
    let rs: Vec<Reading> = (0..40)
        .map(|k| Reading::new(rig.ids[0], Timestamp(now - 100 + k), Value::F64(k as f64)))
        .collect();
    rig.emit_readings(&rs, Intent::Normal);
    let r = Arc::new(
        DurableReader::open(&rig.dir, &rig.env, LONG, Duration::from_secs(60))
            .unwrap()
            .with_memory(tiny(4)),
    );
    let mut s = facts(r, rig.ids[0], all(), View::History).unwrap();
    assert!(s.next().unwrap().is_ok());
    rig.clock.advance(Duration::from_secs(61));
    let rest: Vec<_> = s.by_ref().collect();
    assert!(
        matches!(rest.last(), Some(Err(QueryError::CutExpired))),
        "{rest:?}"
    );
    assert!(rest.len() < 40);
    assert!(s.next().is_none());
    rig.shut();
}
