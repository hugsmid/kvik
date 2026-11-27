//! Consumer starts (§4 New consumer, Resync; Q-131, Q-1403).

use super::{ConsumerId, Error, Mode, Model, ResyncCause, RouteChange, Selection};
use crate::recovery::{PointCheck, SavedPoint};
use crate::types::{Nanos, Position};

/// Where a consumer started at run time begins (§4 New consumer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// After the last committed position.
    Now,
    /// After the last position received before this time (Q-1403).
    Time(Nanos),
    /// After a saved point, judged by [`Model::check_point`].
    Point(SavedPoint),
}

impl Model {
    /// The position before a start (§4 Resync): every start is lowered to the last committed position (Q-1401).
    pub fn start_position(&self, start: Start) -> Result<Position, Error> {
        let v = self.vol()?;
        let head = self.head().unwrap_or(Position::ORIGIN);
        let p = match start {
            Start::Now => head,
            Start::Time(t) => v
                .entries
                .values()
                .filter(|e| e.received_at < t)
                .map(|e| e.position)
                .next_back()
                .unwrap_or(Position::ORIGIN),
            Start::Point(point) => match self.check_point(point) {
                PointCheck::Kept => point.position,
                PointCheck::VoidAbove(x) => x,
                PointCheck::Unknown => return Err(Error::UnknownPoint),
            },
        };
        Ok(p.min(head))
    }

    /// Whether the consumer's cursor file was lost and it waits to be started again.
    pub fn cursor_lost(&self, id: ConsumerId) -> bool {
        self.vol.as_ref().is_some_and(|v| {
            v.consumers
                .iter()
                .any(|c| c.decl.id == id && matches!(c.mode, Mode::Dormant))
        })
    }

    /// Starts a consumer after the position before `start`, which it returns, and writes its cursor at once.
    /// A new consumer, or a lost one no routing change names, is sent no `resync`; one whose cursor file was lost
    /// while routing changes name it is first sent a `resync` of cause `NoCursor` carrying `forget_above` = that
    /// position and every funnel's state, each condition a transition (§4 Resync, Q-131).
    pub fn start_consumer(&mut self, id: ConsumerId, start: Start) -> Result<Position, Error> {
        let p = self.start_position(start)?;
        let i = self.consumer_index(id).ok_or(Error::UnknownConsumer(id))?;
        let s = self.sessions.entry(id).or_insert(0);
        *s += 1;
        let session = *s;
        let v = self.vol.as_ref().expect("up");
        // A declaration that selects something, or a registration, names the consumer as a routing change does.
        let declared = match &v.consumers[i].decl.selection {
            Selection::All => true,
            Selection::Series(set) => !set.is_empty(),
        };
        let named = declared
            || self.disk.registered.contains(&id)
            || v.routes.values().any(|r| match r {
                RouteChange::Add { consumer, .. } | RouteChange::Remove { consumer, .. } => {
                    *consumer == id
                }
            });
        let lost = matches!(v.consumers[i].mode, Mode::Dormant) && named;
        self.restart_at(i, p, session);
        self.vol.as_mut().expect("up").consumers[i].started = lost.then_some(p);
        // Never ahead of the synced mark (invariant 26).
        self.disk.cover(p);
        let reach = self.disk.synced_mark.map_or(Position::ORIGIN, |m| m.last);
        // A lost cursor is written by the confirmation of its `NoCursor` resync; until then every start finds it lost.
        // A start the synced mark cannot reach writes none, as the durable backend (§4, Q-1621).
        if !lost && p <= reach {
            self.disk.cursors.insert(id, p);
        }
        self.disk.registered.insert(id);
        self.disk.latches.retain(|(c, _), _| *c != id);
        if lost {
            self.vol.as_mut().expect("up").consumers[i].awaiting = Some(p);
            self.issue_resync(i, p, None, Some(p), ResyncCause::NoCursor);
        } else {
            self.sync_runners(i);
        }
        Ok(p)
    }

    /// A lost consumer taken again before confirming its `NoCursor` resync: dropped back to the position before its
    /// start and sent the resync again, delivering nothing above it until confirmed (Q-1450).
    pub(super) fn send_no_cursor(&mut self, i: usize, p: Position) {
        let id = self.vol.as_ref().expect("up").consumers[i].decl.id;
        let s = self.sessions.entry(id).or_insert(0);
        *s += 1;
        let session = *s;
        self.restart_at(i, p, session);
        self.vol.as_mut().expect("up").consumers[i].awaiting = Some(p);
        self.issue_resync(i, p, None, Some(p), ResyncCause::NoCursor);
    }

    /// Drops what the consumer was handed and sets it after `p`: catch-up if anything follows, else live.
    fn restart_at(&mut self, i: usize, p: Position, session: u32) {
        let points = self.reset_points();
        let v = self.vol.as_mut().expect("up");
        let behind =
            v.entries.range(p.next()..).next().is_some() || points.iter().any(|r| r.id > p);
        let c = &mut v.consumers[i];
        c.queue.clear();
        c.unconfirmed.clear();
        c.runners.clear();
        c.emitted.clear();
        c.resume = None;
        c.awaiting = None;
        c.pending = None;
        c.session = session;
        c.last_received = p;
        c.mode = if behind {
            Mode::CatchUp { next: p.next() }
        } else {
            Mode::Live
        };
    }
}
