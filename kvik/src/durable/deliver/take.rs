//! Taking a handle for a consumer (`docs/api.md` `Store::consumer`): the position before its start, a saved point
//! judged against the resets since, and how a new, a lost and a known consumer start.

use std::io;

use kvik_log::writer::Position as LogPos;

use super::{Ctx, Deliverer, next};
use crate::backend::Error;
use crate::consume::{ResyncCause, Start};
use crate::durable::index::log_pos;
use crate::durable::saved;
use crate::types::{PointCheck, ResetStamp, SavedPoint};

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

    /// A saved point judged against the current mark (`spec.md` §7 Readers): kept, its position; void above X, X;
    /// both lowered to the last position (Q-1401). The same answer as `Reader::check`.
    fn judge(&self, point: SavedPoint) -> Result<LogPos, Error> {
        let reader = self.reader.borrow();
        let (current, lost) = reader
            .as_ref()
            .map_or((ResetStamp::default(), &[][..]), |r| {
                let st = r.state().stamp;
                (
                    ResetStamp {
                        sequence: st.sequence,
                        nonce: st.nonce,
                    },
                    r.lost_ranges(),
                )
            });
        let entries = self.entries.iter().map(|(p, e)| (*p, e));
        let x = match saved::check(point, self.head.generation, current, entries, lost) {
            PointCheck::Kept => log_pos(point.position),
            PointCheck::VoidAbove(x) => log_pos(x),
            PointCheck::Unknown => return Err(Error::UnknownPoint),
        };
        Ok(x.min(self.head))
    }

    /// Takes a handle for a consumer (`docs/api.md` `Store::consumer`; Q-131, Q-95):
    /// - a new consumer starts after the position before its start, with no resync (it has nothing to forget), its
    ///   cursor file written at once naming that position; facts after it up to the last position are replayed;
    /// - one whose cursor file was missing or unreadable at the start is first sent a resync of cause `NoCursor`
    ///   whose `forget_above` is the position before its start, carrying each funnel's state (its latches are lost),
    ///   then continues from there;
    /// - one taken again before it confirmed that resync is sent it again, with the same `forget_above` (Q-1450);
    /// - one taken again in this run resumes from its confirmed cursor, and one with a cursor file as set up at the
    ///   start, whatever `start`.
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
