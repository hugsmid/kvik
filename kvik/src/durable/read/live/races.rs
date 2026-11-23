//! Live sessions against a writer committing beside them, with the fan-out thread running: rounds end at any point of
//! a session's turn (Q-1118 (a)).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use super::tests::{Held, TICK, World, apply, gauge};
use crate::durable::tests::block_on;
use crate::read::{LiveEvent, LiveOptions, Query};
use crate::time::Timestamp;
use crate::types::{Position, Reading, SeriesId, Value};
use crate::{Answer, Select, Store};

/// A `Tail` read: where it opened, the last cut it was brought to, and what it holds.
type Opened = (Position, Position, Held);

/// Emits readings of one series, each at its own instant.
#[derive(Clone)]
struct Busy {
    store: Store,
    id: SeriesId,
    next: Arc<AtomicI64>,
}

impl Busy {
    fn emit(&self) {
        let t = self.next.fetch_add(1_000_000, Ordering::SeqCst);
        let r = Reading::new(self.id, Timestamp(t), Value::F64((t % 1_000) as f64));
        block_on(self.store.emit(&[r])).unwrap();
    }
}

#[test]
fn tails_opened_beside_a_busy_writer_hold_every_fact_above_their_opened_cut() {
    let w = World::new("open-races");
    let s = w.open();
    let quiet = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    let busy = Busy {
        id: block_on(s.define_series(gauge("a/c/power"))).unwrap(),
        store: s.clone(),
        next: Arc::new(AtomicI64::new(w.clock.wall().0 - 3_600_000_000_000)),
    };
    let r = s.reader();
    let stop = AtomicBool::new(false);
    let tails: Vec<Opened> = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                busy.emit();
            }
        });
        let runs: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| opens(&w, &r, quiet, &busy, 15)))
            .collect();
        let tails = runs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        stop.store(true, Ordering::SeqCst);
        tails
    });
    let history = w.history();
    for (open, cut, held) in &tails {
        let skipped = |p: Position| {
            held.lagged
                .iter()
                .chain(&held.gaps)
                .any(|(a, b)| *a <= p && p <= *b)
        };
        let owed: Vec<_> = history
            .iter()
            .filter(|f| f.series == busy.id && f.position > *open && f.position <= *cut)
            .filter(|f| !skipped(f.position))
            .cloned()
            .collect();
        assert_eq!(held.tail, owed, "opened at {open:?}, brought to {cut:?}");
    }
    block_on(s.shutdown()).unwrap();
}

/// Opens `n` `Tail`s of `busy` one after another in a session that keeps a `Tail` of `quiet`, so that it passes over
/// the rounds between them; each is read for a few batches, then cancelled.
fn opens(w: &World, r: &crate::Reader, quiet: SeriesId, busy: &Busy, n: usize) -> Vec<Opened> {
    let tail = |id: SeriesId| Query::Tail {
        sel: Select::Ids(vec![id]).into(),
    };
    let mut session = r.session();
    session.open(tail(quiet), LiveOptions::default()).unwrap();
    let mut out = Vec::new();
    for _ in 0..n {
        let id = session.open(tail(busy.id), LiveOptions::default()).unwrap();
        let b = busy.clone();
        if let Some(d) = session.durable_mut() {
            d.before_advance.0 = Some(Box::new(move || {
                b.emit();
                std::thread::sleep(Duration::from_millis(20));
            }));
        }
        let mut held = BTreeMap::new();
        let (mut open, mut cut) = (None, None);
        for _ in 0..4 {
            w.clock.advance(TICK);
            let Some(b) = session.next(Duration::from_millis(50)).unwrap() else {
                continue;
            };
            let opened = b
                .events
                .iter()
                .any(|(k, e)| *k == id && matches!(e, LiveEvent::Opened(Answer::Tail)));
            if opened {
                open = Some(b.durable);
            }
            apply(&mut held, &b);
            cut = Some(b.durable);
        }
        session.cancel(id);
        // A turn after the cancel narrows what reaches the session to `quiet`, so it passes over `busy`'s rounds.
        for _ in 0..2 {
            w.clock.advance(TICK);
            assert!(session.next(Duration::from_millis(20)).unwrap().is_none());
        }
        if let (Some(open), Some(cut)) = (open, cut) {
            out.push((open, cut, held.remove(&id).unwrap_or_default()));
        }
    }
    out
}
