//! Name patterns (spec §4 Funnel, Routing; Q-88): the grammar's refusals on both backends, overlap in the cycle
//! check against a brute-force oracle, and the two backends compared on compiling and routing patterns.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::world::{World, drain, gauge, num};
use common::{block_on, start};
use kvik::env::{Env, ManualClock};
use kvik::{
    Backend, ConsumerError, DeliveryKind, Funnel, RouteError, Select, SeriesId, Start, Store,
    StoreConfig,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

fn next_id() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn model_store() -> Store {
    let clock = ManualClock::new(start());
    let mut env = Env::test(clock);
    env.background_timers = false;
    let config = StoreConfig::new(format!("kvik-test/patterns/{}", next_id()), "Europe/London")
        .create_if_missing(true)
        .backend(Backend::Model)
        .env(env);
    block_on(Store::open(config)).unwrap()
}

fn durable_store() -> Store {
    World::quiet(&format!("patterns-{}", next_id())).open()
}

fn store_on(backend: Backend) -> Store {
    match backend {
        Backend::Model => model_store(),
        _ => durable_store(),
    }
}

fn names(p: &str) -> Funnel {
    Funnel::facts(Select::Names(p.into()))
}

fn refusal(r: Result<impl Sized, ConsumerError>) -> Option<RouteError> {
    match r {
        Err(ConsumerError::Pattern(e)) => Some(e),
        _ => None,
    }
}

#[test]
fn a_funnel_pattern_outside_the_grammar_is_refused_on_both_backends() {
    let long = format!("a/b/{}", "c".repeat(253));
    let cases = [
        ("plant/line-*/power", RouteError::PartialWildcard),
        ("plant/*", RouteError::Segments { found: 2 }),
        ("a/b/c/d", RouteError::Segments { found: 4 }),
        ("", RouteError::Segments { found: 1 }),
        ("a//c", RouteError::EmptySegment),
        (long.as_str(), RouteError::TooLong { len: 257 }),
    ];
    for backend in [Backend::Model, Backend::Durable] {
        let s = store_on(backend);
        block_on(s.define_series(gauge("plant/line-1/power"))).unwrap();
        let c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        for (p, want) in &cases {
            assert_eq!(
                refusal(block_on(c.route(names(p)))),
                Some(*want),
                "{backend:?} {p}"
            );
        }
        assert!(
            c.routes().unwrap().is_empty(),
            "{backend:?}: nothing routed"
        );
        block_on(c.route(names("plant/*/power"))).unwrap();
    }
}

#[test]
fn an_emits_pattern_outside_the_grammar_is_refused_on_both_backends() {
    for backend in [Backend::Model, Backend::Durable] {
        let s = store_on(backend);
        let emits = Select::Names("plant/line-*/power".into());
        assert_eq!(
            refusal(block_on(s.consumer("c", Start::Now, emits))),
            Some(RouteError::PartialWildcard),
            "{backend:?}"
        );
    }
    let s = durable_store();
    block_on(s.consumer("c", Start::Now, Select::Names("plant/*/power".into()))).unwrap();
}

#[test]
fn an_id_overlaps_a_pattern_through_an_alias() {
    let s = durable_store();
    let raw = block_on(s.define_series(gauge("site/a/raw"))).unwrap();
    let other = block_on(s.define_series(gauge("site/b/raw"))).unwrap();
    block_on(s.alias(raw, "site/a/power")).unwrap();
    let c = block_on(s.consumer("c", Start::Now, Select::Ids(vec![raw]))).unwrap();
    assert!(matches!(
        block_on(c.route(names("site/*/power"))),
        Err(ConsumerError::RouteCycle(_))
    ));
    drop(c);
    let d = block_on(s.consumer("d", Start::Now, Select::Ids(vec![other]))).unwrap();
    block_on(d.route(names("site/*/power"))).unwrap();
}

#[test]
fn a_cycle_needs_every_segment_equal_or_a_wildcard() {
    let s = durable_store();
    let c = block_on(s.consumer("c", Start::Now, Select::Names("plant/*/power".into()))).unwrap();
    block_on(c.route(names("plant/*/power-smoothed"))).unwrap();
    assert!(matches!(
        block_on(c.route(names("*/line-1/*"))),
        Err(ConsumerError::RouteCycle(_))
    ));
}

const LITERALS: [&str; 2] = ["a", "b"];
/// A segment neither pattern names, standing for every other literal.
const OTHER: &str = "c";

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("*".to_string()),
        prop::sample::select(&LITERALS[..]).prop_map(String::from)
    ]
}

fn pattern() -> impl Strategy<Value = String> {
    prop::collection::vec(segment(), 3).prop_map(|s| s.join("/"))
}

fn oracle_matches(p: &str, name: &str) -> bool {
    let (p, n): (Vec<&str>, Vec<&str>) = (p.split('/').collect(), name.split('/').collect());
    p.len() == n.len() && p.iter().zip(&n).all(|(p, n)| *p == "*" || p == n)
}

/// Whether some name over the literals and one other segment matches both patterns.
fn shared_name(p: &str, q: &str) -> bool {
    let alphabet = [LITERALS[0], LITERALS[1], OTHER];
    alphabet.iter().any(|x| {
        alphabet.iter().any(|y| {
            alphabet.iter().any(|z| {
                let n = format!("{x}/{y}/{z}");
                oracle_matches(p, &n) && oracle_matches(q, &n)
            })
        })
    })
}

fn cycle<T>(r: Result<T, ConsumerError>) -> bool {
    match r {
        Ok(_) => false,
        Err(ConsumerError::RouteCycle(_)) => true,
        Err(e) => panic!("unexpected {e:?}"),
    }
}

