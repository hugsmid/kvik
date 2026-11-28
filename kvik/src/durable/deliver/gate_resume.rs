//! Durable gates (spec §6 Gates, Resume; cond2 design §4.3, §4.4): the gate cursor and the checkpoint a cursor write
//! carries, the replay from them at a consumer's first take in a run, and the reseed where replay is not possible.

use std::collections::{BTreeMap, VecDeque};

use kvik_log::cursor::{Checkpoint, GateCursor};
use kvik_log::entries::{Entry, RouteOp};
use kvik_log::writer::Position as LogPos;
use kvik_model::gate::{ConditionId, Gate, Transition as MTransition};

use super::gate_history::{core_pos, log_pos};
use super::gates::{GatePoint, GateRun, after, covers};
use super::{Ctx, Deliverer, Rec, SECOND, next};
use crate::durable::gates::core_gate;
use crate::gates::{Body, Core, Frontier, Item, Snapshot, id_in, id_out};

/// `conditions.replay_max` and `conditions.checkpoint_interval` (Q-1395).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GateConfig {
    /// Most commit-clock nanoseconds from the checkpoint to the resume.
    pub span: i64,
    /// Most facts after the checkpoint.
    pub facts: u64,
    /// Nanoseconds between timed checkpoints.
    pub interval: i64,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            span: 24 * 3_600 * SECOND,
            facts: 1_000_000,
            interval: 60 * SECOND,
        }
    }
}

/// The gate cursor as the cursor file holds it.
pub(super) fn cursor_out(g: Option<GatePoint>) -> GateCursor {
    match g {
        None => GateCursor::Nothing,
        Some(GatePoint::Transition(id)) => GateCursor::Transition(id_out(&id)),
        Some(GatePoint::Frontier(f)) => GateCursor::Frontier {
            at: log_pos(f.at),
            now: f.now,
        },
    }
}

fn cursor_in(g: GateCursor) -> Option<GatePoint> {
    match g {
        GateCursor::Nothing => None,
        GateCursor::Transition(id) => Some(GatePoint::Transition(id_in(&id))),
        GateCursor::Frontier { at, now } => Some(GatePoint::Frontier(Frontier {
            at: core_pos(at),
            now,
        })),
    }
}

fn frontier_of(c: &Checkpoint) -> Frontier {
    Frontier {
        at: core_pos(c.through),
        now: c.now,
    }
}

/// Why a take reseeds rather than replays.
enum NoReplay {
    /// Beyond `conditions.replay_max` (`transitions_not_replayed`).
    Bound,
    /// No usable checkpoint: none, another generation, past the head, a reset point after it, damage, or a read
    /// that failed.
    Unusable,
}

impl Deliverer {
    /// Sets `conditions.*`.
    pub(crate) fn set_gate_config(&mut self, c: GateConfig) {
        self.gate_config = c;
    }

    /// The instant the commit clock must reach before any batch commits at a start (G10, Q-1394): every
    /// checkpoint's `now` and every gate cursor's instant.
    pub(crate) fn gate_start_floor(&self) -> Option<i64> {
        self.consumers
            .values()
            .flat_map(|rt| {
                let ck = rt.checkpoint.as_ref().map(|c| c.now);
                let g = rt.gate_saved.and_then(|g| g.instant());
                ck.into_iter().chain(g)
            })
            .max()
    }

    /// The core at a consumer's first take in this run: replayed from its checkpoint, the transitions after its
    /// gate cursor deferred as `replayed`, or, where replay is not possible, reseeded (design §4.4 steps 2 and 3).
    pub(super) fn gate_resume(&mut self, name: &str, ctx: &Ctx<'_>) -> Option<GateRun> {
        let saved = self.consumers.get(name).and_then(|rt| rt.gate_saved);
        let confirmed = saved.and_then(cursor_in);
        let mut run = match self.replay(name, saved, confirmed, ctx) {
            Ok(run) => run,
            Err(why) => {
                if matches!(why, NoReplay::Bound) {
                    self.stats.transitions_not_replayed += 1;
                }
                self.reseeded(name, self.head, self.take_commit(ctx))?
            }
        };
        run.confirmed = confirmed;
        Some(run)
    }

    /// The checkpoint a resume starts from: the file's, or with no gate cursor in the file, the empty core at the
    /// cursor (every gate entry of the consumer is after it).
    fn resume_point(&self, name: &str, saved: Option<GateCursor>) -> Result<Snapshot, NoReplay> {
        let rt = &self.consumers[name];
        if !rt.has_cursor || rt.dormant {
            return Err(NoReplay::Unusable);
        }
        match (&rt.checkpoint, saved) {
            (Some(c), _) => Ok(Snapshot::from_checkpoint(c)),
            (None, None) => {
                let now = self.index.commit_time_of(rt.cursor).unwrap_or(i64::MIN);
                Ok(Snapshot::empty(Frontier {
                    at: core_pos(rt.cursor),
                    now,
                }))
            }
            (None, Some(_)) => Err(NoReplay::Unusable),
        }
    }

