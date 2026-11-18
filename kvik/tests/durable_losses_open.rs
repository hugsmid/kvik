//! A loss found while the store is open decides as a restart over it does, and what the process reports, counts and
//! answers after it (design §5.2 of the reconciling design: regions, Q-750 and Q-755, summaries, windows).

mod common;

use common::block_on;
use common::durable::{World, damage, data_lost, gauge, history, note, num};
use kvik::{Health, Kind, LossKind, Outcome, Reason, SeriesSpec, Store};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

/// Where the damaged batch is, and which reader finds it while the store is open.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// The newest batch: the loss watch, at `losses()`.
    Newest,
    /// An older batch of this run: the store's own reader, at its first query.
    Middle,
    /// A batch of the kept log, damaged after a restart: the store's own reader, at its first query (Q-928).
    Kept,
    /// Any batch, each the last of a sealed segment: the store's own reader at its first query, or the loss watch at
    /// `losses()`. A loss's `last` is left out: the bound of a damaged last batch differs by reader (Q-937).
    Sealed { query: bool },
}

/// How a store meets the damage.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Meets {
    /// Found while open.
    Open,
    /// Found while open, then restarted.
    OpenThenRestart,
    /// Restarted without finding it.
    Restart,
}

/// What a store gives after the damage: the replays' outcomes, the losses without `recorded`, and the series'
/// latest `t` and lateness reference.
#[derive(Debug, PartialEq)]
struct Seen {
    outcomes: Vec<String>,
    losses: Vec<String>,
    series: String,
}

fn text(k: usize) -> String {
    format!("NOTE-{k:03}")
}

fn open(w: &World, small: bool, sealed: bool) -> Store {
    let mut config = w.config();
    if sealed {
        config.log.seal_interval = std::time::Duration::from_millis(20);
    }
    if small {
        config.limits.window_memory = 4 << 10;
    }
    block_on(Store::open(config)).unwrap()
}

fn restart(w: &World, s: Store, small: bool, sealed: bool) -> Store {
    block_on(s.shutdown()).unwrap();
    drop(s);
    open(w, small, sealed)
}

fn run(name: &str, n: usize, at: usize, place: Place, meets: Meets, small: bool) -> Seen {
    let w = World::new(name);
    let sealed = matches!(place, Place::Sealed { .. });
    let mut s = open(&w, small, sealed);
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    for k in 0..n {
        block_on(s.emit(&[note(id, k as i64, &text(k))])).unwrap();
        if sealed {
            sealed_with(&w, text(k).as_bytes());
        }
    }
    if matches!(place, Place::Kept) {
        s = restart(&w, s, small, sealed);
    }
    damage(&w, text(at).as_bytes());
    if meets != Meets::Restart {
        match place {
            Place::Newest | Place::Sealed { query: false } => {
                assert_eq!(data_lost(&s.losses()).len(), 1)
            }
            Place::Middle | Place::Kept | Place::Sealed { query: true } => {
                assert_eq!(history(&s.reader(), id).len(), n - 1)
            }
        }
    }
    if meets != Meets::Open {
        s = restart(&w, s, small, sealed);
    }
    let mut outcomes = Vec::new();
    for k in (0..n).chain([n + 10]) {
        let ack = block_on(s.emit(&[note(id, k as i64, &text(k))])).unwrap();
        outcomes.extend(ack.outcomes.iter().map(|o| match o {
            Outcome::Recorded { unchecked, .. } => format!("recorded unchecked={unchecked}"),
            o => format!("{o:?}"),
        }));
    }
    let losses = s
        .losses()
        .iter()
        .map(|l| {
            let last = if sealed { None } else { l.last };
            format!("{:?} {:?} {:?} {:?}", l.kind, l.identity, l.first, last)
        })
        .collect();
    let info = s.series(id).unwrap();
    let series = format!("{:?} {:?}", info.latest_t, info.lateness_reference);
    block_on(s.shutdown()).unwrap();
    Seen {
        outcomes,
        losses,
        series,
    }
}

