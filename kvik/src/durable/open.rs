//! Opening and shutting down the durable store (`docs/api.md` Opening, `spec.md` §3 Order of a start).

use super::*;

impl Writer {
    /// Opens the store (`docs/api.md` Opening).
    pub(crate) fn open(config: &StoreConfig) -> Result<Arc<Writer>, OpenError> {
        config.validate()?;
        let dir = config.dir.clone();
        let env = config.env.clone();
        jiff::tz::TimeZone::get(&config.time_zone).map_err(|_| OpenError::UnknownZone)?;
        let wall = env.clock.wall().0;
        let new = config.create_if_missing.then(|| {
            let mut store_id = [0u8; 16];
            env.entropy.fill(&mut store_id);
            NewStore {
                store_id,
                created_at: wall,
                site_zone: config.time_zone.clone(),
                tzdata: tzdata_name(env.tz.current().version()),
                software: format!("kvik {}", env!("CARGO_PKG_VERSION")),
            }
        });
        let store =
            StoreDir::open(env.fs.clone(), &dir, new.as_ref()).map_err(|e| store_error(&dir, e))?;
        let descriptor = store.descriptor().clone();
        let clock = Arc::new(LogClock::new(env.clock.clone()));
        let wcfg = writer_config(config);
        let writer = LogWriter::open(
            store,
            wcfg,
            env.boot.current(),
            kvik_log::writer::Clock::monotonic(&*clock),
            wall,
        )
        .map_err(|e| match e {
            LogOpenError::Store(e) => store_error(&dir, e),
            LogOpenError::Config(c) => OpenError::Config {
                key: "log",
                reason: c.0.into(),
            },
            other => OpenError::Io {
                path: dir.clone(),
                source: io::Error::other(other.to_string()),
            },
        })?;
        let start_fault = writer.fault().cloned();
        let start_losses = start_losses(&writer, env.boot.current());
        let latch = Arc::new(Latch::default());
        // The kept log, read once in parts: the registry, the decision state, the counters and the losses passed over
        // (roadmap 11.0, Q-947); its reader then becomes the loss watch (Q-927).
        let kept = match start::log_reader(&dir, &env) {
            Ok(r) => start::read_kept(
                r,
                || start::log_reader(&dir, &env),
                start::PART,
                window_budget(&config.limits),
                &latch,
            )
            .map_err(|e| OpenError::Io {
                path: dir.clone(),
                source: io::Error::other(e.to_string()),
            })?,
            Err(e) => return Err(reader::open_error(&dir, e)),
        };
        let start::Kept {
            index,
            decider,
            counters: series_counters,
            local,
            read_whole,
            passed,
            recorded,
            watch,
        } = kept;
        let standing: Vec<u64> = start_losses.iter().filter_map(|l| l.file_id).collect();
        // The site zone in force must be the configured one (`docs/api.md` Opening).
        let tz = env.tz.current();
        let canonical = |z: &str| tz.canonical(z).unwrap_or_else(|| z.to_string());
        let in_force = index
            .registry()
            .site_zone(&descriptor.site_zone)
            .to_string();
        if canonical(&in_force) != canonical(&config.time_zone) {
            return Err(OpenError::SiteZoneMismatch {
                configured: config.time_zone.clone(),
                in_force,
            });
        }
        let tzdata_in_force = index.registry().tzdata(&descriptor.tzdata).to_string();
        let counters = Counters {
            series: series_counters,
            start_whole: read_whole,
            start_regions: passed.unrecorded(&recorded, &standing),
            ..Counters::default()
        };
        // The derived check of the registry file (Q-33, as far as roadmap 5.1 reaches): the registry is rebuilt from
        // the kept log at every start, and the file, which must equal what the kept log gives up to the last synced
        // mark with the recovery entries of the file read carried forward (Q-1033), is written again whenever it does
        // not. Its id floor is the writer's, which recovery folded in from
        // the mark, the kept log, its recovery entries and the file read, so a lost definition's id stays assigned.
        let mut start_fault = start_fault;
        let registry_written: derived::RegistryWritten = Arc::default();
        if start_fault.is_none() {
            let stats = writer.stats();
            let mut expected = index.registry_file(stats.mark_synced, stats.max_series_id);
            let current = kvik_log::registry::read_latest(&*env.fs, &dir, &descriptor.store_id)
                .ok()
                .flatten();
            if let Some((_, f)) = &current {
                expected.carry_recovery(f);
            }
            if current.is_none_or(|(_, f)| f != expected)
                && let Err(e) =
                    kvik_log::registry::write(&*env.fs, &dir, descriptor.store_id, wall, &expected)
            {
                start_fault = Some(registry_fault(&dir, &e));
            } else {
                *lock(&registry_written) = Some(stats.mark_synced);
            }
        }
        let hook = derived::hook(
            &writer,
            &*env.fs,
            &dir,
            descriptor.store_id,
            &latch,
            &recorded,
        );
        // Consumers resume from their cursor files and the kept log (`spec.md` §3 Order of a start, step 7).
        let clocks = Arc::new(clock::Clocks::new(env.clock.clone()));
        let hub = match deliver::Deliverer::open(
            &dir,
            &env,
            &clocks,
            descriptor.store_id,
            config.limits.consumer_queue,
            config.limits.max_consumers,
            window_budget(&config.limits),
            latch.clone(),
            Some(hook.fold.clone()),
            writer.generation(),
        ) {
            Ok(d) => Some(deliver::Hub::new(d, env.clone(), &dir, clocks.clone())),
            Err(_) if start_fault.is_some() => None,
            Err(source) => {
                return Err(OpenError::Io {
                    path: dir.clone(),
                    source,
                });
            }
        };
        let log =
            Log::start_with(writer, clock, Some(derived::options(&hook))).map_err(|source| {
                OpenError::Io {
                    path: dir.clone(),
                    source,
                }
            })?;
        let watch_log = log.watch();
        hook.fold.follow(move || watch_log.mark_synced());
        let counters = Arc::new(Mutex::new(counters));
        let own_fault = Arc::new(Mutex::new(start_fault));
        hook.share(
            log.watch(),
            counters.clone(),
            own_fault.clone(),
            env.fs.clone(),
            &dir,
        );
        if let Some(h) = &hub {
            hook.registry(cover_registry(
                h.clone(),
                log.watch(),
                env.clone(),
                &dir,
                descriptor.store_id,
                registry_written.clone(),
            ));
            h.start(log.watch()).map_err(|source| OpenError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        let w = Arc::new(Writer {
            dir,
            limits: config.limits.clone(),
            log: RwLock::new(Some(log)),
            own_fault,
            store_id: descriptor.store_id,
            decider: Mutex::new(decider),
            local: Mutex::new(local),
            counters,
            reader: Mutex::new(None),
            acknowledged: Mutex::new(BTreeMap::new()),
            inflight: Mutex::new(BTreeMap::new()),
            next_call: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            clocks,
            hub,
            latch,
            watch: Mutex::new(watch),
            start_losses,
            registry_written,
            bucket_warning: config.log.bucket_warning(),
            env,
        });
        // A tzdata entry when the host's release is not the version in force, before ingest (`docs/api.md`
        // Opening). A release whose name is not a valid entry is left out.
        let release = tzdata_name(tz.version());
        if w.fault().is_none() && release != tzdata_in_force && !release.is_empty() {
            w.commit_entry(|_| {
                Ok(Decided::Entry(
                    Entry::Tzdata {
                        version: release.clone(),
                    },
                    (),
                ))
            })
            .map_err(|e| OpenError::Io {
                path: w.dir.clone(),
                source: io::Error::other(e.to_string()),
            })?;
        }
        Ok(w)
    }

    /// Clean shutdown: final commit and mark sync, threads joined, lock released.
    pub(crate) fn shutdown(&self) -> Result<(), WriteError> {
        self.closed.store(true, Ordering::SeqCst);
        let log = self
            .log
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(log) = log else {
            return Ok(());
        };
        let hub_fault = || self.hub.as_ref().and_then(|h| lock(&h.fault).clone());
        // The registry file again, up to the mark the clean shutdown synced, while the store's lock is held: from the
        // deliverer's index once it has taken the log up to that mark.
        let shut = log.shutdown_then(|w| {
            if lock(&self.own_fault).is_some() || hub_fault().is_some() {
                return Ok(());
            }
            let _written = lock(&self.registry_written);
            let floor = w.stats().max_series_id;
            let mut file = None;
            if let Some(h) = &self.hub {
                h.run(|d, c| file = d.registry_file(c, floor));
            }
            match file {
                Some(file) => kvik_log::registry::write(
                    &*self.env.fs,
                    &self.dir,
                    self.store_id,
                    self.env.clock.wall().0,
                    &file,
                )
                .map(|_| ()),
                None => Ok(()),
            }
        });
        // The loss watch gives up its reader; its reports stay (Q-926).
        lock(&self.watch).close();
        // Every cursor file whose state changed, after the last mark sync; consumers waiting are woken.
        if let Some(h) = &self.hub {
            h.closed.store(true, Ordering::SeqCst);
            if shut.is_ok()
                && let Err(e) = h.run(|d, c| {
                    let r = d.flush_all(c);
                    d.close();
                    r
                })
            {
                h.failed(&e);
            }
            h.run(|_, _| ());
        }
        let written = shut.map_err(|f| WriteError::ReadOnly(fault_out(&f)))?;
        if let Some(f) = hub_fault() {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        if let Err(e) = written {
            *lock(&self.own_fault) = Some(registry_fault(&self.dir, &e));
        }
        match lock(&self.own_fault).as_ref() {
            Some(f) => Err(WriteError::ReadOnly(fault_out(f))),
            None => Ok(()),
        }
    }
}

/// The losses a start found and did not record, as reports (`spec.md` §3 Order of a start, step 3).
fn start_losses(w: &LogWriter, boot_id: Option<[u8; 16]>) -> Vec<LossReport> {
    let report = w.start_report();
    let g = w.stats().durable.generation;
    let mark_state = match report.rule {
        kvik_log::writer::StartRule::Reboot(m) => mark_state_out(m),
        _ => crate::health::MarkState::Readable,
    };
    report
        .losses
        .iter()
        .filter(|l| !l.recorded)
        .map(|l| LossReport {
            identity: Position::new(g, l.identity),
            kind: match l.kind {
                kvik_log::writer::LossKind::DataLost => LossKind::DataLost,
                kvik_log::writer::LossKind::PossibleLoss => LossKind::PossibleLoss,
            },
            cause: cause_out(l.cause),
            first: l.first.map(|s| Position::new(g, s)),
            last: l.last.map(|s| Position::new(g, s)),
            file_id: Some(l.file_id),
            bytes: l.bytes,
            readable_commit_blocks: l.readable_commits,
            mark_state,
            boot_id,
            commit_time_before: None,
            commit_time_after: None,
            cursors_missing: Vec::new(),
            recorded: false,
            acknowledged: false,
            absorbed: Vec::new(),
        })
        .collect()
}

/// The fault of a failed registry file write.
pub(super) fn registry_fault(dir: &Path, e: &io::Error) -> LogFault {
    LogFault {
        op: FaultOp::Write,
        path: dir.join(kvik_log::registry::REGISTRY_DIR),
        kind: e.kind(),
        message: e.to_string(),
    }
}

/// What makes the registry file cover every entry a series directory covers (Q-1001): written up to the
/// directory's `through` from the deliverer's index when an entry was taken above the file's `through`, under the
/// lock every write of the file takes. A failed write is no fault: that directory is not written.
fn cover_registry(
    hub: Arc<deliver::Hub>,
    watch: kvik_log::writer::Watch,
    env: Env,
    dir: &Path,
    store_id: [u8; 16],
    written: derived::RegistryWritten,
) -> impl Fn(kvik_log::writer::Position, kvik_log::writer::Position) -> bool + Send + Sync + 'static
{
    let dir = dir.to_path_buf();
    move |through, entry| {
        let mut w = lock(&written);
        if w.is_some_and(|w| w >= entry) {
            return true;
        }
        let floor = watch.stats().max_series_id;
        let Some(file) = hub.run_quiet(|d, c| d.routing_registry_file(c, through, floor)) else {
            return false;
        };
        let wall = env.clock.wall().0;
        let ok = kvik_log::registry::write(&*env.fs, &dir, store_id, wall, &file).is_ok();
        if ok {
            *w = Some(through);
        }
        ok
    }
}

/// The ASCII release name of at most 15 bytes a tzdata entry and the store descriptor carry; anything else is
/// left out.
fn tzdata_name(v: &str) -> String {
    if v.is_empty() || v.len() > 15 || !v.is_ascii() {
        String::new()
    } else {
        v.to_string()
    }
}

fn writer_config(config: &StoreConfig) -> WriterConfig {
    let l = &config.log;
    let lim = &config.limits;
    WriterConfig {
        segment_size: l.segment_size,
        seal_interval: l.seal_interval,
        min_sync_interval: l.min_sync_interval,
        sync_interval_floor: l.sync_interval_floor,
        max_batch_bytes: l.max_batch_bytes,
        mark_sync: match l.mark_sync {
            MarkSync::Interval(d) => LogMarkSync::Interval(d),
            MarkSync::EveryCommit => LogMarkSync::EveryCommit,
        },
        ingest_queue: lim.ingest_queue,
        disk_low_percent: (lim.disk_low.fraction * 100.0).round() as u8,
        disk_low_bytes: lim.disk_low.bytes,
        disk_critical: lim.disk_critical,
        ..WriterConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_store_passes_both_pacing_keys_to_the_log_with_the_logs_defaults() {
        let mut config = StoreConfig::default();
        assert_eq!(
            writer_config(&config).sync_interval_floor,
            kvik_log::writer::DEFAULT_SYNC_INTERVAL_FLOOR
        );
        assert_eq!(
            writer_config(&config).min_sync_interval,
            WriterConfig::default().min_sync_interval
        );
        config.log.sync_interval_floor = Duration::from_millis(50);
        config.log.min_sync_interval = Duration::from_millis(100);
        let w = writer_config(&config);
        assert_eq!(
            (w.sync_interval_floor, w.min_sync_interval),
            (Duration::from_millis(50), Duration::from_millis(100))
        );
    }
}
