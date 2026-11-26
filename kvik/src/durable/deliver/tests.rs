use std::path::Path;
use std::sync::Mutex;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::lease::LeaseSlot;
use kvik_log::manifest::LossCause;
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{Clock, Log, LogWriter, SystemClock, WriterConfig};

use super::*;

#[derive(Debug, Default)]
struct Manual(Mutex<Duration>);

impl Clock for Manual {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn wall(&self) -> i64 {
        1_767_225_600_000_000_000
    }
}

fn counter(fs: &MemFs, r: &LogReader) -> u64 {
    let f = fs.open(r.lease_path(), OpenMode::Read).unwrap();
    let mut b = vec![0u8; 4_096];
    let n = f.read_at(&mut b, 0).unwrap();
    LeaseSlot::decode_file(&b[..n], &[1; 16])
        .unwrap()
        .refresh_counter
}

#[test]
fn a_reader_at_the_writers_durable_position_only_renews_its_lease_when_due() {
    let fs = MemFs::new();
    let new = NewStore {
        store_id: [1; 16],
        created_at: 0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "test".into(),
    };
    let store = StoreDir::open(Arc::new(fs.clone()), Path::new("s"), Some(&new)).unwrap();
    let w = LogWriter::open(
        store,
        WriterConfig::default(),
        Some([2; 16]),
        Duration::ZERO,
        0,
    )
    .unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let alias = |id| {
        vec![Item::from(Entry::Alias {
            id,
            name: format!("a{id}"),
        })]
    };
    log.submit(alias(1)).unwrap().wait().unwrap();
    let clock = Arc::new(Manual::default());
    let mut cfg = LogReaderConfig::new(
        "s",
        Arc::new(fs.clone()),
        clock.clone(),
        Some([2; 16]),
        [7; 16],
    );
    cfg.lease_refresh = Duration::from_secs(2);
    let mut r = LogReader::open(cfg).unwrap();
    assert_eq!(read_new(&mut r, None).unwrap().unwrap().events.len(), 1);
    let at = counter(&fs, &r);
    let durable = log.stats().durable;
    assert!(read_new(&mut r, Some(durable)).unwrap().is_none());
    assert_eq!(counter(&fs, &r), at);
    *clock.0.lock().unwrap() += Duration::from_millis(1_100);
    assert!(read_new(&mut r, Some(durable)).unwrap().is_none());
    assert_eq!(
        counter(&fs, &r),
        at + 1,
        "renewed once half of lease_refresh passed"
    );
    log.submit(alias(2)).unwrap().wait().unwrap();
    let u = read_new(&mut r, Some(log.stats().durable))
        .unwrap()
        .unwrap();
    assert_eq!(u.events.len(), 1);
    assert_eq!(
        counter(&fs, &r),
        at + 1,
        "a refresh writes the lease only when its epoch changes or half of lease_refresh passed"
    );
    *clock.0.lock().unwrap() += Duration::from_millis(1_100);
    log.submit(alias(3)).unwrap().wait().unwrap();
    assert!(
        read_new(&mut r, Some(log.stats().durable))
            .unwrap()
            .is_some()
    );
    assert_eq!(counter(&fs, &r), at + 2);
    log.shutdown().unwrap();
}

mod bounded {
    use std::future::Future;
    use std::task::{Context, Poll, Wake};

