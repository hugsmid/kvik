//! Gates in the deliverer (spec §6 Gates; cond2 design §4): one [`GateRun`] per consumer taken in this run, fed every
//! item of every batch taken, with timers fired under the writer's commit floor. Confirmations move the gate cursor
//! in memory only: until the cursor file holds it (cond2 step 5) every start reseeds (`docs/api.md` Gates).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use kvik_log::entries::{RouteOp, Routing};
use kvik_log::writer::Position as LogPos;
use kvik_model::gate::{ConditionId, GateState, Transition as MTransition, TransitionId};
use kvik_model::types::{Nanos, SeriesId};

use super::gate_history::{LogHistory, core_pos, log_pos};
use super::{Ctx, Deliverer, Pend, next};
use crate::backend::gates::{id_out, state_out, transition_out};
use crate::durable::gates::core_gate;
use crate::gate::{GateId, GateInstanceState};
use crate::gates::{Body, Core, Frontier, Item, Snapshot};

type Key = (ConditionId, SeriesId);

/// The gate cursor (spec §6 Gates, Resume): a transition's id, or the frontier of a resync.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GatePoint {
    Transition(TransitionId),
    Frontier(Frontier),
}

/// One consumer's gates as this process judges them (cond2 design §3.1).
#[derive(Debug)]
pub(super) struct GateRun {
    core: Core,
    /// Queued or handed out and not confirmed, in id order.
    outstanding: VecDeque<MTransition>,
    /// The core at or before the judgement of the oldest outstanding transition (G5); the checkpoint of step 5.
    held: Option<Snapshot>,
    /// Taken before the first judgement after `outstanding` emptied; becomes `held` with the next transition.
    pre: Option<Snapshot>,
    /// Not written to the cursor file yet (cond2 step 5).
    confirmed: Option<GatePoint>,
    /// Judged while the consumer is behind; queued as its catch-up passes their positions, `true` if replayed.
    deferred: VecDeque<(MTransition, bool)>,
    /// `deferred` was full: judging, queuing nothing, until the catch-up ends (design §4.3 Overflow).
    quiet: bool,
    /// Per instance, the state last queued or carried by a resync.
    emitted: BTreeMap<Key, GateState>,
    /// Per instance, the transition that set its state now.
    last: BTreeMap<Key, MTransition>,
    /// Instances a reseed set and nothing has moved since: their `set_by` is none (Q-1392).
    reseeded: BTreeMap<Key, TransitionId>,
    /// The highest id handed out: one at or below it comes again flagged `replayed`.
    seen: Option<TransitionId>,
    /// A core call failed: the core was rebuilt by a reseed (Q-1881).
    pub(super) failed: bool,
}

impl GateRun {
    fn new(core: Core) -> Self {
        GateRun {
            pre: Some(core.snapshot()),
            core,
            outstanding: VecDeque::new(),
            held: None,
            confirmed: None,
            deferred: VecDeque::new(),
            quiet: false,
            emitted: BTreeMap::new(),
            last: BTreeMap::new(),
            reseeded: BTreeMap::new(),
            seen: None,
            failed: false,
        }
    }

    /// Before a judgement that may emit: the snapshot `held` takes when the first transition after an empty
    /// `outstanding` is queued.
    fn ready(&mut self) {
        if self.outstanding.is_empty() && self.pre.is_none() {
            self.pre = Some(self.core.snapshot());
        }
    }

    fn note(&mut self, t: &MTransition) {
        let k = (t.id.condition, t.id.series);
        self.reseeded.remove(&k);
        self.last.insert(k, t.clone());
    }

    /// Each instance's state now as a resync carries it.
    fn states(&self, d: &Deliverer) -> Vec<GateInstanceState> {
        self.core
            .states()
            .into_iter()
            .map(|((g, s), (state, set_by))| GateInstanceState {
                condition: GateId(g.0),
                series: crate::types::SeriesId(s.0),
                state: state_out(&state, d.view.registry.get(s.0).map(|x| &x.spec)),
                set_by: (self.reseeded.get(&(g, s)) != Some(&set_by)).then(|| id_out(&set_by)),
            })
            .collect()
    }
}

