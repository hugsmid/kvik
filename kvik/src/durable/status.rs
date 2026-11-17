//! Health, losses and statistics of the durable store (spec §11).

use super::*;

impl Writer {
    pub(crate) fn losses(&self) -> Vec<LossReport> {
        self.loss_reports().0
    }

    /// Every loss report this process holds, and why the files could not be read, if they could not: the reports
    /// the files hold, then those of the start that could not record them, then those of this process's reads.
    pub(crate) fn loss_reports(&self) -> (Vec<LossReport>, Option<String>) {
        let acked = lock(&self.acknowledged).clone();
        let (read, error) = match self.reader().and_then(|r| r.losses()) {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let (mut out, found): (Vec<_>, Vec<_>) = read.into_iter().partition(|l| l.recorded);
        loss::merge(&mut out, self.start_losses.iter().cloned());
        loss::merge(&mut out, found);
        loss::merge(&mut out, self.latch.found());
        for l in &mut out {
            l.acknowledged = acked
                .get(&l.identity)
                .is_some_and(|(first, last)| *first <= l.first && l.last <= *last);
        }
        (out, error)
    }

    /// Acknowledges losses. Held in memory for this process: the manifest's acknowledgement record is not
    /// written yet.
    pub(crate) fn acknowledge_losses(&self, ids: &[Position]) -> Result<(), WriteError> {
        self.writable().map_err(|e| e.write(&self.dir))?;
        let losses = self.losses();
        lock(&self.acknowledged).extend(ids.iter().filter_map(|id| {
            losses
                .iter()
                .find(|l| l.identity == *id && l.kind == LossKind::DataLost)
                .map(|l| (*id, (l.first, l.last)))
        }));
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
            reasons.extend(log.reasons().into_iter().map(health_reason));
        }
        let (losses, error) = self.loss_reports();
        reasons.extend(
            losses
                .into_iter()
                .filter(|l| l.kind == LossKind::DataLost && !l.acknowledged)
                .map(Reason::DataLost),
        );
        if let Some(error) = error {
            reasons.push(Reason::LossesUnknown { error });
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
        if let Ok(r) = self.reader() {
            let _ = r.with(|i, _| {
                st.latest_t = i.latest_t(id.0).map(Timestamp);
                Ok(())
            });
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

/// A log reason as a health reason; one this version does not name stays visible as [`Reason::Other`] (Q-221 (d)).
fn health_reason(r: LogReason) -> Reason {
    match r {
        LogReason::IngestWaiting(w) => Reason::IngestWaiting(wait_out(w)),
        LogReason::DiskLow { free } => Reason::DiskLow { free },
        LogReason::DiskCritical { free } => Reason::DiskCritical { free },
        LogReason::SealBlocked { file_id } => Reason::SealBlocked { file_id },
        LogReason::SegmentDamaged { file_id } => Reason::SegmentDamaged { file_id },
        LogReason::SegmentUnreadable { file_id } => Reason::SegmentUnreadable { file_id },
        other => Reason::Other {
            reason: format!("{other:?}"),
        },
    }
}

#[cfg(test)]
mod reason_tests {
    use super::{LogReason, Reason, health_reason};

    #[test]
    fn a_log_reason_the_mapping_does_not_name_stays_in_health_with_its_name() {
        let r = health_reason(LogReason::Unmapped);
        assert_eq!(
            r,
            Reason::Other {
                reason: "Unmapped".into()
            }
        );
        assert_eq!(r.to_string(), "log reason Unmapped");
        assert_eq!(
            health_reason(LogReason::SegmentUnreadable { file_id: 4 }),
            Reason::SegmentUnreadable { file_id: 4 }
        );
    }
}
