//! Consumers on the durable backend (roadmap 5.2): routing changes and cursor files survive restarts, a power loss
//! ends in a reset point carrying `forget_above`, `emits` and routing cycles, the start of a new consumer and of one
//! whose cursor file was lost (Q-131), routing again at every start (Q-91 (c)), the cursor file never ahead of the
//! synced mark (Q-132, invariant 26) and the registry file after each routing change (Q-130). The property tests are
//! in `consumer_properties.rs`.

mod common;

use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::block_on;
use common::world::{World, drain, gauge, num, ready, resync};
use kvik::env::{Fs, OpenMode};
use kvik::{
    Condition, ConditionState, Consumer, ConsumerError, DefineError, Delivery, DeliveryKind,
    Funnel, MarkSync, Outcome, Position, Refusal, ResetStamp, ResyncCause, SavedPoint, Select,
    Start, Store, TriggerCause,
};

/// A consumer taken for the first time: it is sent no resync (Q-131).
fn new_consumer(s: &Store, name: &str, emits: Select) -> Consumer {
    let mut c = block_on(s.consumer(name, Start::Now, emits)).unwrap();
    assert!(
        c.try_next().unwrap().is_none(),
        "a new consumer has nothing to forget"
    );
    c
}

#[test]
fn routes_cursors_and_latches_survive_a_restart() {
    let w = World::new("restart");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarms", Select::None);
    let f1 = block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let cond = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }))
    .unwrap();
    block_on(c.unroute(f1)).unwrap();
    // Funnel ids continue above every id named, removed ones included.
    let f3 = block_on(c.route(Funnel::facts(Select::Names("plant/*/power".into())))).unwrap();
    assert_eq!((f1.0, cond.0, f3.0), (1, 2, 3));
    for (i, v) in [5.0, 20.0].into_iter().enumerate() {
        block_on(s.emit(&[num(g, i as i64, v)])).unwrap();
        w.clock.advance(Duration::from_secs(1));
    }
    let ds = drain(&mut c);
    let states: Vec<_> = ds
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Trigger(t) => Some((t.funnel, t.state.clone(), t.cause)),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![(cond, ConditionState::True, TriggerCause::Fact)]
    );
    let confirmed = s.consumer_stats("alarms").unwrap().cursor;
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    // The routing table is rebuilt from the log; the cursor and the latch come from the cursor file.
    let s = w.open();
    let mut c = block_on(s.consumer("alarms", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("alarms").unwrap().cursor, confirmed);
    let routes: Vec<_> = c.routes().unwrap().into_iter().map(|(id, _)| id).collect();
    assert_eq!(routes, vec![cond, f3]);
    let ds = drain(&mut c);
    assert_eq!(ds.len(), 1, "nothing to replay: {ds:?}");
    let r = resync(&ds[0]).unwrap();
    assert_eq!((r.cause, r.forget_above), (ResyncCause::Resume, None));
    let fs = r.funnels.iter().find(|f| f.funnel == cond).unwrap();
    assert_eq!(fs.condition, Some(ConditionState::True));
    assert!(!fs.transition, "the latch holds the confirmed state");
    // The next funnel id stays above the highest one named.
    assert_eq!(block_on(c.route(Funnel::facts(Select::All))).unwrap().0, 4);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_power_loss_ends_the_resume_with_a_reset_point() {
    let mut w = World::new("power-loss");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let mut positions = Vec::new();
    for i in 0..4 {
        w.clock.advance(Duration::from_secs(1));
        let ack = block_on(s.emit(&[num(g, i, i as f64)])).unwrap();
        positions.push(ack.last().unwrap());
        // The first two are confirmed, a second apart, so the cursor file names the second.
        if i < 2 {
            drain(&mut c);
        }
    }
    let k = s.stats().log.durable;
    assert_eq!(s.consumer_stats("log").unwrap().cursor, positions[1]);
    w.crash(s, vec![c], true);

    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("log").unwrap().cursor, positions[1]);
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        got.push(d);
    }
    // The facts after the cursor are replayed, then the resync that ends the resume is also the reset point of
    // the recovery entry: `forget_above` is the last kept position and its id the entry's position.
    let replayed: Vec<Position> = got
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Fact(f) => {
                assert!(d.flags.replayed);
                Some(f.position)
            }
            _ => None,
        })
        .collect();
    assert_eq!(replayed, positions[2..].to_vec());
    let last = got.last().unwrap();
    let r = resync(last).unwrap();
    assert_eq!(r.cause, ResyncCause::Resume);
    assert_eq!(r.forget_above, Some(k));
    assert!(last.position > k);
    // Nothing further until it is confirmed; its confirmation writes the cursor file naming its id.
    block_on(s.emit(&[num(g, 9, 9.0)])).unwrap();
    assert!(c.try_next().unwrap().is_none());
    for d in &got {
        block_on(c.confirm(d.id)).unwrap();
    }
    assert_eq!(s.consumer_stats("log").unwrap().cursor, last.position);
    let next = c.try_next().unwrap().unwrap();
    assert!(matches!(&next.kind, DeliveryKind::Fact(f) if f.position > last.position));
    assert!(!next.flags.replayed);
    // A process crash on the same boot keeps the cursor written at the confirmation.
    let id = last.position;
    w.crash(s, vec![c], false);
    let s = w.open();
    let _c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    assert_eq!(s.consumer_stats("log").unwrap().cursor, id);
}

