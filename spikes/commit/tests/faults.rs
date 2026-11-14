//! Behaviour of the commit path on sync failures and a full disk, through the simulated file system.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use commit::format::{self, Mark, Reading};
use commit::fs::{Fault, SimFs};
use commit::recovery::{self, Rule};
use commit::writer::{Config, Health, Ingest, MARK_NAME, MarkSync, Refused, Writer, segment_name};

const SEGMENT: &str = "log/1-0000000000000001.open";
const BOOT: [u8; 16] = [1; 16];
const OTHER_BOOT: [u8; 16] = [2; 16];

fn config() -> Config {
    Config {
        segment_len: 1 << 16,
        mark_sync: MarkSync::Off,
        preallocate_ahead: false,
        space_check_interval: Duration::from_millis(5),
        boot_id: BOOT,
        ..Config::default()
    }
}

fn reading(i: u32) -> Reading {
    Reading {
        series_id: i % 7,
        t: i64::from(i) * 1_000_000_000,
        value: f64::from(i),
    }
}

/// First and last sequence of every acknowledged batch.
type Acked = Arc<Mutex<Vec<(u64, u64)>>>;

/// Starts a writer that records every acknowledged batch.
fn start(fs: &SimFs, config: Config) -> (Writer, Ingest, Acked) {
    let acked = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&acked);
    let (writer, ingest) = Writer::start(
        Arc::new(fs.clone()),
        config,
        Box::new(move |c| {
            sink.lock()
                .unwrap()
                .push((c.first_sequence, c.last_sequence))
        }),
    )
    .unwrap();
    (writer, ingest, acked)
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Emits reading `i` and waits until it is acknowledged.
fn emit_acked(ingest: &Ingest, acked: &Mutex<Vec<(u64, u64)>>, i: u32) {
    let before = acked.lock().unwrap().len();
    ingest.emit(reading(i), Instant::now()).unwrap();
    wait_until("acknowledgement", || acked.lock().unwrap().len() > before);
}

fn mark_in_cache(fs: &SimFs) -> Option<Mark> {
    Mark::decode(&fs.read(MARK_NAME).unwrap())
}

#[test]
fn readings_arriving_during_a_sync_form_the_next_batch() {
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    let syncs_at_start = fs.syncs_started();
    fs.pause_syncs("");
    ingest.emit(reading(0), Instant::now()).unwrap();
    wait_until("the first sync", || fs.syncs_started() > syncs_at_start);
    for i in 1..=10 {
        ingest.emit(reading(i), Instant::now()).unwrap();
    }
    fs.resume_syncs();
    wait_until("both batches", || acked.lock().unwrap().len() == 2);
    assert_eq!(*acked.lock().unwrap(), [(1, 1), (2, 11)]);
    drop(ingest);
    let report = writer.join();
    assert_eq!((report.batches, report.readings, report.syncs), (2, 11, 2));
}

