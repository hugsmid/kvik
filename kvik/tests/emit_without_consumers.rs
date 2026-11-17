//! An emit while no consumer is registered leaves the read-back of its batch to the `kvik-deliver` thread (Q-706):
//! a consumer registered during or after such emits receives what its start promises, and the delivery counters
//! reach the same counts, later.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::block_on;
use common::world::{World, drain, gauge, num};
use kvik::{DeliveryKind, Funnel, Outcome, Position, Select, SeriesId, Start, Store};

fn recorded(s: &Store, id: SeriesId, secs: i64) -> Position {
    match block_on(s.emit(&[num(id, secs, 1.0)])).unwrap().outcomes[0] {
        Outcome::Recorded { position, .. } => position,
        ref o => panic!("{o:?}"),
    }
}

fn fact_positions(c: &mut kvik::Consumer) -> Vec<Position> {
    drain(c)
        .into_iter()
        .map(|d| match d.kind {
            DeliveryKind::Fact(_) => d.position,
            k => panic!("{k:?}"),
        })
        .collect()
}

#[test]
fn a_consumer_taken_after_many_emits_without_one_receives_what_its_start_promises() {
    let w = World::new("after-many");
    let s = w.open();
    let g = block_on(s.define_series(gauge("p/a/power"))).unwrap();
    for i in 0..1_000 {
        recorded(&s, g, i);
    }
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    assert!(
        c.try_next().unwrap().is_none(),
        "a new consumer is sent no resync"
    );
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    assert!(fact_positions(&mut c).is_empty());
    let later: Vec<Position> = (1_000..1_010).map(|i| recorded(&s, g, i)).collect();
    assert_eq!(fact_positions(&mut c), later);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_consumer_taken_during_emits_receives_every_fact_after_its_route_once_and_in_order() {
    let mut raced = 0;
    for round in 0..12u64 {
        let w = World::new(&format!("during-{round}"));
        let s = w.open();
        let ids: Vec<SeriesId> = (0..4)
            .map(|i| block_on(s.define_series(gauge(&format!("p/s{i}/power")))).unwrap())
            .collect();
        let routed = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = ids
            .iter()
            .map(|&id| {
                let (s, routed) = (s.clone(), routed.clone());
                std::thread::spawn(move || {
                    (0..60)
                        .map(|i| {
                            let after = routed.load(Ordering::SeqCst);
                            (recorded(&s, id, i), after)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        std::thread::sleep(Duration::from_micros(round * 400));
        let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        block_on(c.route(Funnel::facts(Select::All))).unwrap();
        routed.store(true, Ordering::SeqCst);
        let calls: Vec<(Position, bool)> = writers
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all: Vec<Position> = calls.iter().map(|c| c.0).collect();
        all.sort();
        while got.last() != all.last() {
            assert!(Instant::now() < deadline, "round {round}: {got:?}");
            got.extend(fact_positions(&mut c));
        }
        assert!(
            got.windows(2).all(|p| p[0] < p[1]),
            "round {round}: {got:?}"
        );
        let first = got.first().copied().unwrap_or(Position {
            generation: u32::MAX,
            sequence: u64::MAX,
        });
        let expected: Vec<Position> = all.iter().copied().filter(|p| *p >= first).collect();
        assert_eq!(got, expected, "round {round}");
        for (p, after) in &calls {
            assert!(
                !after || got.contains(p),
                "round {round}: {p:?} began after the route"
            );
        }
        raced += usize::from(!got.is_empty() && got.len() < all.len());
        block_on(s.shutdown()).unwrap();
    }
    assert!(
        raced > 0,
        "no round took the consumer while the writers were emitting"
    );
}

/// The first byte at which the open segment differs from `before`.
fn first_change(before: &[u8], now: &[u8]) -> usize {
    before
        .iter()
        .zip(now)
        .position(|(a, b)| a != b)
        .unwrap_or(before.len().min(now.len()))
}

/// Emits two calls, damaging the second's batch while its sync runs, before the mark moves past it. Returns the
/// delivery reader's `corruptions` when the emit returned and once the `kvik-deliver` thread has caught up.
fn corruptions_after_damage(name: &str, consumer: bool) -> (u64, u64) {
    let w = World::new(name);
    let s = w.open();
    let g = block_on(s.define_series(gauge("p/a/power"))).unwrap();
    let c = consumer.then(|| {
        let c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        block_on(c.route(Funnel::facts(Select::All))).unwrap();
        c
    });
    recorded(&s, g, 0);
    let segment =
        w.fs.paths()
            .into_iter()
            .find(|p| p.starts_with(&w.dir) && p.extension().is_some_and(|e| e == "open"))
            .unwrap();
    let before = w.fs.read(&segment).unwrap();
    let armed = Arc::new(AtomicBool::new(true));
    let (fs, at, flag) = (w.fs.clone(), segment.clone(), armed.clone());
    w.fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if p == at && flag.swap(false, Ordering::SeqCst) {
            let now = fs.read(&at).unwrap();
            let i = first_change(&before, &now) + 24;
            fs.poke(&at, i, &[now[i] ^ 0xFF]);
        }
    })));
    let p = recorded(&s, g, 1);
    w.fs.set_sync_hook(None);
    assert!(!armed.load(Ordering::SeqCst), "the batch sync ran the hook");
    let at_return = s.stats().delivery.corruptions;
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.stats().delivery.corruptions == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    let later = s.stats().delivery.corruptions;
    assert!(p.sequence > 0);
    drop(c);
    block_on(s.shutdown()).unwrap();
    (at_return, later)
}

#[test]
fn the_delivery_counters_reach_the_same_counts_with_or_without_a_consumer() {
    let (with_now, with_later) = corruptions_after_damage("counted-with", true);
    let (_, without_later) = corruptions_after_damage("counted-without", false);
    assert_eq!(with_now, 1, "counted when the emit returns");
    assert_eq!(with_later, 1);
    assert_eq!(without_later, 1, "counted later by the delivery thread");
}
