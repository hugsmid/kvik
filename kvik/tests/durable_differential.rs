//! The durable backend decides every outcome as the model backend does, on generated histories that revisit keys
//! outside the recent window and restart the store (roadmap 11.0).

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::durable::{World, gauge};
use common::{at, block_on, start};
use kvik::env::{Env, ManualClock};
use kvik::{
    Backend, ChangedAtKind, Fact, Intent, IntervalLength, Kind, Outcome, Position, Quality,
    Reading, RetentionClass, SeriesId, SeriesSpec, StateValues, Store, StoreConfig, Value,
    ValueType, View, ZoneName,
};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
    /// A call of readings, each (series index, seconds, value code, quality code, `changed_at`, host-stamped), with
    /// revise intent or not.
    Emit(Vec<(usize, i64, u8, u8, Option<u64>, bool)>, bool),
    Retire(usize, i64),
    /// An alias: a name from a small set shared by every series, so that names collide.
    Alias(usize, usize),
    /// The series defined again (a new series once it is retired).
    Define(usize),
    Retention(usize, bool),
    SiteZone(usize, i64),
    /// The clock moves on by this many seconds.
    Advance(u64),
    /// The clock is reported synchronised or not (spec §2.5).
    Synced(bool),
    /// The store is shut down and opened again.
    Restart,
}

fn specs() -> Vec<SeriesSpec> {
    vec![
        // S = 30 s: readings span 600 s, so the lateness reference's clause (spec §2.2) can fire.
        gauge("h/g/power").staleness(Duration::from_secs(30)),
        SeriesSpec::new(
            "h/c/energy",
            Kind::Counter {
                value: ValueType::F64,
            },
            "Wh",
        ),
        SeriesSpec::new(
            "h/s/mode",
            Kind::State {
                values: StateValues::Named(vec!["a".into(), "b".into()]),
            },
            "",
        ),
        SeriesSpec::new("h/t/note", Kind::Text, ""),
        gauge("h/v/price").versioned(ChangedAtKind::Version),
        SeriesSpec::new(
            "h/i/energy",
            Kind::Interval {
                value: ValueType::F64,
                length: IntervalLength::Fixed {
                    d: Duration::from_secs(4),
                    phase: Duration::from_secs(1),
                },
            },
            "kWh",
        ),
        SeriesSpec::new(
            "h/b/door",
            Kind::State {
                values: StateValues::Bool,
            },
            "",
        ),
        SeriesSpec::new("h/w/notice", Kind::Text, "").versioned(ChangedAtKind::Time),
    ]
}

const ALIASES: [&str; 3] = ["h/g/power", "h/x/shared", "h/y/other"];
const ZONES: [&str; 3] = ["Europe/London", "Europe/Oslo", "Not/AZone"];

fn value(code: u8) -> Value {
    match code % 12 {
        0 => Value::F64(1.0),
        1 => Value::F64(2.0),
        2 => Value::F64(-1.0),
        3 => Value::F64(f64::NAN),
        4 => Value::F64(-0.0),
        5 => Value::Absent,
        6 => Value::State(0),
        7 => Value::State(5),
        8 => Value::Text("x".into()),
        9 => Value::Text("é".repeat(200)),
        10 => Value::Bool(true),
        _ => Value::Decimal { units: 1, scale: 1 },
    }
}

