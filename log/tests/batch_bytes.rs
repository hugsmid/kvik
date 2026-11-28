//! How calls become a batch (`spec.md` §3 Group commit, `format.md` A.4): a fixed history through every way a batch
//! is formed or its calls put back, checked against the batch as the concatenation of its calls' entries and
//! against golden digests of every file the writer leaves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{
    Definition, Entry, IntervalLength, Kind, MarkState, Recovery, Retention, Timing, Versioning,
};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value, ValueType};
use kvik_log::fs::MemFs;
use kvik_log::name::SegmentName;
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Ack, Call, CallSource, CommitError, IngestQueue, LogWriter, Wait, WriterConfig, create_segment,
    seal_segment_described,
};

const STORE_ID: [u8; 16] = [7; 16];
const BOOT: [u8; 16] = [9; 16];
const T0: i64 = 1_767_225_600_000_000_000;
const SECOND: i64 = 1_000_000_000;
/// Rows of 255-byte text: a call of this many is over 4 MiB of data, so two never encode together (8 MiB).
const BIG: usize = 16_000;

fn fact(series: u32, t: i64, text: usize) -> Item {
    Item::Fact(FactRow {
        series_id: series,
        t,
        quality: if t % 3 == 0 {
            Quality::Suspect
        } else {
            Quality::Good
        },
        flags: FactFlags {
            late: t % 5 == 0,
            ..FactFlags::default()
        },
        value: if text > 0 {
            Value::Text(char::from(b'a' + (t % 26) as u8).to_string().repeat(text))
        } else {
            Value::F64((t as f64 * 0.5).to_bits())
        },
        changed_at: None,
    })
}

fn facts(series: u32, from: i64, n: usize, text: usize) -> Vec<Item> {
    (0..n as i64)
        .map(|k| fact(series, from + k * SECOND, text))
        .collect()
}

