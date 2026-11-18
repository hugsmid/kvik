//! The backup and restore call signatures (roadmap 8.1d, 8.3d), before they are wired: owned `'static` sinks, the
//! restore modes and the new reasons, errors and statistics, and the warning attaching a bucket gives (Q-970).

mod common;

use std::ops::Range;
use std::sync::{Arc, Mutex};

use common::{BACKENDS, block_on, config_on, store_on};
use kvik::env::ManualClock;
use kvik::{
    Backend, BackupError, BackupRefusal, BackupSink, BackupStats, Encryption, ListPage, LogConfig,
    Reason, RestoreMode, RestoreScope, RestoreStats, SinkError, SinkFuture, Store, Stored,
};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

struct Closed;

impl BackupSink for Closed {
    fn put<'a>(&'a self, key: &'a str, _: Arc<[u8]>, _: [u8; 32]) -> SinkFuture<'a, Stored> {
        Box::pin(async move { Err(SinkError::Failed { reason: key.into() }) })
    }
    fn stat<'a>(&'a self, _: &'a str) -> SinkFuture<'a, Option<Stored>> {
        Box::pin(async { Ok(None) })
    }
    fn get<'a>(&'a self, key: &'a str, _: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        Box::pin(async move { Err(SinkError::NotFound { key: key.into() }) })
    }
    fn list<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        Box::pin(async { Ok(ListPage::default()) })
    }
}

#[test]
fn restore_mode_defaults_to_full_and_partial_takes_a_scope() {
    assert_eq!(RestoreMode::default(), RestoreMode::Full);
    let partial = RestoreMode::Partial(RestoreScope::default());
    assert_ne!(partial, RestoreMode::Drill);
    assert_eq!(RestoreScope::default().time, None);
}

#[test]
fn backup_calls_take_owned_static_sinks_and_are_not_supported_yet() {
    for backend in BACKENDS {
        let (s, _clock) = store_on(backend, &format!("backup-interfaces-{backend:?}"));
        let shared: Arc<dyn BackupSink> = Arc::new(Closed);
        assert!(matches!(
            block_on(s.attach_backup(shared, Encryption::Key([7; 32]))),
            Err(BackupError::NotSupportedYet { .. })
        ));
        block_on(s.shutdown()).unwrap();
    }
    for mode in [
        RestoreMode::Full,
        RestoreMode::Drill,
        RestoreMode::Partial(RestoreScope::default()),
    ] {
        let r = block_on(kvik::Store::restore(
            "restore-target",
            Closed,
            Encryption::Key([7; 32]),
            mode,
        ));
        assert!(matches!(r, Err(BackupError::NotSupportedYet { .. })));
    }
}

#[test]
fn the_backup_reasons_errors_and_statistics_exist() {
    let r = Reason::BackupRefused(BackupRefusal::NoChecksums);
    assert_eq!(
        r.to_string(),
        "backup refused: the bucket does not check checksums"
    );
    let e = BackupError::Refused(BackupRefusal::WrongKey);
    assert!(e.to_string().contains("another key"));
    assert!(!BackupStats::default().attached);
    assert!(!RestoreStats::default().drill);
}

/// Collects the messages of warnings and errors logged on this thread.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<String>>>);

impl Subscriber for Warnings {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        *m.level() <= Level::WARN
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, e: &Event<'_>) {
        struct Message<'a>(&'a mut Vec<String>);
        impl Visit for Message<'_> {
            fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0.push(format!("{v:?}"));
                }
            }
        }
        e.record(&mut Message(&mut self.0.lock().unwrap()));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// The warnings attaching a bucket logs for a store of `backend` with segments of `segment_size` bytes.
fn attach_warnings(backend: Backend, segment_size: u64) -> Vec<String> {
    let name = format!("bucket-warning-{backend:?}-{segment_size}");
    let clock = ManualClock::new(common::start());
    let mut log = LogConfig::default();
    log.segment_size = segment_size;
    let s = block_on(Store::open(config_on(backend, &name, &clock).log(log))).unwrap();
    let warnings = Warnings::default();
    tracing::subscriber::with_default(warnings.clone(), || {
        let _ = block_on(s.attach_backup(Closed, Encryption::Key([7; 32])));
    });
    block_on(s.shutdown()).unwrap();
    std::mem::take(&mut *warnings.0.lock().unwrap())
}

#[test]
fn attaching_a_bucket_warns_only_above_the_default_segment_size_on_the_durable_backend() {
    let above = 64 << 20 | 4_096;
    let w = attach_warnings(Backend::Durable, above);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0].contains(&format!("log.segment_size is {above} bytes")) && w[0].contains("Q-970"),
        "{w:?}"
    );
    for size in [16 << 20, 64 << 20] {
        assert_eq!(
            attach_warnings(Backend::Durable, size),
            Vec::<String>::new()
        );
    }
    assert_eq!(attach_warnings(Backend::Model, above), Vec::<String>::new());
}