#[test]
fn a_window_closes_at_the_reading_limit() {
    let fs = SimFs::new();
    let config = Config {
        linger: Duration::from_secs(60),
        max_batch_readings: 4,
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    for i in 0..8 {
        ingest.emit(reading(i), Instant::now()).unwrap();
    }
    wait_until("two batches", || acked.lock().unwrap().len() == 2);
    assert_eq!(*acked.lock().unwrap(), [(1, 4), (5, 8)]);
    drop(ingest);
    writer.join();
}

#[test]
fn acknowledged_batches_are_on_disk_and_the_mark_covers_them() {
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    for i in 0..5 {
        emit_acked(&ingest, &acked, i);
    }
    let mark = mark_in_cache(&fs).unwrap();
    assert_eq!(mark.last_sequence, 5);
    let on_disk = format::scan(&fs.read_disk(SEGMENT).unwrap(), 1);
    assert_eq!(on_disk.batches.len(), 5);
    assert_eq!(on_disk.valid_end, mark.offset);
    drop(ingest);
    writer.join();
}

#[test]
fn a_sync_error_makes_the_writer_read_only_without_a_retry() {
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    emit_acked(&ingest, &acked, 1);
    let mark_before = mark_in_cache(&fs).unwrap();
    let syncs_before = fs.syncs_of(SEGMENT);

    fs.fail_next_sync("log/1-", Fault::Io);
    ingest.emit(reading(2), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);

    // Not acknowledged, the mark not advanced, exactly one sync of the failed batch, later emits refused.
    assert_eq!(acked.lock().unwrap().len(), 2);
    assert_eq!(mark_in_cache(&fs), Some(mark_before));
    assert_eq!(fs.syncs_of(SEGMENT), syncs_before + 1);
    assert_eq!(
        ingest.emit(reading(3), Instant::now()),
        Err(Refused::ReadOnly)
    );
    let report = writer.join();
    assert!(report.fatal.unwrap().starts_with("log sync"));

    // The failed batch is complete in the page cache but not on the disk.
    assert_eq!(format::scan(&fs.read(SEGMENT).unwrap(), 1).batches.len(), 3);
    assert_eq!(
        format::scan(&fs.read_disk(SEGMENT).unwrap(), 1)
            .batches
            .len(),
        2
    );

    // Restart on the same boot (rule 1): truncated to the mark, so only the acknowledged batches are kept.
    let d = recovery::decide(&fs.read(SEGMENT).unwrap(), 1, mark_in_cache(&fs), BOOT);
    assert_eq!(d.rule, Rule::SameBoot);
    assert_eq!(d.kept.len(), 2);
    assert_eq!(d.truncate_from, mark_before.offset);
}

#[test]
fn after_a_failed_sync_a_retried_sync_reports_success_without_writing() {
    // The Linux behaviour the simulated file system models, and the reason a sync is never retried.
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    fs.fail_next_sync("log/1-", Fault::Io);
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    drop(ingest);
    writer.join();

    assert!(fs.sync_file(SEGMENT).is_ok(), "the retry succeeds");
    assert_eq!(
        format::scan(&fs.read_disk(SEGMENT).unwrap(), 1)
            .batches
            .len(),
        1,
        "but nothing was written"
    );
}

#[test]
fn with_the_mark_lost_after_a_failed_sync_recovery_keeps_a_batch_that_is_not_on_disk() {
    // A problem for spec.md §3 Recovery rules 3 and 4 on the same boot (see the measurement notes).
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    fs.fail_next_sync("log/1-", Fault::Io);
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    drop(ingest);
    writer.join();

    fs.remove(MARK_NAME);
    let d = recovery::decide(&fs.read(SEGMENT).unwrap(), 1, None, BOOT);
    assert_eq!(d.rule, Rule::DifferentBoot);
    assert_eq!(d.kept.len(), 2, "the never-acknowledged batch is kept");
    assert!(d.sync_before_resuming);
    assert!(
        fs.sync_file(SEGMENT).is_ok(),
        "rule 4's sync returns success"
    );
    // From here the batch would be delivered as durable. A power loss shows it never reached the disk.
    fs.power_loss();
    assert_eq!(format::scan(&fs.read(SEGMENT).unwrap(), 1).batches.len(), 1);
}

#[test]
fn after_a_reboot_a_batch_whose_sync_failed_may_be_kept() {
    // Row 12: its fate is unknown; here the disk happened to receive it before the error was reported.
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    fs.pause_syncs("");
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("the sync", || fs.syncs_started() >= 3);
    fs.writeback(SEGMENT); // background writeback reached the disk before the error
    fs.fail_next_sync("log/1-", Fault::Io);
    fs.resume_syncs();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    drop(ingest);
    writer.join();
    fs.power_loss();
    let d = recovery::decide(
        &fs.read(SEGMENT).unwrap(),
        1,
        mark_in_cache(&fs),
        OTHER_BOOT,
    );
    assert_eq!(d.rule, Rule::DifferentBoot);
    assert_eq!(
        d.kept.len(),
        2,
        "kept, never acknowledged; adapters replay it and the duplicate rule absorbs it"
    );
}

#[test]
fn no_space_on_a_log_write_is_fatal_and_nothing_is_acknowledged() {
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    fs.fail_next_write("log/1-", Fault::NoSpace);
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    assert_eq!(acked.lock().unwrap().len(), 1);
    assert_eq!(mark_in_cache(&fs).unwrap().last_sequence, 1);
    let report = writer.join();
    assert!(
        report.fatal.unwrap().starts_with("log write"),
        "observed at the write, before any sync"
    );
    assert_eq!(report.syncs, 1);
}

#[test]
fn no_space_on_a_log_sync_is_fatal() {
    let fs = SimFs::new();
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    fs.fail_next_sync("log/1-", Fault::NoSpace);
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    assert_eq!(acked.lock().unwrap().len(), 1);
    let report = writer.join();
    assert!(report.fatal.unwrap().starts_with("log sync"));
}

#[test]
fn no_space_when_preallocating_the_next_segment_is_fatal_although_nothing_is_lost() {
    // §3 as written treats this as a write failure on the log. Every acknowledged batch is durable, and a
    // partial segment file is left behind.
    let fs = SimFs::new();
    let config = Config {
        segment_len: 4096,
        preallocate_ahead: true,
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    fs.fail_next_create("log/1-0000000000000003", Fault::NoSpace);
    let mut i = 0;
    while ingest.health() != Health::Fatal {
        assert!(i < 10_000, "never reached the failing segment");
        if ingest.emit(reading(i), Instant::now()).is_ok() {
            wait_until("acknowledgement or failure", || {
                acked.lock().unwrap().len() as u32 > i || ingest.health() == Health::Fatal
            });
        }
        i += 1;
    }
    let report = writer.join();
    assert!(report.fatal.unwrap().starts_with("segment creation"));
    assert_eq!(report.segments_rolled, 1);
    let last_acked = acked.lock().unwrap().last().unwrap().1;
    let mark = mark_in_cache(&fs).unwrap();
    assert_eq!((mark.file_id, mark.last_sequence), (2, last_acked));
    let disk = fs.read_disk(&segment_name(1, 2)).unwrap();
    let base_sequence = u64::from_le_bytes(disk[24..32].try_into().unwrap());
    let durable = format::scan(&disk, base_sequence);
    assert_eq!(durable.valid_end, mark.offset);
    assert!(
        fs.names().contains(&segment_name(1, 3)),
        "partial file left"
    );
}

#[test]
fn below_the_critical_mark_emits_wait_and_resume_when_space_returns() {
    let fs = SimFs::new();
    let gib = 1u64 << 30;
    fs.set_space(100 * gib, 1000 * gib);
    let config = Config {
        queue_capacity: 4,
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    emit_acked(&ingest, &acked, 0);
    assert_eq!(ingest.health(), Health::Ok);

    fs.set_space(256 << 20, 1000 * gib);
    wait_until("critical", || ingest.health() == Health::Critical);
    // The writer no longer takes from the queue: it fills, then emits wait.
    for i in 1..=4 {
        ingest.try_emit(reading(i), Instant::now()).unwrap();
    }
    assert_eq!(
        ingest.try_emit(reading(5), Instant::now()),
        Err(Refused::Full)
    );
    let waiting = {
        let ingest = ingest.clone();
        thread::spawn(move || ingest.emit(reading(5), Instant::now()))
    };
    thread::sleep(Duration::from_millis(50));
    assert!(!waiting.is_finished(), "the emit waits");
    assert_eq!(
        acked.lock().unwrap().len(),
        1,
        "nothing committed below the critical mark"
    );

    fs.set_space(100 * gib, 1000 * gib);
    assert_eq!(waiting.join().unwrap(), Ok(()));
    wait_until("all six", || acked.lock().unwrap().last().unwrap().1 == 6);
    let (waited, waits) = ingest.back_pressure();
    assert_eq!(waits, 1);
    assert!(
        waited >= Duration::from_millis(25),
        "the wait is measured: {waited:?}"
    );
    assert_eq!(ingest.health(), Health::Ok);
    drop(ingest);
    assert!(writer.join().fatal.is_none());
}

#[test]
fn below_the_low_mark_health_degrades_and_ingest_continues() {
    let fs = SimFs::new();
    let gib = 1u64 << 30;
    // 30 GiB free of 1000 GiB: below 5 % (50 GiB), above the critical mark.
    fs.set_space(30 * gib, 1000 * gib);
    let (writer, ingest, acked) = start(&fs, config());
    emit_acked(&ingest, &acked, 0);
    assert_eq!(ingest.health(), Health::Low);
    // On a small file system the 2 GiB floor applies: 1.5 GiB free of 10 GiB is low although above 5 %.
    fs.set_space(3 * gib / 2, 10 * gib);
    thread::sleep(Duration::from_millis(20));
    emit_acked(&ingest, &acked, 1);
    assert_eq!(ingest.health(), Health::Low);
    drop(ingest);
    writer.join();
}

#[test]
fn a_failed_timed_mark_sync_makes_the_writer_read_only() {
    let fs = SimFs::new();
    let config = Config {
        mark_sync: MarkSync::Thread(Duration::from_millis(5)),
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    emit_acked(&ingest, &acked, 0);
    fs.fail_next_sync(MARK_NAME, Fault::Io);
    wait_until("read-only", || ingest.health() == Health::Fatal);
    let mark_syncs = fs.syncs_of(MARK_NAME);
    assert_eq!(
        ingest.emit(reading(1), Instant::now()),
        Err(Refused::ReadOnly)
    );
    drop(ingest);
    let report = writer.join();
    assert!(report.fatal.unwrap().starts_with("mark sync"));
    assert_eq!(
        fs.syncs_of(MARK_NAME),
        mark_syncs,
        "no mark sync after the failure, not even at shutdown"
    );
}

#[test]
fn the_clean_shutdown_syncs_the_mark_once_and_the_log_directory_was_synced() {
    for mark_sync in [
        MarkSync::Off,
        MarkSync::Inline(Duration::from_secs(3600)),
        MarkSync::Thread(Duration::from_secs(3600)),
    ] {
        let fs = SimFs::new();
        let (writer, ingest, acked) = start(
            &fs,
            Config {
                mark_sync,
                ..config()
            },
        );
        assert_eq!(fs.dir_syncs(), 1, "after creating the mark");
        emit_acked(&ingest, &acked, 0);
        let before = fs.syncs_of(MARK_NAME);
        drop(ingest);
        assert!(writer.join().fatal.is_none());
        assert_eq!(fs.syncs_of(MARK_NAME), before + 1, "{mark_sync:?}");
        let on_disk = Mark::decode(&fs.read_disk(MARK_NAME).unwrap()).unwrap();
        assert_eq!(on_disk.last_sequence, 1);
    }
}

#[test]
fn the_writer_thread_mark_sync_also_runs_when_ingest_stops() {
    let fs = SimFs::new();
    let config = Config {
        mark_sync: MarkSync::Inline(Duration::from_millis(200)),
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    // The batch is acknowledged well within the first interval, so its own commit does not sync the mark,
    // and nothing else arrives: only the idle path can.
    emit_acked(&ingest, &acked, 0);
    wait_until("the idle mark sync", || {
        Mark::decode(&fs.read_disk(MARK_NAME).unwrap()).is_some_and(|m| m.last_sequence == 1)
    });
    drop(ingest);
    writer.join();
}

#[test]
fn writeback_between_the_mark_writes_can_put_a_half_updated_mark_on_disk() {
    // Problem 3 of the measurement notes: syncing the mark only between updates does not stop background
    // writeback from landing between the three writes of one update.
    let fs = SimFs::new();
    let config = Config {
        mark_sync: MarkSync::Inline(Duration::from_secs(3600)),
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    emit_acked(&ingest, &acked, 0);
    fs.writeback_on_write(MARK_NAME);
    // The next update: `seq` to odd reaches the disk, then the write of the fields fails (standing in for
    // a power loss at that instant).
    fs.fail_write_after(MARK_NAME, 1, Fault::Io);
    ingest.emit(reading(1), Instant::now()).unwrap();
    wait_until("read-only", || ingest.health() == Health::Fatal);
    drop(ingest);
    writer.join();
    fs.power_loss();
    assert_eq!(
        Mark::decode(&fs.read_disk(MARK_NAME).unwrap()),
        None,
        "odd sequence lock on disk"
    );
}

#[test]
fn every_batch_mark_sync_puts_the_mark_on_disk_before_the_acknowledgement() {
    let fs = SimFs::new();
    let config = Config {
        mark_sync: MarkSync::EveryBatch,
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    for i in 0..3 {
        emit_acked(&ingest, &acked, i);
        let on_disk = Mark::decode(&fs.read_disk(MARK_NAME).unwrap()).unwrap();
        assert_eq!(on_disk.last_sequence, u64::from(i) + 1);
    }
    drop(ingest);
    writer.join();
}

#[test]
fn the_timed_mark_sync_leaves_the_mark_on_disk_behind_until_it_runs() {
    let fs = SimFs::new();
    let config = Config {
        mark_sync: MarkSync::Thread(Duration::from_millis(20)),
        ..config()
    };
    let (writer, ingest, acked) = start(&fs, config);
    // Hold the mark thread's syncs so that the check below does not depend on timing.
    fs.pause_syncs(MARK_NAME);
    emit_acked(&ingest, &acked, 0);
    assert_eq!(
        Mark::decode(&fs.read_disk(MARK_NAME).unwrap())
            .unwrap()
            .last_sequence,
        0,
        "lags"
    );
    fs.resume_syncs();
    wait_until("the timed sync", || {
        Mark::decode(&fs.read_disk(MARK_NAME).unwrap()).is_some_and(|m| m.last_sequence == 1)
    });
    drop(ingest);
    writer.join();
}
