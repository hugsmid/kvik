//! A queued call's acknowledgement as the interface waits for it (Q-123 (b)).

use super::*;

/// How a queued call's acknowledgement becomes its [`Ack`].
pub(super) type Finish =
    Box<dyn FnOnce(Result<LogAck, CommitError>) -> Result<Ack, EmitError> + Send + Sync>;

/// Wakes every waker registered with a [`Completion`].
#[derive(Debug, Default)]
struct FanOut(Mutex<Vec<Waker>>);

impl Wake for FanOut {
    fn wake(self: Arc<Self>) {
        let wakers = std::mem::take(&mut *lock(&self.0));
        for w in wakers {
            w.wake();
        }
    }
}

/// Resolves a [`Completion`] when the log completes its call: the log wakes it on its writer thread, after the
/// covering sync, whether or not anyone waits for the call.
#[derive(Debug)]
struct Resolver(Weak<Completion>);

impl Wake for Resolver {
    fn wake(self: Arc<Self>) {
        if let Some(c) = self.0.upgrade() {
            let _ = c.poll(None);
        }
    }
}

/// What a call does once the log has given its result, whoever waits for it.
pub(super) type OnAck = Box<dyn FnOnce(&Result<LogAck, CommitError>) + Send>;

struct CompletionState {
    pending: Option<LogPending>,
    result: Option<Result<LogAck, CommitError>>,
    on_ack: Option<OnAck>,
}

impl std::fmt::Debug for CompletionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionState")
            .field("pending", &self.pending)
            .field("result", &self.result)
            .finish_non_exhaustive()
    }
}

/// The log's acknowledgement of one queued call, shared by the call's own future and by every later call whose
/// outcome depends on it (a duplicate of one of its readings, Q-123 (b)), each of which may wait on it.
#[derive(Debug)]
pub(crate) struct Completion {
    state: Mutex<CompletionState>,
    wakers: Arc<FanOut>,
    resolver: Waker,
}

impl Completion {
    /// A completion whose `on_ack` runs once the log gives the call's result, on the log's writer thread or in the
    /// first poll that finds it.
    pub(super) fn new(pending: LogPending, on_ack: OnAck) -> Arc<Self> {
        let c = Arc::new_cyclic(|me| Completion {
            state: Mutex::new(CompletionState {
                pending: Some(pending),
                result: None,
                on_ack: Some(on_ack),
            }),
            wakers: Arc::new(FanOut::default()),
            resolver: Waker::from(Arc::new(Resolver(me.clone()))),
        });
        // Registers the resolver with the log, or resolves at once.
        let _ = c.poll(None);
        c
    }

    /// The call's result once the log has given it; registers `cx`'s waker otherwise.
    pub(crate) fn poll(&self, cx: Option<&Context<'_>>) -> Poll<Result<LogAck, CommitError>> {
        let mut st = lock(&self.state);
        if let Some(r) = &st.result {
            return Poll::Ready(r.clone());
        }
        if let Some(cx) = cx {
            let mut w = lock(&self.wakers.0);
            if !w.iter().any(|x| x.will_wake(cx.waker())) {
                w.push(cx.waker().clone());
            }
        }
        let mut resolver_cx = Context::from_waker(&self.resolver);
        let Some(pending) = st.pending.as_mut() else {
            return Poll::Pending;
        };
        match std::pin::Pin::new(pending).poll(&mut resolver_cx) {
            Poll::Ready(r) => {
                st.result = Some(r.clone());
                st.pending = None;
                let on_ack = st.on_ack.take();
                drop(st);
                if let Some(f) = on_ack {
                    f(&r);
                }
                Waker::from(self.wakers.clone()).wake();
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A call waiting for the log: its own queued batch, or the queued call an outcome of it depends on; and how that
/// call's acknowledgement becomes this call's [`Ack`].
pub(crate) struct Queued {
    pub(super) completion: Arc<Completion>,
    pub(super) finish: Finish,
}

impl std::fmt::Debug for Queued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queued")
            .field("completion", &self.completion)
            .finish()
    }
}

impl Queued {
    pub(crate) fn poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<LogAck, CommitError>> {
        self.completion.poll(Some(cx))
    }

    pub(crate) fn finish(self, r: Result<LogAck, CommitError>) -> Result<Ack, EmitError> {
        (self.finish)(r)
    }
}

/// What an emit gives back: the outcomes at once, or a call queued in the log.
pub(crate) enum Emitted {
    Ready(Ack),
    Queued(Queued),
}
