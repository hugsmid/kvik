//! The start seams (roadmap 8.3c, 6.3c): the recorder, the seal listener and the start options.

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kvik_log::batch::SegmentIdentity;
use kvik_log::fs::MemFs;
use kvik_log::manifest::{LossCause, LostRange, Recorder};
use kvik_log::mark::ResetStamp;
use kvik_log::segment::SealedFile;
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{
    GenerationStart, LogWriter, OpenError, SealListener, StartOptions, WriterConfig,
};
use support::{MemRecorder, STORE, T0};

fn store(fs: &Arc<MemFs>) -> StoreDir {
    let new = NewStore {
        store_id: STORE,
        created_at: T0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik test".into(),
    };
    StoreDir::open(fs.clone(), Path::new("store"), Some(&new)).unwrap()
}

fn loss(first: u64, last: u64) -> LostRange {
    LostRange {
        identity: (0, first),
        first: (0, first),
        last: (0, last),
        file_id: 1,
        offset: 0,
        length: 0,
        cause: LossCause::Corruption,
        boot_id: [9; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: None,
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<SealedFile>>);

impl SealListener for Collect {
    fn sealed(&self, file: SealedFile) {
        self.0.lock().unwrap().push(file);
    }
}

#[test]
fn mem_recorder_is_a_recorder_and_records_a_loss_once() {
    let mut r = MemRecorder::default();
    let rec: &mut dyn Recorder = &mut r;
    assert_eq!(rec.record_loss(&loss(5, 9), 3, T0).unwrap(), Some(4));
    assert_eq!(rec.record_loss(&loss(6, 8), 0, T0).unwrap(), None);
    assert_eq!(rec.record_loss(&loss(10, 10), 0, T0).unwrap(), Some(5));
    rec.repair().unwrap();
    let read = rec.read(&STORE).unwrap();
    assert_eq!(read.lost_ranges().count(), 2);
    assert_eq!(read.last_epoch(), 5);
    assert_eq!(r.repairs, 1);
}

#[test]
fn a_seal_listener_is_shared_as_a_trait_object() {
    let c = Arc::new(Collect::default());
    let l: Arc<dyn SealListener> = c.clone();
    let f = SealedFile {
        id: SegmentIdentity {
            store_id: STORE,
            file_id: 1,
            generation: 0,
        },
        len: 4096,
        sha256: [7; 32],
        sequences: Some((1, 3)),
        commit_times: Some((T0, T0)),
    };
    l.sealed(f.clone());
    assert_eq!(*c.0.lock().unwrap(), vec![f]);
}

#[test]
fn open_with_default_options_opens_as_open_does() {
    let (a, b) = (Arc::new(MemFs::new()), Arc::new(MemFs::new()));
    let (sa, sb) = (store(&a), store(&b));
    a.clear_ops();
    b.clear_ops();
    let cfg = WriterConfig::default();
    let wa = LogWriter::open(sa, cfg.clone(), Some([9; 16]), Duration::ZERO, T0).unwrap();
    let wb = LogWriter::open_with(
        sb,
        cfg,
        Some([9; 16]),
        Duration::ZERO,
        T0,
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(a.ops(), b.ops());
    assert_eq!(wa.stats(), wb.stats());
    assert_eq!(wa.start_report(), wb.start_report());
    assert!(wb.fault().is_none());
}

#[test]
fn open_with_refuses_every_option_it_does_not_act_on_yet() {
    let generation = GenerationStart {
        generation: 1,
        stamp: ResetStamp {
            sequence: 1,
            nonce: 7,
        },
        set_aside: SetAside::new(vec![RestoredEnd {
            generation: 1,
            end: (0, 10),
        }]),
    };
    let options: [(StartOptions, &str); 3] = [
        (
            StartOptions {
                recorder: Some(Box::new(MemRecorder::default())),
                ..StartOptions::default()
            },
            "StartOptions::recorder",
        ),
        (
            StartOptions {
                sealed: Some(Arc::new(Collect::default())),
                ..StartOptions::default()
            },
            "StartOptions::sealed",
        ),
        (
            StartOptions {
                generation: Some(generation),
                ..StartOptions::default()
            },
            "StartOptions::generation",
        ),
    ];
    for (opts, field) in options {
        let fs = Arc::new(MemFs::new());
        let r = LogWriter::open_with(
            store(&fs),
            WriterConfig::default(),
            Some([9; 16]),
            Duration::ZERO,
            T0,
            opts,
        );
        assert!(
            matches!(r, Err(OpenError::NotSupportedYet { what }) if what == field),
            "{field}"
        );
    }
}