#[test]
fn a_reset_point_in_the_replay_waits_for_its_confirmation() {
    let mut w = World::new("reset-middle");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    // Facts, a power loss, more facts, another power loss; the consumer is not taken in between.
    let s = w.open();
    block_on(s.emit(&[num(g, 1, 1.0)])).unwrap();
    let k1 = s.stats().log.durable;
    w.crash(s, Vec::new(), true);
    let s = w.open();
    block_on(s.emit(&[num(g, 2, 2.0)])).unwrap();
    let k2 = s.stats().log.durable;
    w.crash(s, Vec::new(), true);

    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let mut first = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        first.push(d);
    }
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(matches!(&first[0].kind, DeliveryKind::Fact(f) if f.number() == Some(1.0)));
    let r = resync(&first[1]).unwrap();
    assert_eq!(
        (r.cause, r.forget_above),
        (ResyncCause::ResetPoint, Some(k1))
    );
    assert!(first[1].position > k1 && first[1].position < k2);
    block_on(c.confirm(first[1].id)).unwrap();
    let rest = drain(&mut c);
    assert!(matches!(&rest[0].kind, DeliveryKind::Fact(f) if f.number() == Some(2.0)));
    let r = resync(rest.last().unwrap()).unwrap();
    assert_eq!((r.cause, r.forget_above), (ResyncCause::Resume, Some(k2)));
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_failed_cursor_sync_turns_the_store_read_only_and_is_never_retried() {
    use kvik_log::fs::FaultOp;
    let mut w = World::new("cursor-fault");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let c = new_consumer(&s, "log", Select::None);
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
    w.crash(s, vec![c], true);
    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        got.push(d);
    }
    let last = got.last().unwrap();
    assert!(resync(last).unwrap().forget_above.is_some());
    // The sync of the cursor file that the reset point's confirmation writes fails.
    w.fs.fail_nth(FaultOp::Sync, "cursors", 0, std::io::ErrorKind::Other);
    let syncs = || {
        w.fs.ops()
            .iter()
            .filter(|o| o.op == FaultOp::Sync && o.path.to_string_lossy().contains("cursors"))
            .count()
    };
    let before = syncs();
    assert!(matches!(
        block_on(c.confirm(last.id)),
        Err(ConsumerError::Write(_))
    ));
    assert_eq!(syncs(), before + 1);
    assert!(matches!(s.health(), kvik::Health::ReadOnly { .. }));
    // Nothing further is delivered, and no cursor file is written again, by a call, the timer or the shutdown.
    block_on(s.emit(&[num(g, 1, 2.0)])).ok();
    assert!(c.try_next().is_err());
    w.clock.advance(Duration::from_secs(2));
    s.tick().unwrap();
    assert!(block_on(s.shutdown()).is_err());
    assert_eq!(syncs(), before + 1, "a failed sync is never retried");
}

