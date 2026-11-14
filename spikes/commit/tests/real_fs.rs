//! Smoke test of the real file system: preallocation, every sync call available on this platform, free space
//! and a scan of what was written. On Linux this is the only test that runs `fdatasync`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use commit::format::{self, Reading};
use commit::fs::{FileSystem, RealFs, SyncCall};
use commit::writer::{Config, MarkSync, Writer, segment_name};

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("commit-real-fs")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn rows() -> Vec<Reading> {
    (0..10)
        .map(|i| Reading {
            series_id: i,
            t: i64::from(i),
            value: f64::from(i),
        })
        .collect()
}

#[test]
fn every_sync_call_makes_a_scannable_segment() {
    for call in SyncCall::all() {
        let dir = scratch(&call.name().replace(' ', "-"));
        let fs = RealFs::new(&dir, call).unwrap();
        let header = format::log_segment_header(1, 1, 1, 0, [3; 16]);
        let mut segment = fs
            .create_preallocated("log/1-0000000000000001.open", 1 << 20, &header)
            .unwrap();
        let batch = format::encode_batch(1, 1, 0, 0, &rows());
        segment.write_at(64, &batch.bytes).unwrap();
        segment.sync().unwrap();
        fs.sync_log_dir().unwrap();
        let space = fs.space().unwrap();
        assert!(space.free > 0 && space.total >= space.free, "{call:?}");
        let bytes = std::fs::read(dir.join("log/1-0000000000000001.open")).unwrap();
        assert_eq!(bytes.len(), 1 << 20);
        let scan = format::scan(&bytes, 1);
        assert_eq!(scan.batches.len(), 1, "{call:?}");
        assert_eq!(scan.batches[0].last_sequence, 10);
        assert_eq!(scan.end, format::ScanEnd::EndOfData);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn the_writer_commits_through_the_real_file_system() {
    let dir = scratch("writer");
    let fs: Arc<dyn FileSystem> = Arc::new(RealFs::new(&dir, SyncCall::durable()).unwrap());
    let config = Config {
        segment_len: 1 << 20,
        mark_sync: MarkSync::Inline(std::time::Duration::from_millis(1)),
        ..Config::default()
    };
    let (writer, ingest) = Writer::start(fs, config, Box::new(|_| {})).unwrap();
    for r in rows() {
        ingest.emit(r, Instant::now()).unwrap();
    }
    drop(ingest);
    let report = writer.join();
    assert!(report.fatal.is_none());
    assert_eq!(report.readings, 10);
    let bytes = std::fs::read(dir.join(segment_name(1, 1))).unwrap();
    let scan = format::scan(&bytes, 1);
    assert_eq!(scan.batches.last().unwrap().last_sequence, 10);
    let mark = format::Mark::decode(&std::fs::read(dir.join("log/MARK")).unwrap()).unwrap();
    assert_eq!((mark.last_sequence, mark.offset), (10, scan.valid_end));
    let _ = std::fs::remove_dir_all(&dir);
}
