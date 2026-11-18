//! The views a reader publishes (Q-900, Q-903): what one refresh found, shared by every call that takes it and
//! read with no lock. A view holds one cut of the log reader for as long as it lives; a call's cut is its view's
//! (Q-903).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kvik_log::manifest::LostRange;
use kvik_log::reader::ReaderState;

use super::cut::Cut;
use super::latest::Latest;
use crate::durable::index::Index;

/// What the views of one reader tell it about their cuts.
#[derive(Debug, Default)]
pub(crate) struct Cuts {
    /// Cuts of views that no call holds any more, for the next refresh to release.
    dropped: Mutex<Vec<u64>>,
    /// How many times the lease was found gone or changed, which ends every cut taken before (spec §7 Readers).
    lost: AtomicU64,
}

impl Cuts {
    /// Takes the cuts of the views dropped since the last call.
    pub(crate) fn take_dropped(&self) -> Vec<u64> {
        std::mem::take(&mut *self.dropped.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The lease was found gone or changed: every view taken before ends.
    pub(crate) fn lose_all(&self) {
        self.lost.fetch_add(1, Ordering::SeqCst);
    }
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct View {
    latest: Latest,
    /// Every fact, for the calls that read facts until roadmap 9.0f (Q-1000).
    facts: Option<Index>,
    state: ReaderState,
    lost_ranges: Arc<[LostRange]>,
    cut: Cut,
    /// The number of the refresh's round in the reader's shared tail ([`super::hub`]).
    round: u64,
    /// When the refresh that took the cut began, on the reader's monotonic clock: the cut's epoch was current from
    /// then on.
    current_at: Duration,
    /// [`Cuts::lost`] when the cut was taken.
    lost_seen: u64,
    cuts: Arc<Cuts>,
}

impl Drop for View {
    fn drop(&mut self) {
        self.cuts
            .dropped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.cut.id);
    }
}

impl View {
    /// A view of `latest` and `facts` under `cut`, taken by a refresh that began at `current_at`.
    pub(crate) fn new(
        (latest, facts): (Latest, Option<Index>),
        state: ReaderState,
        lost_ranges: Arc<[LostRange]>,
        (cut, round): (Cut, u64),
        current_at: Duration,
        cuts: Arc<Cuts>,
    ) -> Self {
        let lost_seen = cuts.lost.load(Ordering::SeqCst);
        View {
            latest,
            facts,
            state,
            lost_ranges,
            cut,
            round,
            current_at,
            lost_seen,
            cuts,
        }
    }

    /// What the log reader held after the refresh.
    pub(crate) fn state(&self) -> ReaderState {
        self.state
    }

    /// The manifest's lost ranges at the refresh.
    pub(crate) fn lost_ranges(&self) -> &[LostRange] {
        &self.lost_ranges
    }

    /// The latest values, the registry and the losses as of the cut.
    pub(crate) fn latest(&self) -> &Latest {
        &self.latest
    }

    /// Every fact as of the cut, if the reader holds them (Q-1000).
    pub(crate) fn index(&self) -> Option<&Index> {
        self.facts.as_ref()
    }

    /// The cut.
    pub(crate) fn cut(&self) -> &Cut {
        &self.cut
    }

    /// The number of the refresh's round in the reader's shared tail.
    pub(crate) fn round(&self) -> u64 {
        self.round
    }

    /// How long the cut has been live at `now`.
    pub(crate) fn age(&self, now: Duration) -> Duration {
        now.saturating_sub(self.current_at)
    }

    /// Whether the cut is live at `now`, with no lock: younger than `max_age` and no lease found lost since it was
    /// taken. Later refreshes do not renew its age: a call ends within `cut_max_age` of its own refresh (Q-903).
    pub(crate) fn live(&self, now: Duration, max_age: Duration) -> bool {
        self.cuts.lost.load(Ordering::SeqCst) == self.lost_seen && self.age(now) < max_age
    }
}
