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
        // The kept log, read once in parts: the registry, the decision state and the counters (roadmap 11.0).
        let kept = match start::log_reader(&dir, &env) {
            Ok(r) => start::read_kept(
                r,
                || start::log_reader(&dir, &env),
                start::PART,
                window_budget(&config.limits),
            )
            .map_err(|e| OpenError::Io {
                path: dir.clone(),
                source: io::Error::other(e.to_string()),
            })?,
            // A start that could not write may leave no readable mark: the store opens read-only.
            Err(_) if start_fault.is_some() => start::Kept::unreadable(),
            Err(e) => return Err(reader::open_error(&dir, e)),
        };
        let start::Kept {
            index,
            decider,
            counters: series_counters,
            local,
            read_whole,
            unrecorded,
        } = kept;
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
            start_regions: unrecorded,
            ..Counters::default()
        };
        // The derived check of the registry file (Q-33, as far as roadmap 5.1 reaches): the registry is rebuilt from
        // the kept log at every start, and the file, which must equal what the kept log gives up to the last synced
        // mark, is written again whenever it does not. Its id floor is the writer's, which recovery folded in from
        // the mark, the kept log, its recovery entries and the file read, so a lost definition's id stays assigned.
        let mut start_fault = start_fault;
        if start_fault.is_none() {
            let stats = writer.stats();
            let expected = index.registry_file(stats.mark_synced, stats.max_series_id);
            let current = kvik_log::registry::read_latest(&*env.fs, &dir, &descriptor.store_id)
                .ok()
                .flatten();
            if current.is_none_or(|(_, f)| f != expected)
                && let Err(e) =
                    kvik_log::registry::write(&*env.fs, &dir, descriptor.store_id, wall, &expected)
            {
                start_fault = Some(registry_fault(&dir, &e));
            }
        }
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
        let log = Log::start(writer, clock).map_err(|source| OpenError::Io {
            path: dir.clone(),
            source,
        })?;
        if let Some(h) = &hub {
            h.start(log.watch()).map_err(|source| OpenError::Io {
                path: dir.clone(),
                source,
            })?;
        }
        let w = Arc::new(Writer {
            dir,
            limits: config.limits.clone(),
            log: RwLock::new(Some(log)),
            own_fault: Mutex::new(start_fault),
            store_id: descriptor.store_id,
            decider: Mutex::new(decider),
            local: Mutex::new(local),
            counters: Mutex::new(counters),
            reader: Mutex::new(None),
            acknowledged: Mutex::new(BTreeSet::new()),
            inflight: Mutex::new(BTreeMap::new()),
            next_call: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            clocks,
            hub,
            env,
        });
        // A tzdata entry when the host's release is not the version in force, before ingest (`docs/api.md`
        // Opening). A release whose name is not a valid entry is left out.
        let release = tzdata_name(tz.version());
        if w.fault().is_none() && release != tzdata_in_force && !release.is_empty() {
            let mut d = lock(&w.decider);
            w.commit_entry(&mut d, Entry::Tzdata { version: release })
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
        let floor = log.stats().max_series_id;
        let shut = log.shutdown();
        // Every cursor file whose state changed, after the last mark sync; consumers waiting are woken. The registry
        // file's content comes from the deliverer's index once it has taken the log up to that mark.
        let mut file = None;
        if let Some(h) = &self.hub {
            h.closed.store(true, Ordering::SeqCst);
            if shut.is_ok()
                && let Err(e) = h.run(|d, c| {
                    let r = d.flush_all(c);
                    file = d.registry_file(c, floor);
                    d.close();
                    r
                })
            {
                h.failed(&e);
            }
            h.run(|_, _| ());
        }
        if let Err(f) = shut {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        if let Some(f) = self.hub.as_ref().and_then(|h| lock(&h.fault).clone()) {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        if let Some(f) = lock(&self.own_fault).clone() {
            return Err(WriteError::ReadOnly(fault_out(&f)));
        }
        // The registry file again, up to the mark the clean shutdown synced.
        if let Some(file) = file {
            let wall = self.env.clock.wall().0;
            if let Err(e) =
                kvik_log::registry::write(&*self.env.fs, &self.dir, self.store_id, wall, &file)
            {
                let f = registry_fault(&self.dir, &e);
                *lock(&self.own_fault) = Some(f.clone());
                return Err(WriteError::ReadOnly(fault_out(&f)));
            }
        }
        Ok(())
    }
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
