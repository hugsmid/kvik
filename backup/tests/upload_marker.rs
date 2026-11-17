//! The upload driver's last-uploaded markers (`format.md` A.17; Q-614): one after each record and before anything
//! after it, the marker a restart left unput first at the next start, a bucket with no marker or an unreadable latest
//! one given a marker made from its records, and a sequence another marker holds skipped.

mod upload_support;

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{TestEnvelope, block_on};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::marker::{Marker, Summary};
use kvik_backup::upload::{BackupDriver, Step, UploadError, Uploader};
use kvik_sink_api::{BackupSink, ListPage, SinkFuture, Stored};
use upload_support::*;

/// The harness's sink, with listings that leave out `hidden`.
#[derive(Clone)]
struct Hide {
    inner: RecSink,
    hidden: Arc<Mutex<BTreeSet<String>>>,
}

impl BackupSink for Hide {
    fn put<'a>(&'a self, key: &'a str, b: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        self.inner.put(key, b, sha)
    }
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        self.inner.stat(key)
    }
    fn get<'a>(&'a self, key: &'a str, r: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        self.inner.get(key, r)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        let p = block_on(self.inner.list(prefix, after));
        let hidden = lock(&self.hidden).clone();
        Box::pin(async move {
            let p = p?;
            let keys = p.keys.into_iter().filter(|k| !hidden.contains(k)).collect();
            Ok(ListPage::new(keys, p.more))
        })
    }
}

fn hiding(h: &Harness, keys: &[String]) -> (Hide, Uploader<Hide, TestEnvelope, Port>) {
    let s = Hide {
        inner: h.sink.clone(),
        hidden: Arc::new(Mutex::new(keys.iter().cloned().collect())),
    };
    let d = Uploader::new(
        s.clone(),
        TestEnvelope::new(KEY),
        Keys::new(ID),
        h.local(),
        h.port(),
        cfg(),
    );
    (s, d)
}

fn all_markers(h: &Harness) -> Vec<Marker> {
    (1..).map_while(|s| marker(h, s)).collect()
}

fn marker(h: &Harness, sequence: u64) -> Option<Marker> {
    let k = Keys::new(ID).marker(0, sequence);
    let b = h.sink.mem.objects().get(&k).cloned()?;
    Some(Marker::decode(&TestEnvelope::new(KEY).open(&k, &b).unwrap()).unwrap())
}

fn bucket_summary(h: &Harness) -> Summary {
    let records = in_bucket(&h.sink.mem);
    Summary::of(records.iter().map(|u| &u.record))
}

/// The record and marker keys put since event `from`, in order.
fn puts_since(h: &Harness, from: usize) -> Vec<String> {
    let keys = Keys::new(ID);
    h.events()[from..]
        .iter()
        .filter_map(|e| match e {
            Event::Put(k)
                if matches!(
                    keys.parse(k),
                    Some(ObjectName::Record { .. } | ObjectName::Marker { .. })
                ) =>
            {
                Some(k.clone())
            }
            _ => None,
        })
        .collect()
}

/// Steps until record `epoch` is put.
fn step_to_record(d: &mut Driver, h: &Harness, epoch: u64) {
    let key = Keys::new(ID).record(0, epoch);
    for _ in 0..100 {
        if h.puts_of(&key) > 0 {
            return;
        }
        assert!(matches!(block_on(d.step(Duration::ZERO)), Step::Progress));
    }
    panic!("record {epoch} never put");
}

#[test]
fn q614_each_record_is_followed_by_its_marker_before_any_other_record() {
    let mut h = Harness::new();
    h.seal(10);
    h.seal(20);
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    let puts = puts_since(&h, 0);
    assert!(!puts.is_empty());
    for (i, pair) in puts.chunks(2).enumerate() {
        let Some(ObjectName::Record { epoch, .. }) = keys.parse(&pair[0]) else {
            panic!("{pair:?}");
        };
        assert_eq!(pair.get(1), Some(&keys.marker(0, i as u64 + 1)), "{pair:?}");
        assert_eq!(marker(&h, i as u64 + 1).unwrap().summary.epoch, epoch);
    }
    h.check_done();
}

#[test]
fn q614_a_restart_between_a_record_and_its_marker_puts_that_marker_first_at_the_next_start() {
    let mut h = Harness::new();
    h.seal(10);
    let mut d = h.driver();
    step_to_record(&mut d, &h, 2);
    drop(d);
    assert_eq!(marker(&h, 2), None);
    h.seal(20);
    let from = h.events().len();
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    let first = h.events()[from..].iter().find_map(|e| match e {
        Event::Put(k) => Some(k.clone()),
        Event::Append(..) => None,
    });
    assert_eq!(first, Some(keys.marker(0, 2)));
    assert_eq!(marker(&h, 2).unwrap().summary.epoch, 2);
    h.check_done();
}

#[test]
fn q614_a_bucket_with_records_and_no_marker_gets_one_naming_them_all_at_the_next_start() {
    let mut h = Harness::new();
    h.seal(10);
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    for (k, _) in h.sink.mem.objects() {
        if matches!(keys.parse(&k), Some(ObjectName::Marker { .. })) {
            h.sink.mem.remove(&k);
        }
    }
    let from = h.events().len();
    run_idle(&mut h.driver(), &mut Duration::default());
    assert_eq!(puts_since(&h, from), [keys.marker(0, 1)]);
    assert_eq!(marker(&h, 1).unwrap().summary, bucket_summary(&h));
    h.check_done();
}

