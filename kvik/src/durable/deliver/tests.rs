use std::path::Path;
use std::sync::Mutex;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::lease::LeaseSlot;
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
        vec![Item::Entry(Entry::Alias {
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

    /// A manual clock that reports every read of its monotonic clock.
    #[derive(Debug)]
    struct Reported(ManualClock, std::sync::mpsc::SyncSender<()>);

    impl crate::env::Clock for Reported {
        fn wall(&self) -> Timestamp {
            crate::env::Clock::wall(&self.0)
        }
        fn monotonic(&self) -> crate::env::Monotonic {
            let _ = self.1.try_send(());
            crate::env::Clock::monotonic(&self.0)
        }
        fn synchronised(&self) -> bool {
            crate::env::Clock::synchronised(&self.0)
        }
    }

    /// A call that waits for the deliverer while the clock moves acts at the time after its wait (the delivery
    /// differential's flake: a condition runner created with a time from before the wait).
    #[test]
    fn a_call_that_waits_for_the_deliverer_acts_at_the_time_after_its_wait() {
        let (mut env, dir) = store();
        let clock = ManualClock::new(Timestamp(T0 + 7_200 * S));
        let (tx, rx) = std::sync::mpsc::sync_channel(1_024);
        env.clock = Arc::new(Reported(clock.clone(), tx));
        let d = open(&env, dir);
        let clocks = Arc::new(Clocks::new(env.clock.clone()));
        let hub = Hub::new(d, env.clone(), Path::new(dir), clocks);
        while rx.try_recv().is_ok() {}
        let held = hub.hold();
        let h = hub.clone();
        let call = std::thread::spawn(move || h.run_quiet(|_, c| c.mono));
        // A call that reads the clocks before it takes the deliverer reads them now; one that reads them after
        // cannot, and the wait only bounds how long this test gives the first kind.
        let _read_early = rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok();
        clock.advance(std::time::Duration::from_secs(2_602));
        drop(held);
        let mono = call.join().unwrap();
        assert_eq!(
            mono,
            i64::try_from(crate::env::Clock::monotonic(&clock).0).unwrap(),
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
            None,
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
}