#[test]
fn emits_are_declared_and_routing_cycles_are_refused() {
    let w = World::new("emits");
    let s = w.open();
    let x = block_on(s.define_series(gauge("site/a/in"))).unwrap();
    let y = block_on(s.define_series(gauge("site/a/out"))).unwrap();
    let v = block_on(s.define_series(gauge("other/b/value"))).unwrap();
    assert!(matches!(
        block_on(s.consumer("Bad Name", Start::Now, Select::None)),
        Err(ConsumerError::Route(_))
    ));
    let a = new_consumer(&s, "a", Select::Ids(vec![x]));
    let b = new_consumer(&s, "b", Select::Ids(vec![y]));
    block_on(b.route(Funnel::facts(Select::Ids(vec![x])))).unwrap();
    // a emits x, which b selects; b emits y: a funnel of a selecting y closes the cycle.
    assert!(matches!(
        block_on(a.route(Funnel::facts(Select::Names("site/*/out".into())))),
        Err(ConsumerError::RouteCycle(c)) if c.contains("a") && c.contains("b")
    ));
    // A consumer's own cycle, through two patterns that one name could match.
    let p = new_consumer(&s, "p", Select::Names("loop/*/a".into()));
    assert!(matches!(
        block_on(p.route(Funnel::facts(Select::Names("loop/x/*".into())))),
        Err(ConsumerError::RouteCycle(_))
    ));
    // An alias that would make an id selected by p match what p emits is refused.
    block_on(p.route(Funnel::facts(Select::Ids(vec![v])))).unwrap();
    assert!(matches!(
        block_on(s.alias(v, "loop/x/a")),
        Err(DefineError::RouteCycle(_))
    ));
    block_on(s.alias(v, "other/b/renamed")).unwrap();
    // A consumer emits through its own handle, only on series it declared.
    let ack = block_on(a.try_emit(&[num(x, 0, 1.0), num(y, 0, 2.0)]).unwrap()).unwrap();
    assert!(matches!(ack.outcomes[0], Outcome::Recorded { .. }));
    assert_eq!(ack.outcomes[1], Outcome::Refused(Refusal::NotDeclared));
    assert_eq!(s.series_stats(y).unwrap().refused[&Refusal::NotDeclared], 1);
    // `emits` is recorded with the routing changes: after a restart the cycle is still found.
    block_on(s.shutdown()).unwrap();
    drop((a, b, p, s));
    let s = w.open();
    let a = block_on(s.consumer("a", Start::Now, Select::Ids(vec![x]))).unwrap();
    assert!(matches!(
        block_on(a.route(Funnel::facts(Select::Ids(vec![y])))),
        Err(ConsumerError::RouteCycle(_))
    ));
    // A change of `emits` that would close it is refused too.
    drop(a);
    assert!(matches!(
        block_on(s.consumer("b", Start::Now, Select::All)),
        Err(ConsumerError::RouteCycle(_))
    ));
    block_on(s.shutdown()).unwrap();
}

fn condition(g: kvik::SeriesId) -> Funnel {
    Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }
}

/// Facts at `t` = 0, 1, 2, 3 seconds, a second apart, with their positions.
fn four_facts(w: &World, s: &Store, g: kvik::SeriesId, v: f64) -> Vec<Position> {
    (0..4)
        .map(|i| {
            let p = block_on(s.emit(&[num(g, i, v)])).unwrap().last().unwrap();
            w.clock.advance(Duration::from_secs(1));
            p
        })
        .collect()
}

fn stamp0(position: Position) -> SavedPoint {
    SavedPoint {
        position,
        stamp: ResetStamp::default(),
    }
}

