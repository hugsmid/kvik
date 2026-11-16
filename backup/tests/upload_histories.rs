//! Generated histories of seals, steps, sink faults, kills and manifest crashes against a simple model: every
//! object ever in the bucket is the sealed bytes its key names, and once the bucket heals every sealed file is
//! uploaded and verified once and every record but the last is uploaded, in order.

mod upload_support;

use std::time::Duration;

use kvik_backup::envelope::Envelope;
use kvik_backup::fake::{TestEnvelope, block_on};
use kvik_backup::keys::{Keys, ObjectName};
use kvik_backup::upload::{BackupDriver, Step};
use kvik_files::catalogue::Tier;
use kvik_sink_api::SinkError;
use proptest::prelude::*;
use upload_support::*;

#[derive(Clone, Debug)]
enum Kill {
    Restart,
    PowerLoss,
    Drop,
}

#[derive(Clone, Debug)]
enum Op {
    Seal(usize),
    Steps(usize),
    Fail(u64, SinkError),
    LoseReply(u64),
    Kill(Kill),
    CrashManifest(usize, Kill),
}

fn kill() -> impl Strategy<Value = Kill> {
    prop_oneof![Just(Kill::Restart), Just(Kill::PowerLoss), Just(Kill::Drop)]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1usize..300).prop_map(Op::Seal),
        4 => (1usize..10).prop_map(Op::Steps),
        2 => (0u64..6, prop_oneof![
            Just(SinkError::Transient { reason: "t".into() }),
            Just(SinkError::Busy { queued: 1 }),
            Just(SinkError::HashMismatch { key: "k".into() }),
        ]).prop_map(|(o, e)| Op::Fail(o, e)),
        1 => (0u64..6).prop_map(Op::LoseReply),
        1 => kill().prop_map(Op::Kill),
        1 => (0usize..6, kill()).prop_map(|(n, k)| Op::CrashManifest(n, k)),
    ]
}

/// The model: what each key may hold. Any object in the bucket must be exactly this.
fn check_objects(h: &Harness) {
    let keys = Keys::new(ID);
    let env = TestEnvelope::new(KEY);
    let records = h.records();
    for (k, v) in h.sink.mem.objects() {
        let want = match keys.parse(&k) {
            Some(ObjectName::Store) => env.seal(&k, STORE_BYTES).unwrap(),
            Some(ObjectName::File { file, .. }) => {
                let (_, content) = h
                    .segments
                    .iter()
                    .find(|(d, _)| d.file_ref() == file)
                    .expect("an object for a file never sealed");
                env.seal(&k, content).unwrap()
            }
            Some(ObjectName::Record { epoch, .. }) => {
                let i = records
                    .iter()
                    .position(|r| r.epoch == epoch)
                    .expect("an object for a record not in the manifest");
                env.seal(&k, &uploaded_form(&records, i)).unwrap()
            }
            other => panic!("unexpected object {k}: {other:?}"),
        };
        assert_eq!(v, want, "{k}");
    }
    let log = lock(&h.sink.log);
    assert_eq!(log.violations, Vec::<String>::new());
}

fn restart(h: &Harness, k: &Kill) {
    match k {
        Kill::Restart => h.fs.restart(),
        Kill::PowerLoss => {
            h.fs.restart();
            h.fs.power_loss();
        }
        Kill::Drop => {}
    }
    h.reopen();
}

fn step(h: &Harness, d: &mut Driver, now: &mut Duration) {
    match block_on(d.step(*now)) {
        Step::Retry { after, .. } => *now += after,
        Step::Progress | Step::Idle => {}
        s => panic!("{s:?}"),
    }
    let p = d.progress();
    let snap = lock(&h.app()).catalogue().clone();
    let unverified = h
        .segments
        .iter()
        .filter(|(d, _)| {
            snap.file(d.file_ref())
                .is_some_and(|e| !e.verified.contains(&Tier::Deep))
        })
        .count() as u64;
    if p.failing_since.is_none() {
        assert_eq!(p.queued_files, unverified);
    }
}

fn play(ops: &[Op]) {
    let mut h = Harness::new();
    let mut d = h.driver();
    let mut now = Duration::ZERO;
    for op in ops {
        match op {
            Op::Seal(len) => {
                h.seal(*len);
            }
            Op::Steps(n) => {
                for _ in 0..*n {
                    step(&h, &mut d, &mut now);
                }
            }
            Op::Fail(at, e) => h.fault_at(h.ops() + at, Fault::Fail(e.clone())),
            Op::LoseReply(at) => h.fault_at(h.ops() + at, Fault::LoseReply),
            Op::Kill(k) => {
                drop(d);
                restart(&h, k);
                d = h.driver();
            }
            Op::CrashManifest(n, k) => {
                h.fs.crash_after(*n);
                for _ in 0..20 {
                    match block_on(d.step(now)) {
                        Step::Progress => {}
                        Step::Retry { after, .. } => now += after,
                        _ => break,
                    }
                }
                drop(d);
                h.fs.restart();
                restart(&h, k);
                d = h.driver();
            }
        }
        check_objects(&h);
    }
    h.heal();
    run_idle(&mut d, &mut now);
    check_objects(&h);
    h.check_done();
    let p = d.progress();
    assert_eq!(p.queued_files, 0);
    assert_eq!(p.queued_records, 1);
    let last = h.segments.last().and_then(|(d, _)| d.range).map(|r| r.1);
    assert_eq!(p.backed_up_through, last);
}

proptest! {
    #[test]
    fn every_history_ends_with_each_file_uploaded_once_and_every_record_in_order(
        ops in proptest::collection::vec(op(), 0..40)
    ) {
        play(&ops);
    }
}
