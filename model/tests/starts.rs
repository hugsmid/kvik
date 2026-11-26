//! Consumer starts and resync causes (§4 New consumer, Resync; Q-131, Q-1401, Q-1403).

mod common;

use common::T0;
use kvik_model::condition::{Condition, ConditionState};
use kvik_model::engine::{
    Config, ConsumerDecl, ConsumerId, Envelope, Error, Funnel, FunnelId, Model, ResyncCause,
    Selection, Start,
};
use kvik_model::recovery::{ResetStamp, SavedPoint};
use kvik_model::registry::SeriesOptions;
use kvik_model::types::{HOUR, Kind, Position, Reading, SECOND, SeriesId, ValueType};

const C: ConsumerId = ConsumerId(1);

fn model(selection: Selection) -> (Model, SeriesId) {
    let consumer = ConsumerDecl {
        id: C,
        selection,
        capacity: 100,
    };
    let mut m = Model::new(Config::default(), vec![consumer], T0 + HOUR);
    let s = m
        .define_with(
            "site/pump-2/flow",
            Kind::Gauge,
            ValueType::Number,
            "l/min",
            10 * SECOND,
            SeriesOptions::expected(SECOND),
        )
        .unwrap();
    (m, s)
}

/// Facts a second apart, each received a second after the one before.
fn facts(m: &mut Model, s: SeriesId, from: i64, v: f64) -> Vec<Position> {
    (from..from + 4)
        .map(|i| {
            let p = m
                .ingest(vec![Reading::num(s, T0 + i * SECOND, v)])
                .unwrap()
                .positions[0];
            m.advance(SECOND);
            p
        })
        .collect()
}

fn take(m: &mut Model) -> Vec<Envelope> {
    m.catch_up(C, usize::MAX).unwrap();
    m.receive(C, usize::MAX).unwrap()
}

fn zero(position: Position) -> SavedPoint {
    SavedPoint {
        position,
        stamp: ResetStamp::ZERO,
    }
}

#[test]
fn each_start_kind_begins_after_its_position_with_no_resync() {
    let (mut m, s) = model(Selection::All);
    let at = m.wall;
    let ps = facts(&mut m, s, 0, 1.0);
    let head = *ps.last().unwrap();
    assert_eq!(m.start_consumer(C, Start::Now).unwrap(), head);
    assert!(
        take(&mut m).is_empty(),
        "now: nothing to forget, nothing behind"
    );
    assert_eq!(m.disk.cursors[&C], head);
    // Q-1403: the second fact was received at `at + 1 s`.
    assert_eq!(
        m.start_consumer(C, Start::Time(at + SECOND)).unwrap(),
        ps[0]
    );
    assert_eq!(m.disk.cursors[&C], ps[0]);
    let got = take(&mut m);
    assert!(got.iter().all(|d| d.resync().is_none() && d.replayed));
    let replayed: Vec<Position> = got.iter().map(|d| d.position).collect();
    assert_eq!(replayed, ps[1..]);
    assert_eq!(
        m.start_consumer(C, Start::Point(zero(ps[2]))).unwrap(),
        ps[2]
    );
    let got: Vec<Position> = take(&mut m).iter().map(|d| d.position).collect();
    assert_eq!(got, ps[3..]);
    let above = Position::new(head.generation, head.sequence + 5);
    assert_eq!(
        m.start_consumer(C, Start::Point(zero(above))).unwrap(),
        head,
        "Q-1401"
    );
    assert!(take(&mut m).is_empty());
}

#[test]
fn a_time_before_every_entry_starts_at_the_origin() {
    let (mut m, s) = model(Selection::All);
    let at = m.wall;
    let ps = facts(&mut m, s, 0, 1.0);
    assert_eq!(
        m.start_consumer(C, Start::Time(at - HOUR)).unwrap(),
        Position::ORIGIN
    );
    let got: Vec<Position> = take(&mut m)
        .iter()
        .filter(|d| d.fact().is_some())
        .map(|d| d.position)
        .collect();
    assert_eq!(got, ps);
}

#[test]
fn a_point_void_above_a_power_loss_starts_at_the_kept_position_then_meets_the_reset_point() {
    let (mut m, s) = model(Selection::All);
    let ps = facts(&mut m, s, 0, 1.0);
    m.power_loss();
    m.restart();
    let rp = m.reset_points()[0];
    let beyond = Position::new(rp.id.generation, rp.id.sequence + 3);
    assert_eq!(
        m.start_consumer(C, Start::Point(zero(beyond))).unwrap(),
        rp.forget_above
    );
    let got = take(&mut m);
    let r = got[0].resync().expect("the reset point first");
    assert_eq!(
        (r.cause, r.forget_above),
        (ResyncCause::ResetPoint, Some(rp.forget_above))
    );
    assert_eq!(
        m.start_consumer(C, Start::Point(zero(ps[1]))).unwrap(),
        ps[1]
    );
}