/// A transition as the interface shows it, under its gate's name.
pub(super) fn transition_delivery(
    d: &Deliverer,
    name: &str,
    t: &MTransition,
) -> crate::gate::Transition {
    let gate_name = d
        .view
        .consumers
        .get(name)
        .and_then(|r| r.gates.get(&t.id.condition.0))
        .map(|g| g.spec.name.clone())
        .unwrap_or_default();
    transition_out(
        t,
        gate_name,
        d.view.registry.get(t.id.series.0).map(|s| &s.spec),
    )
}

/// Whether `t` goes before the queued delivery `p`: before a fact at a later position, after the fact at its own,
/// at or before the position of any other delivery (spec §4 Order, §6 Identity; as the model backend orders them).
fn before(t: &MTransition, p: &Pend) -> bool {
    let at = log_pos(t.id.at);
    match p {
        Pend::Fact { rec, .. } => at < rec.stored.position,
        Pend::Trigger { position, .. } | Pend::Resync { position, .. } => at <= *position,
        Pend::Transition { t: q, .. } => t.id < q.id,
    }
}

/// What a log item gives the gates cores: a fact with whether it is outranked, a gate routing entry, or a position.
pub(super) enum Feed<'a> {
    Fact(crate::gates::FactItem, bool),
    Routing(&'a Routing),
    Other,
}

impl Deliverer {
    /// The floor live gate timers fire under (Q-1393, provisional: the commit clock; spec §6 Gates, Clock): an
    /// instant no later batch commits below, with the position every batch committing below it is at or below.
    /// The monotonic alternative replaces this function alone.
    fn floor(&self, ctx: &Ctx<'_>) -> Option<(Nanos, LogPos)> {
        #[cfg(test)]
        if let Some(f) = self.floor_hold {
            return Some(f);
        }
        ctx.watch.map(|w| w.commit_clock().floor(ctx.wall))
    }

    /// Sets the floor in place of the commit clock's, as when a batch is in flight; `None` restores it.
    #[cfg(test)]
    pub(crate) fn hold_floor(&mut self, f: Option<(i64, LogPos)>) {
        self.floor_hold = f;
    }

