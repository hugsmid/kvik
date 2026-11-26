//! Taking a handle for a consumer (`docs/api.md` `Store::consumer`): the position before its start, a saved point
//! judged against the resets since, and how a new, a lost and a known consumer start.

use std::io;

use kvik_log::entries::Entry;
use kvik_log::writer::Position as LogPos;

use super::{Ctx, Deliverer, before, next};
use crate::backend::Error;
use crate::consume::{ResyncCause, Start};
use crate::types::SavedPoint;

impl Deliverer {
    /// The position before a start (`spec.md` §4 New consumer, Resync): the last position for `start: now`, the
    /// last position committed before `T` for `start: time` (the position before the first fact at or after `T`),
    /// and for `start: position` the saved point judged against the resets since it was read: kept, its position;
    /// void above X, X. An unknown point refuses the registration.
    pub(super) fn start_point(&self, start: Start, ctx: &Ctx<'_>) -> Result<LogPos, Error> {
        match start {
            Start::Now => Ok(self.head),
            Start::Time(t) => Ok(self.commit_at(t.0.saturating_sub(1), ctx).min(self.head)),
            Start::Position(p) => self.judge(p),
        }
    }

    /// A saved point judged against the current mark (`spec.md` §7 Readers, Reset stamp and saved points).
    fn judge(&self, point: SavedPoint) -> Result<LogPos, Error> {
        let p = crate::durable::index::log_pos(point.position);
        let current = self
            .reader
            .borrow()
            .as_ref()
            .map(|r| r.state().stamp)
            .unwrap_or_default();
        let saved = (point.stamp.sequence, point.stamp.nonce);
        let carries = |st: Option<kvik_log::mark::ResetStamp>, s: (u64, u64)| {
            st.is_some_and(|st| (st.sequence, st.nonce) == s)
        };
        if p.generation == self.head.generation && (current.sequence, current.nonce) == saved {
            return Ok(p.min(self.head));
        }
        let recoveries: Vec<(LogPos, &kvik_log::entries::Recovery)> = self
            .entries
            .iter()
            .filter_map(|(q, e)| match e {
                Entry::Recovery(r) => Some((*q, r)),
                _ => None,
            })
            .collect();
        // The reset the mark names must be kept, or nothing can be told.
        if current.nonce != 0
            && !recoveries
                .iter()
                .any(|(_, r)| carries(r.stamp, (current.sequence, current.nonce)))
        {
            return Err(Error::UnknownPoint);
        }
        let from = recoveries
            .iter()
            .position(|(_, r)| saved.1 != 0 && carries(r.stamp, saved))
            .map_or(0, |i| i + 1);
        let since = recoveries.get(from).map(|(q, _)| *q);
        let mut x = p;
        for (_, r) in &recoveries[from..] {
            x = x.min(LogPos {
                generation: p.generation,
                sequence: r.after,
            });
        }
        let lost: Vec<_> = self
            .reader
            .borrow()
            .as_ref()
            .map(|r| r.lost_ranges().to_vec())
            .unwrap_or_default();
        for l in lost {
            let first = LogPos {
                generation: l.first.0,
                sequence: l.first.1,
            };
            if first.generation == p.generation && since.is_none_or(|q| first >= before(q)) {
                x = x.min(before(first));
            }
        }
        Ok(x.min(self.head))
    }

    /// Takes a handle for a consumer (`docs/api.md` `Store::consumer`; Q-131, Q-95):
    /// - a new consumer starts after the position before its start, with no resync (it has nothing to forget), its
    ///   cursor file written at once naming that position; facts after it up to the last position are replayed;
    /// - one whose cursor file was missing or unreadable at the start is first sent a resync of cause `NoCursor`
    ///   whose `forget_above` is the position before its start, carrying each funnel's state (its latches are lost),
    ///   then continues from there;
    /// - one taken again in this run resumes from its confirmed cursor, and one with a cursor file as set up at the
    ///   start, whatever `start`; one whose `NoCursor` resync is not yet confirmed is sent it again.
    pub(crate) fn take(
        &mut self,
        name: &str,
        start: Start,
        ctx: &Ctx<'_>,
    ) -> Result<io::Result<()>, Error> {
        let new = match self.consumers.get(name) {
            Some(rt) if rt.handle => return Err(Error::ConsumerInUse(name.to_string())),
            Some(_) => false,
            None => {
                if self.consumers.len() >= self.max_consumers {
                    return Err(Error::TooManyConsumers(self.max_consumers));
                }
                true
            }
        };
        let head = self.head;
        let lost = !new && self.consumers[name].dormant;
        let point = if new || lost {
            Some(self.start_point(start, ctx)?)
        } else {
            None
        };
        let rt = self.consumers.entry(name.to_string()).or_default();
        let again = rt.attached.then_some(rt.forget).flatten();
        if rt.attached {
            // The consumer process restarted (Q-95 (b)): what it was handed is dropped and it resumes from its
            // confirmed cursor, with its latches, as after downtime.
            rt.queue.clear();
            rt.parts.clear();
            rt.unconfirmed.clear();
            rt.gate = None;
            rt.runners.clear();
            rt.emitted.clear();
            rt.evaluated.clear();
            rt.reset_open = false;
            rt.paused = false;
            rt.view = None;
            rt.behind = Some(next(rt.cursor));
            rt.resume = Some(head);
        }
        rt.handle = true;
        rt.attached = true;
        let Some(point) = point.or(again) else {
            return Ok(Ok(()));
        };
        rt.dormant = false;
        rt.resume = None;
        rt.cursor = point;
        rt.behind = (point < head).then(|| next(point));
        if lost || again.is_some() {
            rt.latches.clear();
            rt.forget = Some(point);
            self.issue_resync(name, ResyncCause::NoCursor, Some(point), point, false, ctx);
            return Ok(Ok(()));
        }
        rt.dirty = true;
        self.sync_runners(name, ctx.mono);
        Ok(self.write_now(name, ctx))
    }
}
