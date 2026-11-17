//! Health, losses and statistics of the durable store (spec §11).

use super::*;

impl Writer {
    /// `f` on the deliverer once it has taken what is acknowledged and its summaries are exact.
    pub(super) fn held<R>(&self, f: impl FnOnce(&deliver::Deliverer) -> R) -> Option<R> {
        self.hub.as_ref()?.run_quiet(|d, c| d.held(c).then(|| f(d)))
    }

    // ----- writing -----

    pub(crate) fn losses(&self) -> Vec<LossReport> {
        let acked = lock(&self.acknowledged).clone();
        let held = self.hub.as_ref().map(|h| {
            h.run_quiet(|d, c| {
                d.pump(c);
                d.losses()
            })
        });
        held.or_else(|| self.reader().and_then(|r| r.losses()).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|mut l| {
                l.acknowledged = acked.contains(&l.identity);
                l
            })
            .collect()
    }

    /// Acknowledges losses. Held in memory for this process: the manifest's acknowledgement record is not
    /// written yet.
    pub(crate) fn acknowledge_losses(&self, ids: &[Position]) -> Result<(), WriteError> {
        self.writable().map_err(|e| e.write(&self.dir))?;
        lock(&self.acknowledged).extend(ids.iter().copied());
        Ok(())
    }

    pub(crate) fn acknowledge_manifest_rebuild(&self) -> Result<(), WriteError> {
        self.writable().map_err(|e| e.write(&self.dir))
    }

    pub(crate) fn health(&self) -> Health {
        if self.closed.load(Ordering::SeqCst) {
            return Health::Closed;
        }
        let mut reasons: Vec<Reason> = Vec::new();
        if let Some(log) = self.log().as_ref() {
            for r in log.reasons() {
                reasons.push(match r {
                    LogReason::IngestWaiting(w) => Reason::IngestWaiting(wait_out(w)),
                    LogReason::DiskLow { free } => Reason::DiskLow { free },
                    LogReason::DiskCritical { free } => Reason::DiskCritical { free },
                    LogReason::SealBlocked { file_id } => Reason::SealBlocked { file_id },
                });
            }
        }
        reasons.extend(
            self.losses()
                .into_iter()
                .filter(|l| l.kind == LossKind::DataLost && !l.acknowledged)
                .map(Reason::DataLost),
        );
        // Q-750, Q-755: decisions made this run without data that could not be read, and damaged regions no lost
        // range records, from their own counts (not `duplicates_unchecked`, which a fact on no local tier also raises).
        let (readings, start_regions) = {
            let c = lock(&self.counters);
            (c.unchecked_readings, c.start_regions)
        };
        let delivery = self
            .hub
            .as_ref()
            .map(|h| h.run_quiet(|d, _| d.delivery_stats()))
            .unwrap_or_default();
        let regions = start_regions + delivery.regions_unread;
        if readings + delivery.unchecked + delivery.evaluations_unchecked + regions > 0 {
            reasons.push(Reason::LocalDataUnreadable {
                readings,
                deliveries: delivery.unchecked,
                evaluations: delivery.evaluations_unchecked,
                regions,
            });
        }
        match (self.fault(), reasons.is_empty()) {
            (Some(f), _) => Health::ReadOnly {
                cause: fault_out(&f),
                reasons,
            },
            (None, true) => Health::Ok,
            (None, false) => Health::Degraded { reasons },
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        let log = self.log();
        let ls = log.as_ref().map(Log::stats);
        let waiting = log.as_ref().and_then(|l| {
            l.reasons().into_iter().find_map(|r| match r {
                LogReason::IngestWaiting(w) => Some(wait_out(w)),
                _ => None,
            })
        });
        let queue_depth = log.as_ref().map_or(0, Log::queue_depth);
        drop(log);
        let c = lock(&self.counters);
        let mut sorted: Vec<Duration> = c.latencies.iter().copied().collect();
        sorted.sort();
        let pct = |p: usize| {
            sorted
                .get((sorted.len() * p / 100).min(sorted.len().saturating_sub(1)))
                .copied()
                .unwrap_or_default()
        };
        let free = self.env.fs.free_space(&self.dir).unwrap_or(0);
        Stats {
            ingest: IngestStats {
                queue_depth,
                waiting,
                waited: c.waited,
                waits: c.waits.clone(),
                batches: ls.as_ref().map_or(0, |s| s.batches),
                syncs: ls.as_ref().map_or(0, |s| s.syncs),
                commit_latency: Latency {
                    p50: pct(50),
                    p99: pct(99),
                    max: c.max_latency,
                },
                slow_path: c.slow_path,
                start_whole: c.start_whole,
            },
            log: LogStats {
                durable: ls.as_ref().map_or(Position::ORIGIN, |s| pos(s.durable)),
                mark_synced: ls.as_ref().map_or(Position::ORIGIN, |s| pos(s.mark_synced)),
                mark_sync: match ls.as_ref().map(|s| s.mark_sync) {
                    Some(LogMarkSyncMode::EveryCommit { reason }) => MarkSyncMode::EveryCommit {
                        reason: format!("{reason:?}"),
                    },
                    _ => MarkSyncMode::Interval,
                },
                open_file_id: ls.as_ref().and_then(|s| s.open_file_id),
                sealed_segments: ls.as_ref().map_or(0, |s| s.sealed_segments),
                bytes: ls.as_ref().map_or(0, |s| s.bytes),
                sync_interval: ls.as_ref().map_or(Duration::ZERO, |s| s.sync_interval),
                sync_time: ls.as_ref().and_then(|s| s.sync_time),
                manifest_rebuilds: 0,
                loss_identities_rebuilt: 0,
            },
            disk: DiskStats { free },
            clock: ClockStats::default(),
            delivery: self
                .hub
                .as_ref()
                .map(|h| h.run_quiet(|d, _| d.delivery_stats()))
                .unwrap_or_default(),
        }
    }

    pub(crate) fn series_stats(&self, id: SeriesId) -> Option<SeriesStats> {
        lock(&self.decider).registry.get(id.0)?;
        let mut st = lock(&self.counters)
            .series
            .get(&id.0)
            .cloned()
            .unwrap_or_default();
        match self.held(|d| d.latest_t(id.0)) {
            Some(t) => st.latest_t = t.map(Timestamp),
            None => {
                if let Ok(r) = self.reader() {
                    let _ = r.with(|i, _| {
                        st.latest_t = i.latest_t(id.0).map(Timestamp);
                        Ok(())
                    });
                }
            }
        }
        Some(st)
    }

    /// Orphaned facts with positions in a range (spec §2.1).
    pub(crate) fn export_orphaned(
        &self,
        range: std::ops::RangeInclusive<Position>,
    ) -> Result<Vec<Fact>, QueryError> {
        self.reader()?.with(|i, _| Ok(orphaned(i, &range)))
    }
}

fn orphaned(i: &Index, range: &std::ops::RangeInclusive<Position>) -> Vec<Fact> {
    let mut out: Vec<Fact> = i
        .orphaned()
        .keys()
        .flat_map(|id| i.series_facts(*id))
        .filter(|s| range.contains(&pos(s.position)))
        .map(fact_out)
        .collect();
    out.sort_by_key(|f| f.position);
    out
}