    fn history<'a>(&'a self, ctx: &'a Ctx<'a>) -> LogHistory<'a> {
        LogHistory { d: self, ctx }
    }

    /// The gates in force for `name`, as the core judges them.
    fn gates_in_force(&self, name: &str) -> Vec<(ConditionId, kvik_model::gate::Gate)> {
        self.view
            .consumers
            .get(name)
            .map(|r| {
                r.gates
                    .iter()
                    .map(|(id, g)| (ConditionId(*id), core_gate(&g.spec, &self.view.registry)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A core for `name` with its gates in force started at the head by the seed rule (cond2 design §3.3, reseed).
    fn reseeded(&mut self, name: &str, ctx: &Ctx<'_>) -> Option<GateRun> {
        let gates = self.gates_in_force(name);
        let mut core = Core::new();
        let at = core_pos(self.head);
        let r = core.reseed(&gates, at, self.last_commit_time, &self.history(ctx));
        if r.is_err() {
            self.stats.gate_errors += 1;
            core = Core::new();
        }
        let mut run = GateRun::new(core);
        run.failed = r.is_err();
        run.reseeded = run
            .core
            .states()
            .into_iter()
            .map(|(k, (_, set_by))| (k, set_by))
            .collect();
        self.stats.gates_reseeded += u64::from(!gates.is_empty());
        Some(run)
    }

    /// At a take (`docs/api.md` `Store::consumer`): the first in this run reseeds the consumer's gates in force
    /// (no gate cursor is written yet, so every start does, cond2 step 4); a later one queues its unconfirmed
    /// transitions again under their ids as the catch-up from its cursor passes them (design §4.4 step 4).
    pub(super) fn gate_take(&mut self, name: &str, again: bool, ctx: &Ctx<'_>) {
        let has = self
            .consumers
            .get(name)
            .is_some_and(|rt| rt.gates.is_some());
        if again && has {
            let rt = self.consumers.get_mut(name).expect("taken");
            let run = rt.gates.as_mut().expect("looked at above");
            let seen = run.seen;
            let mut back: VecDeque<(MTransition, bool)> = run
                .outstanding
                .drain(..)
                .map(|t| {
                    let r = seen.is_some_and(|s| t.id <= s);
                    (t, r)
                })
                .collect();
            back.extend(run.deferred.drain(..));
            run.deferred = back;
            run.emitted.clear();
            return;
        }
        let in_force = self
            .view
            .consumers
            .get(name)
            .is_some_and(|r| !r.gates.is_empty());
        if in_force && !has {
            let run = self.reseeded(name, ctx);
            if let Some(rt) = self.consumers.get_mut(name) {
                rt.gates = run;
            }
        }
    }

    /// Before a batch committed at `commit`: timers due by then fire first (Q-1144).
    pub(super) fn gate_batch(&mut self, commit: Nanos, ctx: &Ctx<'_>) {
        let names = self.gate_names();
        for name in names {
            self.gate_call(&name, ctx, |core, _| Ok(core.advance(commit)));
        }
    }

    /// One item of a batch committed at `commit`, at `p`, for every consumer judging gates; a consumer's first
    /// watch starts its run.
    pub(super) fn gate_item(&mut self, p: LogPos, commit: Nanos, feed: &Feed<'_>, ctx: &Ctx<'_>) {
        if let Feed::Routing(r) = feed
            && r.op == RouteOp::Watch
            && let Some(rt) = self.consumers.get_mut(&r.consumer)
            && rt.attached
            && rt.gates.is_none()
        {
            rt.gates = Some(GateRun::new(Core::new()));
        }
        let names = self.gate_names();
        for name in names {
            let body = match feed {
                Feed::Fact(f, outranked) => Body::Fact {
                    fact: f.clone(),
                    outranked: *outranked,
                },
                Feed::Routing(r) if r.consumer == name => match (r.op, &r.gate) {
                    (RouteOp::Watch, Some(g)) => match &g.spec {
                        Some(spec) => Body::Watch {
                            id: ConditionId(g.id),
                            gate: core_gate(spec, &self.view.registry),
                        },
                        None => Body::Other,
                    },
                    (RouteOp::Unwatch, Some(g)) => Body::Unwatch {
                        id: ConditionId(g.id),
                    },
                    _ => Body::Other,
                },
                _ => Body::Other,
            };
            let item = Item {
                position: core_pos(p),
                body,
            };
            self.gate_call(&name, ctx, |core, h| {
                core.apply_batch(commit, std::slice::from_ref(&item), h)
            });
        }
    }

    /// The commit floor: once every batch assigned below it has been taken, timers due by it fire.
    pub(super) fn gate_timers(&mut self, ctx: &Ctx<'_>) {
        let Some((f, p)) = self.floor(ctx) else {
            return;
        };
        if self.head < p {
            return;
        }
        self.gate_floor = Some(f);
        let names = self.gate_names();
        for name in names {
            self.gate_call(&name, ctx, |core, _| Ok(core.advance(f)));
        }
    }

    /// How long the `kvik-deliver` thread may wait before a gate timer is due (design §4.2 Wake).
    pub(crate) fn gate_wait(&self) -> Option<Duration> {
        let floor = self.gate_floor?;
        let due = self
            .consumers
            .values()
            .filter_map(|rt| rt.gates.as_ref()?.core.next_due())
            .min()?;
        let ns = u64::try_from(due.saturating_sub(floor)).unwrap_or(0);
        Some(Duration::from_nanos(ns).clamp(Duration::from_millis(1), Duration::from_secs(1)))
    }

    /// Whether any consumer judges gates.
    pub(super) fn gating(&self) -> bool {
        self.consumers
            .values()
            .any(|rt| rt.attached && rt.gates.is_some())
    }

    fn gate_names(&self) -> Vec<String> {
        self.consumers
            .iter()
            .filter(|(_, rt)| rt.attached && rt.gates.is_some())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Runs one core call for `name` and queues what it emits. A failed call rebuilds the core by a reseed at the
    /// head (Q-1881): what it would have emitted is lost, counted.
    fn gate_call(
        &mut self,
        name: &str,
        ctx: &Ctx<'_>,
        f: impl FnOnce(
            &mut Core,
            &dyn crate::gates::History,
        ) -> Result<Vec<MTransition>, crate::gates::CoreError>,
    ) {
        let Some(mut run) = self.consumers.get_mut(name).and_then(|rt| rt.gates.take()) else {
            return;
        };
        run.ready();
        let r = f(&mut run.core, &self.history(ctx));
        match r {
            Ok(ts) => {
                if let Some(rt) = self.consumers.get_mut(name) {
                    rt.gates = Some(run);
                }
                for t in ts {
                    self.gate_emit(name, t);
                }
            }
            Err(_) => {
                self.stats.gate_errors += 1;
                let mut fresh = self.reseeded(name, ctx).expect("a run");
                fresh.emitted = run.emitted;
                fresh.last = run.last;
                fresh.seen = run.seen;
                fresh.outstanding = run.outstanding;
                fresh.held = run.held;
                fresh.confirmed = run.confirmed;
                fresh.deferred = run.deferred;
                fresh.quiet = true;
                fresh.failed = true;
                if let Some(rt) = self.consumers.get_mut(name) {
                    rt.gates = Some(fresh);
                    rt.behind.get_or_insert(next(self.head));
                }
            }
        }
    }

    /// A transition judged for `name`: queued live, deferred while it is behind, or counted while quiet.
    fn gate_emit(&mut self, name: &str, t: MTransition) {
        let capacity = self.capacity;
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        let Some(run) = rt.gates.as_mut() else { return };
        run.note(&t);
        if run.quiet {
            self.stats.transitions_not_delivered += 1;
            return;
        }
        if rt.behind.is_none() && rt.queue.len() >= capacity {
            // Catch up from after its position (`spec.md` §4 Overflow): the queue holds every earlier delivery.
            rt.behind = Some(next(log_pos(t.id.at)));
        }
        if rt.behind.is_some() || rt.dormant {
            if run.deferred.len() >= capacity {
                run.quiet = true;
                self.stats.transitions_not_delivered += 1;
            } else {
                run.deferred.push_back((t, false));
            }
            return;
        }
        enqueue(rt, t, false);
        self.changed(name);
    }

    /// Queues the deferred transitions at or below `upto` (all with `None`) for a consumer catching up.
    pub(super) fn gate_flush(&mut self, name: &str, upto: Option<LogPos>, strictly: bool) {
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        loop {
            let Some(run) = rt.gates.as_mut() else { return };
            let due = run.deferred.front().is_some_and(|(t, _)| {
                let at = log_pos(t.id.at);
                upto.is_none_or(|u| if strictly { at < u } else { at <= u })
            });
            if !due {
                return;
            }
            let (t, replayed) = run.deferred.pop_front().expect("looked at above");
            rt.queue.push_back(Pend::Transition {
                t: Box::new(t.clone()),
                replayed,
            });
            push_outstanding(rt.gates.as_mut().expect("looked at above"), t);
        }
    }

    /// At the end of a catch-up: the rest of what was deferred, then, if it went quiet, for each instance whose
    /// state differs from the last it was sent, the transition that set it (as condition funnels re-attach, `spec.md`
    /// §4 Overflow; Q-2002).
    pub(super) fn gate_reattach(&mut self, name: &str) {
        self.gate_flush(name, None, false);
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        let Some(run) = rt.gates.as_mut() else { return };
        if !run.quiet {
            return;
        }
        run.quiet = false;
        let mut send: Vec<MTransition> = run
            .core
            .states()
            .into_iter()
            .filter(|(k, (state, _))| run.emitted.get(k) != Some(state))
            .filter_map(|(k, _)| run.last.get(&k).cloned())
            .collect();
        send.sort_by_key(|t| t.id);
        for t in send {
            rt.queue.push_back(Pend::Transition {
                t: Box::new(t.clone()),
                replayed: false,
            });
            push_outstanding(rt.gates.as_mut().expect("looked at above"), t);
        }
    }

    /// Each instance's state now for a resync of `name`, with the frontier it was built at; the states count as
    /// sent.
    pub(super) fn gate_resync(&mut self, name: &str) -> (Vec<GateInstanceState>, Option<Frontier>) {
        let Some(run) = self.consumers.get(name).and_then(|rt| rt.gates.as_ref()) else {
            return (Vec::new(), None);
        };
        let states = run.states(self);
        let frontier = run.core.frontier();
        let rt = self.consumers.get_mut(name).expect("looked at above");
        let run = rt.gates.as_mut().expect("looked at above");
        run.emitted = run
            .core
            .states()
            .into_iter()
            .map(|(k, (s, _))| (k, s))
            .collect();
        (states, Some(frontier))
    }

    /// A transition handed out.
    pub(super) fn gate_handed(&mut self, name: &str, id: TransitionId) {
        if let Some(run) = self
            .consumers
            .get_mut(name)
            .and_then(|rt| rt.gates.as_mut())
        {
            run.seen = run.seen.max(Some(id));
        }
    }

    /// A confirmation through transition `id`, or of a resync built at `frontier` (cumulative): the gate cursor in
    /// memory.
    pub(super) fn gate_confirm(&mut self, name: &str, point: GatePoint) {
        let Some(run) = self
            .consumers
            .get_mut(name)
            .and_then(|rt| rt.gates.as_mut())
        else {
            return;
        };
        match point {
            GatePoint::Transition(id) => {
                while run.outstanding.front().is_some_and(|t| t.id <= id) {
                    run.outstanding.pop_front();
                }
            }
            GatePoint::Frontier(f) => {
                while run.outstanding.front().is_some_and(|t| !after(&t.id, f)) {
                    run.outstanding.pop_front();
                }
            }
        }
        run.confirmed = Some(point);
        if run.outstanding.is_empty() {
            run.held = None;
            run.pre = None;
        }
    }

    /// The gate cursor in memory and the held snapshot's frontier, for the tests.
    #[cfg(test)]
    pub(crate) fn gate_cursor(
        &self,
        name: &str,
    ) -> Option<(Option<GatePoint>, Option<Frontier>, usize)> {
        let run = self.consumers.get(name)?.gates.as_ref()?;
        Some((
            run.confirmed,
            run.held.as_ref().map(Snapshot::frontier),
            run.outstanding.len(),
        ))
    }
}

/// Whether `id` is after the frontier `f`: after every transition at or below its position and due by its instant.
pub(super) fn after(id: &TransitionId, f: Frontier) -> bool {
    id.at > f.at || id.due.is_some_and(|d| d > f.now)
}

fn push_outstanding(run: &mut GateRun, t: MTransition) {
    if run.outstanding.is_empty() {
        run.held = run.pre.take();
    }
    debug_assert!(
        run.held.as_ref().is_none_or(|h| after(&t.id, h.frontier())),
        "G5: the held snapshot is after an outstanding transition"
    );
    run.emitted.insert(
        (t.id.condition, t.id.series),
        t.to.clone().unwrap_or(GateState::Unknown),
    );
    run.outstanding.push_back(t);
}

/// Queues a live transition at its place by position among the deliveries not yet handed out.
fn enqueue(rt: &mut super::Rt, t: MTransition, replayed: bool) {
    let i = rt
        .queue
        .iter()
        .rposition(|p| !before(&t, p))
        .map_or(0, |i| i + 1);
    rt.queue.insert(
        i,
        Pend::Transition {
            t: Box::new(t.clone()),
            replayed,
        },
    );
    push_outstanding(rt.gates.as_mut().expect("a run"), t);
}
