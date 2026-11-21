//! The log started on the directory the restore executor leaves (roadmap 8.3c with 8.3b): `init_generation`, a
//! writer in the claimed generation at its first position, and a reader past what the record sets aside.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kvik_backup::plan::PlanMode;
use kvik_files::desc::{FileClass, FileDescription, kind};
use kvik_log::batch::{BatchBuilder, SegmentIdentity};
use kvik_log::block::block_type;
use kvik_log::body::DataPrefix;
use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::fs::MemFs;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig};
use kvik_log::segment::SegmentImage;
use kvik_log::store::StoreDir;
use kvik_log::writer::{
    Log, LogWriter, Position, StartOptions, StartRule, SystemClock, WriterConfig, init_generation,
};
use support::*;

const T0: i64 = 1_767_225_600_000_000_000;

/// A sealed segment of generation 0 holding one alias entry per position, `first` to `first + n - 1`.
fn sealed(file_id: u64, first: u64, n: u64) -> (FileDescription, Vec<u8>) {
    let id = SegmentIdentity {
        store_id: ID,
        file_id,
        generation: 0,
    };
    let mut seg = SegmentImage::create_ahead(id, T0, 8192).unwrap();
    seg.take_into_use(first).unwrap();
    for seq in first..first + n {
        let mut body = vec![0u8; 24];
        DataPrefix {
            first_sequence: seq,
            received_at: T0,
            count: 1,
        }
        .write(&mut body);
        body.extend_from_slice(&[3, 0, 0, 0, 16, 0, 0, 0]);
        body.extend_from_slice(&[1, 0, 4, 0, 7, 0, 0, 0, 2, 0, 1, 0, b'a', 0, 0, 0]);
        let mut b = BatchBuilder::new();
        b.push(block_type::ENTRIES, body).unwrap();
        seg.append(&b, T0).unwrap();
    }
    let end = seg.write_seal_blocks(Vec::new()).unwrap();
    seg.set_len(end);
    let plain = seg.into_bytes();
    let d = FileDescription {
        kind: kind::LOG_SEGMENT,
        class: FileClass::None,
        generation: 0,
        file_id,
        range: Some(((0, first), (0, first + n - 1))),
        hash: sha256(&plain),
        inputs: Vec::new(),
    };
    (d, plain)
}

#[test]
fn a_full_restore_opens_in_its_generation_at_its_first_position_and_reads_its_restored_log() {
    let mut src = source(ID, 0, false);
    src.segments = vec![sealed(1, 1, 3), sealed(2, 4, 2)];
    let sink = Arc::new(FaultSink::new(mem_with(&bucket(&mut src))));
    let fs = MemFs::new();
    let (p, _) = restore(&mut restorer(sink, &fs, PlanMode::Full, KEY)).unwrap();
    assert_eq!((p.record.generation, p.record.restored_end), (1, (0, 5)));
    let start = p.record.generation_start();
    let store = StoreDir::open(Arc::new(fs.clone()), Path::new(DIR), None).unwrap();
    init_generation(&store, &start, T0).unwrap();
    let w = LogWriter::open_with(
        store,
        WriterConfig::default(),
        Some([3; 16]),
        Duration::ZERO,
        T0,
        StartOptions {
            generation: Some(start),
            ..StartOptions::default()
        },
    )
    .unwrap();
    assert!(w.fault().is_none(), "{:?}", w.fault());
    assert_eq!(w.start_report().rule, StartRule::FirstStart);
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let alias = Item::Entry(Entry::Alias {
        id: 1,
        name: "hall".into(),
    });
    let ack = log.submit(vec![alias]).unwrap().wait().unwrap();
    assert_eq!(
        ack.first,
        Position {
            generation: 1,
            sequence: 1
        }
    );
    let mut cfg = ReaderConfig::new(
        DIR,
        Arc::new(fs.clone()),
        Arc::new(SystemClock::default()),
        Some([3; 16]),
        [9; 16],
    );
    cfg.set_aside = p.record.set_aside();
    let mut reader = LogReader::open(cfg).unwrap();
    let read: Vec<(u32, u64)> = reader
        .refresh()
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some((b.first.generation, b.first.sequence)),
            _ => None,
        })
        .collect();
    assert_eq!(read, vec![(0, 1), (0, 2), (0, 3), (0, 4), (0, 5), (1, 1)]);
    reader.close().unwrap();
    log.shutdown().unwrap();
}
