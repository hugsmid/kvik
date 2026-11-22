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
        let started = Instant::now();
        // Decided and queued in call order under the decision lock; a full queue is waited for with no lock held,
        // and the call decided again.
        let mut provisional = None;
        let (mut d, call, mut plan, refreshes, queued) = loop {
            let mut d = lock(&self.decider);
            let call = self.next_call.fetch_add(1, Ordering::SeqCst);
            self.settle(&mut d, call);
            let mut plan = {
                let mut local = lock(&self.local);
                d.plan(readings, intent, clock, call, &mut **local)
            };
            if plan.items.is_empty() {
                break (d, call, plan, Vec::new(), None);
            }
            let refreshes = refreshing(&plan);
            // The log's read guard is dropped before an error is mapped, which reads the log's fault again.
            // The facts are moved into the log; after a wait for room the call is planned again.
            let len = plan.items.len();
            let r = {
                let log = self.log();
                let log = log.as_ref().ok_or(EmitError::Closed)?;
                provisional = fast::facts_of(&self.fast, &plan.items, clock.wall)
                    .map(|f| (f, log.watch().durable()));
                let items = std::mem::take(&mut plan.items);
                if wait {
                    log.offer(items)
                } else {
                    log.try_submit(items)
                }
            };
            let e = match r {
                Ok(p) => break (d, call, plan, refreshes, Some(p)),
                Err(SubmitError::Busy(LogWait::QueueFull)) if wait => {
                    drop(d);
                    #[cfg(test)]
                    pause::at(&self.dir, pause::Point::WaitingForRoom);
                    match self.wait_room(len) {
                        Ok(()) => continue,
                        Err(e) => e,
                    }
                }
                Err(e) => e,
            };
            return match e {
                SubmitError::Busy(w) => self.busy(wait_out(w)),
                SubmitError::ReadOnly => Err(EmitError::ReadOnly(self.fault_now())),
                SubmitError::ShutDown => Err(EmitError::Closed),
                SubmitError::Call(_) => Err(EmitError::TooLarge {
                    readings: readings.len(),
                    bytes,
                }),
            };
        };
        if wait {
            self.note_waited(started.elapsed());
        }
        d.apply(&mut plan);
        // The call is tracked before the decision lock is released, so a later call that duplicates one of its
        // readings always finds it (lock order: decisions, then calls in flight).
        #[cfg(test)]
        tests::before_tracking(&self.dir);
        let mut inflight = lock(&self.inflight);
        let tracked = match queued {
            Some(pending) => {
                let (clocks, ids) = (self.clocks.clone(), refreshes);
                let c = Completion::new(pending, Box::new(move || clocks.refresh(&ids)));
                inflight.insert(call, c.clone());
                if let Some((facts, floor)) = provisional.take() {
                    self.fast.publish(facts, floor, c.clone());
                }
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

    /// Takes into the decisions the calls acknowledged since the last decision, in call order up to the first still
    /// pending, and the ranges the scanning readers of this process found lost (Q-923); `next` is the call being
    /// decided. Calls known to be acknowledged need no tracking: a later outcome resting on them is final.
    fn settle(&self, d: &mut Decider, next: u64) {
        let mut inflight = lock(&self.inflight);
        let acks = inflight.iter().map(|(call, c)| {
            let ack = match c.poll(None) {
                Poll::Ready(Ok(a)) => Some((a.first, a.last)),
                _ => None,
            };
            (*call, ack)
        });
        let seen = d.settle(acks, next, &self.latch.take_fresh());
        for call in seen {
            inflight.remove(&call);
        }
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
        c.unchecked_readings += c_unchecked + plan.reference_unread;
    }
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use super::{Emitted, Writer};
    use crate::config::{LogConfig, StoreConfig};
    use crate::env::{Env, ManualClock, MemFs};
    use crate::error::EmitError;
    use crate::series::{Kind, SeriesSpec, ValueType};
    use crate::types::{Intent, Reading, Value};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    /// A pause point right before a call is tracked, after its facts joined the decision state and with the
    /// decision lock held, for the store in the given directory.
    static BEFORE_TRACKING: Mutex<Option<(PathBuf, Hook)>> = Mutex::new(None);

    pub(super) fn before_tracking(dir: &Path) {
        let hook = BEFORE_TRACKING
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(d, _)| d == dir)
            .map(|(_, h)| h.clone());
        if let Some(h) = hook {
            h();
        }
    }

    /// Polls a queued call to its end on this thread.
    fn wait(mut q: super::Queued) -> Result<crate::types::Ack, EmitError> {
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(r) = q.poll(&mut cx) {
                return q.finish(r);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A duplicate that arrives while the original's facts are in the decision state but its call is not yet
    /// tracked is decided only after the tracking, and waits for the original: here the original's sync fails, so
    /// neither is acknowledged (Q-123 (b)).
    #[test]
    fn a_duplicate_arriving_before_the_original_is_tracked_waits_for_it() {
        let fs = MemFs::new();
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let mut env = Env::test(clock.clone());
        env.fs = Arc::new(fs.clone());
        let log = LogConfig {
            segment_size: 16 << 20,
            ..LogConfig::default()
        };
        let dir = PathBuf::from("durable-unit/decided");
        let config = StoreConfig::new(&dir, "Europe/London")
            .create_if_missing(true)
            .env(env)
            .log(log);
        let w = Writer::open(&config).unwrap();
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let id = w.define(&spec).unwrap();
        let reading = [Reading::new(id, clock.wall(), Value::F64(1.0))];

        // The original's batch sync is held, then fails.
        let (sync_entered, sync_seen) = mpsc::channel::<()>();
        let (sync_release, sync_wait) = mpsc::channel::<()>();
        let sync_wait = Mutex::new(sync_wait);
        let sync_armed = Arc::new(AtomicBool::new(true));
        let failing = fs.clone();
        let armed = sync_armed.clone();
        fs.set_sync_hook(Some(Arc::new(move |p: &Path| {
            let in_use = p
                .to_str()
                .is_some_and(|p| p.ends_with("-0000000000000001.open"));
            if in_use && armed.swap(false, Ordering::SeqCst) {
                sync_entered.send(()).unwrap();
                sync_wait.lock().unwrap().recv().unwrap();
                failing.fail_syncs(true);
            }
        })));
        // The original's call pauses between joining the decision state and being tracked.
        let (paused_tx, paused) = mpsc::channel::<()>();
        let (resume, resume_rx) = mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let pause_armed = AtomicBool::new(true);
        *BEFORE_TRACKING.lock().unwrap() = Some((
            dir.clone(),
            Arc::new(move || {
                if pause_armed.swap(false, Ordering::SeqCst) {
                    paused_tx.send(()).unwrap();
                    resume_rx.lock().unwrap().recv().unwrap();
                }
            }),
        ));

        let w2 = w.clone();
        let first = reading.clone();
        let original = std::thread::spawn(move || match w2.emit(&first, Intent::Normal, false) {
            Ok(Emitted::Queued(q)) => wait(q),
            Ok(Emitted::Ready(a)) => Ok(a),
            Err(e) => Err(e),
        });
        paused
            .recv_timeout(Duration::from_secs(5))
            .expect("the original paused before its tracking");
        let (decided_tx, decided) = mpsc::channel::<()>();
        let w3 = w.clone();
        let dup = reading.clone();
        let duplicate = std::thread::spawn(move || {
            let r = w3.emit(&dup, Intent::Normal, false);
            decided_tx.send(()).unwrap();
            match r {
                Ok(Emitted::Queued(q)) => Ok(wait(q)),
                Ok(Emitted::Ready(a)) => Err(format!(
                    "acknowledged before the original: {:?}",
                    a.outcomes
                )),
                Err(e) => Err(format!("{e:?}")),
            }
        });
        assert!(
            decided.recv_timeout(Duration::from_millis(300)).is_err(),
            "the duplicate was decided before the original was tracked"
        );
        resume.send(()).unwrap();
        sync_seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the original's sync started");
        sync_release.send(()).unwrap();
        let dup = duplicate.join().unwrap().unwrap();
        assert!(matches!(dup, Err(EmitError::OutcomeUnknown(_))), "{dup:?}");
        assert!(matches!(
            original.join().unwrap(),
            Err(EmitError::OutcomeUnknown(_))
        ));
        *BEFORE_TRACKING.lock().unwrap() = None;
        fs.set_sync_hook(None);
    }

    /// The calls in flight are a memory bound: a call taken in as acknowledged is no longer tracked.
    #[test]
    fn acknowledged_calls_are_no_longer_tracked_once_a_later_call_settles() {
        let fs = MemFs::new();
        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let mut env = Env::test(clock.clone());
        env.fs = Arc::new(fs);
        let dir = PathBuf::from("durable-unit/inflight");
        let config = StoreConfig::new(&dir, "Europe/London")
            .create_if_missing(true)
            .env(env);
        let w = Writer::open(&config).unwrap();
        let spec = SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        );
        let id = w.define(&spec).unwrap();
        let emit = |k: u64| {
            let t = clock.wall() + Duration::from_millis(k);
            match w.emit(
                &[Reading::new(id, t, Value::F64(k as f64))],
                Intent::Normal,
                false,
            ) {
                Ok(Emitted::Queued(q)) => wait(q).unwrap(),
                r => panic!("{:?}", r.map(|_| ())),
            };
        };
        for k in 0..50 {
            emit(k);
        }
        emit(50);
        assert!(w.inflight.lock().unwrap().len() <= 1);
    }
}