fn quality(code: u8) -> Quality {
    [
        Quality::Good,
        Quality::Estimated,
        Quality::Suspect,
        Quality::Bad,
    ][usize::from(code % 4)]
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    // Times far apart as well, so that keys leave the recent window (15 minutes) and are met again (roadmap 11.0).
    let reading = (
        0usize..9,
        prop_oneof![8 => -400i64..200, 2 => -4_000i64..4_000],
        any::<u8>(),
        any::<u8>(),
        // The bound of a `time` `changed_at` (Q-195) as well as small versions.
        prop_oneof![
            1 => Just(None),
            2 => (0u64..3).prop_map(Some),
            1 => prop::sample::select(vec![i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX]).prop_map(Some),
        ],
        prop::bool::weighted(0.15),
    );
    // A few keys of the plain gauge and the versioned series, so that duplicates and outranked facts of both meet
    // within one call, interleaved.
    let keyed = (
        prop::sample::select(vec![0usize, 4]),
        prop::sample::select(vec![0i64, 1, 2, 3, 1_500, 3_000]),
        0u8..2,
        0u8..2,
        0u64..3,
    )
        .prop_map(|(i, t, v, q, c)| (i, t, v, q, (i == 4).then_some(c), false));
    // Calls on two keys of the versioned series alone, so that an earlier reading of a call is often outranked at
    // commit while a later one becomes current (Q-705).
    let versions = (prop::sample::select(vec![0i64, 1]), 0u8..2, 0u64..4)
        .prop_map(|(t, v, c)| (4usize, t, v, 0u8, Some(c), false));
    let step = prop_oneof![
        10 => (prop::collection::vec(reading, 1..6), any::<bool>())
            .prop_map(|(r, rev)| Step::Emit(r, rev)),
        4 => (prop::collection::vec(keyed, 1..6), prop::bool::weighted(0.2))
            .prop_map(|(r, rev)| Step::Emit(r, rev)),
        2 => prop::collection::vec(versions, 2..6).prop_map(|r| Step::Emit(r, false)),
        1 => (0usize..8, -100i64..20).prop_map(|(i, t)| Step::Retire(i, t)),
        1 => (0usize..8, 0usize..ALIASES.len()).prop_map(|(i, n)| Step::Alias(i, n)),
        1 => (0usize..8).prop_map(Step::Define),
        1 => (0usize..8, any::<bool>()).prop_map(|(i, d)| Step::Retention(i, d)),
        1 => (0usize..ZONES.len(), -100i64..100).prop_map(|(z, t)| Step::SiteZone(z, t)),
        2 => (0u64..400).prop_map(Step::Advance),
        1 => prop::bool::weighted(0.7).prop_map(Step::Synced),
        1 => Just(Step::Restart),
    ];
    prop::collection::vec(step, 1..16)
}

fn outranked_on_arrival(s: &Store, ids: &[SeriesId]) -> u64 {
    ids.iter()
        .filter_map(|id| s.series_stats(*id))
        .map(|st| st.outranked_on_arrival)
        .sum()
}

/// What a history gives on one backend: every result in a comparable form, and per series its facts in each view.
struct Run {
    out: Vec<String>,
    facts: Vec<Vec<Vec<Fact>>>,
    infos: Vec<String>,
}

