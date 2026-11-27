//! `Each` gates and reset points on the durable backend (spec §6 Gates, One stream and Resume; Q-1143, Q-2170).

mod common;

use std::time::Duration;

use common::block_on;
use common::world::{World, gauge, ready};
use kvik::{
    Consumer, DeliveryKind, Gate, GateState, Input, Level, Reading, Select, SeriesId, Start, Store,
    Value,
};

fn take(s: &Store) -> Consumer {
    block_on(s.consumer("c", Start::Now, Select::None)).unwrap()
}

fn warm() -> Gate {
    Gate::levels(Input::value_each(), [Level::above(20.0)]).each("room/*/temp")
}

fn emit(w: &World, s: &Store, id: SeriesId, v: f64) {
    block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))])).unwrap();
}

/// Transitions as `(series, from, to, replayed)`.
fn moves(ds: &[kvik::Delivery]) -> Vec<(SeriesId, Option<GateState>, Option<GateState>, bool)> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Transition(t) => {
                Some((t.id.series, t.from.clone(), t.to.clone(), d.flags.replayed))
            }
            _ => None,
        })
        .collect()
}

fn confirm_all(c: &mut Consumer, ds: &[kvik::Delivery]) {
    if let Some(d) = ds.last() {
        block_on(c.confirm(d.id)).unwrap();
    }
}

#[test]
fn each_takes_at_most_its_instance_limit_and_counts_the_series_refused() {
    let w = World::quiet("gates-each-limit");
    let s = w.open();
    let ids: Vec<SeriesId> = (0..257)
        .map(|i| block_on(s.define_series(gauge(&format!("room/{i}/temp")))).unwrap())
        .collect();
    let mut c = take(&s);
    block_on(c.watch("warm", warm())).unwrap();
    let got = ready(&mut c);
    let started = moves(&got);
    assert_eq!(started.len(), 256);
    assert!(started.iter().all(|m| m.1.is_none()));
    assert!(started.iter().all(|m| m.0 != ids[256]));
    assert_eq!(s.stats().delivery.instances_refused, 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_series_refused_by_a_full_each_joins_with_a_start_transition_when_room_frees() {
    let w = World::quiet("gates-each-room");
    let s = w.open();
    let ids: Vec<SeriesId> = (0..257)
        .map(|i| block_on(s.define_series(gauge(&format!("room/{i}/temp")))).unwrap())
        .collect();
    let mut c = take(&s);
    block_on(c.watch("warm", warm())).unwrap();
    let got = ready(&mut c);
    confirm_all(&mut c, &got);
    block_on(s.retire(ids[3], w.clock.wall())).unwrap();
    let got = ready(&mut c);
    assert_eq!(
        moves(&got),
        [
            (ids[3], Some(GateState::Unknown), None, false),
            (ids[256], None, Some(GateState::Unknown), false),
        ]
    );
    assert_eq!(s.stats().delivery.instances_refused, 1);
    block_on(s.shutdown()).unwrap();
}

#[test]
fn a_series_joins_an_each_when_a_name_matches_and_leaves_with_a_transition_to_none() {
    let w = World::quiet("gates-each-join");
    let s = w.open();
    let a = block_on(s.define_series(gauge("room/a/temp"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("warm", warm())).unwrap();
    emit(&w, &s, a, 25.0);
    let b = block_on(s.define_series(gauge("hall/b/temp"))).unwrap();
    emit(&w, &s, b, 25.0);
    block_on(s.alias(b, "room/b/temp")).unwrap();
    block_on(s.retire(a, w.clock.wall())).unwrap();
    let got = ready(&mut c);
    assert_eq!(
        moves(&got),
        [
            (a, None, Some(GateState::Unknown), false),
            (
                a,
                Some(GateState::Unknown),
                Some(GateState::Level(1)),
                false
            ),
            (b, None, Some(GateState::Level(1)), false),
            (a, Some(GateState::Level(1)), None, false),
        ]
    );
    block_on(s.shutdown()).unwrap();
}

#[test]
fn each_instances_replay_after_a_restart_from_the_checkpoint() {
    let w = World::quiet("gates-each-restart");
    let s = w.open();
    let a = block_on(s.define_series(gauge("room/a/temp"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("warm", warm())).unwrap();
    let got = ready(&mut c);
    confirm_all(&mut c, &got);
    let b = block_on(s.define_series(gauge("room/b/temp"))).unwrap();
    emit(&w, &s, a, 25.0);
    let first = moves(&ready(&mut c));
    drop(c);
    block_on(s.shutdown()).unwrap();
    let s = w.open();
    let mut c = take(&s);
    let raw = ready(&mut c);
    confirm_all(&mut c, &raw);
    let again = moves(&raw);
    let expect: Vec<_> = first
        .into_iter()
        .map(|(s, f, t, _)| (s, f, t, true))
        .collect();
    assert_eq!(again, expect);
    emit(&w, &s, b, 25.0);
    let got = ready(&mut c);
    assert_eq!(
        moves(&got),
        [(
            b,
            Some(GateState::Unknown),
            Some(GateState::Level(1)),
            false
        )]
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-2170: a power loss's recovery entry after the checkpoint is a reset point met during the replay: the transitions
/// before it are replayed, then every instance is reseeded there (hysteresis forgotten), its state in the resync with
/// `set_by` none.
#[test]
fn a_reset_point_after_the_checkpoint_reseeds_where_it_lies_after_replaying_up_to_it() {
    let mut w = World::quiet("gates-each-reset");
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    let gate = Gate::levels(Input::value(id), [Level::above(70.0).clear(40.0)]);
    block_on(c.watch("humidity", gate)).unwrap();
    emit(&w, &s, id, 30.0);
    let got = ready(&mut c);
    confirm_all(&mut c, &got);
    emit(&w, &s, id, 80.0);
    w.clock.advance(Duration::from_secs(1));
    emit(&w, &s, id, 50.0);
    assert_eq!(
        moves(&ready(&mut c)),
        [(
            id,
            Some(GateState::Level(0)),
            Some(GateState::Level(1)),
            false
        )]
    );
    w.crash(s, vec![c], true);
    let s = w.open();
    let mut c = take(&s);
    let got = ready(&mut c);
    assert_eq!(
        moves(&got),
        [(
            id,
            Some(GateState::Level(0)),
            Some(GateState::Level(1)),
            true
        )]
    );
    let r = got
        .iter()
        .rev()
        .find_map(common::world::resync)
        .expect("a resync");
    assert!(r.forget_above.is_some());
    let states: Vec<_> = r
        .gates
        .iter()
        .map(|g| (g.state.clone(), g.set_by))
        .collect();
    assert_eq!(states, [(GateState::Level(0), None)]);
    assert_eq!(s.stats().delivery.gates_reseeded, 1);
    block_on(s.shutdown()).unwrap();
}
