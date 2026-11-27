//! When a cursor write carries a gate checkpoint, and a gate cursor without one (spec §6 Gates, Resume; `format.md`
//! A.15; Q-2086).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::block_on;
use common::world::{World, gauge, ready};
use kvik::{
    ConditionsConfig, Consumer, DeliveryKind, Gate, Input, Level, MarkSync, Reading, ReplayMax,
    Select, SeriesId, Start, Store, Value,
};
use kvik_log::cursor::{CHECKPOINT_BLOCK, CURSOR_BLOCK, Checkpoint};

fn humidity(series: SeriesId) -> Gate {
    Gate::levels(
        Input::value(series),
        [Level::above(70.0)
            .clear(65.0)
            .hold_up(Duration::from_secs(120))],
    )
}

fn take(s: &Store) -> Consumer {
    block_on(s.consumer("c", Start::Now, Select::None)).unwrap()
}

fn emit(w: &World, s: &Store, id: SeriesId, v: f64) {
    block_on(s.emit(&[Reading::new(id, w.clock.wall(), Value::F64(v))])).unwrap();
}

/// Transitions as `(gate state after, replayed)` and resyncs as `(cause, [(state, set_by present)])`, in order.
fn seen(ds: &[kvik::Delivery]) -> Vec<String> {
    ds.iter()
        .filter_map(|d| match &d.kind {
            DeliveryKind::Transition(t) => Some(format!("{:?} r={}", t.to, d.flags.replayed)),
            DeliveryKind::Resync(r) => Some(format!(
                "resync {:?} {:?}",
                r.cause,
                r.gates
                    .iter()
                    .map(|g| (&g.state, g.set_by.is_some()))
                    .collect::<Vec<_>>()
            )),
            _ => None,
        })
        .collect()
}

fn cursor_path(w: &World) -> PathBuf {
    w.path_with(Path::new("cursors").join("c.cur")).unwrap()
}

/// The cursor file's whole blocks as `(type, offset, length)`, in order.
fn blocks(bytes: &[u8]) -> Vec<(u8, usize, usize)> {
    use kvik_log::block::{BLOCK_HEADER_LEN, BlockHeader};
    let mut at = kvik_log::header::HEADER_LEN;
    let mut out = Vec::new();
    while let Some(h) = bytes
        .get(at..at + BLOCK_HEADER_LEN)
        .and_then(|b| <&[u8; BLOCK_HEADER_LEN]>::try_from(b).ok())
        .and_then(BlockHeader::decode)
    {
        let len = usize::try_from(h.block_len()).unwrap();
        out.push((h.block_type, at, len));
        at += len;
    }
    out
}

fn checkpoint_count(w: &World) -> usize {
    let bytes = w.fs.read(&cursor_path(w)).unwrap();
    blocks(&bytes)
        .iter()
        .filter(|b| b.0 == CHECKPOINT_BLOCK)
        .count()
}

fn file_checkpoint(w: &World) -> Checkpoint {
    let bytes = w.fs.read(&cursor_path(w)).unwrap();
    let store_id = kvik_log::header::FileHeader::decode(&bytes)
        .unwrap()
        .store_id;
    kvik_log::cursor::decode(&bytes, &store_id)
        .unwrap()
        .checkpoint
        .unwrap()
}

/// A gates-only consumer with its start transitions confirmed, then `n` facts that move no gate, a second apart;
/// `conditions.checkpoint_interval` 10 s, at most 2 facts replayed, the mark synced hourly.
fn quiet_gates(name: &str, n: u32) -> (World, Store, Consumer) {
    let mut w = World::quiet(name);
    w.mark_sync = MarkSync::Interval(Duration::from_secs(3_600));
    w.conditions = Some(
        ConditionsConfig::default()
            .replay_max(ReplayMax::new(Duration::from_secs(24 * 3_600), 2))
            .checkpoint_interval(Duration::from_secs(10)),
    );
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    emit(&w, &s, id, 50.0);
    let got = ready(&mut c);
    assert_eq!(
        seen(&got),
        ["Some(Unknown) r=false", "Some(Level(0)) r=false"]
    );
    block_on(c.confirm(got[1].id)).unwrap();
    for k in 0..n {
        w.clock.advance(Duration::from_secs(1));
        emit(&w, &s, id, 51.0 + f64::from(k));
    }
    assert_eq!(seen(&ready(&mut c)), Vec::<String>::new());
    (w, s, c)
}

/// Q-2086, design §7: with nothing to confirm, `conditions.checkpoint_interval` alone moves the checkpoint, so a
/// crash replays from it within `conditions.replay_max` rather than reseeding.
#[test]
fn a_quiet_gates_only_consumer_writes_a_checkpoint_each_interval() {
    let (mut w, s, c) = quiet_gates("gates-checkpoint-interval", 4);
    assert_eq!(checkpoint_count(&w), 1);
    w.clock.advance(Duration::from_secs(10));
    s.tick().unwrap();
    assert_eq!(checkpoint_count(&w), 2);
    w.crash(s, vec![c], false);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(seen(&ready(&mut c)), ["resync Resume [(Level(0), true)]"]);
    let d = s.stats().delivery;
    assert_eq!((d.transitions_not_replayed, d.gates_reseeded), (0, 0));
    block_on(s.shutdown()).unwrap();
}

