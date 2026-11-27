//! Pinned cases of the gates core.

use kvik_model::gate::{
    Cause, ConditionId, Gate, GateState, Input, Kind, Level, Selector, TimerKind, Window, judge,
};
use kvik_model::summary::Aggregate;
use kvik_model::types::{Nanos, Position, Quality, SECOND, SeriesId, Value};

use super::differential::{Drive, Log, S, gate, run};
use super::{Body, Core, CoreError, FactItem, History, HistoryError, Item, Replay, SeriesState};

const T: SeriesId = SeriesId(2);

fn num(v: f64) -> Value {
    Value::Num(v)
}

fn one_batch_each() -> Drive {
    Drive {
        cuts: vec![true],
        floors: vec![0],
        restores: vec![false],
    }
}

fn hold_up(secs: i64) -> Gate {
    gate(
        Kind::Levels(vec![Level::at(5.0).hold(secs * SECOND, 0)]),
        None,
    )
}

#[test]
fn series_holds_only_watched_series_and_an_unwatch_frees_instance_timer_and_series() {
    let mut log = Log::default();
    let on_t = Gate {
        selector: Selector::Series(T),
        ..hold_up(5)
    };
    log.watch(0, ConditionId(1), hold_up(5));
    log.watch(0, ConditionId(2), on_t);
    log.watch(0, ConditionId(3), hold_up(9));
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(T, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(SeriesId(3), SECOND, SECOND, num(7.0), Quality::Good);
    let mut core = Core::new();
    for (c, i) in &log.items {
        core.apply_batch(*c, std::slice::from_ref(i), &log.replay)
            .unwrap();
    }
    assert_eq!(core.series.keys().copied().collect::<Vec<_>>(), [S, T]);
    assert_eq!(core.instances.len(), 3);
    assert_eq!(core.timers.len(), 3);

    let unwatch = |id: u32, seq: u64| Item {
        position: Position::new(0, seq),
        body: Body::Unwatch {
            id: ConditionId(id),
        },
    };
    core.apply_batch(2 * SECOND, &[unwatch(2, 100)], &log.replay)
        .unwrap();
    assert_eq!(core.series.keys().copied().collect::<Vec<_>>(), [S]);
    assert_eq!(core.readers.keys().copied().collect::<Vec<_>>(), [S]);
    assert_eq!(core.instances.len(), 2);
    assert_eq!(core.timers.len(), 2);
    assert_eq!(core.scheduled.len(), 2);

    core.apply_batch(2 * SECOND, &[unwatch(1, 101)], &log.replay)
        .unwrap();
    assert_eq!(core.series.len(), 1, "gate 3 still reads S");
    core.apply_batch(2 * SECOND, &[unwatch(3, 102)], &log.replay)
        .unwrap();
    assert!(core.series.is_empty() && core.readers.is_empty());
    assert!(core.instances.is_empty() && core.timers.is_empty() && core.scheduled.is_empty());
}

#[test]
fn a_timer_takes_the_last_position_of_the_whole_log_before_its_due_instant() {
    let mut log = Log::default();
    log.watch(0, ConditionId(1), hold_up(5));
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(T, 3 * SECOND, 3 * SECOND, num(1.0), Quality::Good);
    log.other(4 * SECOND);
    log.fact(T, 8 * SECOND, 8 * SECOND, num(1.0), Quality::Good);
    let until = 20 * SECOND;
    let (got, _) = run(&log, &one_batch_each(), until);
    let hold = got
        .iter()
        .find(|t| {
            matches!(
                t.cause,
                Cause::Timer {
                    kind: TimerKind::Hold,
                    ..
                }
            )
        })
        .expect("the hold completes at 6 s");
    assert_eq!(hold.id.due, Some(6 * SECOND));
    assert_eq!(
        hold.id.at,
        Position::new(0, 4),
        "the definition at 4 s, not S's fact"
    );
    assert_eq!(got, judge(&log.entries, until).unwrap().transitions);
}

#[test]
fn a_timer_due_at_a_batch_commit_time_fires_before_that_batch() {
    let mut log = Log::default();
    log.watch(0, ConditionId(1), hold_up(5));
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(S, 6 * SECOND, 6 * SECOND, num(1.0), Quality::Good);
    let (got, _) = run(&log, &one_batch_each(), 20 * SECOND);
    let states: Vec<_> = got.iter().map(|t| t.to.clone()).collect();
    assert_eq!(
        states,
        [
            Some(GateState::Unknown),
            Some(GateState::Level(0)),
            Some(GateState::Level(1)),
            Some(GateState::Level(0)),
        ]
    );
    assert_eq!(got[2].id.due, Some(6 * SECOND));
    assert_eq!(got, judge(&log.entries, 20 * SECOND).unwrap().transitions);
}

#[test]
fn a_watch_mid_event_seeds_its_hold_from_the_look_back() {
    let mut log = Log::default();
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(S, 4 * SECOND, 4 * SECOND, num(7.0), Quality::Good);
    log.watch(6 * SECOND, ConditionId(1), hold_up(5));
    let (got, core) = run(&log, &one_batch_each(), 20 * SECOND);
    assert_eq!(
        got[0].to,
        Some(GateState::Level(1)),
        "held since 1 s, crossed at 6 s"
    );
    assert_eq!(got[0].from, None);
    assert_eq!(core.states()[&(ConditionId(1), S)].0, GateState::Level(1));
    assert_eq!(got, judge(&log.entries, 20 * SECOND).unwrap().transitions);
}

#[test]
fn the_seed_starts_from_the_live_fact_in_force_at_the_look_back_start() {
    let mut log = Log::default();
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.watch(30 * SECOND, ConditionId(1), hold_up(5));
    let (got, _) = run(&log, &one_batch_each(), 60 * SECOND);
    assert_eq!(got[0].to, Some(GateState::Level(1)));
    assert_eq!(got, judge(&log.entries, 60 * SECOND).unwrap().transitions);
}

#[test]
fn a_backfill_inside_the_look_back_that_was_never_live_seeds_nothing() {
    let level = gate(
        Kind::Levels(vec![Level::at(10.0).hold(5 * SECOND, 5 * SECOND)]),
        None,
    );
    let mut log = Log::default();
    log.fact(S, 50 * SECOND, 50 * SECOND, num(5.0), Quality::Good);
    log.fact(S, 92 * SECOND, 40 * SECOND, num(50.0), Quality::Good);
    log.watch(100 * SECOND, ConditionId(1), level);
    let (got, _) = run(&log, &one_batch_each(), 120 * SECOND);
    let moves: Vec<_> = got.iter().map(|t| (t.from.clone(), t.to.clone())).collect();
    assert_eq!(moves, [(None, Some(GateState::Level(0)))]);
    assert_eq!(got, judge(&log.entries, 120 * SECOND).unwrap().transitions);
}

#[test]
fn a_bad_fact_replacing_the_live_one_falls_back_to_the_older_value() {
    let mut log = Log::default();
    log.watch(
        0,
        ConditionId(1),
        gate(Kind::Change { deadband: 0.0 }, None),
    );
    log.fact(S, SECOND, SECOND, num(1.0), Quality::Good);
    log.fact(S, 2 * SECOND, 2 * SECOND, num(2.0), Quality::Good);
    log.fact(S, 3 * SECOND, 2 * SECOND, num(9.0), Quality::Bad);
    log.fact(S, 4 * SECOND, 4 * SECOND, num(1.0), Quality::Good);
    let (got, _) = run(&log, &one_batch_each(), 10 * SECOND);
    let values: Vec<_> = got.iter().map(|t| t.to.clone()).collect();
    assert_eq!(
        values,
        [
            Some(GateState::Unknown),
            Some(GateState::Value(num(1.0))),
            Some(GateState::Value(num(2.0))),
            Some(GateState::Value(num(1.0))),
        ]
    );
    assert_eq!(
        got[3].id.at,
        Position::new(0, 4),
        "the bad fact's own position"
    );
    assert_eq!(got, judge(&log.entries, 10 * SECOND).unwrap().transitions);
}

#[test]
fn an_outranked_fact_is_no_arrival() {
    let mut log = Log::default();
    log.watch(
        0,
        ConditionId(1),
        gate(Kind::Change { deadband: 0.0 }, Some(5 * SECOND)),
    );
    let fact = |v, changed_at| super::FactItem {
        series: S,
        t: SECOND,
        value: num(v),
        quality: Quality::Good,
        changed_at: Some(changed_at),
    };
    let mut replay = Replay::default();
    let mut core = Core::new();
    let mut out = Vec::new();
    let (c0, w) = log.items[0].clone();
    out.extend(core.apply_batch(c0, &[w], &replay).unwrap());
    for (seq, commit, v, changed_at) in [(2, SECOND, 1.0, 9), (3, 4 * SECOND, 2.0, 3)] {
        let position = Position::new(0, seq);
        let outranked = replay.push(commit, position, fact(v, changed_at));
        assert_eq!(outranked, seq == 3);
        let body = Body::Fact {
            fact: fact(v, changed_at),
            outranked,
        };
        out.extend(
            core.apply_batch(commit, &[Item { position, body }], &replay)
                .unwrap(),
        );
    }
    out.extend(core.advance(20 * SECOND));
    let stale = out.last().unwrap();
    assert_eq!(stale.to, Some(GateState::Stale));
    assert_eq!(
        stale.id.due,
        Some(6 * SECOND),
        "stale from the arrival at 1 s"
    );
}

#[test]
fn a_snapshot_keeps_a_pending_hold() {
    let mut log = Log::default();
    log.watch(0, ConditionId(1), hold_up(5));
    log.fact(S, SECOND, SECOND, num(7.0), Quality::Good);
    log.fact(S, 3 * SECOND, 3 * SECOND, num(7.0), Quality::Good);
    let restore_each = Drive {
        restores: vec![true],
        ..one_batch_each()
    };
    let (got, _) = run(&log, &restore_each, 20 * SECOND);
    assert_eq!(got.last().unwrap().id.due, Some(6 * SECOND));
    assert_eq!(got, judge(&log.entries, 20 * SECOND).unwrap().transitions);
}

#[test]
fn window_gates_are_refused_as_not_supported_yet() {
    let window = Gate {
        input: Input::Window(Window {
            width: 10 * SECOND,
            aggregate: Aggregate::Max,
            min_coverage: 500,
            cadence: SECOND,
        }),
        ..hold_up(0)
    };
    let item = Item {
        position: Position::new(0, 1),
        body: Body::Watch {
            id: ConditionId(1),
            gate: window,
        },
    };
    let err = Core::new().apply_batch(0, &[item], &Replay::default());
    assert_eq!(err, Err(CoreError::NotSupported(ConditionId(1))));
}

#[test]
fn a_second_watch_of_an_id_an_unwatch_of_an_unknown_id_and_a_refused_gate_are_errors() {
    let r = Replay::default();
    let item = |seq, body| Item {
        position: Position::new(0, seq),
        body,
    };
    let watch = |seq, id, gate| item(seq, Body::Watch { id, gate });
    let mut core = Core::new();
    core.apply_batch(0, &[watch(1, ConditionId(1), hold_up(5))], &r)
        .expect("first watch");
    assert_eq!(
        core.apply_batch(0, &[watch(2, ConditionId(1), hold_up(3))], &r),
        Err(CoreError::Id(ConditionId(1)))
    );
    let unwatch = item(3, Body::Unwatch { id: ConditionId(2) });
    assert_eq!(
        core.apply_batch(0, &[unwatch], &r),
        Err(CoreError::Id(ConditionId(2)))
    );
    let refused = gate(Kind::Levels(Vec::new()), None);
    assert_eq!(
        core.apply_batch(0, &[watch(4, ConditionId(3), refused)], &r),
        Err(CoreError::Invalid(
            ConditionId(3),
            kvik_model::gate::GateError::Levels
        ))
    );
    assert_eq!(core.states().len(), 1);
}

/// A [`Replay`] whose read named `fail` fails.
struct Failing<'a> {
    replay: &'a Replay,
    fail: &'static str,
}