    use super::*;
    use crate::durable::history::WINDOW_FACTS;
    use crate::env::ManualClock;
    use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};
    use crate::time::Timestamp;
    use crate::types::{Quality, Reading, SeriesId, Value};
    use crate::{Store, StoreConfig};

    const T0: i64 = 1_767_225_600_000_000_000;
    const S: i64 = 1_000_000_000;

    fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut f = std::pin::pin!(f);
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    fn gauge(name: &str) -> SeriesSpec {
        SeriesSpec::new(
            name,
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        )
    }

    /// A manual clock whose next wall-clock read, once armed, takes its value, reports it and then waits to be let go:
    /// the pause between a clock read and what follows it.
    #[derive(Debug)]
    struct Paused {
        clock: ManualClock,
        armed: std::sync::atomic::AtomicBool,
        read: std::sync::mpsc::SyncSender<()>,
        go: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl crate::env::Clock for Paused {
        fn wall(&self) -> Timestamp {
            let v = crate::env::Clock::wall(&self.clock);
            if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = self.read.try_send(());
                let _ = self.go.lock().unwrap().recv();
            }
            v
        }
        fn monotonic(&self) -> crate::env::Monotonic {
            crate::env::Clock::monotonic(&self.clock)
        }
        fn synchronised(&self) -> bool {
            crate::env::Clock::synchronised(&self.clock)
        }
    }

    /// A call that waits for the deliverer while the clock moves acts at the time after its wait: a condition runner
    /// created with a time from before the wait would fire early.
    #[test]
    fn a_call_that_waits_for_the_deliverer_acts_at_the_time_after_its_wait() {
        let (mut env, dir) = store();
        let clock = ManualClock::new(Timestamp(T0 + 7_200 * S));
        let (read_tx, read_rx) = std::sync::mpsc::sync_channel(1);
        let (go_tx, go_rx) = std::sync::mpsc::sync_channel(1);
        let paused = Arc::new(Paused {
            clock: clock.clone(),
            armed: std::sync::atomic::AtomicBool::new(false),
            read: read_tx,
            go: Mutex::new(go_rx),
        });
        env.clock = paused.clone();
        let d = open(&env, dir);
        let clocks = Arc::new(Clocks::new(env.clock.clone()));
        let hub = Hub::new(d, env.clone(), Path::new(dir), clocks);
        let held = hub.hold();
        paused
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let h = hub.clone();
        let call = std::thread::spawn(move || h.run_quiet(|_, c| c.wall));
        // A call that reads the clock before it takes the deliverer is paused in that read now, its value taken; one
        // that reads it after cannot have read it yet, and the wait only bounds how long this test gives the first.
        let _read_early = read_rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok();
        clock.advance(std::time::Duration::from_secs(2_602));
        go_tx.send(()).unwrap();
        drop(held);
        let wall = call.join().unwrap();
        assert_eq!(
            wall,
            clock.wall().0,
            "the call acted with a clock read before it waited"
        );
    }

    /// A store holding two hours of one reading a second on a gauge (1), a versioned series (2) and a gauge (3) whose
    /// readings after its first are bad; then late versions of series 2 at `T0 + 10 s`, below every window.
    fn store() -> (Env, &'static str) {
        let clock = ManualClock::new(Timestamp(T0 + 7_200 * S));
        let env = Env::test(clock);
        let dir = "bounded-deliverer";
        let s = block_on(Store::open(
            StoreConfig::new(dir, "Etc/UTC")
                .create_if_missing(true)
                .env(env.clone()),
        ))
        .unwrap();
        for spec in [
            gauge("a/b/one"),
            gauge("a/b/two").versioned(ChangedAtKind::Version),
            gauge("a/b/three"),
        ] {
            block_on(s.define_series(spec)).unwrap();
        }
        for chunk in (0..7_200).collect::<Vec<i64>>().chunks(600) {
            let mut readings = Vec::new();
            for &k in chunk {
                let t = Timestamp(T0 + k * S);
                let three = if k == 0 { 3.0 } else { f64::NAN };
                readings.extend([
                    Reading::new(SeriesId(1), t, Value::F64(k as f64)),
                    Reading {
                        changed_at: Some(5),
                        ..Reading::new(SeriesId(2), t, Value::F64(k as f64))
                    },
                    Reading::new(SeriesId(3), t, Value::F64(three)).with_quality(Quality::Good),
                ]);
            }
            block_on(s.emit(&readings)).unwrap();
        }
        for c in [3, 9] {
            let r = Reading {
                changed_at: Some(c),
                ..Reading::new(SeriesId(2), Timestamp(T0 + 10 * S), Value::F64(c as f64))
            };
            block_on(s.emit(&[r])).unwrap();
        }
        block_on(s.shutdown()).unwrap();
        (env, dir)
    }

    fn open(env: &Env, dir: &str) -> Deliverer {
        let clocks = Clocks::new(env.clock.clone());
        Deliverer::open(
            Path::new(dir),
            env,
            &clocks,
            [0; 16],
            4_096,
            64,
            usize::MAX,
            Arc::default(),
            None,
            0,
        )
        .unwrap()
    }

    #[test]
    fn the_deliverer_holds_its_recent_window_and_reads_older_facts_from_the_log() {
        let (env, dir) = store();
        let d = open(&env, dir);
        // Fifteen minutes of one reading a second, per series.
        assert!(d.recent.held() <= 3 * 901, "{}", d.recent.held());
        assert!(d.recent.held() <= 3 * WINDOW_FACTS);
        let now = T0 + 7_200 * S;
        // Series 3's live-state fact is its first, two hours before the window: found in two reads, the second
        // reaching sixteen times as far back as the first.
        let reads = d.slow.get();
        let live = d.live_fact(3, now).unwrap();
        assert_eq!((live.row.t, live.position.sequence > 0), (T0, true));
        assert_eq!(d.slow.get() - reads, 2);
        assert!(!d.incomplete.get());
        assert_eq!(d.live_fact(1, now).unwrap().row.t, T0 + 7_199 * S);
        // The late versions: 3 was outranked by the version 5 recorded before it, 9 was not.
        let clocks = Clocks::new(env.clock.clone());
        let ctx = Ctx {
            watch: None,
            env: &env,
            dir: Path::new(dir),
            clocks: &clocks,
            now,
            wall: now,
            mono: 0,
        };
        let late: Vec<(Option<u64>, bool)> = d
            .facts_between(LogPos::default(), next(d.head), &ctx)
            .unwrap()
            .into_iter()
            .filter(|(s, _, _)| s.row.series_id == 2 && s.row.t == T0 + 10 * S)
            .map(|(s, o, u)| {
                assert!(!u);
                (s.row.changed_at, o)
            })
            .collect();
        assert_eq!(late, [(Some(5), false), (Some(3), true), (Some(9), false)]);
        assert!(d.slow.get() > 0);
    }

    #[test]
    fn the_open_builds_no_record_and_holds_none() {
        let (env, dir) = store();
        let d = open(&env, dir);
        assert_eq!(d.records, 0);
        assert!(d.items.is_empty());
        assert_eq!(d.items_from, next(d.head));
        assert!(d.head.sequence > 3 * 7_200);
    }

    #[test]
    fn no_registry_file_is_made_while_a_batch_up_to_its_position_waits() {
        let (env, dir) = store();
        let mut d = open(&env, dir);
        let through = d.head;
        assert!(d.registry_file_through(through, through, 0).is_some());
        let waiting = |last: LogPos| ReadBatch {
            file_id: 1,
            offset: 0,
            first: last,
            last,
            commit_time: 0,
            content: kvik_log::content::BatchContent {
                first_sequence: last.sequence,
                received_at: 0,
                items: Vec::new(),
            },
        };
        d.pending.push_back(waiting(through));
        assert!(d.registry_file_through(through, through, 0).is_none());
        d.pending.clear();
        d.pending.push_back(waiting(next(through)));
        assert!(d.registry_file_through(through, through, 0).is_some());
        assert!(d.registry_file_through(next(through), through, 0).is_none());
    }

    #[test]
    fn no_routing_registry_file_is_made_before_the_log_is_taken_up_to_its_position() {
        let (env, dir) = store();
        let mut d = open(&env, dir);
        let clocks = Clocks::new(env.clock.clone());
        let now = T0 + 7_200 * S;
        let ctx = Ctx {
            watch: None,
            env: &env,
            dir: Path::new(dir),
            clocks: &clocks,
            now,
            wall: now,
            mono: 0,
        };
        let through = d.head;
        assert!(d.routing_registry_file(&ctx, next(through), 0).is_none());
        assert!(d.routing_registry_file(&ctx, through, 0).is_some());
    }

    #[test]
    fn a_fact_taken_below_the_window_is_judged_outranked_from_the_log() {
        let (env, dir) = store();
        let d = open(&env, dir);
        let read = HashMap::new();
        // Below the window, without the key read: nothing known, so not outranked, and flagged.
        assert_eq!(
            d.outranked((2, T0 + 10 * S), next(d.head), 4, &read),
            (false, true)
        );
        let found = d.read_keys(&[(2, T0 + 10 * S)]);
        let read: HashMap<_, _> = found
            .facts
            .into_iter()
            .map(|(k, f)| (k, (f, false)))
            .collect();
        assert_eq!(
            d.outranked((2, T0 + 10 * S), next(d.head), 4, &read),
            (true, false)
        );
        assert_eq!(
            d.outranked((2, T0 + 10 * S), next(d.head), 9, &read),
            (false, false)
        );
        // Read but not all of it: an outranking fact found is enough; none found is flagged.
        let partial: HashMap<_, _> = read.into_iter().map(|(k, (f, _))| (k, (f, true))).collect();
        assert_eq!(
            d.outranked((2, T0 + 10 * S), next(d.head), 4, &partial),
            (true, false)
        );
        assert_eq!(
            d.outranked((2, T0 + 10 * S), next(d.head), 9, &partial),
            (false, true)
        );
    }

    #[test]
    fn a_read_before_the_window_takes_only_what_has_been_taken() {
        let (env, dir) = store();
        let d = open(&env, dir);
        // A good reading of series 3 arrives below its window after the deliverer last read the log.
        let s = block_on(Store::open(
            StoreConfig::new(dir, "Etc/UTC").env(env.clone()),
        ))
        .unwrap();
        let late = Reading::new(SeriesId(3), Timestamp(T0 + 10 * S), Value::F64(7.0));
        block_on(s.emit(&[late])).unwrap();
        block_on(s.shutdown()).unwrap();
        let live = d.live_fact(3, T0 + 7_200 * S).unwrap();
        assert_eq!(live.row.t, T0, "a fact not yet taken is not yet in force");
    }

    #[test]
    fn a_read_before_the_window_that_fails_marks_the_evaluation_incomplete() {
        let (env, dir) = store();
        let d = open(&env, dir);
        let reader = d.reader.borrow_mut().take();
        assert!(d.live_fact(3, T0 + 7_200 * S).is_none());
        assert!(d.incomplete.get());
        drop(reader);
    }

    #[test]
    fn a_consumer_resumes_over_several_parts_of_the_log_receiving_each_fact_once_in_order() {
        use crate::consume::{DeliveryKind, Funnel, Select, Start};
        let env = Env::test(ManualClock::new(Timestamp(T0 + 40_000 * S)));
        let dir = "resume-parts";
        let config = || {
            let mut c = StoreConfig::new(dir, "Etc/UTC")
                .create_if_missing(true)
                .env(env.clone());
            c.log.min_sync_interval = std::time::Duration::ZERO;
            c
        };
        let emit = |s: &Store, from: i64, to: i64| {
            for chunk in (from..to).collect::<Vec<i64>>().chunks(1_000) {
                let rs: Vec<Reading> = chunk
                    .iter()
                    .flat_map(|&k| {
                        (1..=2).map(move |id| {
                            Reading::new(SeriesId(id), Timestamp(T0 + k * S), Value::F64(k as f64))
                        })
                    })
                    .collect();
                block_on(s.emit(&rs)).unwrap();
            }
        };
        let s = block_on(Store::open(config())).unwrap();
        block_on(s.define_series(gauge("a/b/one"))).unwrap();
        block_on(s.define_series(gauge("a/b/two"))).unwrap();
        let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        block_on(c.route(Funnel::facts(Select::All))).unwrap();
        emit(&s, 0, 10);
        while let Some(d) = c.try_next().unwrap() {
            block_on(c.confirm(d.id)).unwrap();
        }
        drop(c);
        block_on(s.shutdown()).unwrap();
        drop(s);
        let s = block_on(Store::open(config())).unwrap();
        emit(&s, 10, 36_000);
        block_on(s.shutdown()).unwrap();
        drop(s);
        let s = block_on(Store::open(config())).unwrap();
        let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        let mut got = Vec::new();
        while let Some(d) = c.try_next().unwrap() {
            if let DeliveryKind::Fact(f) = &d.kind {
                got.push((f.position, f.series.0, f.t));
            }
            block_on(c.confirm(d.id)).unwrap();
        }
        assert_eq!(got.len(), 2 * (36_000 - 10));
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!((got[0].1, got[0].2), (1, Timestamp(T0 + 10 * S)));
        block_on(s.shutdown()).unwrap();
    }

    #[test]
    fn a_condition_a_void_brings_back_into_force_is_evaluated_at_the_next_poll() {
        use crate::config::LogConfig;
        use crate::consume::{Condition, ConditionState, DeliveryKind, Funnel, Select, Start};
        use crate::durable::{Emitted, Writer, tests::wait};
        use crate::types::Intent;

        let mut env = Env::test(ManualClock::new(Timestamp(T0 + 100 * S)));
        env.fs = Arc::new(kvik_log::fs::MemFs::new());
        let log = LogConfig {
            min_sync_interval: std::time::Duration::ZERO,
            ..LogConfig::default()
        };
        let config = StoreConfig::new("void-runners", "Etc/UTC")
            .create_if_missing(true)
            .env(env)
            .log(log);
        let w = Writer::open(&config).unwrap();
        let g = w.define(&gauge("a/b/g")).unwrap();
        let emit = |secs: i64, v: f64| {
            let r = Reading::new(g, Timestamp(T0 + secs * S), Value::F64(v));
            match w.emit(&[r], Intent::Normal, true) {
                Ok(Emitted::Queued(q)) => {
                    wait(q).unwrap();
                }
                Ok(Emitted::Ready(_)) => {}
                Err(e) => panic!("{e:?}"),
            }
        };
        let drain = || {
            let mut out = Vec::new();
            while let Some(d) = w.poll_delivery("c", None).unwrap() {
                w.confirm("c", d.id).unwrap();
                out.push(d.kind);
            }
            out
        };
        w.take_consumer("c", Start::Now, &Select::None).unwrap();
        w.route("c", &Funnel::facts(Select::Ids(vec![g]))).unwrap();
        let cond = w
            .route(
                "c",
                &Funnel {
                    series: Select::Ids(vec![g]),
                    operator: None,
                    condition: Some(Condition::above(10.0)),
                },
            )
            .unwrap();
        w.unroute("c", cond).unwrap();
        emit(0, 1.0);
        drain();
        // The removal is voided: the condition is in force again, without a routing entry being taken.
        let hub = w.hub.clone().unwrap();
        hub.run_quiet(|d, _| {
            let removal = d
                .entries
                .iter()
                .rev()
                .find(|(_, e)| matches!(e, Entry::Routing(_)))
                .map(|(p, _)| *p)
                .unwrap();
            d.void(before(removal));
            assert_eq!(d.view().funnels("c").count(), 2);
        });
        emit(1, 20.0);
        let triggers: Vec<_> = drain()
            .into_iter()
            .filter_map(|k| match k {
                DeliveryKind::Trigger(t) => Some((t.funnel, t.state)),
                _ => None,
            })
            .collect();
        assert_eq!(triggers, vec![(cond, ConditionState::True)]);
        w.shutdown().unwrap();
    }
}