/// G5, spec §3 Durable mark: a timed checkpoint whose frontier is above the synced mark forces a mark sync first, so
/// the cursor file never names a position the mark does not cover.
#[test]
fn a_timed_checkpoint_above_the_synced_mark_syncs_the_mark_first() {
    let (w, s, c) = quiet_gates("gates-checkpoint-mark", 4);
    let before = s.stats().log.mark_synced;
    w.clock.advance(Duration::from_secs(10));
    s.tick().unwrap();
    let ck = file_checkpoint(&w);
    let synced = s.stats().log.mark_synced;
    assert!(ck.through.sequence > before.sequence, "{ck:?} {before:?}");
    assert!(
        ck.highest().sequence <= synced.sequence,
        "{ck:?} {synced:?}"
    );
    drop(c);
    block_on(s.shutdown()).unwrap();
}

/// Failure table, "a `0x31` damaged later, none valid": a gate cursor with no checkpoint before it is damage, so the
/// gates are reseeded and counted, nothing is replayed, and no checkpoint mismatch is counted.
#[test]
fn a_gate_cursor_without_a_valid_checkpoint_reseeds() {
    let (mut w, s, c) = quiet_gates("gates-checkpoint-none-valid", 0);
    w.crash(s, vec![c], false);
    let path = cursor_path(&w);
    let bytes = w.fs.read(&path).unwrap();
    let bs = blocks(&bytes);
    let n = bs.len();
    assert!(
        n >= 2 && bs[n - 2].0 == CHECKPOINT_BLOCK && bs[n - 1].0 == CURSOR_BLOCK,
        "{bs:?}"
    );
    assert_eq!(checkpoint_count(&w), 1);
    let (_, at, len) = bs[n - 2];
    let mut spliced = bytes[at + len..].to_vec();
    spliced.resize(bytes.len() - at, 0);
    w.fs.poke(&path, at, &spliced);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(seen(&ready(&mut c)), ["resync Resume [(Level(0), false)]"]);
    let d = s.stats().delivery;
    assert_eq!(
        (
            d.gates_reseeded,
            d.checkpoint_mismatch,
            d.transitions_not_replayed
        ),
        (1, 0, 0)
    );
    block_on(s.shutdown()).unwrap();
}

/// Q-2086: a gate confirmation writes a checkpoint at once, inside `conditions.checkpoint_interval`, so the facts
/// before it are not replayed again and the replay stays within `conditions.replay_max`.
#[test]
fn a_gate_confirmation_writes_a_checkpoint_within_the_interval() {
    let mut w = World::quiet("gates-checkpoint-confirmation");
    w.conditions = Some(
        ConditionsConfig::default()
            .replay_max(ReplayMax::new(Duration::from_secs(24 * 3_600), 2))
            .checkpoint_interval(Duration::from_secs(3_600)),
    );
    let s = w.open();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("humidity", humidity(id))).unwrap();
    emit(&w, &s, id, 50.0);
    let got = ready(&mut c);
    block_on(c.confirm(got[1].id)).unwrap();
    assert_eq!(checkpoint_count(&w), 1);
    for v in [80.0, 81.0, 82.0] {
        emit(&w, &s, id, v);
        w.clock.advance(Duration::from_secs(1));
    }
    w.clock.advance(Duration::from_secs(120));
    s.tick().unwrap();
    let got = ready(&mut c);
    assert_eq!(seen(&got), ["Some(Level(1)) r=false"]);
    block_on(c.confirm(got[0].id)).unwrap();
    assert_eq!(checkpoint_count(&w), 2);
    w.crash(s, vec![c], false);
    let s = w.open();
    let mut c = take(&s);
    assert_eq!(seen(&ready(&mut c)), ["resync Resume [(Level(1), true)]"]);
    assert_eq!(s.stats().delivery.transitions_not_replayed, 0);
    block_on(s.shutdown()).unwrap();
}

/// Q-1396, Q-2002: a transition dropped while quiet keeps the checkpoint before it until the resync ending the
/// catch-up is confirmed, so a crash after every transition but that resync is confirmed replays it.
#[test]
fn a_transition_dropped_while_quiet_is_replayed_after_a_crash_before_the_catch_up_resync_is_confirmed()
 {
    let mut w = World::quiet("gates-checkpoint-quiet");
    let mut config = w.config();
    config.limits.consumer_queue = 2;
    let s = block_on(Store::open(config.clone())).unwrap();
    let id = block_on(s.define_series(gauge("hall/air/humidity"))).unwrap();
    let mut c = take(&s);
    block_on(c.watch("moved", Gate::change(Input::value(id)).deadband(5.0))).unwrap();
    let mut k = 0.0;
    while s.stats().delivery.transitions_not_delivered == 0 {
        w.clock.advance(Duration::from_secs(1));
        k += 10.0;
        emit(&w, &s, id, k);
    }
    assert_eq!(s.stats().delivery.transitions_not_delivered, 1);
    let mut confirmed = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        match &d.kind {
            DeliveryKind::Transition(t) => {
                confirmed.push(t.id);
                block_on(c.confirm(d.id)).unwrap();
            }
            DeliveryKind::Resync(_) => break,
            _ => block_on(c.confirm(d.id)).unwrap(),
        }
    }
    let last = *confirmed.last().unwrap();
    w.crash(s, vec![c], false);
    let s = block_on(Store::open(config)).unwrap();
    let mut c = take(&s);
    let replayed: Vec<_> = ready(&mut c)
        .into_iter()
        .filter_map(|d| match d.kind {
            DeliveryKind::Transition(t) if d.flags.replayed && t.id > last => Some(t.to),
            _ => None,
        })
        .collect();
    assert_eq!(replayed, [Some(kvik::GateState::Value(Value::F64(k)))]);
    block_on(s.shutdown()).unwrap();
}
