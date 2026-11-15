//! Condition funnels (`spec.md` §6 Conditions, Cadence, Restart and resume): runners of the reference model's
//! conditions fed with the live state of roadmap 5.4, evaluated on each taken batch whose series they watch and on
//! their timers, and the condition deliveries they emit.

use std::collections::BTreeMap;
use std::ops::Bound;

use kvik_log::entries::{RouteCondition, Versioning};
use kvik_log::facts::Value as LogValue;
use kvik_log::writer::Position as LogPos;
use kvik_model::condition::{ConditionState as MState, ResumeHistory, Runner};

use super::{Ctx, Deliverer, Pend, next};
use crate::consume::TriggerCause;
use crate::durable::index::Stored;
use crate::durable::routing::{FunnelRec, model_condition, numeric};
use crate::durable::windows::live_fact;
use crate::read::Live;
use crate::series::Timing;
use crate::types::SeriesId;

fn number(s: &Stored) -> Option<f64> {
    match s.row.value {
        LogValue::F64(bits) => Some(f64::from_bits(bits)),
        _ => None,
    }
}

impl Deliverer {
    /// Creates a runner for each condition funnel in force that has none and drops those of funnels no longer in
    /// force. A consumer resuming gets its runners with its resync.
    pub(super) fn sync_runners(&mut self, name: &str, mono: i64) {
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.resume.is_some() || rt.dormant {
            return;
        }
        let funnels: BTreeMap<u32, &FunnelRec> = self.view.funnels(name).collect();
        rt.runners.retain(|f, _| funnels.contains_key(f));
        rt.emitted.retain(|f, _| funnels.contains_key(f));
        rt.evaluated.retain(|f, _| funnels.contains_key(f));
        for (id, f) in funnels {
            if let (Some(c), Some(s)) = (&f.condition, f.condition_series())
                && !rt.runners.contains_key(&id)
            {
                let r = Runner::new(model_condition(c, &self.view.registry, s), mono);
                rt.emitted.insert(id, r.state());
                rt.runners.insert(id, r);
            }
        }
    }

    /// The value a condition sees now: the live-state value of its series (`spec.md` §2.3), for a numeric series.
    pub(super) fn value_of(&self, s: u32, now: i64) -> Option<f64> {
        if !numeric(&self.view.registry, s) {
            return None;
        }
        live_fact(&self.index, SeriesId(s), now)
            .ok()
            .flatten()
            .and_then(number)
    }

    /// For a stale-for condition over a scheduled series, `now − t` of its live-state fact (`None` without one).
    pub(super) fn scheduled_since(
        &self,
        c: &RouteCondition,
        s: u32,
        now: i64,
    ) -> Option<Option<i64>> {
        let scheduled = self
            .view
            .registry
            .get(s)
            .is_some_and(|x| x.spec.timing == Timing::Scheduled);
        (scheduled && matches!(c, RouteCondition::StaleFor { .. })).then(|| {
            live_fact(&self.index, SeriesId(s), now)
                .ok()
                .flatten()
                .map(|x| now - x.row.t)
        })
    }

    /// Whether a funnel's live-state value may have changed since `since` without a new fact: a fact of its series
    /// that was not yet in force then (a future fact, a scheduled value, an observed interval not yet ended).
    fn may_mature(&self, s: u32, since: i64) -> bool {
        let Some(series) = self.view.registry.get(s) else {
            return false;
        };
        let length = match (&series.spec.timing, &series.def.interval) {
            (Timing::Observed, Some(kvik_log::entries::IntervalLength::Fixed { length, .. })) => {
                *length
            }
            _ => 0,
        };
        let versioned = series.def.versioning != Versioning::None;
        self.index
            .current_between(s, versioned, Bound::Unbounded, Bound::Unbounded, true)
            .next()
            .is_some_and(|f| f.row.t.saturating_add(length) > since)
    }

    /// What a resume rebuilds of a condition's past (`spec.md` §6 Restart and resume), as the reference model does.
    pub(super) fn resume_history(
        &self,
        c: &RouteCondition,
        s: u32,
        ctx: &Ctx<'_>,
    ) -> ResumeHistory {
        let mut h = ResumeHistory::default();
        match c {
            RouteCondition::HeldFor { above, .. } => {
                let versioned = self
                    .view
                    .registry
                    .get(s)
                    .is_some_and(|x| x.def.versioning != Versioning::None);
                let mut start = None;
                for f in self.index.current_between(
                    s,
                    versioned,
                    Bound::Unbounded,
                    Bound::Included(ctx.now),
                    true,
                ) {
                    if f.row.quality == kvik_log::facts::Quality::Bad || f.no_value() {
                        continue;
                    }
                    if number(f).is_some_and(|v| v > *above) {
                        start = Some(f.row.t);
                    } else {
                        break;
                    }
                }
                h.above_for = start.map_or(0, |t| ctx.now - t);
            }
            RouteCondition::StaleFor { .. } => {
                h.since_arrival = self.last_arrival.get(&s).map(|r| (ctx.wall - r).max(0));
            }
            RouteCondition::Threshold { .. } | RouteCondition::OnChange { .. } => {}
        }
        h
    }

