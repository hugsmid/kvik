//! The bounded ingest queue (`spec.md` §3 Back-pressure) and how a batch is taken from it (`format.md` A.4 Sizes).

use std::collections::VecDeque;

use super::Call;

/// Where the writer takes the calls of its next batch from.
pub trait CallSource {
    /// Whether no call waits.
    fn is_empty(&self) -> bool;
    /// Takes whole calls, in order, while their data blocks stay within `max_bytes`; always at least one call if
    /// any waits.
    fn take(&mut self, max_bytes: u64) -> Vec<Call>;
    /// Puts calls taken by [`CallSource::take`] back at the front, in the same order.
    fn put_back(&mut self, calls: Vec<Call>);
    /// The wall clock read after the last [`CallSource::take`], so after every call it took was queued: a batch's
    /// `received_at` is no earlier (`spec.md` §2.2 `received_at`, §2.3 As-of time; Q-1070). A source fed from other
    /// threads must implement it; `None` keeps the step's own read.
    fn taken_at(&self) -> Option<i64> {
        None
    }
}

/// The queue is full: a call must wait (`emit`) or is refused as busy (`try_emit`).
#[derive(Debug)]
pub struct QueueFull(pub Call);

/// A bounded queue of calls, counted in entries (`limits.ingest_queue`, default 65,536).
#[derive(Debug)]
pub struct IngestQueue {
    calls: VecDeque<Call>,
    entries: usize,
    capacity: usize,
    next_id: u64,
}

impl IngestQueue {
    /// An empty queue holding at most `capacity` entries.
    pub fn new(capacity: usize) -> Self {
        Self {
            calls: VecDeque::new(),
            entries: 0,
            capacity,
            next_id: 1,
        }
    }

    /// Queues a call and returns its id, unless it would take the queue past its capacity. A call larger than the
    /// whole capacity is accepted into an empty queue (calls are limited to the capacity when made).
    pub fn push(&mut self, mut call: Call) -> Result<u64, QueueFull> {
        if !self.calls.is_empty() && self.entries + call.len() > self.capacity {
            return Err(QueueFull(call));
        }
        call.id = self.next_id;
        self.next_id += 1;
        self.entries += call.len();
        self.calls.push_back(call);
        Ok(self.next_id - 1)
    }

    /// Entries queued.
    pub fn depth(&self) -> usize {
        self.entries
    }

    /// Whether a call of `len` entries would be accepted now.
    pub fn has_room(&self, len: usize) -> bool {
        self.calls.is_empty() || self.entries + len <= self.capacity
    }

    /// Removes every queued call.
    pub fn drain(&mut self) -> Vec<Call> {
        self.entries = 0;
        self.calls.drain(..).collect()
    }
}

impl CallSource for IngestQueue {
    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    fn take(&mut self, max_bytes: u64) -> Vec<Call> {
        let mut out = Vec::new();
        let mut bytes = 0u64;
        while let Some(c) = self.calls.front() {
            // A call holding a recovery entry is alone in its batch (`format.md` A.6).
            let alone = c.is_alone() || out.first().is_some_and(Call::is_alone);
            if !out.is_empty() && (alone || bytes + c.data_len() > max_bytes) {
                break;
            }
            bytes += c.data_len();
            self.entries -= c.len();
            out.extend(self.calls.pop_front());
        }
        out
    }

    fn put_back(&mut self, calls: Vec<Call>) {
        for c in calls.into_iter().rev() {
            self.entries += c.len();
            self.calls.push_front(c);
        }
    }
}
