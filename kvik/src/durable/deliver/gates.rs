//! Gates in the deliverer (spec §6 Gates; cond2 design §4): one [`GateRun`] per consumer taken in this run, fed every
//! item of every batch taken, with timers fired under the writer's commit floor. The cursor file holds the gate cursor
//! and a checkpoint; the first take in a run replays from them ([`super::gate_resume`]).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use kvik_log::entries::{RouteOp, Routing};
use kvik_log::writer::Position as LogPos;
use kvik_model::gate::{ConditionId, Transition as MTransition, TransitionId};
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
    pub(super) core: Core,
    /// Queued or handed out and not confirmed, in id order.
    pub(super) outstanding: VecDeque<MTransition>,
    /// The core at or before the judgement of the oldest transition judged and not confirmed, queued, deferred or
    /// dropped (G5): the checkpoint while there is one.
    pub(super) held: Option<Snapshot>,
    /// Taken while nothing is unconfirmed, before a judgement; becomes `held` at the next transition judged.
    pub(super) pre: Option<Snapshot>,
    /// The gate cursor.
    pub(super) confirmed: Option<GatePoint>,
    /// Judged while the consumer is behind; queued as its catch-up passes their positions, `true` if replayed.
    pub(super) deferred: VecDeque<(MTransition, bool)>,
    /// `deferred` was full: judging, queuing nothing, until a resync carries the states (design §4.3 Overflow;
    /// Q-2002).
    quiet: bool,
    /// The highest transition dropped while quiet and not yet covered by the gate cursor: the checkpoint stays before
    /// it until the resync ending the catch-up is confirmed (Q-1396, Q-2270).
    dropped: Option<TransitionId>,
    /// The id each reseeded instance was set by: while its `set_by` is still that id it reports none (Q-1392), and
    /// a checkpoint flags it (Q-2160). Never pruned: a reseed id names the frontier it started at, so no later
    /// transition has it, and the held snapshot may be older than the core.
    pub(super) reseeded: BTreeMap<Key, TransitionId>,
    /// The highest id handed out: one at or below it comes again flagged `replayed`.
    seen: Option<TransitionId>,
    /// The frontier a replay at this run's first take reached: every transition at or before it is `replayed`.
    pub(super) restart: Option<Frontier>,
    /// Every gate watched in this run, by id: a transition queued before an unwatch keeps its name.
    pub(super) names: BTreeMap<u32, String>,
    /// A core call failed: the core was rebuilt by a reseed (Q-1881).
    pub(super) failed: bool,
}

impl GateRun {
    pub(super) fn new(core: Core) -> Self {
        GateRun {
            pre: Some(core.snapshot()),
            core,
            outstanding: VecDeque::new(),
            held: None,
            confirmed: None,
            deferred: VecDeque::new(),
            quiet: false,
            dropped: None,
            reseeded: BTreeMap::new(),
            seen: None,
            restart: None,
            names: BTreeMap::new(),
            failed: false,
        }
    }

    /// Before a judgement that may emit: the snapshot `held` takes at the next transition judged.
    fn ready(&mut self) {
        if self.held.is_none() && self.pre.is_none() {
            self.pre = Some(self.core.snapshot());
        }
    }

    /// A transition was judged: the checkpoint stays at or before it until it is confirmed.
    pub(super) fn hold(&mut self) {
        if self.held.is_none() {
            self.held = self.pre.take();
        }
    }

    /// Nothing judged is unconfirmed.
    pub(super) fn settled(&self) -> bool {
        self.outstanding.is_empty()
            && self.deferred.is_empty()
            && !self.quiet
            && self.dropped.is_none()
    }