fn compare(n: usize, at: usize, place: Place, small: bool) -> Result<(), TestCaseError> {
    let at = match place {
        Place::Newest => n - 1,
        Place::Middle => at % (n - 1),
        Place::Kept | Place::Sealed { .. } => at % n,
    };
    let place_name: String = format!("{place:?}")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let name = |m: &str| format!("loss-open-{place_name}-{n}-{at}-{small}-{m}");
    let open = run(&name("open"), n, at, place, Meets::Open, small);
    let then = run(&name("then"), n, at, place, Meets::OpenThenRestart, small);
    let restarted = run(&name("restart"), n, at, place, Meets::Restart, small);
    prop_assert_eq!(&open, &then);
    prop_assert_eq!(&open, &restarted);
    prop_assert_eq!(
        open.outcomes[at].as_str(),
        "recorded unchecked=false",
        "the lost note's replay"
    );
    Ok(())
}

/// Three stores per case, compared on outcomes and flags (positions left out), on `losses()` apart from `recorded`
/// (and `last` with sealed segments, Q-937) and on `series()`: one finds the loss while open, one finds it and restarts, one restarts without finding it.
#[test]
fn a_loss_found_while_open_decides_as_a_restart_over_it_does() {
    let cases = (ProptestConfig::default().cases / 8).max(8);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let place = prop_oneof![
        Just(Place::Newest),
        Just(Place::Middle),
        Just(Place::Kept),
        any::<bool>().prop_map(|query| Place::Sealed { query }),
    ];
    runner
        .run(
            &(3usize..9, 0usize..8, place, any::<bool>()),
            |(n, at, place, small)| compare(n, at, place, small),
        )
        .unwrap();
}

/// Q-750 and Q-755 as ruled (design §2.3): after a found loss, a replay whose key's segment holds a second damaged
/// batch that no scanning reader has met is recorded unchecked and counted for the run.
#[test]
fn after_a_found_loss_a_replay_beside_unfound_damage_is_recorded_unchecked_and_counted() {
    let w = World::new("second-damage");
    let mut config = w.config();
    config.log.seal_interval = std::time::Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    block_on(s.emit(&[note(id, 0, "SD-ONE"), note(id, 1, "SD-TWO")])).unwrap();
    sealed_with(&w, b"SD-TWO");
    block_on(s.emit(&[note(id, 5, "SD-THREE")])).unwrap();
    // Damage is made to sealed segments only: a seal writes the segment from the writer's own bytes.
    sealed_with(&w, b"SD-THREE");
    damage(&w, b"SD-THREE");
    assert_eq!(data_lost(&s.losses()).len(), 1);
    // Damage no scanning reader is asked to meet before the replays.
    damage(&w, b"SD-ONE");
    let ack = block_on(s.emit(&[note(id, 1, "SD-TWO"), note(id, 5, "SD-THREE")])).unwrap();
    assert!(
        matches!(
            ack.outcomes[..],
            [
                Outcome::Recorded {
                    unchecked: true,
                    ..
                },
                Outcome::Recorded {
                    unchecked: false,
                    ..
                }
            ]
        ),
        "{:?}",
        ack.outcomes
    );
    let health = s.health();
    assert!(
        matches!(&health, Health::Degraded { reasons } if reasons.iter().any(|r| matches!(r, Reason::LocalDataUnreadable { readings: 1, .. }))),
        "{health:?}"
    );
    block_on(s.shutdown()).unwrap();
}

