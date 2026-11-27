//! Confirmations and cursor files (`spec.md` §4 Consumer state, At-least-once; `format.md` A.15): a confirmation
//! moves the cursor and the latches; the cursor file holds them with the last reset point confirmed.
//!
//! A cursor file is written when a confirmation changes a latch's state or confirms a resync carrying
//! `forget_above` (at once, before anything further is delivered), otherwise at most once a second (at a
//! confirmation, and by the `kvik-deliver` thread), and at clean shutdown. It never names a position above the last
//! synced mark: a write that would forces a mark sync first, and is skipped if the mark could not be synced past it.
//! One failed write or sync is never retried: the store is read-only from then on (`spec.md` §3).

use std::collections::BTreeMap;
use std::io;

use kvik_log::cursor::{self, CursorState, Latch, LatchState};
use kvik_log::writer::Position as LogPos;
use kvik_model::condition::ConditionState as MState;

use super::{Ctx, Deliverer, Out, SECOND};
use crate::backend::Error;
use crate::consume::{ConditionState, DeliveryId};

pub(super) fn state_in(l: LatchState) -> MState {
    match l {
        LatchState::Active(b) => MState::Active(b),
        LatchState::Value(v) => MState::Value(v),
    }
}

fn state_log(s: &MState) -> LatchState {
    match s {
        MState::Active(b) => LatchState::Active(*b),
        MState::Value(v) => LatchState::Value(*v),
    }
}

pub(super) fn state_out(s: &MState) -> ConditionState {
    match s {
        MState::Active(true) => ConditionState::True,
        MState::Active(false) => ConditionState::False,
        MState::Value(v) => ConditionState::Value(*v),
    }
}

/// When a cursor file write is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// At once.
    Now,
    /// If a second has passed since the last write.
    Timed,
    /// At once, the last write before a clean shutdown: it carries a checkpoint if the gates moved.
    Last,
}

impl Deliverer {
    /// Confirms a delivery and every delivery handed out before it (`docs/api.md` `Consumer::confirm`): the cursor
    /// moves to the highest position among them and each condition state they carry becomes its funnel's latch. A
    /// resync is confirmed by its last part.
    pub(crate) fn confirm(
        &mut self,
        name: &str,
        id: DeliveryId,
        ctx: &Ctx<'_>,
    ) -> Result<io::Result<()>, Error> {
        self.bound(name)?;
        self.writable()?;
        let rt = self.consumers.get_mut(name).expect("bound above");
        if let DeliveryId::Resync { part } = id {
            if rt.gate != Some(part) {
                return Ok(Ok(()));
            }
            rt.gate = None;
        }
        let n = rt
            .unconfirmed
            .iter()
            .rposition(|o| o.id == id)
            .ok_or(Error::BadConfirm)?
            + 1;
        let done: Vec<Out> = rt.unconfirmed.drain(..n).collect();
        let gate = done.iter().filter_map(|o| o.gate).next_back();
        // The condition funnels in force, only when a confirmed delivery carries a condition state.
        let funnels: BTreeMap<u32, LogPos> = if done.iter().any(|o| !o.latches.is_empty()) {
            self.view
                .funnels(name)
                .filter(|(_, f)| f.condition.is_some())
                .map(|(id, f)| (id, f.incarnation))
                .collect()
        } else {
            BTreeMap::new()
        };
        let mut forced = false;
        let mut latch_changed = false;
        for o in &done {
            for (f, inc, st) in &o.latches {
                if funnels.get(f) == Some(inc) {
                    let state = state_log(st);
                    latch_changed |= rt
                        .latches
                        .get(f)
                        .is_none_or(|l| l.incarnation != *inc || l.state != state);
                    rt.latches.insert(
                        *f,
                        Latch {
                            funnel: *f,
                            incarnation: *inc,
                            emitted: o.position,
                            state,
                        },
                    );
                }
            }
            if o.reset {
                rt.reset = Some(o.position);
            }
            forced |= o.forced;
            if o.forced {
                rt.forget = None;
            }
            rt.cursor = rt.cursor.max(o.position);
        }
        rt.dirty = true;
        rt.gate_dirty |= gate.is_some();
        let transition = matches!(gate, Some(super::gates::GatePoint::Transition(_)));
        if let Some(g) = gate {
            self.gate_confirm(name, g);
        }
        // Only this consumer's own poll can wait on what a confirm frees: room in its queue or the end of a pause.
        self.changed(name);
        if forced {
            // Nothing further is delivered until the cursor file naming the resync's id is synced.
            let r = self.write_cursor(name, Due::Now, ctx);
            if let (Ok(true), Some(rt)) = (&r, self.consumers.get_mut(name)) {
                rt.paused = false;
                rt.reset_open = false;
            }
            return Ok(r.map(|_| ()));
        }
        // A latch whose state changed, or a confirmed gate transition, is written at once (Q-132, spec §6 Gates),
        // anything else at most once a second.
        let due = if latch_changed || transition {
            Due::Now
        } else {
            Due::Timed
        };
        Ok(self.write_cursor(name, due, ctx).map(|_| ()))
    }