    /// Whether `id` comes again flagged `replayed`: handed out before, or replayed at this run's first take.
    pub(super) fn again(&self, id: &TransitionId) -> bool {
        self.seen.is_some_and(|s| *id <= s) || self.restart.is_some_and(|f| !after(id, f))
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
        .consumers
        .get(name)
        .and_then(|rt| rt.gates.as_ref()?.names.get(&t.id.condition.0))
        .cloned()
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
    /// The monotonic alternative replaces this function and clamps `commit` in [`Deliverer::gate_item`] to the
    /// floor given, since no batch may commit below a core's `now` (G3).
    pub(super) fn floor(&self, ctx: &Ctx<'_>) -> Option<(Nanos, LogPos)> {
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

    /// Makes the gate call for the item at `p` fail, as a failed history read does (Q-1881).
    #[cfg(test)]
    pub(crate) fn fail_gate_at(&mut self, p: Option<LogPos>) {
        self.gate_fail_at = p;
    }

    /// Gate history reads from the log so far.
    #[cfg(test)]
    pub(crate) fn gate_reads(&self) -> u64 {
        self.gate_reads.get()
    }

    pub(super) fn history(&self) -> LogHistory<'_> {
        LogHistory::new(self)
    }

    /// The instant a reseed at a take starts at: the floor, not the head's commit time, so a hold that completed by
    /// now is in the state, not a transition (spec §6 Gates, Resume: beyond replay the resync alone is sent; Q-2087).
    /// G3 allows it once the head is at the floor's position.
    pub(super) fn take_commit(&self, ctx: &Ctx<'_>) -> Nanos {
        match self.floor(ctx) {
            Some((f, p)) if self.head >= p => f.max(self.last_commit_time),
            _ => self.last_commit_time,
        }
    }

    /// The gates in force for `name`, as the core judges them.
    pub(super) fn gates_in_force(&self, name: &str) -> Vec<(ConditionId, kvik_model::gate::Gate)> {
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

    /// A core for `name` with its gates in force started at `at`, committed at `commit`, by the seed rule (cond2
    /// design §3.3, reseed).
    pub(super) fn reseeded(&mut self, name: &str, at: LogPos, commit: Nanos) -> Option<GateRun> {
        let gates = self.gates_in_force(name);
        let mut core = Core::new();
        let r = core.reseed(&gates, core_pos(at), commit, &self.history());
        if r.is_err() {
            self.stats.gate_errors += 1;
            core = Core::new();
        }
        let mut run = GateRun::new(core);
        run.failed = r.is_err();
        if let Some(routes) = self.view.consumers.get(name) {
            run.names = routes
                .gates
                .iter()
                .map(|(id, g)| (*id, g.spec.name.clone()))
                .collect();
        }
        run.reseeded = run
            .core
            .states()
            .into_iter()
            .map(|(k, (_, set_by))| (k, set_by))
            .collect();
        self.stats.gates_reseeded += u64::from(!gates.is_empty());
        Some(run)
    }

    /// At a take (`docs/api.md` `Store::consumer`): the first in this run replays from the checkpoint or reseeds
    /// (design §4.4 steps 2 and 3); a later one queues its unconfirmed transitions again under their ids as the
    /// catch-up from its cursor passes them (design §4.4 step 4).
    pub(super) fn gate_take(&mut self, name: &str, again: bool, ctx: &Ctx<'_>) {
        let has = self
            .consumers
            .get(name)
            .is_some_and(|rt| rt.gates.is_some());
        if again && has {
            let rt = self.consumers.get_mut(name).expect("taken");
            let run = rt.gates.as_mut().expect("looked at above");
            let outstanding: Vec<MTransition> = run.outstanding.drain(..).collect();
            let mut back: VecDeque<(MTransition, bool)> = outstanding
                .into_iter()
                .map(|t| {
                    let r = run.again(&t.id);
                    (t, r)
                })
                .collect();
            back.extend(run.deferred.drain(..));
            run.deferred = back;
            return;
        }
        let in_force = self
            .view
            .consumers
            .get(name)
            .is_some_and(|r| !r.gates.is_empty());
        // Gates held at the stop, or watched and unwatched since the checkpoint, still replay.
        let saved = self
            .consumers
            .get(name)
            .is_some_and(|rt| rt.gate_saved.is_some() || rt.checkpoint.is_some());
        if (in_force || saved) && !has {
            let run = self.gate_resume(name, ctx);
            if let Some(rt) = self.consumers.get_mut(name) {
                rt.gates = run;
            }
        }
    }

    /// One item of a batch committed at `commit`, at `p`, for every consumer judging gates (`names`, kept by the
    /// caller for the batch and renewed here at a routing entry); a consumer's first watch starts its run.
    pub(super) fn gate_item(
        &mut self,
        p: LogPos,
        commit: Nanos,
        feed: &Feed<'_>,
        names: &mut Vec<String>,
    ) {
        if let Feed::Routing(r) = feed
            && r.op == RouteOp::Watch
            && self
                .consumers
                .get(&r.consumer)
                .is_some_and(|rt| rt.attached && rt.gates.is_none())
        {
            // An empty core just before the watch: the resume point until a checkpoint is written (design §4.4).
            let empty = Snapshot::empty(Frontier {
                at: core_pos(super::before(p)),
                now: commit,
            });
            let core = Core::restore(empty, BTreeMap::new(), &self.history()).unwrap_or_default();
            let rt = self
                .consumers
                .get_mut(&r.consumer)
                .expect("looked up above");
            rt.gates = Some(GateRun::new(core));
            // The first cursor write after the first watch carries the gate cursor and a checkpoint.
            rt.dirty = true;
        }
        if let Feed::Routing(r) = feed
            && let (RouteOp::Watch, Some(g)) = (r.op, &r.gate)
            && let Some(spec) = &g.spec
            && let Some(run) = self
                .consumers
                .get_mut(&r.consumer)
                .and_then(|rt| rt.gates.as_mut())
        {
            run.names.insert(g.id, spec.name.clone());
        }
        if let Feed::Routing(_) = feed {
            *names = self.gate_names();
        }
        for name in names.iter() {
            let reads = |s| {
                self.consumers
                    .get(name)
                    .and_then(|rt| rt.gates.as_ref())
                    .is_some_and(|run| run.core.reads(s))
            };
            let body = match feed {
                Feed::Fact(f, outranked) if reads(f.series) => Body::Fact {
                    fact: f.clone(),
                    outranked: *outranked,
                },
                Feed::Routing(r) if r.consumer == *name => match (r.op, &r.gate) {
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
            #[cfg(test)]
            let fail = self.gate_fail_at == Some(p);
            self.gate_call(name, (p, commit), |core, h| {
                #[cfg(test)]
                if fail {
                    return Err(crate::gates::CoreError::Id(ConditionId(u32::MAX)));
                }
                core.apply_batch(commit, std::slice::from_ref(&item), h)
            });
        }
    }

    /// Before a batch's items: each run with nothing unconfirmed takes `pre` afresh, so the snapshot held for its
    /// next transition is at most a batch behind its judgement (design §4.3; a replay from it stays short, step 5).
    pub(super) fn gate_batch(&mut self) {
        for rt in self.consumers.values_mut().filter(|rt| rt.attached) {
            if let Some(run) = rt.gates.as_mut()
                && run.held.is_none()
            {
                run.pre = Some(run.core.snapshot());
            }
        }
    }

    /// The commit floor: once every batch assigned below it has been taken, timers due by it fire.
    pub(super) fn gate_timers(&mut self, ctx: &Ctx<'_>) {
        // The floor raises the commit clock: on a store with no gate, commit times stay as without gates (§2).
        if !self.gating() {
            return;
        }
        let Some((f, p)) = self.floor(ctx) else {
            return;
        };
        if self.head < p {
            return;
        }
        self.gate_floor = Some(f);
        let names = self.gate_names();
        for name in names {
            let at = (self.head, self.last_commit_time);
            self.gate_call(&name, at, |core, _| Ok(core.advance(f)));
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

    pub(super) fn gate_names(&self) -> Vec<String> {
        self.consumers
            .iter()
            .filter(|(_, rt)| rt.attached && rt.gates.is_some())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Runs one core call for `name` on the item at `at.0` (committed at `at.1`), or the head for a timer, and
    /// queues what it emits. A failed call rebuilds the core by a reseed there (Q-1881): what it would have emitted
    /// is lost, counted; the item's deliveries are already queued, so the catch-up starts after it.
    fn gate_call(
        &mut self,
        name: &str,
        at: (LogPos, Nanos),
        f: impl FnOnce(
            &mut Core,
            &dyn crate::gates::History,
        ) -> Result<Vec<MTransition>, crate::gates::CoreError>,
    ) {
        let Some(mut run) = self.consumers.get_mut(name).and_then(|rt| rt.gates.take()) else {
            return;
        };
        run.ready();
        let r = f(&mut run.core, &self.history());
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
                let mut fresh = self.reseeded(name, at.0, at.1).expect("a run");
                fresh.seen = run.seen;
                fresh.names.extend(run.names);
                fresh.outstanding = run.outstanding;
                fresh.held = run.held;
                fresh.confirmed = run.confirmed;
                fresh.deferred = run.deferred;
                fresh.dropped = run.dropped;
                fresh.restart = run.restart;
                fresh.quiet = true;
                fresh.failed = true;
                if let Some(rt) = self.consumers.get_mut(name) {
                    rt.gates = Some(fresh);
                    rt.behind.get_or_insert(next(at.0));
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
        run.hold();
        if run.quiet {
            run.dropped = run.dropped.max(Some(t.id));
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
                run.dropped = run.dropped.max(Some(t.id));
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

    /// At the end of a catch-up: the rest of what was deferred; whether it went quiet, so a resync must carry the
    /// gate states (design §4.3 Overflow; Q-2002).
    pub(super) fn gate_reattach(&mut self, name: &str) -> bool {
        self.gate_flush(name, None, false);
        self.consumers
            .get_mut(name)
            .and_then(|rt| rt.gates.as_mut())
            .is_some_and(|run| std::mem::take(&mut run.quiet))
    }

    /// A resync queued for `name`: it carries every gate state, so what a quiet run dropped is superseded.
    pub(super) fn gate_resynced(&mut self, name: &str) {
        if let Some(run) = self
            .consumers
            .get_mut(name)
            .and_then(|rt| rt.gates.as_mut())
        {
            run.quiet = false;
        }
    }

    /// Each instance's state now for a resync of `name` at `position`, with the gate cursor its confirmation sets:
    /// the resync's place in delivery order, since every transition at or below its position is queued before it
    /// and none above.
    pub(super) fn gate_resync(
        &self,
        name: &str,
        position: LogPos,
    ) -> (Vec<GateInstanceState>, Option<Frontier>) {
        let Some(run) = self.consumers.get(name).and_then(|rt| rt.gates.as_ref()) else {
            return (Vec::new(), None);
        };
        let states = run.states(self);
        let frontier = Frontier {
            at: core_pos(position),
            now: run.core.frontier().now,
        };
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
        if run.dropped.is_some_and(|d| covers(&point, &d)) {
            run.dropped = None;
        }
        run.confirmed = Some(point);
        if run.settled() {
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

/// Whether the gate point `g` covers `id`: `id` is at or before it.
pub(super) fn covers(g: &GatePoint, id: &TransitionId) -> bool {
    match g {
        GatePoint::Transition(t) => id <= t,
        GatePoint::Frontier(f) => !after(id, *f),
    }
}

fn push_outstanding(run: &mut GateRun, t: MTransition) {
    run.hold();
    debug_assert!(
        run.held.as_ref().is_none_or(|h| after(&t.id, h.frontier())),
        "G5: the held snapshot is after an outstanding transition"
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