/// Waits until every segment holding `text` is sealed.
fn sealed_with(w: &World, text: &[u8]) {
    for _ in 0..4_000 {
        let held: Vec<_> = w
            .segments()
            .into_iter()
            .filter(|(_, b)| b.windows(text.len()).any(|x| x == text))
            .collect();
        if !held.is_empty()
            && held
                .iter()
                .all(|(p, _)| p.extension().is_some_and(|e| e == "kvl"))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the segment holding the text was not sealed");
}

/// Q-930: after a loss found while open, `series` answers as a fresh reader of every fact does.
#[test]
fn series_after_a_loss_found_while_open_equals_a_fresh_full_read() {
    let w = World::new("summaries-after-loss");
    let s = w.open();
    let g = block_on(s.define_series(gauge("a/b/power"))).unwrap();
    for k in 0..3 {
        block_on(s.emit(&[num(g, k, 1_000.0 + k as f64)])).unwrap();
    }
    assert_eq!(
        s.series(g).unwrap().latest_t,
        w.reader().series(g).unwrap().latest_t
    );
    let newest = 1_002.0f64.to_bits().to_le_bytes();
    damage(&w, &newest);
    assert_eq!(data_lost(&s.losses()).len(), 1);
    let fresh = w.reader().series(g).unwrap();
    let held = s.series(g).unwrap();
    assert_eq!(
        (held.latest_t, held.lateness_reference),
        (fresh.latest_t, fresh.lateness_reference)
    );
    assert_eq!(held.latest_t, Some(common::at(1)));
    block_on(s.shutdown()).unwrap();
}

/// Design §2.2: after a loss found while open, in-order ingest takes no slow path; a start that passed over an old
/// loss keeps its windows, so a replay of an intact recent fact is decided in memory.
#[test]
fn in_order_ingest_after_a_loss_and_a_start_over_one_take_no_slow_path() {
    let w = World::new("windows-kept");
    let s = w.open();
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    for (k, t) in ["WK-ONE", "WK-TWO", "WK-THREE"].iter().enumerate() {
        block_on(s.emit(&[note(id, k as i64, t)])).unwrap();
    }
    damage(&w, b"WK-THREE");
    assert_eq!(data_lost(&s.losses()).len(), 1);
    let before = s.stats().ingest.slow_path;
    for k in 3..10 {
        block_on(s.emit(&[note(id, k, "WK-LATER")])).unwrap();
    }
    assert_eq!(s.stats().ingest.slow_path, before);
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = w.open();
    let ack = block_on(s.emit(&[note(id, 1, "WK-TWO")])).unwrap();
    assert!(matches!(ack.outcomes[..], [Outcome::Duplicate]));
    assert_eq!(s.stats().ingest.slow_path, 0);
    assert!(
        s.losses()
            .iter()
            .any(|l| l.kind == LossKind::DataLost && l.recorded)
    );
    block_on(s.shutdown()).unwrap();
}

/// The Q-925 count at the store, under either rule: after a found loss, a reading whose key and lateness reference both
/// lie in damage no scanning reader has met is recorded unchecked and counted once in `readings`; one whose reference
/// alone lies there is counted too.
#[test]
fn a_reading_with_its_key_and_reference_unreadable_is_counted_once() {
    let w = World::new("reference-unread");
    let mut config = w.config();
    config.log.seal_interval = std::time::Duration::from_millis(20);
    let s = block_on(Store::open(config)).unwrap();
    let g = block_on(
        s.define_series(
            gauge("a/b/power")
                .staleness(std::time::Duration::from_secs(10))
                .lateness_bound(std::time::Duration::from_secs(3_600)),
        ),
    )
    .unwrap();
    let bytes = |v: f64| v.to_bits().to_le_bytes();
    block_on(s.emit(&[num(g, -100, 1_234.5), num(g, -40, 1_235.5)])).unwrap();
    sealed_with(&w, &bytes(1_235.5));
    block_on(s.emit(&[num(g, 0, 777.25)])).unwrap();
    sealed_with(&w, &bytes(777.25));
    damage(&w, &bytes(777.25));
    assert_eq!(data_lost(&s.losses()).len(), 1);
    damage(&w, &bytes(1_234.5));
    let readings = |s: &Store| match s.health() {
        Health::Degraded { reasons } => reasons.iter().find_map(|r| match r {
            Reason::LocalDataUnreadable { readings, .. } => Some(*readings),
            _ => None,
        }),
        _ => None,
    };
    // -110: its key outside the damaged segment, its reference facts inside: checked, counted.
    // -70: its key inside as well: unchecked, counted once.
    let ack = block_on(s.emit(&[num(g, -110, 6.0), num(g, -70, 5.0)])).unwrap();
    let unchecked: Vec<bool> = ack
        .outcomes
        .iter()
        .map(|o| match o {
            Outcome::Recorded { unchecked, .. } => *unchecked,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(unchecked, [false, true]);
    assert_eq!(readings(&s), Some(2));
    block_on(s.shutdown()).unwrap();
}
