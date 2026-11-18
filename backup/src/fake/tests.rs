use std::path::PathBuf;

use kvik_files::appender::Opened;
use kvik_files::catalogue::Tier;
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::fs::{Fs, MemFs};
use kvik_log::manifest::encode_initial;
use kvik_log::mark::ResetStamp;
use kvik_sink_api::Verified;

use super::*;
use crate::keys::Keys;
use crate::plan::{PlanError, PlanFn, PlanInput, PlanMode, RestorePlan};

const ID: [u8; 16] = [1; 16];

fn sink_put(s: &dyn BackupSink, key: &str, b: &[u8]) -> Result<Stored, SinkError> {
    block_on(s.put(key, b.into(), sha256(b)))
}

#[test]
fn mem_sink_keeps_the_sink_rules() {
    let m = MemSink::new();
    let s: &dyn BackupSink = &m;
    let st = sink_put(s, "a/1", b"one").unwrap();
    assert_eq!(st, Stored::new(3, sha256(b"one")));
    assert_eq!(sink_put(s, "a/1", b"one").unwrap(), st);
    assert_eq!(
        sink_put(s, "a/1", b"two"),
        Err(SinkError::Exists { key: "a/1".into() })
    );
    assert_eq!(
        block_on(s.put("a/2", Arc::from(&b"x"[..]), [0; 32])),
        Err(SinkError::HashMismatch { key: "a/2".into() })
    );
    sink_put(s, "b/1", b"three").unwrap();
    assert_eq!(block_on(s.get("a/1", Some(1..3))).unwrap(), b"ne");
    assert_eq!(block_on(s.get("a/1", Some(3..3))).unwrap(), b"");
    assert_eq!(
        block_on(s.get("a/1", Some(2..4))),
        Err(SinkError::InvalidRange { key: "a/1".into() })
    );
    assert_eq!(
        block_on(s.get("a/9", None)),
        Err(SinkError::NotFound { key: "a/9".into() })
    );
    assert_eq!(block_on(s.list("a/", None)).unwrap().keys, ["a/1"]);
    assert_eq!(block_on(s.list("", Some("a/1"))).unwrap().keys, ["b/1"]);
    assert!(block_on(Verified::check(s, "a/1", &st)).is_ok());
    assert_eq!(m.puts(), 3);
    assert_eq!(m.objects().len(), 2);
}

#[test]
fn a_killed_mem_sink_fails_every_call_and_changes_nothing() {
    let m = MemSink::new();
    sink_put(&m, "k", b"v").unwrap();
    m.kill_at(Some(m.ops()));
    let before = m.objects();
    assert!(matches!(
        sink_put(&m, "k2", b"v"),
        Err(SinkError::Transient { .. })
    ));
    assert!(matches!(
        block_on(m.stat("k")),
        Err(SinkError::Transient { .. })
    ));
    assert_eq!(m.objects(), before);
    m.kill_at(None);
    sink_put(&m, "k2", b"v").unwrap();
    assert_eq!(m.ops(), 4);
}

#[test]
fn fake_bucket_and_mem_sink_are_both_sinks() {
    let sinks: Vec<Box<dyn BackupSink>> = vec![
        Box::new(MemSink::new()),
        Box::new(kvik_sink::fake::FakeBucket::new().sink()),
    ];
    for s in &sinks {
        let st = sink_put(s.as_ref(), "k", b"v").unwrap();
        assert_eq!(block_on(s.stat("k")).unwrap(), Some(st));
    }
}

