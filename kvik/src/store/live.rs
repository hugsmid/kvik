//! The [`LiveSession`] handle (`docs/api.md` Live reads).

use std::time::Duration;

use super::reader::{Reader, ReaderInner};
use crate::error::QueryError;
use crate::read::{LiveId, LiveOptions, Query, SessionOptions, UpdateBatch};

/// Live reads sharing cuts (the query design's section 3.6): each [`Query`] opened gets the result at the cut of its
/// `Opened`, then the changes as new data becomes durable, every live read of the session at one cut per batch.
/// There is no thread and no queue: [`LiveSession::next`] computes the batch when called. Dropping the session
/// cancels every live read.
#[derive(Debug)]
pub struct LiveSession {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Model(crate::backend::live::Session),
    /// The store's own reader is opened at the first batch.
    Durable {
        reader: Reader,
        opts: SessionOptions,
        pending: Vec<(LiveId, Query, LiveOptions)>,
        session: Option<crate::durable::read::live::Session>,
        last_id: u64,
    },
}

impl LiveSession {
    pub(crate) fn new(reader: &Reader, opts: SessionOptions) -> Result<Self, QueryError> {
        opts.check()?;
        let inner = match &reader.inner {
            ReaderInner::Model(s) => Inner::Model(crate::backend::live::Session::new(s.clone())),
            _ => Inner::Durable {
                reader: reader.clone(),
                opts,
                pending: Vec::new(),
                session: None,
                last_id: 0,
            },
        };
        Ok(LiveSession { inner })
    }

    /// Opens a live read; its `Opened` comes with the next batch, at that batch's cut. A selection by a pattern
    /// outside the routing grammar is [`QueryError::Invalid`].
    pub fn open(&mut self, query: Query, options: LiveOptions) -> Result<LiveId, QueryError> {
        match &mut self.inner {
            Inner::Model(s) => s.open(query),
            Inner::Durable {
                session,
                pending,
                last_id,
                ..
            } => {
                check(&query)?;
                *last_id += 1;
                let id = LiveId(*last_id);
                match session {
                    Some(s) => s.open(id, query, options),
                    None => pending.push((id, query, options)),
                }
                Ok(id)
            }
        }
    }

    /// Ends a live read: nothing more is sent for it. Returns whether it was open.
    pub fn cancel(&mut self, id: LiveId) -> bool {
        match &mut self.inner {
            Inner::Model(s) => s.cancel(id),
            Inner::Durable {
                session: Some(s), ..
            } => s.cancel(id),
            Inner::Durable { pending, .. } => {
                let n = pending.len();
                pending.retain(|(k, ..)| *k != id);
                pending.len() != n
            }
        }
    }

    /// The next batch, waiting up to `timeout` for one: `None` if nothing changed by then. A batch comes when new
    /// data became durable, a live read was opened, or a result changes with `now` alone, and at least
    /// `min_batch_interval` after the previous one. An error of the shared tail read ([`QueryError::NotAvailable`]
    /// for a segment that cannot be read, Q-951) is returned at every call it lasts, never passed over.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<UpdateBatch>, QueryError> {
        match &mut self.inner {
            Inner::Model(s) => s.next(timeout),
            Inner::Durable {
                reader,
                opts,
                pending,
                session,
                ..
            } => {
                if session.is_none() {
                    let r = reader.durable().expect("a durable reader")?;
                    let mut s =
                        crate::durable::read::live::Session::new(r, reader.progress(), *opts);
                    for (id, q, o) in pending.drain(..) {
                        s.open(id, q, o);
                    }
                    *session = Some(s);
                }
                session.as_mut().expect("opened above").next(timeout)
            }
        }
    }
}

pub(crate) fn check(q: &Query) -> Result<(), QueryError> {
    match q {
        Query::Latest(sel) | Query::Tail { sel } => sel.check(),
    }
}
