//! A start's lost range across recorded losses (`spec.md` §3 Order of a start step 4, Q-1102): one record, under the
//! lowest identity it overlaps, and nothing more at later starts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::MemFs;
use kvik_log::manifest::{LossCause, LostRange, RECORD_LOST_RANGE, encode_record, read_manifest};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    Call, CallSource as _, IngestQueue, LogWriter, MarkSync, StartReport, WriterConfig,
    create_segment, seal_segment_described,
};

const STORE_ID: [u8; 16] = [7; 16];
const T0: i64 = 1_767_225_600_000_000_000;

fn dir() -> PathBuf {
    PathBuf::from("store")
}

fn cfg() -> WriterConfig {
    WriterConfig {
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        mark_sync: MarkSync::EveryCommit,
        ..WriterConfig::default()
    }
}

/// A store whose first segment holds batches of one fact each, at positions 1 to 12.
struct Rig {
    fs: MemFs,
    boot: u8,
    now: Duration,
    w: Option<LogWriter>,
}

impl Rig {
    fn new() -> Self {
        let mut r = Rig {
            fs: MemFs::new(),
            boot: 1,
            now: Duration::ZERO,
            w: None,
        };
        r.open();
        let mut q = IngestQueue::new(cfg().ingest_queue);
        for i in 0..12 {
            let row = Item::Fact(FactRow {
                series_id: 1,
                t: T0 + i,
                quality: Quality::Good,
                flags: FactFlags::default(),
                value: Value::F64(i as u64),
                changed_at: None,
            });
            q.push(Call::new(vec![row], cfg().call_limit()).unwrap())
                .unwrap();
            for _ in 0..20 {
                if q.is_empty() {
                    break;
                }
                let (now, wall) = (r.now, r.wall());
                r.w.as_mut().unwrap().step(now, wall, &mut q);
                r.maint();
                r.now += Duration::from_millis(10);
            }
        }
        assert!(q.is_empty());
        r.stop();
        r
    }

    fn wall(&self) -> i64 {
        T0 + self.now.as_nanos() as i64
    }

    fn open(&mut self) -> StartReport {
        self.w = None;
        let store = StoreDir::open(Arc::new(self.fs.clone()), &dir(), Some(&new_store())).unwrap();
        let w =
            LogWriter::open(store, cfg(), Some([self.boot; 16]), self.now, self.wall()).unwrap();
        assert!(w.fault().is_none());
        let report = w.start_report().clone();
        self.w = Some(w);
        self.maint();
        report
    }

    /// Creates the segment the writer asks for and seals what it hands over.
    fn maint(&mut self) {
        let (now, wall) = (self.now, self.wall());
        let w = self.w.as_mut().unwrap();
        if let Some(req) = w.segment_request(wall) {
            let r = create_segment(&self.fs, &req, &w.halt());
            w.segment_created(r);
        }
        for job in w.take_seal_jobs(now) {
            let r = seal_segment_described(&self.fs, &job, &w.halt());
            w.seal_done(job, r, now);
        }
    }

    /// A clean stop, then the process ends.
    fn stop(&mut self) {
        let now = self.now;
        self.w.as_mut().unwrap().shutdown(now).unwrap();
        self.w = None;
        self.fs.restart();
    }

    /// Damages the body of the batch at `position`, with no writer open.
    fn damage(&mut self, position: u64) {
        let seg = dir().join("log").join("00000000-0000000000000001.open");
        let bytes = self.fs.read(&seg).unwrap();
        let id = SegmentIdentity {
            store_id: STORE_ID,
            file_id: 1,
            generation: 0,
        };
        let b = scan(&bytes, &id, ScanLimit::End)
            .unwrap()
            .batches
            .into_iter()
            .find(|b| b.commit.first_sequence == position)
            .expect("an intact batch at the position");
        self.fs.poke(&seg, b.offset as usize + 40, &[0xff; 8]);
    }

    /// A power loss and a reboot, then a start.
    fn restart(&mut self) -> StartReport {
        self.w = None;
        self.fs.power_loss();
        self.fs.restart();
        self.boot += 1;
        self.open()
    }

    /// `(identity, first, last, extends)` of each lost-range record, in the manifest's order.
    fn records(&self) -> Vec<(u64, u64, u64, bool)> {
        let bytes = self.fs.read(&dir().join("MANIFEST")).unwrap();
        read_manifest(&bytes, &STORE_ID)
            .unwrap()
            .lost_ranges()
            .map(|l| (l.identity.1, l.first.1, l.last.1, l.extends))
            .collect()
    }

