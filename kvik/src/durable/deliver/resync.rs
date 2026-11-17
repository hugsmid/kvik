//! Resyncs and reading behind (`spec.md` §4 Resync, Overflow, §6 Restart and resume): the resync that ends a resume
//! or marks a reset point, catch-up from the log with the routes in force at each position, and handing deliveries
//! out.

use std::collections::BTreeMap;

use kvik_log::writer::Position as LogPos;
use kvik_model::condition::{ConditionState as MState, Runner};

use super::confirm::{state_in, state_out};
use super::{Ctx, Deliverer, FState, Out, Pend, Rec, next};
use crate::backend::Error;
use crate::consume::{
    Delivery, DeliveryFlags, DeliveryId, DeliveryKind, FunnelId, FunnelState, RESYNC_PART_FUNNELS,
    ResyncCause, ResyncPart, Trigger, TriggerCause,
};
use crate::durable::index::{fact_out, pos};
use crate::durable::routing::{View, model_condition};
use crate::time::Timestamp;

impl Deliverer {
    /// Queues a resync at `position` with the state now of every funnel in force (`spec.md` §4 Resync, §6 Restart
    /// and resume): conditions are rebuilt as of now from the latch; a state that differs from it, or with no
    /// latch, is a transition. `reset`: it carries a reset point's `forget_above`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn issue_resync(
        &mut self,
        name: &str,
        cause: ResyncCause,
        forget_above: Option<LogPos>,
        position: LogPos,
        reset: bool,
        ctx: &Ctx<'_>,
    ) {
        let mut states = Vec::new();
        let mut runners = BTreeMap::new();
        let mut emitted = BTreeMap::new();
        self.incomplete.set(false);
        let latches = self
            .consumers
            .get(name)
            .map(|rt| rt.latches.clone())
            .unwrap_or_default();
        for (id, f) in self.view.funnels(name) {
            let live = self
                .view
                .selected(name, id, f)
                .into_iter()
                .map(|s| (s, self.live_of(s, ctx)))
                .collect();
            let condition = match (&f.condition, f.condition_series()) {
                (Some(c), Some(s)) => {
                    let cond = model_condition(c, &self.view.registry, s);
                    let latch = latches
                        .get(&id)
                        .filter(|l| l.incarnation == f.incarnation)
                        .map(|l| state_in(l.state));
                    let history = self.resume_history(c, s, ctx);
                    let mut runner = Runner::resume(
                        cond.clone(),
                        ctx.mono,
                        self.value_of(s, ctx.now),
                        latch.as_ref(),
                        history,
                    );
                    if let Some(since) = self.scheduled_since(c, s, ctx.now) {
                        runner.on_scheduled(since);
                    }
                    let state = runner.state();
                    let transition = cond.is_transition(&state, latch.as_ref());
                    runners.insert(id, runner);
                    emitted.insert(id, state.clone());
                    Some((state, transition))
                }
                _ => None,
            };
            states.push(FState {
                funnel: id,
                incarnation: f.incarnation,
                live,
                condition,
            });
        }
        let unchecked = self.incomplete.get();
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        rt.queue.push_back(Pend::Resync {
            cause,
            forget_above,
            position,
            funnels: states,
            reset,
            unchecked,
        });
        self.unchecked += u64::from(unchecked);
        rt.evaluated = runners.keys().map(|id| (*id, ctx.now)).collect();
        rt.runners = runners;
        rt.emitted = emitted;
        rt.resume = None;
        self.notify = true;
    }

    /// The routes and registry as of the position before `at`, built from the entries before it.
    fn view_before(&self, at: LogPos) -> View {
        let mut v = View::default();
        for (p, e) in self.entries.range(..at) {
            v.apply(*p, e);
        }
        v
    }

    /// Reads the log for a consumer that is behind into its queue, as far as its queue allows: replayed facts with
    /// the routes in force at each position, a resync at each reset point (consecutive ones with nothing routed
    /// between them as one), the resync that ends a resume, and back to live at the last position taken.
    pub(super) fn catch_up(&mut self, name: &str, ctx: &Ctx<'_>) {
        loop {
            let Some(rt) = self.consumers.get(name) else {
                return;
            };
            let Some(mut at) = rt.behind else { return };
            if rt.paused || rt.dormant {
                return;
            }
            let limit = rt.resume.unwrap_or(self.head);
            if !self.restore_items(at, ctx) {
                return;
            }
            let mut view = match self.consumers.get_mut(name).and_then(|rt| rt.view.take()) {
                Some((p, v)) if p == at => v,
                _ => self.view_before(at),
            };
            let mut paused = false;
            while at <= limit {
                let a = self.items.range(at..=limit).next().map(|(p, _)| *p);
                let b = self.resets.range(at..=limit).next().map(|(p, _)| *p);
                let Some(p) = a.into_iter().chain(b).min() else {
                    break;
                };
                let rt = self.consumers.get_mut(name).expect("looked up above");
                if rt.queue.len() >= self.capacity {
                    paused = true;
                    break;
                }
                if let Some(fa) = self.resets.get(&p).copied() {
                    match rt.queue.back_mut() {
                        Some(Pend::Resync {
                            cause: ResyncCause::ResetPoint,
                            forget_above: Some(f),
                            position,
                            ..
                        }) if rt.reset_open => {
                            *f = (*f).min(fa);
                            *position = (*position).max(p);
                        }
                        _ => {
                            rt.queue.push_back(Pend::Resync {
                                cause: ResyncCause::ResetPoint,
                                forget_above: Some(fa),
                                position: p,
                                funnels: Vec::new(),
                                reset: true,
                                unchecked: false,
                            });
                            rt.reset_open = true;
                        }
                    }
                }
                match self.items.get(&p) {
                    Some(Rec::Entry(e)) => view.apply(p, e),
                    Some(Rec::Fact(f)) if view.routed(name, f.stored.row.series_id) => {
                        if rt.reset_open {
                            // Nothing further until the reset point's resync is confirmed.
                            rt.paused = true;
                            paused = true;
                            break;
                        }
                        rt.queue.push_back(Pend::Fact {
                            rec: f.clone(),
                            replayed: true,
                        });
                    }
                    _ => {}
                }
                at = next(p);
            }
            let rt = self.consumers.get_mut(name).expect("looked up above");
            if paused {
                rt.behind = Some(at);
                rt.view = Some((at, view));
                return;
            }
            at = at.max(next(limit));
            rt.behind = Some(at);
            rt.view = Some((at, view));
            if let Some(stop) = rt.resume {
                // A reset point at the last position is the same delivery as the resync that ends the resume, whose
                // confirmation then records the reset point in the cursor file.
                let mut forget = None;
                if rt.reset_open {
                    match rt.queue.back() {
                        Some(Pend::Resync {
                            cause: ResyncCause::ResetPoint,
                            forget_above,
                            position,
                            ..
                        }) if *position == stop => {
                            forget = *forget_above;
                            rt.queue.pop_back();
                            rt.reset_open = false;
                        }
                        _ => {
                            rt.paused = true;
                            return;
                        }
                    }
                }
                self.issue_resync(
                    name,
                    ResyncCause::Resume,
                    forget,
                    stop,
                    forget.is_some(),
                    ctx,
                );
                continue;
            }
            if rt.reset_open {
                rt.paused = true;
                return;
            }
            rt.behind = None;
            rt.view = None;
            self.reattach(name);
            return;
        }
    }

    /// On re-attaching to live after catch-up, a condition whose state differs from the last state emitted is sent.
    fn reattach(&mut self, name: &str) {
        let Some(rt) = self.consumers.get(name) else {
            return;
        };
        let changed: Vec<(u32, MState)> = rt
            .runners
            .iter()
            .filter(|(id, r)| rt.emitted.get(id) != Some(&r.state()))
            .map(|(id, r)| (*id, r.state()))
            .collect();
        for (id, state) in changed {
            self.emit_trigger(name, id, state, TriggerCause::Reattach, false);
        }
    }

    /// The next delivery for a bound consumer, if one is ready: the next part of a resync being handed out, else,
    /// unless a resync awaits confirmation, the next from its queue after running timers and catch-up.
    pub(crate) fn poll(&mut self, name: &str, ctx: &Ctx<'_>) -> Result<Option<Delivery>, Error> {
        self.bound(name)?;
        self.writable()?;
        if let Some(d) = self.hand_out(name) {
            return Ok(Some(d));
        }
        // Nothing further while a resync awaits confirmation, nor while as many deliveries as the queue holds are
        // handed out and not confirmed.
        let rt = &self.consumers[name];
        if rt.gate.is_some() || rt.unconfirmed.len() >= self.capacity {
            return Ok(None);
        }
        self.tick(ctx);
        self.catch_up(name, ctx);
        let Some(p) = self
            .consumers
            .get_mut(name)
            .and_then(|rt| rt.queue.pop_front())
        else {
            return Ok(None);
        };
        let parts = self.deliveries(p, ctx);
        let rt = self.consumers.get_mut(name).expect("bound above");
        rt.parts.extend(parts);
        Ok(self.hand_out(name))
    }

    pub(super) fn hand_out(&mut self, name: &str) -> Option<Delivery> {
        let rt = self.consumers.get_mut(name)?;
        let (d, out) = rt.parts.pop_front()?;
        if let DeliveryKind::Resync(p) = &d.kind
            && p.last
        {
            rt.gate = Some(p.part);
        }
        rt.unconfirmed.push(out);
        Some(d)
    }

    pub(super) fn deliveries(&self, p: Pend, ctx: &Ctx<'_>) -> Vec<(Delivery, Out)> {
        let delivered_at = Timestamp(ctx.now);
        match p {
            Pend::Fact { rec, replayed } => {
                let unchecked = rec.unchecked;
                let position = rec.stored.position;
                let id = DeliveryId::Fact(pos(position));
                vec![(
                    Delivery {
                        id,
                        position: pos(position),
                        kind: DeliveryKind::Fact(fact_out(&rec.stored)),
                        flags: DeliveryFlags {
                            replayed,
                            future: rec.stored.row.flags.future,
                            outranked: rec.outranked,
                            unchecked,
                            ..DeliveryFlags::default()
                        },
                        delivered_at,
                    },
                    Out {
                        id,
                        position,
                        latches: Vec::new(),
                        forced: false,
                        reset: false,
                    },
                )]
            }
            Pend::Trigger {
                funnel,
                incarnation,
                sequence,
                state,
                cause,
                position,
                unchecked,
            } => {
                let id = DeliveryId::Condition {
                    funnel: FunnelId(funnel),
                    sequence,
                };
                vec![(
                    Delivery {
                        id,
                        position: pos(position),
                        kind: DeliveryKind::Trigger(Trigger {
                            funnel: FunnelId(funnel),
                            state: state_out(&state),
                            value: None,
                            coverage: None,
                            cause,
                        }),
                        flags: DeliveryFlags {
                            unchecked,
                            ..DeliveryFlags::default()
                        },
                        delivered_at,
                    },
                    Out {
                        id,
                        position,
                        latches: vec![(funnel, incarnation, state)],
                        forced: false,
                        reset: false,
                    },
                )]
            }
            Pend::Resync {
                cause,
                forget_above,
                position,
                funnels,
                reset,
                unchecked,
            } => {
                let latches: Vec<(u32, LogPos, MState)> = funnels
                    .iter()
                    .filter_map(|f| {
                        f.condition
                            .as_ref()
                            .map(|(s, _)| (f.funnel, f.incarnation, s.clone()))
                    })
                    .collect();
                let states: Vec<FunnelState> = funnels
                    .into_iter()
                    .flat_map(|f| {
                        let condition = f.condition.as_ref().map(|(s, _)| state_out(s));
                        let transition = f.condition.as_ref().is_some_and(|(_, t)| *t);
                        let funnel = FunnelId(f.funnel);
                        f.live.into_iter().map(move |(s, live)| FunnelState {
                            funnel,
                            series: crate::types::SeriesId(s),
                            live,
                            condition: condition.clone(),
                            transition,
                        })
                    })
                    .collect();
                let chunks: Vec<Vec<FunnelState>> = if states.is_empty() {
                    vec![Vec::new()]
                } else {
                    states
                        .chunks(RESYNC_PART_FUNNELS)
                        .map(<[FunnelState]>::to_vec)
                        .collect()
                };
                let n = chunks.len();
                chunks
                    .into_iter()
                    .enumerate()
                    .map(|(k, funnels)| {
                        let part = u32::try_from(k + 1).unwrap_or(u32::MAX);
                        let last = k + 1 == n;
                        let id = DeliveryId::Resync { part };
                        (
                            Delivery {
                                id,
                                position: pos(position),
                                kind: DeliveryKind::Resync(ResyncPart {
                                    cause,
                                    forget_above: forget_above.map(pos),
                                    part,
                                    last,
                                    funnels,
                                }),
                                flags: DeliveryFlags {
                                    unchecked,
                                    ..DeliveryFlags::default()
                                },
                                delivered_at,
                            },
                            Out {
                                id,
                                position,
                                latches: if last { latches.clone() } else { Vec::new() },
                                forced: last && forget_above.is_some(),
                                reset: last && reset,
                            },
                        )
                    })
                    .collect()
            }
        }
    }
}