/// Runs a history on a store. Positions differ between the backends only by entries that are not facts (the model
/// writes no recovery or tzdata entry); facts are compared without them, and the as-of views at the positions each
/// backend gave the same readings.
fn run(open: &dyn Fn(&str) -> Store, clock: &ManualClock, history: &[Step]) -> (Run, Store) {
    // The zone in force: the site-zone entry of the greatest effective time, the later on a tie (spec §5.7).
    let mut zones: std::collections::BTreeMap<i64, &str> = std::collections::BTreeMap::new();
    let in_force = |z: &std::collections::BTreeMap<i64, &'static str>| {
        z.values().next_back().copied().unwrap_or("Europe/London")
    };
    let mut s = open(in_force(&zones));
    let mut out = Vec::new();
    let mut ids: Vec<SeriesId> = specs()
        .into_iter()
        .map(|spec| block_on(s.define_series(spec)).unwrap())
        .collect();
    let mut all = ids.clone();
    let mut recorded: Vec<Position> = Vec::new();
    let mut times = Vec::new();
    let id = |ids: &[SeriesId], i: usize| ids.get(i).copied().unwrap_or(SeriesId(999));
    for step in history {
        match step {
            Step::Emit(rs, revise) => {
                let readings: Vec<_> = rs
                    .iter()
                    .map(|(i, t, v, q, c, host)| {
                        let mut r =
                            Reading::new(id(&ids, *i), at(*t), value(*v)).with_quality(quality(*q));
                        r.changed_at = *c;
                        if *host {
                            r = r.host_stamped();
                        }
                        r
                    })
                    .collect();
                let outranked_before = outranked_on_arrival(&s, &all);
                let intent = if *revise {
                    Intent::Revise
                } else {
                    Intent::Normal
                };
                // Host-stamped readings wait for a synchronised clock: asked without waiting.
                let waits = rs.iter().any(|r| r.5);
                let ack = if waits {
                    match s.try_emit(&readings) {
                        Ok(p) => block_on(p),
                        Err(e) => {
                            out.push(format!("emit {e:?}"));
                            continue;
                        }
                    }
                } else {
                    block_on(s.emit_with(&readings, intent))
                }
                .unwrap();
                let outranked_then_current =
                    rs.iter().zip(&ack.outcomes).enumerate().any(|(k, (r, o))| {
                        matches!(
                            o,
                            Outcome::Recorded {
                                outranked: true,
                                ..
                            }
                        ) && rs.iter().zip(&ack.outcomes).skip(k + 1).any(|(later, lo)| {
                            (later.0, later.1) == (r.0, r.1)
                                && matches!(
                                    lo,
                                    Outcome::Recorded {
                                        outranked: false,
                                        ..
                                    }
                                )
                        })
                    });
                WITHIN_CALL.fetch_add(u64::from(outranked_then_current), Ordering::Relaxed);
                let outcomes: Vec<String> = ack
                    .outcomes
                    .iter()
                    .map(|o| match o {
                        Outcome::Recorded {
                            outranked,
                            position,
                            ..
                        } => {
                            recorded.push(*position);
                            format!("recorded outranked={outranked}")
                        }
                        other => format!("{other:?}"),
                    })
                    .collect();
                out.push(format!("emit {outcomes:?}"));
                // Per call: the backends rebuild different counters at a start.
                out.push(format!(
                    "outranked_on_arrival +{}",
                    outranked_on_arrival(&s, &all) - outranked_before
                ));
            }
            Step::Retire(i, t) => out.push(format!(
                "retire {:?}",
                block_on(s.retire(id(&ids, *i), at(*t))).map_err(|e| e.to_string())
            )),
            Step::Alias(i, n) => out.push(format!(
                "alias {:?}",
                block_on(s.alias(id(&ids, *i), ALIASES[*n])).map_err(|e| e.to_string())
            )),
            Step::Define(i) => {
                let r = block_on(s.define_series(specs()[*i].clone()));
                if let Ok(new) = &r {
                    if !all.contains(new) {
                        all.push(*new);
                    }
                    ids[*i] = *new;
                }
                out.push(format!(
                    "define {:?}",
                    r.map(|_| ()).map_err(|e| e.to_string())
                ));
            }
            Step::Retention(i, diagnostic) => {
                let class = if *diagnostic {
                    RetentionClass::Diagnostic {
                        period: Duration::from_secs(3_600),
                    }
                } else {
                    RetentionClass::LongTerm
                };
                let change = kvik::ParameterChange::default().retention(class);
                out.push(format!(
                    "retention {:?}",
                    block_on(s.set_parameters(id(&ids, *i), at(0), change))
                        .map_err(|e| e.to_string())
                ));
            }
            Step::SiteZone(z, t) => {
                let r = block_on(s.set_site_zone(&ZoneName::new(ZONES[*z]), at(*t)));
                if r.is_ok() {
                    zones.insert(*t, ZONES[*z]);
                }
                out.push(format!("zone {:?}", r.map_err(|e| e.to_string())));
            }
            Step::Advance(secs) => {
                clock.advance(Duration::from_secs(*secs));
                times.push(clock.wall());
            }
            Step::Synced(on) => clock.set_synchronised(*on),
            Step::Restart => {
                block_on(s.shutdown()).unwrap();
                drop(s);
                s = open(in_force(&zones));
            }
        }
    }
    clock.set_synchronised(true);
    let r = s.reader();
    let range = at(-1_000_000)..at(1_000_000);
    let mut views = vec![View::History, View::Current];
    views.extend(
        [recorded.len() / 3, recorded.len() / 2]
            .iter()
            .filter_map(|k| recorded.get(*k))
            .map(|p| View::AsOf(*p)),
    );
    views.extend(times.iter().take(2).map(|t| View::AsOfTime(*t)));
    let facts = all
        .iter()
        .map(|id| {
            views
                .iter()
                .map(|v| {
                    r.facts(*id, range.clone(), *v)
                        .unwrap()
                        .map(|f| {
                            let mut f = f.unwrap();
                            f.position = Position::ORIGIN;
                            f
                        })
                        .collect()
                })
                .collect()
        })
        .collect();
    let infos = all
        .iter()
        .map(|id| {
            let i = s.series(*id).unwrap();
            format!(
                "{} {:?} {:?} {:?} {:?} {:?}",
                i.name, i.names, i.retired_at, i.latest_t, i.lateness_reference, i.spec.retention
            )
        })
        .collect();
    (Run { out, facts, infos }, s)
}

