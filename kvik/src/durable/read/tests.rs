//! Calls that share views (query design §3.5): no call waits for another's work, concurrent calls equal sequential
//! ones at the same cuts, and a view's cut follows the lease rules (Q-903).

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Wake};
use std::thread;
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use crate::durable::index::pos;
use crate::durable::reader::DurableReader;
use crate::env::{Env, ManualClock};
use crate::error::QueryError;
use crate::read::View;
use crate::series::{Kind, SeriesKey, SeriesSpec, ValueType};
use crate::time::{TimeRange, Timestamp};
use crate::types::{Fact, Position, Reading, SeriesId, Value};
use crate::{LogConfig, Store, StoreConfig};

const T0: i64 = 1_767_225_600_000_000_000;
const DIR: &str = "views";
const LONG: Duration = Duration::from_secs(1_800);

fn block_on<F: Future>(f: F) -> F::Output {
    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut f = std::pin::pin!(f);
    let waker = std::task::Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park();
    }
}

/// A store with `series` gauges, ids 1 to `series`.
fn store(series: u32) -> (Store, Env, ManualClock) {
    let clock = ManualClock::new(Timestamp(T0));
    let env = Env::test(clock.clone());
    let log = LogConfig {
        segment_size: 16 << 20,
        min_sync_interval: Duration::ZERO,
        ..LogConfig::default()
    };
    let store = block_on(Store::open(
        StoreConfig::new(DIR, "Etc/UTC")
            .create_if_missing(true)
            .env(env.clone())
            .log(log),
    ))
    .unwrap();
    for k in 1..=series {
        let spec = SeriesSpec::new(
            format!("a/s/v{k}"),
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        block_on(store.define_series(spec)).unwrap();
    }
    (store, env, clock)
}

fn reading(id: u32, t: i64, v: f64) -> Reading {
    Reading::new(SeriesId(id), Timestamp(T0 + t), Value::F64(v))
}

fn reader(env: &Env, lease_refresh: Duration, cut_max_age: Duration) -> Arc<DurableReader> {
    Arc::new(DurableReader::open(Path::new(DIR), env, lease_refresh, cut_max_age).unwrap())
}

fn all() -> TimeRange {
    TimeRange::new(Timestamp(i64::MIN), Timestamp(i64::MAX))
}

/// One call: its cut, and the history and current views of a series under it.
fn facts_at_cut(
    r: &DurableReader,
    id: u32,
) -> Result<(Position, Vec<Fact>, Vec<Fact>), QueryError> {
    r.with(|i, v| {
        Ok((
            pos(v.state().durable),
            i.facts(SeriesId(id), all(), View::History)?,
            i.facts(SeriesId(id), all(), View::Current)?,
        ))
    })
}

/// Deletes every lease file, as a writer does with an expired one.
fn delete_leases(env: &Env) {
    let leases = Path::new(DIR).join("leases");
    for e in env.fs.read_dir(&leases).unwrap() {
        if e.name.ends_with(".lease") {
            env.fs.remove_file(&leases.join(&e.name)).unwrap();
        }
    }
}

/// A call on another thread that holds its view until released; returns once it holds it.
fn held_call(
    r: &Arc<DurableReader>,
) -> (
    thread::JoinHandle<Result<Position, QueryError>>,
    mpsc::Sender<()>,
) {
    let (entered, entered_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let r = r.clone();
    let h = thread::spawn(move || {
        r.with(|_, v| {
            entered.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(pos(v.state().durable))
        })
    });
    entered_rx.recv().unwrap();
    (h, release)
}

#[test]
fn a_call_runs_while_another_call_holds_its_view_and_each_view_keeps_its_cut_until_released() {
    let (store, env, _) = store(1);
    block_on(store.emit(&[reading(1, 1, 1.0), reading(1, 2, 2.0)])).unwrap();
    let r = reader(&env, LONG, LONG);
    let (held, release) = held_call(&r);
    block_on(store.emit(&[reading(1, 3, 3.0)])).unwrap();
    let (done, done_rx) = mpsc::channel();
    let other = r.clone();
    thread::spawn(move || done.send(facts_at_cut(&other, 1)));
    let (cut, history, _) = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a call waited for another call's work")
        .unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(r.cuts_held(), 2, "the held view's and the current view's");
    release.send(()).unwrap();
    let first = held.join().unwrap().unwrap();
    assert!(first < cut);
    assert_eq!(
        r.cuts_held(),
        1,
        "the held view's cut is released at its call's end"
    );
    block_on(store.shutdown()).unwrap();
}

#[test]
fn a_lease_another_calls_refresh_finds_gone_ends_a_call_in_flight() {
    let (store, env, clock) = store(1);
    block_on(store.emit(&[reading(1, 1, 1.0)])).unwrap();
    block_on(store.shutdown()).unwrap();
    let r = reader(&env, Duration::from_secs(60), LONG);
    clock.advance(Duration::from_secs(20));
    let (held, release) = held_call(&r);
    delete_leases(&env);
    clock.advance(Duration::from_secs(11));
    assert!(
        r.with(|_, _| Ok(())).is_ok(),
        "a view taken after the renewal"
    );
    release.send(()).unwrap();
    let ended = held.join().unwrap();
    assert!(matches!(ended, Err(QueryError::CutExpired)), "{ended:?}");
}

#[test]
fn a_call_as_old_as_half_lease_refresh_renews_the_lease_at_its_check_and_finds_it_gone() {
    let (store, env, clock) = store(1);
    block_on(store.emit(&[reading(1, 1, 1.0)])).unwrap();
    block_on(store.shutdown()).unwrap();
    let r = reader(&env, Duration::from_secs(60), LONG);
    let young = r.with(|_, _| {
        delete_leases(&env);
        clock.advance(Duration::from_secs(29));
        Ok(())
    });
    assert!(young.is_ok(), "{young:?}");
    let old = r.with(|_, _| {
        delete_leases(&env);
        clock.advance(Duration::from_secs(30));
        Ok(())
    });
    assert!(matches!(old, Err(QueryError::CutExpired)), "{old:?}");
    assert!(r.with(|_, _| Ok(())).is_ok());
}

#[derive(Clone, Debug)]
struct History {
    series: u32,
    batches: Vec<Vec<(u32, i64, i8)>>,
}

fn history() -> impl Strategy<Value = History> {
    (1u32..4).prop_flat_map(|series| {
        prop::collection::vec(
            prop::collection::vec((1..=series, 0i64..40, -4i8..4), 1..6),
            1..24,
        )
        .prop_map(move |batches| History { series, batches })
    })
}

/// The calls a reader thread records while the history is written.
const MAX_CALLS: usize = 256;

/// A call of one thread: the series, the cut, and the history and current views under it.
type Call = (u32, Position, Vec<Fact>, Vec<Fact>);

/// Writes `h` from one thread while `readers` threads call until it is written; returns each thread's calls in order.
fn run(h: &History, readers: u32) -> (Arc<DurableReader>, Vec<Vec<Call>>) {
    let (store, env, _) = store(h.series);
    let r = reader(&env, LONG, LONG);
    let written = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicU32::new(0));
    let threads: Vec<_> = (0..readers)
        .map(|_| {
            let (r, written, next, series) = (r.clone(), written.clone(), next.clone(), h.series);
            thread::spawn(move || {
                let mut calls = Vec::new();
                loop {
                    let last = written.load(Ordering::SeqCst);
                    if calls.len() >= MAX_CALLS && !last {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let id = next.fetch_add(1, Ordering::SeqCst) % series + 1;
                    let (cut, history, current) = facts_at_cut(&r, id).unwrap();
                    calls.push((id, cut, history, current));
                    if last {
                        return calls;
                    }
                }
            })
        })
        .collect();
    for b in &h.batches {
        let rs: Vec<_> = b
            .iter()
            .map(|&(id, t, v)| reading(id, t, f64::from(v)))
            .collect();
        block_on(store.emit(&rs)).unwrap();
    }
    written.store(true, Ordering::SeqCst);
    let calls = threads.into_iter().map(|t| t.join().unwrap()).collect();
    block_on(store.shutdown()).unwrap();
    (r, calls)
}

#[test]
fn concurrent_calls_equal_sequential_ones_at_the_same_cuts() {
    // Each case opens a store and four threads; a sixteenth of the configured cases keeps the deep run in bounds.
    let mut runner = TestRunner::new(ProptestConfig {
        cases: (ProptestConfig::default().cases / 16).max(4),
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(&history(), |h| {
            let (r, calls) = run(&h, 3);
            let mut sequential = HashMap::new();
            for thread in &calls {
                for w in thread.windows(2) {
                    prop_assert!(
                        w[0].1 <= w[1].1,
                        "a later call of one thread saw an earlier cut"
                    );
                }
                for (id, cut, history, current) in thread {
                    let (want_history, want_current) =
                        sequential.entry((*id, *cut)).or_insert_with(|| {
                            r.with(|i, _| {
                                let every = i.facts(SeriesId(*id), all(), View::History)?;
                                let at = i.facts(SeriesId(*id), all(), View::AsOf(*cut))?;
                                let upto = every.into_iter().filter(|f| f.position <= *cut);
                                Ok((upto.collect::<Vec<_>>(), at))
                            })
                            .unwrap()
                        });
                    prop_assert_eq!(history, &*want_history);
                    prop_assert_eq!(current, &*want_current);
                }
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn many_calls_of_every_kind_from_many_threads_while_the_writer_writes() {
    const SERIES: u32 = 20;
    const BATCHES: i64 = 100;
    let (store, env, _) = store(SERIES);
    let store = Arc::new(store);
    let r = reader(&env, LONG, LONG);
    let written = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = (0..6u32)
        .map(|k| {
            let (r, store, written) = (r.clone(), store.clone(), written.clone());
            thread::spawn(move || {
                let mut last = Position::default();
                let mut calls = 0u32;
                loop {
                    let done = written.load(Ordering::SeqCst);
                    let id = (k + calls) % SERIES + 1;
                    match calls % 4 {
                        0 => {
                            let (cut, history, _) = facts_at_cut(&r, id).unwrap();
                            assert!(cut >= last);
                            assert!(history.iter().all(|f| f.position <= cut));
                            last = cut;
                        }
                        1 => {
                            let st = r.refresh().unwrap();
                            assert!(st.durable >= last);
                            last = st.durable;
                        }
                        2 => {
                            crate::durable::live(&r, SeriesId(id)).unwrap();
                            r.losses().unwrap();
                        }
                        _ => {
                            store.series(SeriesKey::Id(SeriesId(id))).unwrap();
                        }
                    }
                    calls += 1;
                    if done {
                        return calls;
                    }
                }
            })
        })
        .collect();
    for n in 0..BATCHES {
        let rs: Vec<_> = (1..=SERIES)
            .filter(|id| (i64::from(*id) + n) % 3 == 0)
            .map(|id| reading(id, n, n as f64))
            .collect();
        block_on(store.emit(&rs)).unwrap();
    }
    written.store(true, Ordering::SeqCst);
    let calls: u32 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(calls >= 6);
    let total: usize = (1..=SERIES)
        .map(|id| facts_at_cut(&r, id).unwrap().1.len())
        .sum();
    assert_eq!(
        total,
        (0..BATCHES)
            .map(|n| (1..=i64::from(SERIES))
                .filter(|id| (id + n) % 3 == 0)
                .count())
            .sum()
    );
    assert!(r.cuts_held() <= 2, "{}", r.cuts_held());
    if let Some(s) = Arc::into_inner(store) {
        block_on(s.shutdown()).unwrap();
    }
}