fn big(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn test_envelope_round_trips_and_is_deterministic() {
    let e: Arc<dyn Envelope> = Arc::new(TestEnvelope::new([5; 32]));
    let boxed: Box<dyn Envelope> = Box::new(e.clone());
    for n in [0, 1, CHUNK, CHUNK + 1, 2 * CHUNK + 10] {
        let p = big(n);
        let o = e.seal("k", &p).unwrap();
        assert_eq!(boxed.seal("k", &p).unwrap(), o);
        if n > 0 {
            assert_ne!(e.seal("k2", &p).unwrap()[HEADER..], o[HEADER..]);
        }
        assert_eq!(e.open("k", &o).unwrap(), p);
        e.check_header(&o[..e.header_len() as usize]).unwrap();
    }
}

#[test]
fn test_envelope_detects_a_wrong_key_from_the_header_alone() {
    let o = TestEnvelope::new([5; 32]).seal("k", b"plain").unwrap();
    let other = TestEnvelope::new([6; 32]);
    assert_ne!(other.key_check(), TestEnvelope::new([5; 32]).key_check());
    assert_eq!(
        other.check_header(&o[..HEADER]),
        Err(EnvelopeError::WrongKey)
    );
    assert_eq!(other.open("k", &o), Err(EnvelopeError::WrongKey));
}

#[test]
fn test_envelope_detects_damage_truncation_and_another_key_name() {
    let e = TestEnvelope::new([5; 32]);
    let p = big(CHUNK + 100);
    let o = e.seal("k", &p).unwrap();
    let mut d = o.clone();
    d[HEADER + CHUNK + TAG + 3] ^= 1;
    assert_eq!(
        e.open("k", &d),
        Err(EnvelopeError::Authentication { chunk: 1 })
    );
    assert_eq!(
        e.open("other", &o),
        Err(EnvelopeError::Authentication { chunk: 0 })
    );
    assert_eq!(
        e.open("j", &o),
        Err(EnvelopeError::Authentication { chunk: 0 })
    );
    assert_eq!(e.open("k", &o[..HEADER - 1]), Err(EnvelopeError::Truncated));
    assert_eq!(
        e.open("k", &o[..o.len() - 100]),
        Err(EnvelopeError::Truncated)
    );
    let mut v = o.clone();
    v[2] = 9;
    assert_eq!(
        e.open("k", &v),
        Err(EnvelopeError::Unsupported {
            version: 9,
            algorithm: ALGORITHM
        })
    );
}

#[test]
fn test_envelope_opens_a_span_of_whole_chunks() {
    let e = TestEnvelope::new([5; 32]);
    let p = big(3 * CHUNK + 5);
    let o = e.seal("k", &p).unwrap();
    let c = CHUNK as u64;
    for plain in [
        0..1,
        5..c,
        c - 1..c + 1,
        2 * c..3 * c + 5,
        3 * c + 1..3 * c + 4,
    ] {
        let span = e.span(plain.clone());
        let end = span.end.min(o.len() as u64);
        let bytes = &o[span.start as usize..end as usize];
        let got = e
            .open_span("k", &o[..HEADER], plain.clone(), bytes)
            .unwrap();
        assert_eq!(
            got,
            p[plain.start as usize..plain.end as usize],
            "{plain:?}"
        );
    }
}

fn port() -> (Arc<MemFs>, DirectPort) {
    let fs = Arc::new(MemFs::new());
    let dir = PathBuf::from("store");
    let f = fs.open(&dir.join(MANIFEST), OpenMode::CreateNew).unwrap();
    f.write_at(&encode_initial(ID, 0), 0).unwrap();
    f.sync_all().unwrap();
    drop(f);
    let Opened::Ready(a) = Appender::open(&*fs, &dir, ID, 0).unwrap() else {
        panic!("corrupt")
    };
    let local = LocalStore {
        fs: fs.clone(),
        dir,
    };
    (fs, DirectPort::new(*a, local, ID, 10))
}

fn segment(file_id: u64) -> FileDescription {
    FileDescription {
        kind: kind::LOG_SEGMENT,
        class: FileClass::None,
        generation: 0,
        file_id,
        range: Some(((0, 1), (0, 9))),
        hash: [3; 32],
        inputs: Vec::new(),
    }
}

#[test]
fn direct_port_is_a_manifest_port() {
    let (_fs, mut p) = port();
    let m: &mut dyn ManifestPort = &mut p;
    assert!(!m.holds());
    let e = m.append(Body::Sealed(vec![segment(1)])).unwrap();
    assert!(m.snapshot().file(segment(1).file_ref()).is_some());
    let s = m.settle(e).unwrap();
    assert_eq!(s.epoch(), e);
    let after = m.records_after(1).unwrap();
    assert_eq!(
        after.iter().map(|r| r.epoch).collect::<Vec<_>>(),
        [e, e + 1]
    );
    assert_eq!(after[0].body, Body::Sealed(vec![segment(1)]));
    assert_eq!(after[0].written_at, 10);
    assert!(m.records_after(e + 1).unwrap().is_empty());
}

#[test]
fn scripted_driver_replays_its_steps_then_is_idle() {
    let mut d = ScriptedDriver {
        steps: [Step::Progress, Step::Held].into(),
        progress: Progress {
            queued_files: 2,
            ..Progress::default()
        },
        ..ScriptedDriver::default()
    };
    let b: &mut dyn BackupDriver = &mut d;
    assert_eq!(block_on(b.probe()), Ok(()));
    assert!(matches!(
        block_on(b.step(Duration::from_secs(1))),
        Step::Progress
    ));
    assert!(matches!(
        block_on(b.step(Duration::from_secs(2))),
        Step::Held
    ));
    assert!(matches!(
        block_on(b.step(Duration::from_secs(3))),
        Step::Idle
    ));
    assert_eq!(b.progress().queued_files, 2);
    assert_eq!(d.calls.len(), 3);
    let mut r = ScriptedDriver {
        refusal: Some(Refused::NoChecksums),
        ..ScriptedDriver::default()
    };
    let boxed: Box<dyn BackupDriver> = Box::new(ScriptedDriver::default());
    drop(boxed);
    assert_eq!(block_on(r.probe()), Err(Refused::NoChecksums));
}

fn record() -> RestoreRecord {
    RestoreRecord {
        generation: 1,
        stamp: ResetStamp {
            sequence: 10,
            nonce: 7,
        },
        source_generation: 0,
        restored_end: (0, 9),
        carried_ends: Vec::new(),
        losses: Vec::new(),
        acknowledgements: Vec::new(),
        entry_losses: Vec::new(),
        files: vec![RestoredFile {
            description: segment(1),
            tier: Tier::Deep,
            copy: DeepCopy {
                size: 100,
                sha256: [4; 32],
            },
        }],
        drill: false,
    }
}

#[test]
fn scripted_restore_replays_then_is_done() {
    let prepared = Prepared {
        store_id: ID,
        record: record(),
        resumed: false,
        unnamed: Vec::new(),
        marker_fallback: None,
        earlier_state: None,
    };
    let mut d = ScriptedRestore {
        prepared: [Ok(prepared.clone())].into(),
        steps: [RestoreStep::Installed {
            file: FileRef {
                kind: kind::LOG_SEGMENT,
                file_id: 1,
            },
        }]
        .into(),
        ..ScriptedRestore::default()
    };
    let r: &mut dyn RestoreDriver = &mut d;
    assert_eq!(block_on(r.prepare(0)).unwrap(), prepared);
    assert!(matches!(block_on(r.prepare(0)), Err(RestoreError::NoStore)));
    assert!(matches!(
        block_on(r.step(Duration::ZERO)),
        RestoreStep::Installed { .. }
    ));
    assert!(matches!(
        block_on(r.step(Duration::ZERO)),
        RestoreStep::Done
    ));
    assert_eq!(r.progress(), RestoreProgress::default());
}

fn hand_plan(i: &PlanInput<'_>) -> Result<RestorePlan, PlanError> {
    if i.source.last_epoch() == 0 {
        return Err(PlanError::NoRecords);
    }
    let mut record = record();
    record.generation = i.generation;
    record.stamp.nonce = i.nonce;
    record.drill = i.mode == PlanMode::Drill;
    Ok(RestorePlan {
        fetch: record.files.clone(),
        record,
    })
}

#[test]
fn a_hand_written_plan_is_a_plan_fn() {
    let plan: PlanFn = hand_plan;
    let empty = Catalogue::new();
    let input = PlanInput {
        generation: 1,
        nonce: 3,
        source_generation: 0,
        source: &empty,
        mode: PlanMode::Drill,
        recovery_after: &[],
    };
    assert_eq!(plan(&input), Err(PlanError::NoRecords));
    let (_fs, p) = port();
    let snap = p.snapshot();
    let input = PlanInput {
        source: &snap,
        ..input
    };
    let out = plan(&input).unwrap();
    assert!(out.record.drill);
    assert_eq!(out.record.stamp.nonce, 3);
}

#[test]
fn record_keys_seal_with_the_envelope() {
    let keys = Keys::new(ID);
    let e = TestEnvelope::new([5; 32]);
    let key = keys.record(0, 2);
    let rec = Record::new(2, 0, 10, Body::Empty).encode().unwrap();
    let o = e.seal(&key, &rec).unwrap();
    let m = MemSink::new();
    sink_put(&m, &key, &o).unwrap();
    let back = block_on(m.get(&key, None)).unwrap();
    assert_eq!(e.open(&key, &back).unwrap(), rec);
    assert_eq!(Keys::store_id_of(&key), Some(ID));
}
