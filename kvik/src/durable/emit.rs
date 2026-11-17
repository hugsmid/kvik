//! Deciding and queueing a call of readings (`docs/api.md` Writing, Q-123 (b)).

use super::*;

impl Writer {
    /// Decides a call and queues it in the log. With `wait`, waits while the ingest queue is full; without, a
    /// wait is [`EmitError::Busy`] with nothing queued.
    pub(crate) fn emit(
        self: &Arc<Self>,
        readings: &[Reading],
        intent: Intent,
        wait: bool,
    ) -> Result<Emitted, EmitError> {
        self.writable().map_err(|e| e.emit(&self.dir))?;
        // Timers due run before a call is decided, as the model backend runs them.
        self.run_timers();
        let bytes: usize = readings
            .iter()
            .map(|r| {
                32 + match &r.value {
                    Value::Text(t) => t.len(),
                    _ => 8,
                }
            })
            .sum();
        let limit = CALL_READINGS.min(self.limits.ingest_queue);
        if readings.len() > limit || bytes > CALL_BYTES {
            return Err(EmitError::TooLarge {
                readings: readings.len(),
                bytes,
            });
        }
        let clock = ingest::Clock {
            wall: self.env.clock.wall().0,
            synced: self.env.clock.synchronised(),
            future_tolerance: convert::dur_nanos(self.limits.future_tolerance).unwrap_or(i64::MAX),
        };
        // Host-stamped readings wait until the clock is synchronised (spec §2.5).
        if !clock.synced && readings.iter().any(|r| r.source == TimeSource::Host) {
            return self.busy(Wait::UnsyncedClock);
        }
        let mut d = lock(&self.decider);
        let call = self.next_call.fetch_add(1, Ordering::SeqCst);
        // Calls below the oldest one in flight are acknowledged, so the log shows their facts to the slow path.
        let readable = {
            let mut inflight = lock(&self.inflight);
            inflight.retain(|_, c| !c.acknowledged());
            inflight.keys().next().copied().unwrap_or(call)
        };
        d.readable_below(readable);
        let mut plan = {
            let mut local = lock(&self.local);
            d.plan(readings, intent, clock, call, &mut **local)
        };
        let started = Instant::now();
        let refreshes = refreshing(&plan);
        let queued = if plan.items.is_empty() {
            None
        } else {
            // The log's read guard is dropped before an error is mapped, which reads the log's fault again.
            let r = {
                let log = self.log();
                let log = log.as_ref().ok_or(EmitError::Closed)?;
                let items = std::mem::take(&mut plan.items);
                if wait {
                    log.submit(items)
                } else {
                    log.try_submit(items)
                }
            };
            if wait {
                self.note_waited(started.elapsed());
            }
            match r {
                Ok(p) => Some(p),
                Err(SubmitError::Busy(w)) => {
                    drop(d);
                    return self.busy(wait_out(w));
                }
                Err(SubmitError::ReadOnly) => {
                    return Err(EmitError::ReadOnly(self.fault_now()));
                }
                Err(SubmitError::ShutDown) => return Err(EmitError::Closed),
                Err(SubmitError::Call(_)) => {
                    return Err(EmitError::TooLarge {
                        readings: readings.len(),
                        bytes,
                    });
                }
            }
        };
        debug_assert!(
            plan.items.is_empty(),
            "the planned facts are moved into the log"
        );
        d.apply(&mut plan);
        // The call is tracked before the decision lock is released, so a later call that duplicates one of its
        // readings always finds it (lock order: decisions, then calls in flight).
        let mut inflight = lock(&self.inflight);
        // Calls known to be acknowledged need no tracking: a later outcome resting on them is final.
        inflight.retain(|_, c| !c.acknowledged());
        let tracked = match queued {
            Some(pending) => {
                let (clocks, ids) = (self.clocks.clone(), refreshes);
                let c = Completion::new(pending, Box::new(move || clocks.refresh(&ids)));
                inflight.insert(call, c.clone());
                Some((c, true))
            }
            // Nothing to record. A duplicate of a reading queued by an earlier call that is not yet acknowledged
            // resolves when that call does, with its result (Q-123 (b)): an acknowledgement means durable.
            None => plan
                .depends
                .and_then(|c| inflight.get(&c).cloned())
                .map(|c| (c, false)),
        };
        drop(inflight);
        drop(d);
        #[cfg(test)]
        pause::at(&self.dir, pause::Point::AfterDecision);
        self.count(readings, &plan);
        let decisions = plan.decisions;
        let Some((completion, own)) = tracked else {
            return Ok(Emitted::Ready(Ack::new(outcomes(&decisions, None))));
        };
        let me = self.clone();
        Ok(Emitted::Queued(Queued {
            completion,
            finish: Box::new(move |r| {
                if own {
                    me.note_latency(started.elapsed());
                }
                if r.is_ok() {
                    #[cfg(test)]
                    pause::at(&me.dir, pause::Point::AfterAck);
                    me.deliver();
                }
                match r {
                    Ok(ack) if own => Ok(Ack::new(outcomes(&decisions, Some(ack.first)))),
                    Ok(_) => Ok(Ack::new(outcomes(&decisions, None))),
                    Err(CommitError::ShutDown) => Err(EmitError::Closed),
                    Err(CommitError::ReadOnly) => Err(EmitError::ReadOnly(me.fault_now())),
                    Err(CommitError::OutcomeUnknown) => {
                        Err(EmitError::OutcomeUnknown(me.fault_now()))
                    }
                }
            }),
        }))
    }