/// Runs `history` on both backends and compares what they give; with `small`, the durable store's windows hold a
/// few dozen facts (`limits.window_memory` of 8 KiB, so that the byte bound sends keys to the slow path, Q-751).
/// Returns the readings the durable backend decided by the slow path.
fn compare(history: &[Step], small: bool) -> Result<u64, TestCaseError> {
    let m_clock = ManualClock::new(start());
    let m_config = StoreConfig::new(
        format!("kvik-test/differential/{}", next()),
        "Europe/London",
    )
    .create_if_missing(true)
    .backend(Backend::Model)
    .env(Env::test(m_clock.clone()));
    let w = World::new("differential");
    let mut config = w.config();
    config.log.min_sync_interval = Duration::ZERO;
    if small {
        config.limits.window_memory = 8 << 10;
    }
    // A restart opens with the site zone in force (`docs/api.md` Opening).
    let in_zone = |c: &StoreConfig, z: &str| {
        let mut c = c.clone();
        c.time_zone = z.into();
        block_on(Store::open(c)).unwrap()
    };
    let (m, model) = run(&|z| in_zone(&m_config, z), &m_clock, history);
    let (d, durable) = run(&|z| in_zone(&config, z), &w.clock, history);
    let slow = durable.stats().ingest.slow_path;
    prop_assert_eq!(&m.out, &d.out);
    prop_assert_eq!(&m.infos, &d.infos);
    prop_assert_eq!(&m.facts, &d.facts);
    block_on(durable.shutdown()).unwrap();
    block_on(model.shutdown()).unwrap();
    Ok(slow)
}

/// Runs the differential histories, each with small windows when `small` gives true. Returns the histories run with
/// small windows and with the default, and the readings decided by the slow path.
fn differential(small: impl Strategy<Value = bool>) -> (u64, u64, u64) {
    // Each case opens a durable store; a quarter of the configured cases keeps the deep run in bounds.
    let cases = (ProptestConfig::default().cases / 4).max(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let (smalls, defaults, slow) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    runner
        .run(&(steps(), small), |(history, small)| {
            let n = if small { &smalls } else { &defaults };
            n.fetch_add(1, Ordering::Relaxed);
            slow.fetch_add(compare(&history, small)?, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    (
        smalls.into_inner(),
        defaults.into_inner(),
        slow.into_inner(),
    )
}

/// Half the histories, chosen with each history, run with small windows.
#[test]
fn the_durable_backend_decides_as_the_model_does() {
    let (small, default, slow) = differential(prop::bool::weighted(0.5));
    assert!(small > 0 && default > 0, "{small} small, {default} default");
    assert!(slow > 0, "some history takes the slow path");
    assert!(
        WITHIN_CALL.load(Ordering::Relaxed) > 0,
        "some call records a reading outranked at commit and then its key's current fact (Q-705)"
    );
}

/// Calls of the differential histories holding a reading outranked at commit and a later reading of its key that is
/// not (Q-705).
static WITHIN_CALL: AtomicU64 = AtomicU64::new(0);

#[test]
fn small_windows_decide_as_the_model_does() {
    let (small, default, slow) = differential(Just(true));
    assert_eq!(default, 0);
    assert!(small > 0 && slow > 0);
}

#[test]
fn a_reading_taken_unsynchronised_leaves_the_lateness_reference_in_both_backends() {
    let emit = |t| Step::Emit(vec![(0, t, 0, 0, None, false)], false);
    compare(
        &[
            emit(-100),
            Step::Synced(false),
            emit(100),
            Step::Synced(true),
            emit(0),
        ],
        false,
    )
    .unwrap();
}

fn next() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}
