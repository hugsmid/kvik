//! The sink's worker threads, and the runtime-neutral future that waits for a
//! job's result: `poll` only looks at a slot and never blocks.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use kvik_sink_api::SinkError;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Named worker threads behind a bounded queue, joined when the pool is
/// dropped, after the queued jobs have run.
pub(crate) struct Pool {
    tx: Option<mpsc::SyncSender<Job>>,
    queue: usize,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    pub(crate) fn new(workers: usize, queue: usize, name: &str) -> Result<Self, SinkError> {
        let (tx, rx) = mpsc::sync_channel::<Job>(queue);
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::new();
        for i in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            let handle = std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
                            rx.recv()
                        };
                        match job {
                            Ok(job) => job(),
                            Err(_) => break,
                        }
                    }
                })
                .map_err(|e| SinkError::Failed {
                    reason: format!("cannot start a worker thread: {e}"),
                })?;
            threads.push(handle);
        }
        Ok(Pool {
            tx: Some(tx),
            queue,
            threads,
        })
    }

    /// Queues `f` and returns a future of its result, or of `Busy` when the
    /// queue is full. A queued job runs to the end even if the future is
    /// dropped.
    pub(crate) fn run<T, F>(&self, f: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, SinkError> + Send + 'static,
    {
        let slot = Arc::new(Mutex::new(Slot {
            value: None,
            waker: None,
        }));
        let pending = Pending {
            slot: Arc::clone(&slot),
        };
        let job: Job = Box::new(move || {
            let value = catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
                Err(SinkError::Failed {
                    reason: "a sink worker panicked".into(),
                })
            });
            complete(&slot, value);
        });
        let refused = match self.tx.as_ref().map(|tx| tx.try_send(job)) {
            Some(Ok(())) => None,
            Some(Err(mpsc::TrySendError::Full(_))) => Some(SinkError::Busy { queued: self.queue }),
            _ => Some(SinkError::Failed {
                reason: "the sink's workers have stopped".into(),
            }),
        };
        if let Some(e) = refused {
            complete(&pending.slot, Err(e));
        }
        pending
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.tx = None;
        for t in self.threads.drain(..) {
            // Workers hold no pool, so this never joins its own thread.
            let _ = t.join();
        }
    }
}

struct Slot<T> {
    value: Option<Result<T, SinkError>>,
    waker: Option<Waker>,
}

fn complete<T>(slot: &Mutex<Slot<T>>, value: Result<T, SinkError>) {
    let waker = {
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        s.value = Some(value);
        s.waker.take()
    };
    if let Some(w) = waker {
        w.wake();
    }
}

/// The result of a queued job.
pub(crate) struct Pending<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Future for Pending<T> {
    type Output = Result<T, SinkError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match s.value.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
