//! A log dropped while its owner unwinds must release the store.

mod seal_rig;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kvik_log::fs::MemFs;
use kvik_log::store::StoreDir;
use kvik_log::writer::{Log, LogWriter, SystemClock};
use seal_rig::{T0, cfg, dir, new_store};

#[test]
fn a_log_dropped_by_a_panic_releases_the_store() {
    let fs = MemFs::new();
    let store = StoreDir::open(Arc::new(fs.clone()), &dir(), Some(&new_store())).unwrap();
    let w = LogWriter::open(store, cfg(), Some([1; 16]), Duration::ZERO, T0).unwrap();
    let log = Log::start(w, Arc::new(SystemClock::default())).unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _log = log;
        panic!("the caller panics while it owns the log");
    }));
    assert!(r.is_err());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match StoreDir::open(Arc::new(fs.clone()), &dir(), None) {
            Ok(_) => break,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "the store is still locked: {e:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
