//! The commit clock: commit times as the writer assigns them, and the floor below which no later batch commits
//! (cond2 design §4.2). A leaf lock: nothing else is taken under it.

use std::sync::{Mutex, PoisonError};

use super::Position;

#[derive(Debug, Default)]
struct State {
    last_commit_time: i64,
    last_assigned: Position,
    /// The first position of a batch given a commit time and not yet committed or given up.
    in_flight: Option<Position>,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct CommitClock {
    state: Mutex<State>,
}

impl CommitClock {
    pub(super) fn new(last_commit_time: i64, last_assigned: Position) -> Self {
        CommitClock {
            state: Mutex::new(State {
                last_commit_time,
                last_assigned,
                in_flight: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `f = max(wall, last commit time)`, raising the clock to it, so every batch given a commit time later commits
    /// at or after `f`; with the position every batch committing below `f` is at or below: once a reader has taken
    /// the log through it, nothing with a commit time below `f` is still to come.
    pub fn floor(&self, wall: i64) -> (i64, Position) {
        let mut s = self.lock();
        let f = wall.max(s.last_commit_time);
        s.last_commit_time = f;
        (f, s.in_flight.unwrap_or(s.last_assigned))
    }

    /// Raises the clock to at least `t`: no batch commits below it after this returns (Q-1394).
    pub fn raise(&self, t: i64) {
        let mut s = self.lock();
        s.last_commit_time = s.last_commit_time.max(t);
    }

    /// The commit time of a batch starting at `first` that the writer would give it at `wall`; the batch is in
    /// flight until [`CommitClock::end`].
    pub(super) fn begin(&self, wall: i64, first: Position) -> i64 {
        let mut s = self.lock();
        let t = wall.max(s.last_commit_time);
        s.last_commit_time = t;
        s.in_flight = Some(first);
        t
    }

    /// The batch in flight committed through `last`, or (`None`) was given up.
    pub(super) fn end(&self, last: Option<Position>) {
        let mut s = self.lock();
        if let Some(l) = last {
            s.last_assigned = s.last_assigned.max(l);
        }
        s.in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(sequence: u64) -> Position {
        Position {
            generation: 1,
            sequence,
        }
    }

    #[test]
    fn a_floor_raises_the_clock_so_a_later_batch_commits_at_or_after_it_whatever_the_wall() {
        let c = CommitClock::new(100, p(5));
        assert_eq!(c.floor(250), (250, p(5)));
        assert_eq!(c.begin(120, p(6)), 250);
        c.end(Some(p(7)));
        assert_eq!(c.floor(90), (250, p(7)));
    }

    #[test]
    fn a_floor_taken_while_a_batch_is_in_flight_names_its_first_position() {
        let c = CommitClock::new(100, p(5));
        assert_eq!(c.begin(200, p(6)), 200);
        assert_eq!(c.floor(300), (300, p(6)));
        c.end(None);
        assert_eq!(c.floor(300), (300, p(5)));
    }

    #[test]
    fn raise_never_lowers_the_clock() {
        let c = CommitClock::new(100, p(5));
        c.raise(50);
        assert_eq!(c.floor(0).0, 100);
        c.raise(400);
        assert_eq!(c.floor(0).0, 400);
    }
}
