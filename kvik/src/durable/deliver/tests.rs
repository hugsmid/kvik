use std::path::Path;
use std::sync::Mutex;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::fs::{Fs, MemFs, OpenMode};
use kvik_log::lease::LeaseSlot;
use kvik_log::store::{NewStore, StoreDir};
use kvik_log::writer::{Clock, Log, LogWriter, SystemClock, WriterConfig};

use super::*;

#[derive(Debug, Default)]
struct Manual(Mutex<Duration>);

impl Clock for Manual {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }
    fn wall(&self) -> i64 {
        1_767_225_600_000_000_000
    }
}

fn counter(fs: &MemFs, r: &LogReader) -> u64 {
    let f = fs.open(r.lease_path(), OpenMode::Read).unwrap();
    let mut b = vec![0u8; 4_096];
    let n = f.read_at(&mut b, 0).unwrap();
    LeaseSlot::decode_file(&b[..n], &[1; 16])
        .unwrap()
        .refresh_counter
}

#[test]
fn a_reader_at_the_writers_durable_position_only_renews_its_lease_when_due() {
    let fs = MemFs::new();
    let new = NewStore {
        store_id: [1; 16],
        created_at: 0,
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "test".into(),
    };
    let store = StoreDir::open(Arc::new(fs.clone()), Path::new("s"), Some(&new)).unwrap();
    let w = LogWriter::open(
        store,
        WriterConfig::default(),
        Some([2; 16]),
        Duration::ZERO,
        0,
    )
    .unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let alias = |id| {
        vec![Item::Entry(Entry::Alias {
            id,
            name: format!("a{id}"),
        })]
    };
    log.submit(alias(1)).unwrap().wait().unwrap();
    let clock = Arc::new(Manual::default());
    let mut cfg = LogReaderConfig::new(
        "s",
        Arc::new(fs.clone()),
        clock.clone(),
        Some([2; 16]),
        [7; 16],
    );
    cfg.lease_refresh = Duration::from_secs(2);
    let mut r = LogReader::open(cfg).unwrap();
    assert_eq!(read_new(&mut r, None).unwrap().unwrap().events.len(), 1);
    let at = counter(&fs, &r);
    let durable = log.stats().durable;
    assert!(read_new(&mut r, Some(durable)).unwrap().is_none());
    assert_eq!(counter(&fs, &r), at);
    *clock.0.lock().unwrap() += Duration::from_millis(1_100);
    assert!(read_new(&mut r, Some(durable)).unwrap().is_none());
    assert_eq!(
        counter(&fs, &r),
        at + 1,
        "renewed once half of lease_refresh passed"
    );
    log.submit(alias(2)).unwrap().wait().unwrap();
    let u = read_new(&mut r, Some(log.stats().durable))
        .unwrap()
        .unwrap();
    assert_eq!(u.events.len(), 1);
    assert_eq!(counter(&fs, &r), at + 2);
    log.shutdown().unwrap();
}