#[test]
fn an_unknown_point_refuses_the_start() {
    let (mut m, s) = model(Selection::All);
    facts(&mut m, s, 0, 1.0);
    m.power_loss();
    m.restart();
    // A mark whose stamp no reset carries (§7 Readers): the point cannot be judged.
    m.disk.mark.as_mut().unwrap().stamp = ResetStamp {
        sequence: 1,
        nonce: u64::MAX,
    };
    assert!(matches!(
        m.start_consumer(C, Start::Point(zero(Position::new(0, 2)))),
        Err(Error::UnknownPoint)
    ));
}

fn above(on: f64) -> Condition {
    Condition::Threshold { on, clear: on }
}

#[test]
fn a_lost_cursor_gives_a_no_cursor_resync_instead_of_a_replay_from_the_origin() {
    let (mut m, s) = model(Selection::Series(Default::default()));
    m.add_route(C, FunnelId(1), Funnel::condition(s, None, above(10.0)))
        .unwrap();
    m.add_route(C, FunnelId(2), Funnel::facts(Selection::All))
        .unwrap();
    facts(&mut m, s, 0, 20.0);
    m.drain().unwrap();
    m.crash();
    m.lose_cursor_file(C).unwrap();
    m.restart();
    assert!(m.cursor_lost(C));
    facts(&mut m, s, 4, 20.0);
    assert!(take(&mut m).is_empty(), "nothing until it is started");
    let head = m.start_consumer(C, Start::Now).unwrap();
    assert!(!m.cursor_lost(C));
    let got = take(&mut m);
    assert_eq!(got.len(), 1, "nothing before the resync is confirmed");
    let r = got[0].resync().unwrap();
    assert_eq!(
        (r.cause, r.forget_above),
        (ResyncCause::NoCursor, Some(head))
    );
    let cond = r.funnels.iter().find_map(|f| f.condition.as_ref()).unwrap();
    assert_eq!(cond.state, ConditionState::Active(true));
    assert!(cond.transition, "no latch to compare");
    let live = facts(&mut m, s, 8, 20.0);
    assert!(take(&mut m).is_empty(), "held until confirmed");
    m.confirm(C, got[0].position).unwrap();
    let after: Vec<Position> = take(&mut m)
        .iter()
        .filter(|d| d.fact().is_some())
        .map(|d| d.position)
        .collect();
    assert_eq!(after, live);
}

#[test]
fn a_lost_cursor_no_routing_change_names_starts_as_a_new_consumer() {
    let (mut m, s) = model(Selection::Series(Default::default()));
    facts(&mut m, s, 0, 1.0);
    m.drain().unwrap();
    m.crash();
    m.lose_cursor_file(C).unwrap();
    m.restart();
    assert!(m.cursor_lost(C));
    m.reconnect(C).unwrap();
    assert!(m.cursor_lost(C), "a reconnect does not start it");
    m.start_consumer(C, Start::Now).unwrap();
    assert!(take(&mut m).is_empty());
}

#[test]
fn a_resume_resync_says_resume() {
    let (mut m, s) = model(Selection::All);
    facts(&mut m, s, 0, 1.0);
    m.drain().unwrap();
    m.reconnect(C).unwrap();
    let got = take(&mut m);
    assert_eq!(got[0].resync().unwrap().cause, ResyncCause::Resume);
}

#[test]
fn a_lost_cursor_of_a_declared_selection_gives_a_no_cursor_resync() {
    let (mut m, s) = model(Selection::All);
    facts(&mut m, s, 0, 1.0);
    m.drain().unwrap();
    m.crash();
    m.lose_cursor_file(C).unwrap();
    m.restart();
    let p = m
        .start_consumer(C, Start::Point(zero(Position::ORIGIN)))
        .unwrap();
    let got = take(&mut m);
    let r = got[0].resync().unwrap();
    assert_eq!((r.cause, r.forget_above), (ResyncCause::NoCursor, Some(p)));
}

#[test]
fn a_routed_consumer_started_again_is_sent_no_resync() {
    let (mut m, s) = model(Selection::Series(Default::default()));
    m.add_route(C, FunnelId(1), Funnel::facts(Selection::All))
        .unwrap();
    facts(&mut m, s, 0, 1.0);
    m.drain().unwrap();
    m.start_consumer(C, Start::Now).unwrap();
    assert!(
        take(&mut m).is_empty(),
        "only a lost cursor is sent `NoCursor`"
    );
}

#[test]
fn a_lost_consumer_taken_again_before_confirming_is_sent_no_cursor_again() {
    let (mut m, s) = model(Selection::All);
    facts(&mut m, s, 0, 1.0);
    m.drain().unwrap();
    m.crash();
    m.lose_cursor_file(C).unwrap();
    m.restart();
    m.start_consumer(C, Start::Now).unwrap();
    assert_eq!(
        take(&mut m)[0].resync().unwrap().cause,
        ResyncCause::NoCursor
    );
    facts(&mut m, s, 10, 1.0);
    m.reconnect(C).unwrap();
    let got = take(&mut m);
    let r = got[0].resync().expect("the resync is sent again");
    assert_eq!(r.cause, ResyncCause::NoCursor);
    assert_eq!(
        got.len(),
        1,
        "nothing above forget_above before the confirmation"
    );
}
