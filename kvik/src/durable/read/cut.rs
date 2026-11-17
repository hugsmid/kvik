//! The durable cut per log (query design §3.2): `(manifest epoch, durable position, reset stamp)`, held by a read
//! while it runs, with the stamp rule of its call kind (spec §7 Readers).
//!
//! - A materialised call (`live`, `at`, `window`, `buckets`, `calendar`, `interval`, `series`, and `facts` until it
//!   streams, roadmap 9.0f) refreshes, holds its cut for the call and ends it at its return. A pass the log reader
//!   discarded because the mark's reset stamp changed while it read is read once more, under the new cut; a second
//!   such change is [`QueryError::CutExpired`] (Q-850).
//! - A cut held across calls (a snapshot, roadmap 9.0c; an iteration, 9.0f) ends with `CutExpired` once its epoch was
//!   last found current `cut_max_age` ago or a renewal found the lease gone or changed; the lease names the lowest
//!   epoch of the live cuts.

use kvik_log::reader::{LogReader, ReaderError, Update};

use crate::error::QueryError;

/// A cut of the one log this build reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cut {
    pub(crate) id: u64,
    /// The manifest epoch whose files it reads.
    pub(crate) epoch: u64,
    pub(crate) durable: kvik_log::writer::Position,
    pub(crate) stamp: kvik_log::mark::ResetStamp,
}

/// Refreshes `log` with the stamp rule of a materialised call and returns the update to apply.
pub(crate) fn refresh(log: &mut LogReader) -> Result<Result<Update, QueryError>, ReaderError> {
    let first = log.refresh()?;
    if !first.discarded {
        return Ok(Ok(first));
    }
    let again = log.refresh()?;
    Ok(if again.discarded {
        Err(QueryError::CutExpired)
    } else {
        Ok(again)
    })
}

/// Takes the cut of what `log` holds.
pub(crate) fn take(log: &mut LogReader) -> Cut {
    let c = log.hold();
    Cut {
        id: c.id,
        epoch: c.epoch,
        durable: c.durable,
        stamp: c.stamp,
    }
}

