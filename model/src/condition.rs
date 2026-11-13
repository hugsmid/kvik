//! Conditions on an operator's output (§6) driven by an injected monotonic
//! clock.
//!
//! A runner is fed the operator value whenever it is evaluated
//! ([`Runner::on_value`]), series arrivals ([`Runner::on_arrival`]), and clock
//! ticks ([`Runner::on_tick`]). All durations are monotonic nanoseconds; the
//! wall clock never reaches a runner, so system clock corrections cannot fire or
//! suppress a timer. A runner acts only on what it is told when it is told: it
//! cannot fire retroactively.

use crate::types::Nanos;

/// A condition.
#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    /// Becomes active when the value rises above `on`; clears only when it
    /// falls below `clear` (`clear ≤ on`).
    Threshold {
        /// Activation level.
        on: f64,
        /// Clear level.
        clear: f64,
    },
    /// Fires when the value differs from the last fired value by more than the
    /// dead-band (always fires on the first value).
    OnChange {
        /// Dead-band.
        deadband: f64,
    },
    /// Fires once the value has been above `above` continuously for `n`
    /// monotonic nanoseconds; clears when it is no longer above.
    HeldFor {
        /// Level.
        above: f64,
        /// Duration.
        n: Nanos,
    },
    /// Fires once no reading has arrived for `staleness + n` monotonic
    /// nanoseconds; clears on the next arrival.
    StaleFor {
        /// The series' staleness limit.
        staleness: Nanos,
        /// Extra duration.
        n: Nanos,
    },
}

/// Something a runner emits.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The condition became true.
    Fired,
    /// The condition cleared.
    Cleared,
    /// On-change delivery of a new value.
    Changed(f64),
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Threshold { active: bool },
    OnChange { reference: Option<f64> },
    HeldFor { since: Option<Nanos>, fired: bool },
    StaleFor { last_arrival: Nanos, fired: bool },
}

/// A running condition.
#[derive(Clone, Debug, PartialEq)]
pub struct Runner {
    /// The condition.
    pub cond: Condition,
    state: State,
}

impl Runner {
    /// A fresh runner at monotonic time `mono`.
    pub fn new(cond: Condition, mono: Nanos) -> Self {
        let state = match &cond {
            Condition::Threshold { .. } => State::Threshold { active: false },
            Condition::OnChange { .. } => State::OnChange { reference: None },
            Condition::HeldFor { .. } => State::HeldFor {
                since: None,
                fired: false,
            },
            Condition::StaleFor { .. } => State::StaleFor {
                last_arrival: mono,
                fired: false,
            },
        };
        Runner { cond, state }
    }

    /// Rebuilds a runner after a restart from what the tree knows (§6 "Restart"):
    /// `history` is the operator's value sequence up to now (oldest first) and
    /// `now_value` its value now. A condition that is true now is treated as
    /// having fired already, so it does not fire again until it clears
    /// The specification replaces this with a persisted latch and `resync`
    /// (§6); see "Where the model differs" in `OPEN-QUESTIONS.md`.
    pub fn rebuild(cond: Condition, mono: Nanos, history: &[f64], now_value: Option<f64>) -> Self {
        let state = match &cond {
            Condition::Threshold { on, clear } => {
                // Hysteresis needs the last crossing: scan the whole history.
                let mut active = false;
                for &v in history.iter().chain(now_value.iter()) {
                    if !active && v > *on {
                        active = true;
                    } else if active && v < *clear {
                        active = false;
                    }
                }
                State::Threshold { active }
            }
            Condition::OnChange { .. } => State::OnChange {
                reference: now_value,
            },
            Condition::HeldFor { above, .. } => {
                let held = now_value.is_some_and(|v| v > *above);
                State::HeldFor {
                    since: held.then_some(mono),
                    fired: held,
                }
            }
            Condition::StaleFor { .. } => State::StaleFor {
                last_arrival: mono,
                fired: false,
            },
        };
        Runner { cond, state }
    }

    /// Whether the condition is currently active (fired and not cleared).
    pub fn active(&self) -> bool {
        match &self.state {
            State::Threshold { active } => *active,
            State::OnChange { .. } => false,
            State::HeldFor { fired, .. } | State::StaleFor { fired, .. } => *fired,
        }
    }

    /// Feeds the operator value at monotonic time `mono`.
    pub fn on_value(&mut self, mono: Nanos, value: Option<f64>) -> Vec<Event> {
        let mut out = Vec::new();
        match (&self.cond, &mut self.state) {
            (Condition::Threshold { on, clear }, State::Threshold { active }) => {
                if let Some(v) = value {
                    if !*active && v > *on {
                        *active = true;
                        out.push(Event::Fired);
                    } else if *active && v < *clear {
                        *active = false;
                        out.push(Event::Cleared);
                    }
                }
            }
            (Condition::OnChange { deadband }, State::OnChange { reference }) => {
                if let Some(v) = value
                    && reference.is_none_or(|r| (v - r).abs() > *deadband)
                {
                    *reference = Some(v);
                    out.push(Event::Changed(v));
                }
            }
            (Condition::HeldFor { above, .. }, State::HeldFor { since, fired }) => {
                if value.is_some_and(|v| v > *above) {
                    if since.is_none() {
                        *since = Some(mono);
                    }
                } else {
                    *since = None;
                    if *fired {
                        *fired = false;
                        out.push(Event::Cleared);
                    }
                }
                out.extend(self.on_tick(mono));
            }
            (Condition::StaleFor { .. }, _) => {}
            _ => unreachable!("state matches condition"),
        }
        out
    }

    /// Records that a reading arrived at monotonic time `mono`.
    pub fn on_arrival(&mut self, mono: Nanos) -> Vec<Event> {
        if let State::StaleFor {
            last_arrival,
            fired,
        } = &mut self.state
        {
            *last_arrival = mono;
            if *fired {
                *fired = false;
                return vec![Event::Cleared];
            }
        }
        Vec::new()
    }

    /// Advances timers to monotonic time `mono`.
    pub fn on_tick(&mut self, mono: Nanos) -> Vec<Event> {
        match (&self.cond, &mut self.state) {
            (Condition::HeldFor { n, .. }, State::HeldFor { since, fired }) => {
                if let Some(s) = since
                    && !*fired
                    && mono - *s >= *n
                {
                    *fired = true;
                    return vec![Event::Fired];
                }
            }
            (
                Condition::StaleFor { staleness, n },
                State::StaleFor {
                    last_arrival,
                    fired,
                },
            ) if !*fired && mono - *last_arrival >= staleness + n => {
                *fired = true;
                return vec![Event::Fired];
            }
            _ => {}
        }
        Vec::new()
    }
}