impl Failing<'_> {
    fn check(&self, read: &'static str) -> Result<(), HistoryError> {
        if self.fail == read {
            Err(HistoryError(read.into()))
        } else {
            Ok(())
        }
    }
}

impl History for Failing<'_> {
    fn series(
        &self,
        s: SeriesId,
        now: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.check("series")?;
        self.replay.series(s, now, through)
    }

    fn at_look_back(
        &self,
        s: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<SeriesState, HistoryError> {
        self.check("at_look_back")?;
        self.replay.at_look_back(s, from, through)
    }

    fn facts_since(
        &self,
        s: SeriesId,
        from: Nanos,
        through: Position,
    ) -> Result<Vec<(Nanos, Position, FactItem, bool)>, HistoryError> {
        self.check("facts_since")?;
        self.replay.facts_since(s, from, through)
    }

    fn matching(&self, pattern: &str, through: Position) -> Result<Vec<SeriesId>, HistoryError> {
        self.check("matching")?;
        self.replay.matching(pattern, through)
    }
}

#[test]
fn a_failed_history_read_fails_the_call_and_a_watch_adds_nothing() {
    let mut log = Log::default();
    log.fact(S, SECOND, SECOND, num(1.0), Quality::Good);
    log.watch(10 * SECOND, ConditionId(1), hold_up(5));
    log.fact(S, 11 * SECOND, SECOND, num(9.0), Quality::Bad);
    let batch = |k: usize| (log.items[k].0, [log.items[k].1.clone()]);
    let failing = |fail| Failing {
        replay: &log.replay,
        fail,
    };
    let mut core = Core::new();
    let (c, items) = batch(0);
    core.apply_batch(c, &items, &log.replay).expect("fact");
    let (c, items) = batch(1);
    for read in ["series", "at_look_back", "facts_since"] {
        let mut attempt = core.clone();
        assert_eq!(
            attempt.apply_batch(c, &items, &failing(read)),
            Err(CoreError::History(HistoryError(read.into()))),
            "{read}"
        );
        let added = (
            &attempt.defs,
            &attempt.series,
            &attempt.readers,
            &attempt.instances,
        );
        assert_eq!(
            added,
            (&core.defs, &core.series, &core.readers, &core.instances),
            "{read}"
        );
    }
    core.apply_batch(c, &items, &log.replay).expect("watch");
    let (c, items) = batch(2);
    assert_eq!(
        core.clone().apply_batch(c, &items, &failing("series")),
        Err(CoreError::History(HistoryError("series".into()))),
        "the fallback read"
    );
    let defs = [(ConditionId(1), hold_up(5))].into();
    assert_eq!(
        Core::restore(core.snapshot(), defs, &failing("series")),
        Err(CoreError::History(HistoryError("series".into())))
    );
}