    /// Writes a consumer's cursor file if its state changed and the write is due. Returns whether it was written.
    fn write_cursor(&mut self, name: &str, due: Due, ctx: &Ctx<'_>) -> io::Result<bool> {
        if self.failed {
            return Err(io::Error::other("a cursor file write failed before"));
        }
        let fresh = self.checkpoint_wanted(name, due == Due::Last, ctx.mono);
        let Some(rt) = self.consumers.get_mut(name) else {
            return Ok(false);
        };
        if !rt.dirty && !fresh {
            return Ok(true);
        }
        if due == Due::Timed && rt.last_write.is_some_and(|t| ctx.mono - t < SECOND) {
            return Ok(false);
        }
        let checkpoint = if fresh {
            self.checkpoint_content(name)
        } else {
            None
        };
        let rt = self.consumers.get_mut(name).expect("looked up above");
        let funnels: BTreeMap<u32, LogPos> = self
            .view
            .funnels(name)
            .map(|(id, f)| (id, f.incarnation))
            .collect();
        let state = CursorState {
            consumer: name.to_string(),
            position: rt.cursor,
            latches: rt
                .latches
                .values()
                .filter(|l| funnels.get(&l.funnel) == Some(&l.incarnation))
                .copied()
                .collect(),
            reset: rt.reset,
            gate: match &rt.gates {
                Some(run) => Some(super::gate_resume::cursor_out(run.confirmed)),
                None => rt.gate_saved,
            },
        };
        let highest = checkpoint
            .as_ref()
            .map_or(state.highest(), |c| c.highest().max(state.highest()));
        // A cursor file never names a position above the last synced mark (`spec.md` §3 Durable mark): a write that
        // would forces a mark sync, and is skipped unless the mark synced reaches it (a writer that has stopped
        // syncs no more).
        if let Some(w) = ctx.watch
            && highest > w.stats().mark_synced
        {
            match w.sync_mark() {
                Ok(synced) if synced >= highest => {}
                Ok(_) => return Ok(false),
                Err(f) => {
                    self.failed = true;
                    return Err(io::Error::other(f.to_string()));
                }
            }
        }
        let rt = self.consumers.get_mut(name).expect("looked up above");
        let rec = cursor::Record {
            state: &state,
            checkpoint: checkpoint.as_ref().or(rt.checkpoint.as_ref()),
            fresh: checkpoint.is_some(),
        };
        let end = match cursor::append_record(
            &*ctx.env.fs,
            ctx.dir,
            self.store_id,
            rt.cursor.generation,
            ctx.wall,
            rec,
            rt.file_end,
        ) {
            Ok(end) => end,
            Err(e) => {
                self.failed = true;
                return Err(e);
            }
        };
        rt.file_end = Some(end);
        if checkpoint.is_some() {
            rt.checkpoint = checkpoint;
            rt.checkpoint_at = Some(ctx.mono);
            rt.gate_dirty = false;
        }
        rt.dirty = false;
        rt.last_write = Some(ctx.mono);
        rt.has_cursor = true;
        Ok(true)
    }

    /// Writes a consumer's cursor file at once if its state changed.
    pub(super) fn write_now(&mut self, name: &str, ctx: &Ctx<'_>) -> io::Result<()> {
        self.write_cursor(name, Due::Now, ctx).map(|_| ())
    }

    /// Writes the cursor files that are due (at most once a second each).
    pub(crate) fn flush_due(&mut self, ctx: &Ctx<'_>) -> io::Result<()> {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.write_cursor(&n, Due::Timed, ctx)?;
        }
        Ok(())
    }

    /// Writes every cursor file whose state changed (clean shutdown, after the last mark sync).
    pub(crate) fn flush_all(&mut self, ctx: &Ctx<'_>) -> io::Result<()> {
        let names: Vec<String> = self.consumers.keys().cloned().collect();
        for n in names {
            self.write_cursor(&n, Due::Last, ctx)?;
        }
        Ok(())
    }
}