/// Q-1450: a lost consumer taken again before it confirms its `NoCursor` resync is sent it again, with the same
/// `forget_above`, and nothing above that until it is confirmed.
#[test]
fn a_lost_consumer_taken_again_before_confirming_gets_no_cursor_again() {
    let w = World::new("lost-retake");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarm", Select::None);
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let ps = four_facts(&w, &s, g, 20.0);
    drain(&mut c);
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    let path = w.path_with(Path::new("cursors").join("alarm.cur")).unwrap();
    w.fs.poke(&path, 0, &[0; 16]);
    let s = w.open();
    let lost = |c: &mut Consumer| {
        let ds = ready(c);
        let r = resync(&ds[0]).unwrap();
        assert_eq!(
            (r.cause, r.forget_above),
            (ResyncCause::NoCursor, Some(ps[0]))
        );
        assert_eq!(
            ds.len(),
            1,
            "nothing above forget_above before it is confirmed"
        );
        ds[0].id
    };
    let mut c =
        block_on(s.consumer("alarm", Start::Position(stamp0(ps[0])), Select::None)).unwrap();
    lost(&mut c);
    drop(c);
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    let d = lost(&mut c);
    block_on(c.confirm(d)).unwrap();
    let facts = drain(&mut c)
        .iter()
        .filter(|d| matches!(&d.kind, DeliveryKind::Fact(_)))
        .count();
    assert_eq!(
        facts, 3,
        "the facts above forget_above follow the confirmation"
    );
    drop(c);
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    assert!(
        ready(&mut c)
            .iter()
            .all(|d| resync(d).is_none_or(|r| r.cause != ResyncCause::NoCursor))
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-91 (c), row 16: routing the same funnel at every start adds no funnel and keeps its latch; a condition that
/// became true while the consumer was down is delivered once, as a transition, and not again after the next start.
#[test]
fn routing_again_at_every_start_keeps_the_funnel_and_delivers_a_change_once() {
    let w = World::new("route-again");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarm", Select::None);
    let id = block_on(c.route(condition(g))).unwrap();
    block_on(s.emit(&[num(g, 0, 5.0)])).unwrap();
    w.clock.advance(Duration::from_secs(1));
    drain(&mut c);
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    let trues = |ds: &[Delivery]| {
        ds.iter()
            .map(|d| match &d.kind {
                DeliveryKind::Trigger(t) => {
                    usize::from(t.funnel == id && t.state == ConditionState::True)
                }
                DeliveryKind::Resync(r) => r
                    .funnels
                    .iter()
                    .filter(|f| {
                        f.funnel == id && f.transition && f.condition == Some(ConditionState::True)
                    })
                    .count(),
                _ => 0,
            })
            .sum::<usize>()
    };
    let s = w.open();
    // The condition becomes true while the consumer is down.
    block_on(s.emit(&[num(g, 1, 20.0)])).unwrap();
    w.clock.advance(Duration::from_secs(1));
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    assert_eq!(block_on(c.route(condition(g))).unwrap(), id);
    assert_eq!(c.routes().unwrap().len(), 1);
    let ds = drain(&mut c);
    assert_eq!(trues(&ds), 1, "{ds:?}");
    block_on(s.emit(&[num(g, 2, 25.0)])).unwrap();
    assert_eq!(trues(&drain(&mut c)), 0);
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    let s = w.open();
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    assert_eq!(block_on(c.route(condition(g))).unwrap(), id);
    let ds = drain(&mut c);
    assert_eq!(trues(&ds), 0, "{ds:?}");
    let r = resync(ds.last().unwrap()).unwrap();
    let f = r.funnels.iter().find(|f| f.funnel == id).unwrap();
    assert_eq!(
        (f.condition.clone(), f.transition),
        (Some(ConditionState::True), false)
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-132, invariant 26: with the mark synced only on request, the cursor file written at once when a latch changes,
/// and the ones written by the timer, never name a position above the last synced mark: the mark is synced first.
#[test]
fn a_cursor_file_never_names_a_position_above_the_synced_mark() {
    use kvik_log::fs::FaultOp;
    let mut w = World::new("cursor-mark");
    w.mark_sync = MarkSync::Interval(Duration::from_secs(3_600));
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarm", Select::None);
    let id = block_on(c.route(condition(g))).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let ahead = Arc::new(Mutex::new(Vec::new()));
    let checked = Arc::new(Mutex::new(0usize));
    let (fs, dir, seen, count) = (w.fs.clone(), w.dir.clone(), ahead.clone(), checked.clone());
    w.fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
        if !p.to_string_lossy().ends_with(".cur") {
            return;
        }
        *count.lock().unwrap() += 1;
        let bytes = fs.read(p).unwrap();
        let id = kvik_log::header::FileHeader::decode(&bytes)
            .unwrap()
            .store_id;
        let state = kvik_log::cursor::decode(&bytes, &id).unwrap().state;
        let mark = fs
            .read_durable(&Path::new(&dir).join("log").join("MARK"))
            .unwrap();
        let (_, m) = kvik_log::mark::read_mark(&mark, &id).unwrap().unwrap();
        let high = state.highest();
        if (high.generation, high.sequence) > (m.generation, m.last_sequence) {
            seen.lock().unwrap().push(format!(
                "{high:?} above {}:{}",
                m.generation, m.last_sequence
            ));
        }
    })));
    let syncs = || {
        w.fs.ops()
            .iter()
            .filter(|o| o.op == FaultOp::Sync && o.path.to_string_lossy().ends_with(".cur"))
            .count()
    };
    let before = syncs();
    for (t, v) in [(0, 5.0), (1, 20.0)] {
        block_on(s.emit(&[num(g, t, v)])).unwrap();
        w.clock.advance(Duration::from_secs(1));
    }
    let ds = drain(&mut c);
    assert!(
        ds.iter()
            .any(|d| matches!(&d.kind, DeliveryKind::Trigger(t) if t.funnel == id)),
        "{ds:?}"
    );
    assert!(
        syncs() > before,
        "a latch change writes the cursor file at once"
    );
    // A confirmed fact alone is written by the timer, a second later.
    block_on(s.emit(&[num(g, 2, 21.0)])).unwrap();
    drain(&mut c);
    let mid = syncs();
    w.clock.advance(Duration::from_secs(2));
    s.tick().unwrap();
    assert!(syncs() > mid);
    w.fs.set_sync_hook(None);
    assert!(
        *checked.lock().unwrap() >= 2,
        "every cursor file sync is checked"
    );
    assert!(
        ahead.lock().unwrap().is_empty(),
        "{:?}",
        ahead.lock().unwrap()
    );
    // The process stops: the latch confirmed is in the cursor file, so the resume reports no transition.
    w.crash(s, vec![c], false);
    let s = w.open();
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    let ds = ready(&mut c);
    let r = resync(ds.last().unwrap()).unwrap();
    let f = r.funnels.iter().find(|f| f.funnel == id).unwrap();
    assert_eq!(
        (f.condition.clone(), f.transition),
        (Some(ConditionState::True), false)
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-132: a latch change confirmed less than a second after the last cursor file write is written at once, so a
/// crash straight after it repeats no transition.
#[test]
fn a_latch_change_within_a_second_of_the_last_cursor_write_survives_a_crash() {
    let mut w = World::new("latch-at-once");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "alarm", Select::None);
    let id = block_on(c.route(condition(g))).unwrap();
    let triggers = |ds: &[Delivery]| {
        ds.iter()
            .filter(|d| matches!(&d.kind, DeliveryKind::Trigger(t) if t.funnel == id))
            .count()
    };
    w.clock.advance(Duration::from_secs(10));
    block_on(s.emit(&[num(g, 0, 20.0)])).unwrap();
    assert_eq!(triggers(&drain(&mut c)), 1);
    w.clock.advance(Duration::from_millis(500));
    block_on(s.emit(&[num(g, 1, 5.0)])).unwrap();
    assert_eq!(triggers(&drain(&mut c)), 1);
    w.crash(s, vec![c], false);
    let s = w.open();
    let mut c = block_on(s.consumer("alarm", Start::Now, Select::None)).unwrap();
    let ds = ready(&mut c);
    assert_eq!(triggers(&ds), 0, "{ds:?}");
    let r = resync(ds.last().unwrap()).unwrap();
    let f = r.funnels.iter().find(|f| f.funnel == id).unwrap();
    assert_eq!(
        (f.condition.clone(), f.transition),
        (Some(ConditionState::False), false)
    );
    block_on(s.shutdown()).unwrap();
}

/// Spec §4 Overflow: a condition delivery that meets a full queue is not dropped; the consumer catches up and gets it.
#[test]
fn a_condition_delivery_meeting_a_full_queue_arrives_through_catch_up() {
    let w = World::new("trigger-overflow");
    let mut cfg = w.config();
    cfg.limits.consumer_queue = 4;
    let s = block_on(Store::open(cfg)).unwrap();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let h = block_on(s.define_series(gauge("plant/b/power"))).unwrap();
    let mut c = new_consumer(&s, "alarms", Select::None);
    block_on(c.route(Funnel::facts(Select::Ids(vec![h])))).unwrap();
    let cond = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }))
    .unwrap();
    for i in 0..4 {
        block_on(s.emit(&[num(h, i, 1.0)])).unwrap();
        w.clock.advance(Duration::from_secs(1));
    }
    block_on(s.emit(&[num(g, 4, 20.0)])).unwrap();
    let ds = drain(&mut c);
    let triggers: Vec<_> = ds
        .iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Trigger(t) => Some((t.funnel, t.state.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(triggers, vec![(cond, ConditionState::True)]);
    block_on(s.shutdown()).unwrap();
}

/// Spec §4 Resync: a `NoCursor` resync the consumer never confirmed is sent again, with the same `forget_above` and
/// nothing before it, when the consumer is taken again in the same run.
#[test]
fn an_unconfirmed_no_cursor_resync_is_sent_again_on_a_second_take() {
    let w = World::new("retake-no-cursor");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    for i in 0..4 {
        block_on(s.emit(&[num(g, i, 1.0)])).unwrap();
        w.clock.advance(Duration::from_secs(1));
    }
    drain(&mut c);
    block_on(s.shutdown()).unwrap();
    drop((c, s));
    let path = w.path_with(Path::new("cursors").join("log.cur")).unwrap();
    w.fs.poke(&path, 0, &[0; 16]);

    let s = w.open();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let ds = ready(&mut c);
    let first = resync(&ds[0]).unwrap();
    assert_eq!(first.cause, ResyncCause::NoCursor);
    let forget = first.forget_above;
    assert!(forget.is_some());
    drop(c);
    block_on(s.emit(&[num(g, 10, 2.0)])).unwrap();
    let mut c = block_on(s.consumer("log", Start::Now, Select::None)).unwrap();
    let ds = ready(&mut c);
    let again = resync(&ds[0]).expect("the resync comes first");
    assert_eq!(
        (again.cause, again.forget_above),
        (ResyncCause::NoCursor, forget)
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-130: the registry file holds a routing change when the routing call returns, so a consumer whose routing
/// entries are no longer in the log is still known at a start; with its cursor file lost it is named in the
/// recovery entry and sent the `NoCursor` resync.
#[test]
fn the_registry_file_holds_each_routing_change_when_the_call_returns() {
    use kvik_log::entries::{Entry, RouteOp};
    let mut w = World::new("registry");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let c = new_consumer(&s, "watch", Select::None);
    let f = block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    let cur = w.path_with(Path::new("cursors").join("watch.cur")).unwrap();
    let id = kvik_log::header::FileHeader::decode(&w.fs.read(&cur).unwrap())
        .unwrap()
        .store_id;
    let (_, file) = kvik_log::registry::read_latest(&*w.fs, Path::new(&w.dir), &id)
        .unwrap()
        .unwrap();
    let at = file.entries.iter().find_map(|(p, _, e)| match e {
        Entry::Routing(r)
            if r.consumer == "watch" && r.op == RouteOp::Add && r.funnel == Some(f.0) =>
        {
            Some(*p)
        }
        _ => None,
    });
    assert!(at.is_some_and(|p| p <= file.through), "{:?}", file.entries);
    w.crash(s, vec![c], true);
    w.fs.poke(&cur, 0, &[0; 16]);
    let s = w.open();
    assert!(
        s.losses().iter().any(|l| l.cursors_missing == ["watch"]),
        "{:?}",
        s.losses()
    );
    let mut c = block_on(s.consumer("watch", Start::Now, Select::None)).unwrap();
    let d = c.try_next().unwrap().unwrap();
    assert_eq!(resync(&d).unwrap().cause, ResyncCause::NoCursor);
    assert_eq!(c.routes().unwrap().len(), 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn path_with_matches_whole_trailing_components_with_the_platform_separator() {
    let w = World::new("paths");
    let decoy = Path::new(&w.dir).join("a-cursors").join("x.cur");
    let sep = MAIN_SEPARATOR;
    let real = PathBuf::from(format!("{}{sep}cursors{sep}x.cur", w.dir));
    for p in [&decoy, &real] {
        drop(w.fs.open(p, OpenMode::CreateNew).unwrap());
    }
    assert_eq!(w.path_with(Path::new("cursors").join("x.cur")), Some(real));
    assert_eq!(w.path_with(Path::new("cursors").join("y.cur")), None);
}

#[test]
fn a_condition_routed_between_polls_is_evaluated_at_the_next() {
    let w = World::new("route-between-polls");
    let s = w.open();
    let g = block_on(s.define_series(gauge("plant/a/power"))).unwrap();
    let mut c = new_consumer(&s, "late-route", Select::None);
    block_on(c.route(Funnel::facts(Select::Ids(vec![g])))).unwrap();
    block_on(s.emit(&[num(g, 0, 5.0)])).unwrap();
    assert_eq!(drain(&mut c).len(), 1);
    let cond = block_on(c.route(Funnel {
        series: Select::Ids(vec![g]),
        operator: None,
        condition: Some(Condition::above(10.0)),
    }))
    .unwrap();
    w.clock.advance(Duration::from_secs(1));
    block_on(s.emit(&[num(g, 1, 20.0)])).unwrap();
    let triggers: Vec<_> = drain(&mut c)
        .into_iter()
        .filter_map(|d| match d.kind {
            DeliveryKind::Trigger(t) => Some((t.funnel, t.state)),
            _ => None,
        })
        .collect();
    assert_eq!(triggers, vec![(cond, ConditionState::True)]);
    block_on(s.shutdown()).unwrap();
}