    /// Live state (`spec.md` §2.3, §5.4) of a series for a resync.
    pub(super) fn live_of(&self, s: u32, ctx: &Ctx<'_>) -> Live {
        crate::durable::windows::live_in(&self.index, ctx.clocks, SeriesId(s), ctx.now)
            .unwrap_or(Live::None)
    }

    /// Evaluates a consumer's condition funnels: on a taken batch, those whose series has a fact fed in it; on the
    /// timer, those whose timer is due (held for, stale for, or a value that may have come into force since the
    /// funnel was last evaluated), which is every evaluation that can change a state. Replayed facts never reach a
    /// condition (`spec.md` §4 Replay).
    pub(super) fn evaluate(&mut self, name: &str, batch: Option<&[u32]>, ctx: &Ctx<'_>) {
        match self.consumers.get(name) {
            Some(rt) if rt.resume.is_none() && !rt.dormant => {}
            _ => return,
        }
        self.sync_runners(name, ctx.mono);
        let evaluated = self
            .consumers
            .get(name)
            .map(|rt| rt.evaluated.clone())
            .unwrap_or_default();
        let mut work = Vec::new();
        for (id, f) in self.view.funnels(name) {
            let (Some(c), Some(s)) = (&f.condition, f.condition_series()) else {
                continue;
            };
            let arrived = batch.map(|b| b.contains(&s));
            match arrived {
                Some(false) => continue,
                None => {
                    let timed = matches!(
                        c,
                        RouteCondition::HeldFor { .. } | RouteCondition::StaleFor { .. }
                    );
                    let since = evaluated.get(&id).copied().unwrap_or(i64::MIN);
                    if !timed && !self.may_mature(s, since) {
                        continue;
                    }
                }
                Some(true) => {}
            }
            work.push((
                id,
                arrived,
                self.value_of(s, ctx.now),
                self.scheduled_since(c, s, ctx.now),
            ));
        }
        let cause = if batch.is_some() {
            TriggerCause::Fact
        } else {
            TriggerCause::Timer
        };
        for (id, arrived, value, scheduled) in work {
            let Some(rt) = self.consumers.get_mut(name) else {
                continue;
            };
            rt.evaluated.insert(id, ctx.now);
            let Some(runner) = rt.runners.get_mut(&id) else {
                continue;
            };
            let mut events = Vec::new();
            if let Some(since) = scheduled {
                events.extend(runner.on_scheduled(since));
            } else {
                if arrived == Some(true) {
                    events.extend(runner.on_arrival(ctx.mono));
                }
                events.extend(runner.on_value(ctx.mono, value));
                if batch.is_none() {
                    events.extend(runner.on_tick(ctx.mono));
                }
            }
            for e in events {
                self.emit_trigger(name, id, e.state(), cause);
            }
        }
    }

    /// Queues a condition delivery with the funnel's next sequence and the last position taken. A consumer that
    /// is behind is not sent it; a full queue switches it to catch-up (`spec.md` §4 Overflow).
    pub(super) fn emit_trigger(
        &mut self,
        name: &str,
        funnel: u32,
        state: MState,
        cause: TriggerCause,
    ) {
        let head = self.head;
        let incarnation = self
            .view
            .funnels(name)
            .find(|(id, _)| *id == funnel)
            .map_or(LogPos::default(), |(_, f)| f.incarnation);
        let Some(rt) = self.consumers.get_mut(name) else {
            return;
        };
        if rt.behind.is_some() {
            return;
        }
        if rt.queue.len() >= self.capacity {
            rt.behind = Some(next(head));
            return;
        }
        let seq = self
            .sequences
            .entry((name.to_string(), funnel))
            .or_insert(0);
        *seq += 1;
        rt.queue.push_back(Pend::Trigger {
            funnel,
            incarnation,
            sequence: *seq,
            state: state.clone(),
            cause,
            position: head,
        });
        rt.emitted.insert(funnel, state);
        self.notify = true;
    }

    /// Runs the timers of every condition funnel that is due (`spec.md` §4 Timers, §6 Cadence).
    pub(crate) fn tick(&mut self, ctx: &Ctx<'_>) {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.evaluate(&n, None, ctx);
        }
    }
}