    /// The wait a call meets.
    fn busy(&self, w: Wait) -> Result<Emitted, EmitError> {
        Err(EmitError::Busy(w))
    }

    /// Counts a call that waits (`IngestStats::waits`), once per call.
    pub(crate) fn count_wait(&self, w: Wait) {
        *lock(&self.counters).waits.entry(w).or_insert(0) += 1;
    }

    /// Adds the time a queued call spent waiting for room in the ingest queue.
    fn note_waited(&self, d: Duration) {
        // A submit that found room at once did not wait.
        if d > Duration::from_millis(1) {
            lock(&self.counters).waited += d;
        }
    }

    fn note_latency(&self, d: Duration) {
        let mut c = lock(&self.counters);
        if c.latencies.len() == LATENCIES {
            c.latencies.pop_front();
        }
        c.latencies.push_back(d);
        c.max_latency = c.max_latency.max(d);
    }

    fn fault_now(&self) -> Fault {
        self.fault().map_or(
            Fault::SyncFailed {
                path: self.dir.clone(),
            },
            |f| fault_out(&f),
        )
    }

    fn count(&self, readings: &[Reading], plan: &ingest::Plan) {
        let mut c = lock(&self.counters);
        c.slow_path += plan.slow;
        let mut c_unchecked = 0;
        for (r, d) in readings.iter().zip(&plan.decisions) {
            let st = c.series.entry(r.series.0).or_default();
            match d {
                Decision::Refused(x) => *st.refused.entry(*x).or_insert(0) += 1,
                Decision::Duplicate => st.duplicates_dropped += 1,
                Decision::Record {
                    outranked,
                    absent,
                    late,
                    future,
                    unchecked,
                    ..
                } => {
                    st.recorded += 1;
                    st.duplicates_unchecked += u64::from(*unchecked);
                    c_unchecked += u64::from(*unchecked);
                    st.outranked_on_arrival += u64::from(*outranked);
                    st.absent_supplied += u64::from(*absent);
                    st.late += u64::from(*late);
                    st.future += u64::from(*future);
                }
            }
        }
        c.unchecked_readings += c_unchecked;
    }

    // ----- health and statistics -----
}

/// One outcome per reading; positions follow the call's first, each recorded fact taking one.
fn outcomes(decisions: &[Decision], first: Option<kvik_log::writer::Position>) -> Vec<Outcome> {
    decisions
        .iter()
        .map(|d| match d {
            Decision::Refused(x) => Outcome::Refused(*x),
            Decision::Duplicate => Outcome::Duplicate,
            Decision::Record {
                item,
                outranked,
                unchecked,
                ..
            } => {
                let f = first.unwrap_or_default();
                Outcome::Recorded {
                    position: Position::new(f.generation, f.sequence + *item as u64),
                    outranked: *outranked,
                    unchecked: *unchecked,
                    rounded: false,
                    out_of_range: false,
                }
            }
        })
        .collect()
}

/// The series whose live staleness a call refreshes when the log acknowledges it, after the covering sync, whether
/// or not the caller waits (spec §5.4): those of its facts that are recorded, not outranked, not bad quality and not
/// future.
fn refreshing(plan: &ingest::Plan) -> Vec<u32> {
    plan.decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Record {
                item,
                outranked: false,
                ..
            } => match plan.items.get(*item) {
                Some(Item::Fact(row))
                    if row.quality != kvik_log::facts::Quality::Bad
                        && !matches!(row.value, kvik_log::facts::Value::Absent { .. })
                        && !row.flags.future =>
                {
                    Some(row.series_id)
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}