/// Whether `cut` is live, renewing the lease if due: [`ReaderError::CutEnded`] once its epoch was last found current
/// `cut_max_age` ago or its lease was found gone or changed.
pub(crate) fn check(log: &mut LogReader, cut: &Cut) -> Result<(), ReaderError> {
    log.check_cut(cut.id)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake};
    use std::time::Duration;

    use crate::durable::reader::DurableReader;
    use crate::env::{DirEntry, Env, Fs, FsFile, ManualClock, MemFs, OpenMode};
    use crate::error::QueryError;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::time::Timestamp;
    use crate::types::{Reading, SeriesId, Value};
    use crate::{Store, StoreConfig};

    const T0: i64 = 1_767_225_600_000_000_000;
    const MAX_AGE: Duration = Duration::from_secs(60);

    fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut f = std::pin::pin!(f);
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    /// Called at each open of the mark with how many opens came before.
    type MarkHook = Box<dyn FnMut(&MemFs, u32) + Send>;

    #[derive(Debug)]
    struct HookFs {
        inner: MemFs,
        hook: Mutex<(u32, Option<HookBox>)>,
    }

    struct HookBox(MarkHook);

    impl std::fmt::Debug for HookBox {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("HookBox")
        }
    }

    impl Fs for HookFs {
        fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn FsFile>> {
            if mode == OpenMode::Read && path.ends_with("log/MARK") {
                let mut h = self.hook.lock().unwrap();
                let n = h.0;
                h.0 += 1;
                if let Some(HookBox(f)) = h.1.as_mut() {
                    f(&self.inner, n);
                }
            }
            self.inner.open(path, mode)
        }
        fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
            self.inner.read_dir(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.inner.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }
        fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
            self.inner.sync_dir(path)
        }
        fn free_space(&self, path: &Path) -> std::io::Result<u64> {
            self.inner.free_space(path)
        }
    }

    /// A store holding one series and two facts, shut down, on a file system that hooks the reads of the mark.
    struct Rig {
        dir: &'static str,
        clock: ManualClock,
        env: Env,
        fs: Arc<HookFs>,
    }

    impl Rig {
        fn new(dir: &'static str) -> Self {
            let clock = ManualClock::new(Timestamp(T0));
            let fs = Arc::new(HookFs {
                inner: MemFs::new(),
                hook: Mutex::new((0, None)),
            });
            let mut env = Env::test(clock.clone());
            env.fs = fs.clone();
            let store = block_on(Store::open(
                StoreConfig::new(dir, "Etc/UTC")
                    .create_if_missing(true)
                    .env(env.clone()),
            ))
            .unwrap();
            let spec = SeriesSpec::new(
                "a/s/v",
                Kind::Gauge {
                    value: ValueType::F64,
                },
                "W",
            );
            block_on(store.define_series(spec)).unwrap();
            for k in 1..=2 {
                let r = Reading::new(SeriesId(1), Timestamp(T0 + k), Value::F64(k as f64));
                block_on(store.emit(&[r])).unwrap();
            }
            block_on(store.shutdown()).unwrap();
            Rig {
                dir,
                clock,
                env,
                fs,
            }
        }

        fn reader(&self) -> DurableReader {
            DurableReader::open(
                Path::new(self.dir),
                &self.env,
                Duration::from_secs(1_800),
                MAX_AGE,
            )
            .unwrap()
        }

        /// From now on, the second read of the mark in each of the next `times` passes finds a new reset stamp.
        fn reset_at_second_reads(&self, times: u32) {
            let path = Path::new(self.dir).join("log/MARK");
            let mut left = times;
            *self.fs.hook.lock().unwrap() = (
                0,
                Some(HookBox(Box::new(move |m: &MemFs, n: u32| {
                    if n.is_multiple_of(2) || left == 0 {
                        return;
                    }
                    left -= 1;
                    let b = m.read(&path).unwrap();
                    let (_, mark) = kvik_log::mark::read_mark(&b, &store_id(&b))
                        .unwrap()
                        .unwrap();
                    let next = kvik_log::mark::MarkSlot {
                        counter: mark.counter + 1,
                        stamp: kvik_log::mark::ResetStamp::new_at(
                            mark.last_sequence + u64::from(n),
                        ),
                        ..mark
                    };
                    m.poke(
                        &path,
                        0,
                        &kvik_log::mark::encode_mark_file([Some(next), None]),
                    );
                }))),
            );
        }
    }

    fn facts(r: &DurableReader) -> Result<usize, QueryError> {
        r.with(|i, _| {
            i.facts(
                SeriesId(1),
                crate::time::TimeRange::new(Timestamp(i64::MIN), Timestamp(i64::MAX)),
                crate::read::View::History,
            )
            .map(|f| f.len())
        })
    }

    fn store_id(mark_file: &[u8]) -> [u8; 16] {
        let mut id = [0u8; 16];
        id.copy_from_slice(&mark_file[16..32]);
        id
    }

    #[test]
    fn a_call_that_outlives_cut_max_age_ends_with_cut_expired_and_releases_its_cut() {
        let rig = Rig::new("cut-age");
        let r = rig.reader();
        let clock = rig.clock.clone();
        let quick = r.with(|_, _| {
            clock.advance(MAX_AGE - Duration::from_nanos(1));
            Ok(())
        });
        assert!(quick.is_ok());
        let slow = r.with(|_, _| {
            clock.advance(MAX_AGE);
            Ok(())
        });
        assert!(matches!(slow, Err(QueryError::CutExpired)), "{slow:?}");
        assert_eq!(facts(&r).unwrap(), 2);
        assert_eq!(r.cuts_held(), 0);
    }

    #[test]
    fn the_stores_own_reads_call_once_more_after_a_cut_ends_and_then_report_it() {
        let rig = Rig::new("cut-again");
        let r = rig.reader();
        let clock = rig.clock.clone();
        let mut calls = 0;
        let once = r.with_again(|_, _| {
            calls += 1;
            if calls == 1 {
                clock.advance(MAX_AGE);
            }
            Ok(calls)
        });
        assert_eq!(once.unwrap(), 2);
        let always = r.with_again(|_, _| {
            clock.advance(MAX_AGE);
            Ok(())
        });
        assert!(matches!(always, Err(QueryError::CutExpired)));
    }

    #[test]
    fn a_reset_during_a_call_is_read_again_under_the_new_cut_and_a_second_is_cut_expired() {
        let rig = Rig::new("cut-stamp");
        let once = rig.reader();
        rig.reset_at_second_reads(1);
        assert_eq!(facts(&once).unwrap(), 2);
        assert_ne!(
            once.refresh().unwrap().stamp.nonce,
            0,
            "the new cut's stamp"
        );
        let twice = rig.reader();
        rig.reset_at_second_reads(2);
        assert!(matches!(facts(&twice), Err(QueryError::CutExpired)));
        assert_eq!(
            facts(&twice).unwrap(),
            2,
            "nothing of the failed call was kept"
        );
    }
}
