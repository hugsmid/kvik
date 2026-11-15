//! When log syncs start under `min_sync_interval` (spec §3 Group commit, Q-310): the synchronous writer core driven
//! with explicit times, woken as the writer thread wakes, late where a real thread is.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::MemFs;
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{Call, CallSource, IngestQueue, LogWriter, WriterConfig, create_segment};

const T0: i64 = 1_767_225_600_000_000_000;
const US: Duration = Duration::from_micros(1);
const MS: Duration = Duration::from_millis(1);
const INTERVAL: Duration = Duration::from_millis(10);
/// How long a batch sync keeps the writer busy.
const SYNC: Duration = Duration::from_millis(2);
/// Loads start well after the writer opens, so no bound applies to their first call.
const START: Duration = Duration::from_secs(1);
/// A seal interval no test reaches.
const NO_SEAL: Duration = Duration::from_secs(300);

fn fact(t: i64) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64(t as u64),
        changed_at: None,
    })
}

struct Sim {
    fs: MemFs,
    w: LogWriter,
    q: IngestQueue,
    /// Start of every batch sync.
    starts: Vec<Duration>,
    /// Start of the sync that covered each call, by call id.
    covered: BTreeMap<u64, Duration>,
    pushed: i64,
}

impl Sim {
    fn new(seal_interval: Duration) -> Self {
        let fs = MemFs::new();
        let new = NewStore {
            store_id: [7; 16],
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        let store =
            StoreDir::open(Arc::new(fs.clone()), &PathBuf::from("store"), Some(&new)).unwrap();
        let cfg = WriterConfig {
            segment_size: 1 << 20,
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            seal_interval,
            ..WriterConfig::default()
        };
        assert_eq!(cfg.min_sync_interval, INTERVAL);
        let w = LogWriter::open(store, cfg, Some([9; 16]), Duration::ZERO, T0).unwrap();
        let mut s = Self {
            fs,
            w,
            q: IngestQueue::new(1 << 16),
            starts: Vec::new(),
            covered: BTreeMap::new(),
            pushed: 0,
        };
        s.create();
        s
    }

    fn create(&mut self) {
        if let Some(req) = self.w.segment_request(T0) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    fn push(&mut self) -> u64 {
        self.pushed += 1;
        let call = Call::new(vec![fact(T0 + self.pushed)], self.w.config().call_limit()).unwrap();
        self.q.push(call).unwrap()
    }

    /// One turn of the writer at `now`: its next deadline, and whether it synced.
    fn turn(&mut self, now: Duration) -> (Option<Duration>, bool) {
        let (batches, syncs) = (self.w.stats().batches, self.w.stats().syncs);
        let step = self.w.step(now, T0 + now.as_nanos() as i64, &mut self.q);
        for (id, r) in step.done {
            r.unwrap();
            self.covered.insert(id, now);
        }
        self.create();
        if self.w.stats().batches > batches {
            self.starts.push(now);
        }
        (step.next, self.w.stats().syncs > syncs)
    }
}

/// When the writer thread next turns: at once if busy past its deadline, else `late` after the deadline.
fn timed(next: Option<Duration>, free: Duration, late: Duration) -> Option<Duration> {
    next.map(|t| if t <= free { free } else { t + late })
}

/// One call every `INTERVAL` from `START`, the writer woken `wake(k)` after call `k` arrives and `late` after each
/// deadline. Returns each call's wait from its arrival to the start of the sync that covered it.
fn one_call_per_interval(
    n: usize,
    wake: impl Fn(usize) -> Duration,
    late: Duration,
    seal_interval: Duration,
) -> Vec<Duration> {
    let mut s = Sim::new(seal_interval);
    let arrivals: Vec<Duration> = (0..n).map(|k| START + INTERVAL * k as u32).collect();
    let mut ids = Vec::new();
    let (mut k, mut free, mut next) = (0, Duration::ZERO, None);
    for _ in 0..100 * n {
        if k == n && s.q.is_empty() {
            let waits = ids
                .iter()
                .zip(&arrivals)
                .map(|(id, a)| s.covered[id] - *a)
                .collect();
            return waits;
        }
        let by_call = (k < n).then(|| arrivals[k] + wake(k));
        let at = [by_call, timed(next, free, late)]
            .into_iter()
            .flatten()
            .min()
            .unwrap()
            .max(free);
        while k < n && arrivals[k] <= at {
            ids.push(s.push());
            k += 1;
        }
        let (n2, synced) = s.turn(at);
        free = at + if synced { SYNC } else { Duration::ZERO };
        next = n2;
    }
    panic!("the calls do not drain");
}

#[test]
fn late_wakes_do_not_add_up_at_one_call_per_interval() {
    let waits = one_call_per_interval(
        300,
        |k| if k % 50 == 1 { 300 * US } else { 20 * US },
        100 * US,
        NO_SEAL,
    );
    let worst = waits.iter().max().unwrap();
    assert!(*worst <= 300 * US, "a call waited {worst:?}");
}

#[test]
fn a_stalled_turn_delays_no_call_after_the_stall_is_caught_up() {
    for stall in [3, 8, 30] {
        let waits = one_call_per_interval(
            300,
            |k| if k == 20 { stall * MS } else { 20 * US },
            100 * US,
            NO_SEAL,
        );
        let worst = waits[25..].iter().max().unwrap();
        assert!(
            *worst <= 20 * US,
            "after a {stall} ms stall a call waited {worst:?}"
        );
    }
}

#[test]
fn a_timed_seal_delays_at_most_one_call_by_under_half_an_interval() {
    for (phase, delayed) in [(3, 0), (7, 1)] {
        let seal = Duration::from_secs(2) + MS * phase;
        let waits = one_call_per_interval(300, |_| 20 * US, 100 * US, seal);
        let late: Vec<_> = waits.iter().filter(|w| **w > 20 * US).collect();
        assert_eq!(
            late.len(),
            delayed,
            "a seal {phase} ms into the interval: {late:?}"
        );
        assert!(late.iter().all(|w| **w < INTERVAL / 2), "{late:?}");
    }
}

#[test]
fn syncs_under_saturation_keep_the_rate_bound_through_late_wakes_and_stalls() {
    let mut s = Sim::new(NO_SEAL);
    let mut at = Duration::ZERO;
    for n in 0..800u32 {
        if s.q.is_empty() {
            s.push();
        }
        let (next, synced) = s.turn(at);
        let mut free = at + if synced { SYNC } else { Duration::ZERO };
        if n == 100 {
            free += 8 * MS;
        }
        if n == 300 {
            free += 30 * MS;
        }
        // A call arriving during a sync wakes the writer as the sync ends.
        at = if synced {
            free
        } else {
            timed(next, free, US * (n % 5) * 400).unwrap()
        };
    }
    let st = &s.starts;
    assert!(st.len() > 300);
    for i in 0..st.len() {
        for j in i + 1..st.len() {
            let floor = (INTERVAL * (j - i - 1) as u32).max(INTERVAL / 2);
            assert!(
                st[j] - st[i] >= floor,
                "syncs {i} to {j} span {:?}",
                st[j] - st[i]
            );
        }
    }
}

#[test]
fn after_a_quiet_period_a_burst_waits_half_an_interval_and_forms_one_batch() {
    let mut s = Sim::new(NO_SEAL);
    let a = s.push();
    s.turn(START);
    assert_eq!(
        s.covered[&a], START,
        "the first call after a quiet period is synced at once"
    );
    let mut burst = Vec::new();
    for i in 1..5u32 {
        burst.push(s.push());
        let (next, _) = s.turn(START + MS * i);
        assert_eq!(next, Some(START + INTERVAL / 2));
    }
    s.turn(START + INTERVAL / 2);
    assert!(burst.iter().all(|c| s.covered[c] == START + INTERVAL / 2));
    assert_eq!(s.w.stats().batches, 2, "the burst forms one batch");
    s.push();
    let (next, _) = s.turn(START + INTERVAL / 2 + SYNC);
    assert_eq!(
        next,
        Some(START + INTERVAL),
        "the burst's sync was due at the first reading"
    );
}