fn recovery_file(generation: u32) -> kvik_log::registry::RegistryFile {
    let entry = |after| {
        Entry::Recovery(kvik_log::entries::Recovery {
            after,
            stamp: None,
            boot_id: [0; 16],
            tail_bytes: 0,
            readable_commits: 0,
            mark_state: kvik_log::entries::MarkState::Readable,
            id_floor: 0,
            consumers: vec!["c".into()],
        })
    };
    kvik_log::registry::RegistryFile {
        generation,
        through: 20,
        max_series_id: 0,
        entries: vec![(5, 0, entry(4)), (12, 0, entry(9))],
    }
}

fn lost_range(first: u64, last: u64) -> kvik_log::manifest::LostRange {
    kvik_log::manifest::LostRange {
        identity: (1, first),
        first: (1, first),
        last: (1, last),
        file_id: 0,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [0; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: None,
    }
}

#[test]
fn kept_resets_are_the_registry_files_recovery_entries_of_the_writers_generation_outside_lost_ranges()
 {
    let p = |s| LogPos {
        generation: 1,
        sequence: s,
    };
    let file = recovery_file(1);
    let all: Vec<_> = kept_resets(&file, 1, &[])
        .map(|(id, fa, c)| (id, fa, c.to_vec()))
        .collect();
    let c = vec!["c".to_string()];
    assert_eq!(all, vec![(p(5), p(4), c.clone()), (p(12), p(9), c.clone())]);
    assert_eq!(
        kept_resets(&file, 0, &[]).count(),
        0,
        "another generation's file"
    );
    assert_eq!(kept_resets(&recovery_file(0), 1, &[]).count(), 0);
    let held: Vec<_> = kept_resets(&file, 1, &[lost_range(10, 14)])
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(held, vec![p(5)], "an entry a recorded lost range holds");
}
