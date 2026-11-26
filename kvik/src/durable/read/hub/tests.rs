use kvik_log::facts::{FactFlags, FactRow, Quality};

use super::*;

#[test]
fn a_run_of_empty_refreshes_a_cursor_stands_inside_is_kept_until_taken() {
    let hub = Hub::default();
    let now = Duration::ZERO;
    let (sub, _) = hub.subscribe((100, 1 << 20), now);
    hub.every(sub);
    hub.start(sub, 0, (100, 1 << 20));
    for _ in 0..3 {
        hub.finish(hub.collector(), now, Duration::MAX);
    }
    hub.advance(sub, 2, (Reach::All, Reach::All));
    let mut c = hub.collector().unwrap();
    let p = LogPos {
        generation: 0,
        sequence: 1,
    };
    c.push(Piece::Fact(Stored {
        position: p,
        received_at: 0,
        row: FactRow {
            series_id: 1,
            t: 0,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: LogValue::F64(0),
            changed_at: None,
        },
    }));
    let seq = hub.finish(Some(c), now, Duration::MAX);
    assert_eq!(seq, 4);
    let t = hub.take(sub, seq, now);
    assert!(!t.lagged, "{t:?}");
    assert_eq!(t.rounds.len(), 2);
}

fn fact(series: u32, sequence: u64) -> Piece {
    Piece::Fact(Stored {
        position: LogPos {
            generation: 0,
            sequence,
        },
        received_at: 0,
        row: FactRow {
            series_id: series,
            t: 0,
            quality: Quality::Good,
            flags: FactFlags::default(),
            value: LogValue::F64(0),
            changed_at: None,
        },
    })
}

fn round(hub: &Hub, pieces: Vec<Piece>) -> u64 {
    let mut c = hub.collector().unwrap();
    for p in pieces {
        c.push(p);
    }
    let seq = hub.finish(Some(c), Duration::ZERO, Duration::MAX);
    hub.ring_pending();
    seq
}

#[test]
fn a_round_rings_only_the_subscribers_it_reaches_and_the_others_caught_up_pass_over_it() {
    let hub = Hub::default();
    let cap = (100, 1 << 20);
    let subs: Vec<(u64, Arc<Bell>)> = (0..3).map(|_| hub.subscribe(cap, Duration::ZERO)).collect();
    for (k, (s, _)) in subs.iter().enumerate() {
        hub.start(*s, 0, cap);
        hub.advance(*s, 0, (Reach::Series([k as u32 + 1].into()), Reach::All));
    }
    let rung = || subs.iter().map(|(_, b)| b.rung()).collect::<Vec<_>>();
    let seq = round(&hub, vec![fact(2, 1)]);
    assert_eq!(rung(), [0, 1, 0]);
    // Subscribers 1 and 3 passed over it; 2 must take it, so it is kept.
    assert_eq!(hub.held().2, 1);
    assert!(hub.take(subs[0].0, seq, Duration::ZERO).rounds.is_empty());
    let t = hub.take(subs[1].0, seq, Duration::ZERO);
    assert_eq!((t.rounds.len(), t.lagged), (1, false));
    // Subscriber 2 has a round still to take: the next round rings it whatever it holds.
    let seq = round(&hub, vec![fact(3, 2)]);
    assert_eq!(rung(), [0, 2, 1]);
    hub.advance(subs[1].0, seq - 1, (Reach::Series([2].into()), Reach::All));
    // An entry, a void or a gap reaches everyone.
    round(&hub, vec![Piece::Void(LogPos::default())]);
    assert_eq!(rung(), [1, 3, 2]);
}

#[test]
fn a_round_ending_between_an_open_and_the_batch_that_answers_it_is_not_passed_over() {
    let hub = Hub::default();
    let cap = (100, 1 << 20);
    let (sub, bell) = hub.subscribe(cap, Duration::ZERO);
    hub.start(sub, 0, cap);
    hub.advance(sub, 0, (Reach::Series([1].into()), Reach::All));
    hub.open(sub, cap);
    let seq = round(&hub, vec![fact(2, 1)]);
    hub.advance(sub, 0, (Reach::Series([1, 2].into()), Reach::All));
    let t = hub.take(sub, seq, Duration::ZERO);
    assert_eq!(bell.rung(), 1);
    assert!(!t.lagged);
    assert_eq!(
        t.rounds.len(),
        1,
        "the fact of the series just opened was passed over"
    );
}