fn definition(id: u32, interval: Option<IntervalLength>) -> Item {
    Item::from(Entry::Definition(Box::new(Definition {
        id,
        name: format!("s{id}"),
        kind: if interval.is_some() {
            Kind::Interval
        } else {
            Kind::Gauge
        },
        value_type: ValueType::F64,
        scale: None,
        unit: "u".into(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: SECOND,
        state_names: vec![],
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    })))
}

fn recovery() -> Item {
    Item::from(Entry::Recovery(Recovery {
        after: 0,
        stamp: None,
        boot_id: BOOT,
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: MarkState::Readable,
        id_floor: 0,
        consumers: vec![],
    }))
}

/// Hands over every call at once, ignoring the batch rules, so that calls which do not encode together reach the
/// writer.
struct Greedy(Vec<Call>);

impl CallSource for Greedy {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn take(&mut self, _: u64) -> Vec<Call> {
        std::mem::take(&mut self.0)
    }
    fn put_back(&mut self, mut calls: Vec<Call>) {
        calls.append(&mut self.0);
        self.0 = calls;
    }
}

struct H {
    fs: MemFs,
    w: LogWriter,
    /// Gives every call its id, for the normal path and for [`Greedy`] alike.
    q: IngestQueue,
    now: Duration,
    wall: i64,
    calls: BTreeMap<u64, Vec<Item>>,
    done: Vec<(u64, Result<Ack, CommitError>)>,
    maint: bool,
}

impl H {
    fn new() -> Self {
        Self::with_min_sync(Duration::ZERO)
    }

    fn with_min_sync(min_sync_interval: Duration) -> Self {
        let fs = MemFs::new();
        let new = NewStore {
            store_id: STORE_ID,
            created_at: T0,
            site_zone: "Etc/UTC".into(),
            tzdata: "2026a".into(),
            software: "kvik test".into(),
        };
        let store = StoreDir::open(Arc::new(fs.clone()), Path::new("store"), Some(&new)).unwrap();
        let cfg = WriterConfig {
            segment_size: 12 << 20,
            min_sync_interval,
            disk_critical: 0,
            disk_low_bytes: 0,
            relaxed_limits: true,
            ..WriterConfig::default()
        };
        let w = LogWriter::open(store, cfg.clone(), Some(BOOT), Duration::ZERO, T0).unwrap();
        let mut h = Self {
            fs,
            w,
            q: IngestQueue::new(cfg.ingest_queue),
            now: Duration::ZERO,
            wall: T0,
            calls: BTreeMap::new(),
            done: Vec::new(),
            maint: true,
        };
        h.run_maint();
        h
    }

    fn submit(&mut self, items: Vec<Item>) {
        let call = Call::new(items.clone(), self.w.config().call_limit()).unwrap();
        let id = self.q.push(call).unwrap();
        self.calls.insert(id, items);
    }

    fn greedy(&mut self, calls: Vec<Vec<Item>>) -> Greedy {
        for items in calls {
            self.submit(items);
        }
        Greedy(self.q.drain())
    }

    fn run_maint(&mut self) {
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
        for job in self.w.take_seal_jobs(self.now) {
            let r = seal_segment_described(&self.fs, &job, &self.w.halt());
            self.w.seal_done(job, r, self.now);
        }
        if let Some(req) = self.w.segment_request(self.wall) {
            let r = create_segment(&self.fs, &req, &self.w.halt());
            self.w.segment_created(r);
        }
    }

    /// One writer turn from `src`, the clock moved on by a millisecond; returns how many calls finished.
    fn step(&mut self, src: &mut dyn CallSource) -> usize {
        self.now += Duration::from_millis(1);
        self.wall += 1_000_000;
        let s = self.w.step(self.now, self.wall, src);
        let n = s.done.len();
        self.done.extend(s.done);
        if self.maint {
            self.run_maint();
        }
        n
    }

    fn step_queue(&mut self) -> usize {
        let mut q = std::mem::replace(&mut self.q, IngestQueue::new(1));
        let n = self.step(&mut q);
        self.q = q;
        n
    }

    fn items_of(&self, src: &Greedy) -> Vec<(u64, Vec<Item>)> {
        src.0.iter().map(|c| (c.id(), c.items().to_vec())).collect()
    }

    fn assert_intact(&self, src: &Greedy, ids: &[u64]) {
        let want: Vec<_> = ids.iter().map(|id| (*id, self.calls[id].clone())).collect();
        assert_eq!(self.items_of(src), want, "calls put back whole, in order");
    }
}

fn history() -> H {
    let mut h = H::new();
    let day = IntervalLength::Fixed {
        length: 60 * SECOND,
        phase: 0,
    };
    // Several calls in one batch, entries and facts mixed.
    h.submit(vec![definition(1, Some(day)), definition(2, None)]);
    h.submit(facts(1, T0, 40, 0));
    h.submit([facts(2, T0, 3, 17), facts(1, T0 + 100 * SECOND, 2, 0)].concat());
    h.submit(vec![Item::from(Entry::Alias {
        id: 2,
        name: "a2".into(),
    })]);
    assert_eq!(h.step_queue(), 4);
    h.submit(facts(2, T0 + 10 * SECOND, 25, 200));
    h.submit(facts(1, T0 + 200 * SECOND, 7, 0));
    assert_eq!(h.step_queue(), 2);

    // A recovery entry handed over with other calls: each call goes alone (calls 7 to 9).
    let mut g = h.greedy(vec![
        facts(1, T0 + 300 * SECOND, 3, 0),
        vec![recovery()],
        facts(2, T0, 2, 9),
    ]);
    assert_eq!(h.step(&mut g), 1);
    h.assert_intact(&g, &[8, 9]);
    assert_eq!(h.step(&mut g), 1);
    assert_eq!(h.step(&mut g), 1);
    assert!(g.is_empty());

    // Two calls over 8 MiB together: the first goes alone, the rest back in order (calls 10 to 12).
    let mut g = h.greedy(vec![
        facts(3, T0, BIG, 255),
        facts(4, T0, BIG, 255),
        facts(1, T0 + 400 * SECOND, 5, 0),
    ]);
    assert_eq!(h.step(&mut g), 1);
    h.assert_intact(&g, &[11, 12]);
    assert_eq!(h.step(&mut g), 2);

    // No creation ahead from here: the next roll uses the segment created ahead and leaves none.
    h.maint = false;
    let mut g = h.greedy(vec![
        facts(5, T0, BIG, 255),
        facts(1, T0 + 500 * SECOND, 1, 0),
    ]);
    assert_eq!(h.step(&mut g), 2);
    let mut g = h.greedy(vec![facts(6, T0, BIG, 255)]);
    assert_eq!(h.step(&mut g), 1);

    // A batch that encodes but does not fit, with no segment ahead: every call back whole (calls 16, 17).
    let mut a = h.greedy(vec![
        facts(7, T0, BIG, 255),
        facts(2, T0 + 50 * SECOND, 4, 3),
    ]);
    assert_eq!(h.step(&mut a), 0);
    assert_eq!(h.w.waiting(), Some(Wait::NoSegment));
    h.assert_intact(&a, &[16, 17]);

    // Calls that do not encode together, the first then not fitting: every call back whole (calls 18 to 20).
    let mut b = h.greedy(vec![
        facts(8, T0, BIG, 255),
        facts(9, T0, BIG, 255),
        facts(1, T0 + 600 * SECOND, 2, 0),
    ]);
    assert_eq!(h.step(&mut b), 0);
    h.assert_intact(&b, &[18, 19, 20]);

    h.maint = true;
    h.run_maint();
    while !a.is_empty() {
        h.step(&mut a);
    }
    while !b.is_empty() {
        h.step(&mut b);
    }
    h.submit(facts(1, T0 + 700 * SECOND, 3, 0));
    assert_eq!(h.step_queue(), 1);
    h.w.shutdown(h.now).unwrap();
    h
}

fn segments(fs: &MemFs) -> BTreeMap<u64, (SegmentName, Vec<u8>)> {
    let log = PathBuf::from("store").join("log");
    fs.paths()
        .into_iter()
        .filter(|p| p.parent() == Some(log.as_path()))
        .filter_map(|p| {
            let name = SegmentName::parse(p.file_name()?.to_str()?)?;
            Some((name.file_id, (name, fs.read(&p)?)))
        })
        .collect()
}

fn fnv(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

#[test]
fn calls_put_back_follow_at_once_while_the_budget_holds_and_a_turn_that_commits_nothing_spends_nothing()
 {
    let mut h = H::with_min_sync(Duration::from_millis(10));
    h.maint = false;
    let mut g = h.greedy(vec![facts(1, T0, 3, 0), vec![recovery()]]);
    assert_eq!(h.step(&mut g), 1);
    assert_eq!(
        h.w.sync_ready_at(),
        Duration::ZERO,
        "the budget still holds syncs"
    );
    assert_eq!(h.step(&mut g), 1);
    for s in 2..4 {
        h.submit(facts(s, T0, 1, 0));
        assert_eq!(h.step_queue(), 1);
    }
    // No clock: one sync per 10 ms. Four syncs a millisecond apart leave three tenths of one; each round below
    // refills one and spends one, so the budget holds 1.3 syncs as each large call comes and a turn that spent one
    // would leave it short.
    for series in 4.. {
        assert!(series < 12, "a roll found no segment ahead");
        h.now += Duration::from_millis(9);
        let mut big = h.greedy(vec![facts(series, T0, BIG, 255)]);
        if h.step(&mut big) == 0 {
            g = big;
            break;
        }
    }
    assert_eq!(h.w.waiting(), Some(Wait::NoSegment));
    assert!(
        h.w.sync_ready_at() <= h.now,
        "a turn that commits nothing spends nothing of the budget"
    );
    h.maint = true;
    h.run_maint();
    assert_eq!(h.step(&mut g), 1);
}

#[test]
fn every_call_is_acknowledged_once_in_order_with_dense_positions() {
    let h = history();
    assert_eq!(h.done.len(), h.calls.len());
    let mut next = 1;
    for ((id, r), (want, items)) in h.done.iter().zip(&h.calls) {
        assert_eq!(id, want);
        let ack = r.as_ref().unwrap();
        assert_eq!(ack.first.sequence, next);
        next += items.len() as u64;
        assert_eq!(ack.last.sequence, next - 1);
    }
}

#[test]
fn each_batch_is_its_calls_entries_in_order_encoded_as_one_content() {
    let h = history();
    let acks: Vec<(u64, u64, &Vec<Item>)> = h
        .done
        .iter()
        .map(|(id, r)| {
            let a = r.as_ref().unwrap();
            (a.first.sequence, a.last.sequence, &h.calls[id])
        })
        .collect();
    let mut batches = 0;
    for (name, bytes) in segments(&h.fs).values() {
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: name.file_id,
            generation: name.generation,
        };
        for b in &scan(bytes, &id, ScanLimit::End).unwrap().batches {
            let items: Vec<Item> = acks
                .iter()
                .filter(|(first, last, _)| {
                    *first >= b.commit.first_sequence && *last <= b.commit.last_sequence
                })
                .flat_map(|(_, _, items)| items.iter().cloned())
                .collect();
            let content = BatchContent {
                first_sequence: b.commit.first_sequence,
                received_at: b.commit.commit_time,
                items,
            };
            assert_eq!(content.last_sequence(), b.commit.last_sequence);
            let want = content.encode(&id, b.offset, b.commit.commit_time).unwrap();
            assert_eq!(&bytes[b.offset as usize..b.end() as usize], &want[..]);
            batches += 1;
        }
    }
    assert_eq!(batches, 13);
}

#[test]
fn every_file_matches_its_golden_digest() {
    let h = history();
    let mut digests = BTreeMap::new();
    for p in h.fs.paths() {
        let bytes = h.fs.read(&p).unwrap();
        let name: Vec<_> = p.iter().map(|c| c.to_string_lossy()).collect();
        digests.insert(
            name.join("/"),
            (bytes.len(), fnv(&bytes, 0xcbf2_9ce4_8422_2325)),
        );
    }
    let got: Vec<String> = digests
        .iter()
        .map(|(p, (n, d))| format!("{p} {n} {d:016x}"))
        .collect();
    let want = [
        "store/LOCK 0 cbf29ce484222325",
        "store/MANIFEST 104 e8fb5991b111ccb5",
        "store/STORE 120 10b1e14d8226b165",
        "store/log/00000000-0000000000000001.kvl 8840992 74cc103f0ef31095",
        "store/log/00000000-0000000000000002.kvl 8832784 5adf4e4e680f1ce0",
        "store/log/00000000-0000000000000003.kvl 8832856 4770137d3bbd613b",
        "store/log/00000000-0000000000000004.open 12582912 e5e22282c0a1b4af",
        "store/log/00000000-0000000000000005.open 12582912 cc94c3263d0871ca",
        "store/log/MARK 4224 5227eb368920baaf",
    ];
    assert_eq!(got, want);
}

#[test]
fn a_batch_commits_at_or_after_a_floor_already_given_and_the_floor_then_names_its_last_position() {
    let mut h = H::new();
    let clock = h.w.commit_clock();
    let ahead = h.wall + 3_600 * SECOND;
    assert_eq!(clock.floor(ahead).0, ahead);
    h.submit(facts(1, T0, 3, 0));
    assert_eq!(h.step_queue(), 1);
    let last = h.done.last().unwrap().1.as_ref().unwrap().last;
    assert!(h.wall < ahead);
    let commit_time = segments(&h.fs)
        .values()
        .flat_map(|(name, bytes)| {
            let id = SegmentIdentity {
                store_id: STORE_ID,
                file_id: name.file_id,
                generation: name.generation,
            };
            scan(bytes, &id, ScanLimit::End).unwrap().batches
        })
        .find(|b| b.commit.last_sequence == last.sequence)
        .map(|b| b.commit.commit_time)
        .unwrap();
    assert_eq!(commit_time, ahead);
    assert_eq!(clock.floor(i64::MIN), (ahead, last));
}
