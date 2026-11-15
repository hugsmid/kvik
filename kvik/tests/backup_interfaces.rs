//! The backup and restore call signatures (roadmap 8.1d, 8.3d), before they are wired: owned `'static` sinks, the
//! restore modes and the new reasons, errors and statistics.

mod common;

use std::ops::Range;
use std::sync::Arc;

use common::{BACKENDS, block_on, store_on};
use kvik::{
    BackupError, BackupRefusal, BackupSink, BackupStats, Encryption, ListPage, Reason, RestoreMode,
    RestoreScope, RestoreStats, SinkError, SinkFuture, Stored,
};

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