#[test]
fn a_waiting_session_is_not_marked_idle_and_a_refresh_rings_only_its_series() {
    let hub = Hub::default();
    let (sub, bell) = hub.subscribe((100, 1 << 20), Duration::ZERO);
    hub.advance(sub, 0, (Reach::All, Reach::Series([4].into())));
    std::thread::scope(|s| {
        let waiter = s.spawn(|| bell.wait(0, Duration::from_secs(20)));
        while !bell.waiting.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        hub.finish(
            hub.collector(),
            Duration::from_secs(600),
            Duration::from_secs(60),
        );
        hub.ring_refreshed([5].into());
        assert!(hub.subscribed());
        hub.ring_refreshed([4].into());
        assert_eq!(waiter.join().unwrap(), 1);
    });
    assert!(!hub.take(sub, 1, Duration::from_secs(600)).idle);
}

#[test]
fn rounds_every_subscriber_took_are_dropped_by_one_scan_when_the_last_holder_advances() {
    let hub = Hub::default();
    let cap = (100, 1 << 20);
    let subs: Vec<u64> = (0..50)
        .map(|_| hub.subscribe(cap, Duration::ZERO).0)
        .collect();
    for s in &subs {
        hub.start(*s, 0, cap);
    }
    let seq = round(&hub, vec![fact(1, 1)]);
    let scans = hub.scans();
    for s in &subs[..49] {
        hub.advance(*s, seq, (Reach::All, Reach::All));
    }
    assert_eq!(hub.held(), (1, std::mem::size_of::<Piece>() as u64, 1));
    assert_eq!(hub.scans(), scans);
    hub.advance(subs[49], seq, (Reach::All, Reach::All));
    assert_eq!(hub.held(), (0, 0, 0));
    assert_eq!(hub.scans(), scans + 1);
    // A subscriber advancing again over a round it took frees nothing and scans nothing.
    hub.advance(subs[0], seq, (Reach::All, Reach::All));
    assert_eq!(hub.scans(), scans + 1);
}

#[test]
fn the_oldest_round_waits_for_a_subscriber_started_below_it() {
    let hub = Hub::default();
    let cap = (100, 1 << 20);
    let (a, _) = hub.subscribe(cap, Duration::ZERO);
    hub.start(a, 0, cap);
    let first = round(&hub, vec![fact(1, 1)]);
    let second = round(&hub, vec![fact(1, 2)]);
    let (b, _) = hub.subscribe(cap, Duration::ZERO);
    hub.start(b, first, cap);
    hub.advance(a, second, (Reach::All, Reach::All));
    assert_eq!(hub.held().2, 1);
    hub.advance(b, second, (Reach::All, Reach::All));
    assert_eq!(hub.held().2, 0);
}

#[test]
fn only_a_holder_of_the_oldest_round_counts_down_and_a_start_above_it_stops_holding_it() {
    let hub = Hub::default();
    let cap = (100, 1 << 20);
    let subs: Vec<u64> = (0..3)
        .map(|_| hub.subscribe(cap, Duration::from_secs(100)).0)
        .collect();
    for s in &subs {
        hub.start(*s, 0, cap);
    }
    // Subscriber 2 is idle: it holds nothing back, and its advance counts nothing down.
    hub.touch(subs[0], Duration::from_secs(200));
    hub.touch(subs[1], Duration::from_secs(200));
    let mut c = hub.collector().unwrap();
    c.push(fact(1, 1));
    let seq = hub.finish(Some(c), Duration::from_secs(200), Duration::from_secs(50));
    let scans = hub.scans();
    hub.advance(subs[2], seq, (Reach::All, Reach::All));
    hub.advance(subs[1], seq, (Reach::All, Reach::All));
    hub.advance(subs[1], seq, (Reach::All, Reach::All));
    assert_eq!((hub.held().2, hub.scans()), (1, scans));
    hub.start(subs[0], seq, cap);
    let seq = round(&hub, vec![fact(1, 2)]);
    hub.start(subs[0], seq, cap);
    hub.advance(subs[1], seq, (Reach::All, Reach::All));
    assert_eq!(hub.held().2, 0);
}
