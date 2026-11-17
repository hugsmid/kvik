use std::path::Path;

use super::*;
use crate::content::Item;
use crate::fs::{FsFile, MemFs, OpenMode};

fn file(len: usize) -> Box<dyn FsFile> {
    let fs = MemFs::new();
    let f = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
    f.write_at(&bytes, 0).unwrap();
    f
}

#[test]
fn a_read_up_to_the_mark_writes_only_the_header_and_the_bytes_from_the_resume_point() {
    let f = file(4_096);
    let mut b = vec![0xAA; 1_024];
    read_part(&mut b, &*f, 2_048, 3_072, false).unwrap();
    let mut want = vec![0u8; 4_096];
    f.read_at(&mut want, 0).unwrap();
    assert_eq!(b.len(), 3_072);
    assert_eq!(b[..HEADER_LEN], want[..HEADER_LEN]);
    assert!(b[HEADER_LEN..1_024].iter().all(|&x| x == 0xAA));
    assert!(b[1_024..2_048].iter().all(|&x| x == 0));
    assert_eq!(b[2_048..], want[2_048..3_072]);
}

#[test]
fn a_read_to_the_end_zeroes_the_gap_and_bytes_past_the_end_of_the_file() {
    let f = file(3_000);
    let mut b = vec![0xAA; 4_096];
    read_part(&mut b, &*f, 2_048, 3_072, true).unwrap();
    let mut want = vec![0u8; 3_000];
    f.read_at(&mut want, 0).unwrap();
    assert_eq!(b[..HEADER_LEN], want[..HEADER_LEN]);
    assert!(b[HEADER_LEN..2_048].iter().all(|&x| x == 0));
    assert_eq!(b[2_048..3_000], want[2_048..]);
    assert!(b[3_000..].iter().all(|&x| x == 0));
    let short = file(40);
    let mut b = vec![0xAA; 128];
    read_part(&mut b, &*short, HEADER_LEN as u64, 128, false).unwrap();
    assert!(b[40..].iter().all(|&x| x == 0));
}

#[test]
fn the_kept_bytes_are_those_of_the_followed_segment_and_a_whole_file_read_is_freed() {
    use crate::facts::{FactFlags, FactRow, Quality, Value};
    use crate::store::{NewStore, StoreDir};
    use crate::writer::{Log, LogWriter, SystemClock, WriterConfig};

    let fs = Arc::new(MemFs::new());
    let new = NewStore {
        store_id: [1; 16],
        created_at: 0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "test".into(),
    };
    let store = StoreDir::open(fs.clone(), Path::new("s"), Some(&new)).unwrap();
    let cfg = WriterConfig {
        segment_size: 16 << 10,
        max_batch_bytes: 4 << 10,
        disk_critical: 0,
        disk_low_bytes: 0,
        relaxed_limits: true,
        ..WriterConfig::default()
    };
    let w = LogWriter::open(store, cfg, Some([2; 16]), Duration::ZERO, 0).unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let clock = Arc::new(SystemClock::default());
    let mut r = LogReader::open(ReaderConfig::new(
        "s",
        fs.clone(),
        clock,
        Some([2; 16]),
        [7; 16],
    ))
    .unwrap();
    let facts = |k: i64| -> Vec<Item> {
        (0..20)
            .map(|i| {
                Item::Fact(FactRow {
                    series_id: i,
                    t: k * 100 + i64::from(i),
                    quality: Quality::Good,
                    flags: FactFlags::default(),
                    value: Value::F64(k as u64),
                    changed_at: None,
                })
            })
            .collect()
    };
    let mut k = 0;
    let mut followed = false;
    while log.stats().open_file_id != Some(2) {
        log.submit(facts(k)).unwrap().wait().unwrap();
        k += 1;
        if log.stats().open_file_id == Some(1) && k == 5 {
            r.refresh().unwrap();
            assert_eq!(r.scratch.file_id, Some(1));
            assert_eq!(r.scratch.bytes.len() as u64, r.read_mark().unwrap().offset);
            followed = true;
        }
    }
    assert!(followed);
    log.submit(facts(k)).unwrap().wait().unwrap();
    r.refresh().unwrap();
    assert_eq!(r.scratch.file_id, Some(2));
    assert!(
        r.scratch.bytes.capacity() < 4 << 10,
        "the first segment's bytes are freed: {} kept",
        r.scratch.bytes.capacity()
    );
    log.shutdown().unwrap();
    // A mark behind the first segment's last batch: the pass ends inside that whole-file read, and still frees it.
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(crate::store::MARK);
    let (_, m) = crate::mark::read_mark(&fs.read(&path).unwrap(), &[1; 16])
        .unwrap()
        .unwrap();
    let behind = crate::mark::MarkSlot {
        counter: m.counter + 1,
        last_sequence: 20,
        ..m
    };
    fs.poke(
        &path,
        0,
        &crate::mark::encode_mark_file([Some(behind), None]),
    );
    let clock = Arc::new(SystemClock::default());
    let mut r = LogReader::open(ReaderConfig::new("s", fs, clock, Some([2; 16]), [8; 16])).unwrap();
    let u = r.refresh().unwrap();
    assert_eq!(u.state.durable.sequence, 20);
    assert_eq!((r.scratch.file_id, r.scratch.bytes.capacity()), (None, 0));
}

mod lookup_properties;
mod parts_and_lookup;