    fn replay(
        &mut self,
        name: &str,
        saved: Option<GateCursor>,
        confirmed: Option<GatePoint>,
        ctx: &Ctx<'_>,
    ) -> Result<GateRun, NoReplay> {
        let mut snap = self.resume_point(name, saved)?;
        let k = snap.frontier();
        let through = log_pos(k.at);
        let head = self.head;
        if through.generation != head.generation || through > head {
            return Err(NoReplay::Unusable);
        }
        let from = next(through);
        // Positions after the checkpoint less those of entries: the facts, counted without reading them.
        let entries: u64 = self
            .entries
            .range(from..)
            .take_while(|(p, _)| **p <= head)
            .map(|(_, e)| e.sequences())
            .sum();
        let facts = (head.sequence - through.sequence).saturating_sub(entries);
        let floor = self.floor(ctx);
        let until = floor.map_or(self.last_commit_time, |(f, _)| f);
        if facts > self.gate_config.facts || until.saturating_sub(k.now) > self.gate_config.span {
            return Err(NoReplay::Bound);
        }
        if !self.restore_items(from, ctx) {
            return Err(NoReplay::Unusable);
        }
        let mut view = self.view_before(from);
        let defs: BTreeMap<ConditionId, Gate> = view
            .consumers
            .get(name)
            .map(|r| {
                r.gates
                    .iter()
                    .map(|(id, g)| (ConditionId(*id), core_gate(&g.spec, &view.registry)))
                    .collect()
            })
            .unwrap_or_default();
        // A transition queued before an unwatch keeps its gate's name.
        let mut names: BTreeMap<u32, String> = view
            .consumers
            .get(name)
            .map(|r| {
                r.gates
                    .iter()
                    .map(|(id, g)| (*id, g.spec.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let (dropped, missing) = snap.retain_in_force(&defs);
        self.stats.checkpoint_mismatch += (dropped + missing.len()) as u64;
        let mut restore_defs = defs.clone();
        let reseed: Vec<(ConditionId, Gate)> = missing
            .iter()
            .filter_map(|g| restore_defs.remove_entry(g))
            .collect();
        let held = snap.clone();
        let history = self.history();
        let mut core =
            Core::restore(snap, restore_defs, &history).map_err(|_| NoReplay::Unusable)?;
        core.reseed(&reseed, k.at, k.now, &history)
            .map_err(|_| NoReplay::Unusable)?;
        let mut reseeded: BTreeMap<_, _> = self.consumers[name]
            .checkpoint
            .as_ref()
            .map(Snapshot::reseeded_in)
            .unwrap_or_default();
        reseeded.extend(
            core.states()
                .into_iter()
                .filter(|((g, _), _)| missing.contains(g))
                .map(|(key, (_, set_by))| (key, set_by)),
        );
        let mut out = Vec::new();
        let items: Vec<(LogPos, Item)> = self
            .items
            .range(from..)
            .take_while(|(p, _)| **p <= head)
            .map(|(p, rec)| {
                let body = match rec {
                    Rec::Fact(f) => Body::Fact {
                        fact: super::gate_history::fact_item(&f.stored.row, self),
                        outranked: f.outranked,
                    },
                    Rec::Entry(e) => {
                        if let Entry::Routing(r) = e
                            && r.consumer == name
                            && let Some(g) = &r.gate
                            && let Some(spec) = &g.spec
                        {
                            names.insert(g.id, spec.name.clone());
                        }
                        let body = gate_body(name, e, &view);
                        view.apply(*p, e);
                        match super::gate_history::defined(e) {
                            Some(id) => Body::Define {
                                series: kvik_model::types::SeriesId(id),
                                names: super::gate_history::names_of(id, &view),
                            },
                            None => body,
                        }
                    }
                };
                let item = Item {
                    position: core_pos(*p),
                    body,
                };
                (*p, item)
            })
            .collect();
        // A reset point after the checkpoint reseeds every instance where it lies (design §4.4 step 5, Q-2170): at its
        // recovery entry, or for a lost range at its last position, before the next entry and at the commit time
        // before it, as the model's history places it.
        let resets: Vec<LogPos> = self
            .resets
            .range(from..)
            .take_while(|(p, _)| **p <= head)
            .map(|(p, _)| *p)
            .collect();
        let mut items: Vec<(LogPos, Item)> = items
            .into_iter()
            .map(|(p, i)| match resets.binary_search(&p) {
                Ok(_) => (
                    p,
                    Item {
                        position: i.position,
                        body: Body::Reset,
                    },
                ),
                Err(_) => (p, i),
            })
            .collect();
        for r in &resets {
            if let Err(at) = items.binary_search_by_key(r, |(p, _)| *p) {
                let item = Item {
                    position: core_pos(*r),
                    body: Body::Reset,
                };
                items.insert(at, (*r, item));
            }
        }
        let mut last_commit = k.now;
        for (p, item) in items {
            let commit = match item.body {
                Body::Reset if !self.items.contains_key(&p) => last_commit,
                _ => self.index.commit_time_of(p).unwrap_or(k.now),
            };
            last_commit = last_commit.max(commit);
            let ts = core
                .apply_batch(commit, std::slice::from_ref(&item), &history)
                .map_err(|_| NoReplay::Unusable)?;
            out.extend(ts);
        }
        if let Some((f, p)) = floor
            && head >= p
        {
            out.extend(core.advance(f));
        }
        if !resets.is_empty() && !core.states().is_empty() {
            self.stats.gates_reseeded += 1;
        }
        // An instance a reset point reseeded and nothing has moved since carries `set_by` none (Q-1392).
        reseeded.extend(core.states().into_iter().filter_map(|(key, (_, set_by))| {
            (set_by.due.is_none() && resets.binary_search(&log_pos(set_by.at)).is_ok())
                .then_some((key, set_by))
        }));
        let mut run = self.replayed_run(name, core, held, confirmed, reseeded, out);
        names.append(&mut run.names);
        run.names = names;
        Ok(run)
    }

    /// The run a replay leaves: its transitions after the gate cursor deferred as `replayed`, the checkpoint held
    /// before them.
    fn replayed_run(
        &self,
        name: &str,
        core: Core,
        held: Snapshot,
        confirmed: Option<GatePoint>,
        reseeded: BTreeMap<
            (ConditionId, kvik_model::types::SeriesId),
            kvik_model::gate::TransitionId,
        >,
        out: Vec<MTransition>,
    ) -> GateRun {
        let restart = Frontier {
            at: core_pos(self.head),
            now: core.frontier().now,
        };
        let mut run = GateRun::new(core);
        run.pre = Some(held);
        run.reseeded = reseeded;
        run.restart = Some(restart);
        if let Some(routes) = self.view.consumers.get(name) {
            run.names = routes
                .gates
                .iter()
                .map(|(id, g)| (*id, g.spec.name.clone()))
                .collect();
        }
        let mut deferred = VecDeque::new();
        for t in out {
            if confirmed.is_none_or(|g| !covers(&g, &t.id)) {
                deferred.push_back((t, true));
            }
        }
        if deferred.is_empty() {
            run.pre = Some(run.core.snapshot());
        } else {
            run.hold();
        }
        run.deferred = deferred;
        run
    }

    /// Whether a cursor write for `name` should carry a fresh checkpoint (design §4.3): the content's frontier is not
    /// the one last written, and a transition was confirmed since, none was written yet, this is the last write
    /// before a clean shutdown, or `conditions.checkpoint_interval` has passed.
    pub(super) fn checkpoint_wanted(&self, name: &str, last: bool, mono: i64) -> bool {
        let Some(rt) = self.consumers.get(name) else {
            return false;
        };
        let Some(run) = rt.gates.as_ref() else {
            return false;
        };
        let at = run
            .held
            .as_ref()
            .map_or(run.core.frontier(), Snapshot::frontier);
        let written = rt.checkpoint.as_ref().map(frontier_of);
        written != Some(at)
            && (rt.gate_dirty
                || written.is_none()
                || last
                || rt
                    .checkpoint_at
                    .is_none_or(|t| mono.saturating_sub(t) >= self.gate_config.interval))
    }

    /// The checkpoint a cursor write carries: the held snapshot, or the core now when nothing is unconfirmed. Never
    /// after an unconfirmed transition (G5).
    pub(super) fn checkpoint_content(&mut self, name: &str) -> Option<Checkpoint> {
        let run = self.consumers.get_mut(name)?.gates.as_mut()?;
        let snap = match &run.held {
            Some(h) => h.clone(),
            None => {
                let s = run.core.snapshot();
                run.pre = Some(s.clone());
                s
            }
        };
        debug_assert!(
            run.outstanding
                .iter()
                .chain(run.deferred.iter().map(|(t, _)| t))
                .all(|t| after(&t.id, snap.frontier())),
            "G5: the checkpoint is after an unconfirmed transition"
        );
        Some(snap.to_checkpoint(&run.reseeded))
    }
}

/// What a log entry is to `name`'s core.
fn gate_body(name: &str, e: &Entry, view: &crate::durable::routing::View) -> Body {
    match e {
        Entry::Routing(r) if r.consumer == name => match (r.op, &r.gate) {
            (RouteOp::Watch, Some(g)) => match &g.spec {
                Some(spec) => Body::Watch {
                    id: ConditionId(g.id),
                    gate: core_gate(spec, &view.registry),
                },
                None => Body::Other,
            },
            (RouteOp::Unwatch, Some(g)) => Body::Unwatch {
                id: ConditionId(g.id),
            },
            _ => Body::Other,
        },
        _ => Body::Other,
    }
}