#[test]
fn q614_an_unreadable_latest_marker_is_followed_by_one_made_from_the_records() {
    let mut h = Harness::new();
    h.seal(10);
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    let n = (1..).take_while(|s| marker(&h, *s).is_some()).count() as u64;
    let k = keys.marker(0, n);
    let mut b = h.sink.mem.objects()[&k].clone();
    let mid = b.len() / 2;
    b[mid] ^= 1;
    h.sink.mem.set(&k, &b);
    let from = h.events().len();
    run_idle(&mut h.driver(), &mut Duration::default());
    assert_eq!(puts_since(&h, from), [keys.marker(0, n + 1)]);
    assert_eq!(marker(&h, n + 1).unwrap().summary, bucket_summary(&h));
}

#[test]
fn q614_a_marker_sequence_another_marker_holds_is_skipped() {
    let mut h = Harness::new();
    h.seal(10);
    let mut d = h.driver();
    step_to_record(&mut d, &h, 2);
    let keys = Keys::new(ID);
    h.sink.mem.set(&keys.marker(0, 2), b"another marker");
    run_idle(&mut d, &mut Duration::default());
    assert_eq!(h.sink.mem.objects()[&keys.marker(0, 2)], b"another marker");
    let m = marker(&h, 3).unwrap();
    assert_eq!(m.summary.epoch, 2);
    assert_eq!(marker(&h, 4).unwrap().summary, bucket_summary(&h));
}

#[test]
fn q614_a_record_put_before_a_restart_and_a_rebuild_and_hidden_by_the_listing_is_named_by_the_next_marker()
 {
    let mut h = Harness::new();
    h.seal(20);
    h.seal(20);
    run_idle(&mut h.driver(), &mut Duration::default());
    h.seal(20);
    let keys = Keys::new(ID);
    let mut d = h.driver();
    let unmarked = loop {
        assert!(matches!(block_on(d.step(Duration::ZERO)), Step::Progress));
        if let Some(Event::Put(k)) = h.events().last()
            && let Some(ObjectName::Record { epoch, .. }) = keys.parse(k)
        {
            break epoch;
        }
    };
    drop(d);
    assert!(all_markers(&h).iter().all(|m| m.summary.epoch < unmarked));
    h.close();
    h.set_app(rebuild_final(&h));
    let (_, mut d) = hiding(&h, &[keys.record(0, unmarked)]);
    let step = run(&mut d, &mut Duration::default(), 10_000);
    assert!(matches!(step, Step::Idle), "{step:?}");
    let next = all_markers(&h)
        .into_iter()
        .find(|m| m.summary.epoch >= unmarked)
        .unwrap();
    assert_eq!(next.summary.epoch, unmarked);
    check_markers(&h.sink.mem, true);
}

#[test]
fn q614_a_catch_up_whose_listing_hides_a_record_the_others_name_retries_instead_of_putting_a_marker()
 {
    let mut h = Harness::new();
    for _ in 0..3 {
        h.seal(20);
    }
    run_idle(&mut h.driver(), &mut Duration::default());
    h.close();
    h.set_app(rebuild_final(&h));
    let keys = Keys::new(ID);
    let mut d = h.driver();
    let opener = loop {
        assert!(matches!(block_on(d.step(Duration::ZERO)), Step::Progress));
        let opened = in_bucket(&h.sink.mem)
            .into_iter()
            .filter(|u| u.record.old_epoch.is_some())
            .count();
        if opened >= 3 {
            break in_bucket(&h.sink.mem)
                .into_iter()
                .find(|u| matches!(u.record.body, kvik_files::record::Body::Rebuild(_)))
                .unwrap()
                .record
                .epoch;
        }
    };
    drop(d);
    for m in all_markers(&h) {
        h.sink.mem.remove(&keys.marker(0, m.sequence));
    }
    let (s, mut d) = hiding(&h, &[keys.record(0, opener)]);
    for _ in 0..3 {
        let step = block_on(d.step(Duration::ZERO));
        assert!(matches!(step, Step::Retry { .. }), "{step:?}");
    }
    assert!(all_markers(&h).is_empty());
    lock(&s.hidden).clear();
    let step = run(&mut d, &mut Duration::default(), 10_000);
    assert!(matches!(step, Step::Idle), "{step:?}");
    check_markers(&h.sink.mem, true);
}

#[test]
fn q960_a_latest_marker_of_a_later_version_stops_uploads_and_is_not_followed() {
    let mut h = Harness::new();
    h.seal(10);
    run_idle(&mut h.driver(), &mut Duration::default());
    let n = all_markers(&h).len() as u64;
    let k = Keys::new(ID).marker(0, n);
    let mut later = marker(&h, n).unwrap().encode();
    later[4] = 2;
    h.sink
        .mem
        .set(&k, &TestEnvelope::new(KEY).seal(&k, &later).unwrap());
    h.seal(10);
    let step = run(&mut h.driver(), &mut Duration::default(), 1_000);
    assert!(
        matches!(step, Step::Failed(UploadError::MarkerVersion { sequence, version: 2 }) if sequence == n),
        "{step:?}"
    );
    assert_eq!(marker(&h, n + 1), None);
}

#[test]
fn q614_a_catch_up_over_a_record_the_bucket_lost_puts_its_marker_instead_of_retrying() {
    let mut h = Harness::new();
    h.seal(10);
    h.seal(10);
    run_idle(&mut h.driver(), &mut Duration::default());
    let keys = Keys::new(ID);
    for m in all_markers(&h) {
        h.sink.mem.remove(&keys.marker(0, m.sequence));
    }
    h.sink.mem.remove(&keys.record(0, 2));
    let (_, mut d) = hiding(&h, &[]);
    let step = run(&mut d, &mut Duration::default(), 1_000);
    assert!(matches!(step, Step::Idle), "{step:?}");
    assert_eq!(
        marker(&h, 1).unwrap().summary.epoch,
        in_bucket(&h.sink.mem).last().unwrap().record.epoch
    );
}