    /// Appends a lost-range record for `first..=last` under `identity`, with no writer open.
    fn record(&self, identity: u64, first: u64, last: u64) {
        let path = dir().join("MANIFEST");
        let bytes = self.fs.read(&path).unwrap();
        let m = read_manifest(&bytes, &STORE_ID).unwrap();
        let l = LostRange {
            identity: (0, identity),
            first: (0, first),
            last: (0, last),
            file_id: 1,
            offset: 0,
            length: 0,
            cause: LossCause::Corruption,
            boot_id: [0; 16],
            id_floor: 0,
            time_before: None,
            time_after: None,
            extends: identity != first,
            stamp: None,
        };
        let rec = encode_record(RECORD_LOST_RANGE, m.last_epoch() + 1, 0, T0, Some(&l));
        self.fs.poke(&path, m.valid_end as usize, &rec);
    }
}

fn new_store() -> NewStore {
    NewStore {
        store_id: STORE_ID,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    }
}

/// The losses of a start's report as `(first, last, recorded)`.
fn found(rep: &StartReport) -> Vec<(Option<u64>, Option<u64>, bool)> {
    rep.losses
        .iter()
        .map(|l| (l.first, l.last, l.recorded))
        .collect()
}

/// Losses 10 and 8 recorded by two starts, then 9 damaged: the third start finds 8..10.
fn bridged() -> Rig {
    let mut r = Rig::new();
    r.damage(10);
    r.restart();
    r.stop();
    r.damage(8);
    r.restart();
    r.stop();
    assert_eq!(r.records(), [(10, 10, 10, false), (8, 8, 8, false)]);
    r.damage(9);
    r
}

#[test]
fn a_range_across_two_recorded_losses_is_one_record_under_the_lower_identity() {
    let mut r = bridged();
    let rep = r.restart();
    assert_eq!(found(&rep), [(Some(8), Some(10), true)]);
    assert_eq!(
        r.records(),
        [(10, 10, 10, false), (8, 8, 8, false), (8, 8, 10, true)]
    );
}

#[test]
fn the_bridging_record_survives_a_power_loss_and_later_starts_write_none() {
    let mut r = bridged();
    r.restart();
    let after = r.records();
    assert_eq!(after.len(), 3);
    for _ in 0..2 {
        let rep = r.restart();
        assert_eq!(found(&rep), [(Some(8), Some(10), true)]);
        assert_eq!(r.records(), after);
    }
    r.stop();
    r.open();
    assert_eq!(r.records(), after);
}

#[test]
fn a_range_across_two_records_of_one_identity_extends_it_once() {
    let mut r = Rig::new();
    r.damage(8);
    r.restart();
    r.stop();
    r.damage(9);
    r.restart();
    r.stop();
    assert_eq!(r.records(), [(8, 8, 8, false), (8, 8, 9, true)]);
    r.damage(11);
    r.damage(10);
    let rep = r.restart();
    assert_eq!(found(&rep), [(Some(8), Some(11), true)]);
    assert_eq!(
        r.records(),
        [(8, 8, 8, false), (8, 8, 9, true), (8, 8, 11, true)]
    );
}

#[test]
fn a_range_beside_a_recorded_loss_is_a_loss_of_its_own() {
    let mut r = Rig::new();
    r.record(11, 11, 11);
    r.damage(10);
    let rep = r.restart();
    assert_eq!(found(&rep), [(Some(10), Some(10), true)]);
    assert_eq!(r.records(), [(11, 11, 11, false), (10, 10, 10, false)]);
}

#[test]
fn a_range_across_three_losses_takes_the_lowest_identity_and_spans_them_all() {
    let mut r = Rig::new();
    r.record(10, 10, 12);
    r.record(4, 4, 5);
    r.record(7, 7, 8);
    for p in 5..=10 {
        r.damage(p);
    }
    let rep = r.restart();
    assert_eq!(found(&rep), [(Some(5), Some(10), true)]);
    assert_eq!(r.records().last(), Some(&(4, 4, 12, true)));
    assert_eq!(r.records().len(), 4);
}

#[test]
fn a_bridge_recorded_under_the_higher_identity_is_recorded_once_more_under_the_lower() {
    let mut r = Rig::new();
    r.record(10, 10, 10);
    r.record(8, 8, 8);
    r.record(10, 8, 10);
    for p in 8..=10 {
        r.damage(p);
    }
    let rep = r.restart();
    assert_eq!(found(&rep), [(Some(8), Some(10), true)]);
    assert_eq!(r.records().last(), Some(&(8, 8, 10, true)));
    assert_eq!(r.records().len(), 4);
    r.restart();
    assert_eq!(r.records().len(), 4);
}