/// A consumer that emits `emits` and adds a funnel on `funnel`.
fn cycle_by_route(emits: &str, funnel: &str) -> bool {
    let s = durable_store();
    let c = block_on(s.consumer("c", Start::Now, Select::Names(emits.into()))).unwrap();
    cycle(block_on(c.route(names(funnel))))
}

/// A consumer with a funnel on `funnel` that then declares `emits`.
fn cycle_by_emits(emits: &str, funnel: &str) -> bool {
    let s = durable_store();
    let c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(names(funnel))).unwrap();
    drop(c);
    cycle(block_on(s.consumer(
        "c",
        Start::Now,
        Select::Names(emits.into()),
    )))
}

fn runner(divisor: u32) -> TestRunner {
    let cases = (ProptestConfig::default().cases / divisor).max(16);
    TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    })
}

#[test]
fn pattern_overlap_is_symmetric_and_means_a_shared_name() {
    runner(4)
        .run(&(pattern(), pattern()), |(p, q)| {
            let want = shared_name(&p, &q);
            prop_assert_eq!(cycle_by_route(&p, &q), want, "route {} {}", p, q);
            prop_assert_eq!(cycle_by_emits(&q, &p), want, "emits {} {}", q, p);
            Ok(())
        })
        .unwrap();
}

/// A segment of a pattern that may be outside the grammar.
fn any_segment() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => Just("*".to_string()),
        4 => prop::sample::select(&["a", "b", "c"][..]).prop_map(String::from),
        1 => Just("a*".to_string()),
        1 => Just("*b".to_string()),
        1 => Just(String::new()),
    ]
}

fn any_pattern() -> impl Strategy<Value = String> {
    let count = prop_oneof![8 => Just(3usize), 1 => Just(2usize), 1 => Just(4usize)];
    count
        .prop_flat_map(|n| prop::collection::vec(any_segment(), n))
        .prop_map(|s| s.join("/"))
}

fn name() -> impl Strategy<Value = String> {
    let count = prop_oneof![8 => Just(3usize), 1 => Just(2usize), 1 => Just(4usize)];
    count
        .prop_flat_map(|n| {
            prop::collection::vec(prop::sample::select(&["a", "b", "c", "*"][..]), n)
        })
        .prop_map(|s| s.join("/"))
}

#[derive(Clone, Debug)]
struct History {
    /// Distinct names: the first `series` name a series each, the rest are aliases, given in turn.
    names: Vec<String>,
    series: usize,
    /// How many series are defined before the funnels are routed.
    before: usize,
    patterns: Vec<String>,
}

fn history() -> impl Strategy<Value = History> {
    (
        prop::collection::btree_set(name(), 1..8),
        prop::collection::vec(any_pattern(), 1..5),
    )
        .prop_flat_map(|(names, patterns)| {
            let names: Vec<String> = names.into_iter().collect();
            let n = names.len();
            (1..=n).prop_flat_map(move |series| {
                let (names, patterns) = (names.clone(), patterns.clone());
                (0..=series).prop_map(move |before| History {
                    names: names.clone(),
                    series,
                    before,
                    patterns: patterns.clone(),
                })
            })
        })
}

/// Each pattern's compile result and the series index its consumer was delivered facts on.
fn run(s: &Store, h: &History) -> Vec<(Result<(), String>, BTreeSet<usize>)> {
    let mut ids: Vec<SeriesId> = Vec::new();
    let define = |ids: &mut Vec<SeriesId>, i: usize| {
        ids.push(block_on(s.define_series(gauge(&h.names[i]))).unwrap());
    };
    for i in 0..h.before {
        define(&mut ids, i);
    }
    let mut consumers = Vec::new();
    let mut compiled = Vec::new();
    for (i, p) in h.patterns.iter().enumerate() {
        let c = block_on(s.consumer(&format!("f{i}"), Start::Now, Select::None)).unwrap();
        compiled.push(
            block_on(c.route(names(p)))
                .map(|_| ())
                .map_err(|e| format!("{e:?}")),
        );
        consumers.push(c);
    }
    for i in h.before..h.series {
        define(&mut ids, i);
    }
    for (k, alias) in h.names[h.series..].iter().enumerate() {
        block_on(s.alias(ids[k % h.series], alias)).unwrap();
    }
    for id in &ids {
        block_on(s.emit(&[num(*id, 0, 1.0)])).unwrap();
    }
    consumers
        .iter_mut()
        .zip(compiled)
        .map(|(c, r)| {
            let got = drain(c)
                .into_iter()
                .filter_map(|d| match d.kind {
                    DeliveryKind::Fact(f) => ids.iter().position(|i| *i == f.series),
                    _ => None,
                })
                .collect();
            (r, got)
        })
        .collect()
}

#[test]
fn both_backends_compile_and_route_patterns_alike() {
    runner(4)
        .run(&history(), |h| {
            let m = run(&model_store(), &h);
            let d = run(&durable_store(), &h);
            prop_assert_eq!(&m, &d);
            for ((r, got), p) in m.iter().zip(&h.patterns) {
                let want: BTreeSet<usize> = if r.is_ok() {
                    (0..h.series)
                        .filter(|i| {
                            std::iter::once(*i)
                                .chain(
                                    (h.series..h.names.len())
                                        .filter(|a| (a - h.series) % h.series == *i),
                                )
                                .any(|n| oracle_matches(p, &h.names[n]))
                        })
                        .collect()
                } else {
                    BTreeSet::new()
                };
                prop_assert_eq!(got, &want, "{}", p);
            }
            Ok(())
        })
        .unwrap();
}
