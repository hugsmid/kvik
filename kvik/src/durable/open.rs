//! Opening and shutting down the durable store (`docs/api.md` Opening, `spec.md` §3 Order of a start).

use super::loss::Regions;
use super::seed::Seed;
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
        let clocks = Arc::new(clock::Clocks::new(env.clock.clone()));
        // A seeded start where the files allow it (`spec.md` §3 Order of a start); otherwise the kept log, read once
        // in parts: the registry, the decision state, the counters and the losses passed over (roadmap 11.0, Q-947);
        // its reader then becomes the loss watch (Q-927).
        let seeded = start_fault
            .is_none()
            .then(|| Seed::find(&writer, &*env.fs, &dir, &descriptor.store_id))
            .flatten()
            .and_then(|seed| {
                let through = seed.through;
                start_seeded(
                    config,
                    &writer,
                    &env,
                    &clocks,
                    &latch,
                    descriptor.store_id,
                    seed,
                )
                .map(|s| (s, through))
            });
        let (kept, seeded) = match seeded {
            Some((s, through)) => (s.kept, Some((s.hook, s.deliverer, through, s.scanned))),
            None => {
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
                (kept, None)
            }
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
        // Under `ScanOrder::BeforeOpen` the scan below the seed has run: its regions the pass did not meet count here,
        // as the background scan counts them when it ends.
        let scanned_regions = match &seeded {
            Some((_, _, _, Some(scanned))) => scan::unmet(scanned, &passed, &recorded, &standing),
            _ => 0,
        };
        let counters = Counters {
            series: series_counters,
            start_whole: read_whole,
            start_seeded: seeded.is_some(),
            start_regions: passed.unrecorded(&recorded, &standing) + scanned_regions,
            ..Counters::default()
        };
        // The derived check of the registry file (Q-33): the registry is rebuilt at every start, from the kept log or,
        // in a seeded start, from the file up to the seed and the log above it, and the file, which must equal that
        // up to the last synced mark with the recovery entries of the file read carried forward (Q-1033), is
        // written again whenever it does not. Its id floor is the writer's, which recovery folded in from the mark,
        // the kept log, its recovery entries and the file read, so a lost definition's id stays assigned.
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
        // Consumers resume from their cursor files and the kept log (`spec.md` §3 Order of a start, step 7).
        let (hook, delivering, seed_through) = match seeded {
            Some((hook, mut d, through, scanned)) => {
                d.finish(&dir, &env, writer.generation());
                (hook, Ok(d), scanned.is_none().then_some(through))
            }
            None => {
                let hook = derived::hook(
                    &writer,
                    &*env.fs,
                    &dir,
                    descriptor.store_id,
                    &latch,
                    &recorded,
                );
                let d = deliver::Deliverer::open(
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
                );
                (hook, d, None)
            }
        };
        let hub = match delivering {
            Ok(d) => Some(deliver::Hub::new(d, env.clone(), &dir, clocks.clone())),
            Err(_) if start_fault.is_some() => None,
            Err(source) => {
                return Err(OpenError::Io {
                    path: dir.clone(),
                    source,
                });
            }
        };
        let dirty = Arc::new(tree::Dirty::default());
        let tree_job = start_fault.is_none().then(|| {
            let ds = writer.directory_start();
            let job = Arc::new(tree::Job::new(tree::job::Parts {
                dir: dir.clone(),
                env: env.clone(),
                latch: latch.clone(),
                recorded: recorded.to_vec(),
                id: kvik_tree::write::Identity {
                    generation: ds.generation,
                    reset: ds.stamp.unwrap_or_default(),
                    store_id: descriptor.store_id,
                },
                fresh: writer.stats().max_series_id == 0,
                dirty: dirty.clone(),
            }));
            hook.tree(job.clone());
            job
        });
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
        let scan = seed_through.map(|through| {
            let c = counters.clone();
            scan::Scan::spawn(
                &dir,
                &env,
                through,
                latch.clone(),
                passed.clone(),
                recorded.clone(),
                standing.clone(),
                move |n| lock(&c).start_regions += n,
            )
        });
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
            scan: Mutex::new(scan),
            bucket_warning: config.log.bucket_warning(),
            fast: Arc::default(),
            dirty,
            tree: tree_job,
            env,
        });
        if let Some(job) = &w.tree {
            let weak = Arc::downgrade(&w);
            job.readers(Box::new(move |open| {
                let Some(w) = weak.upgrade() else {
                    return Ok(None);
                };
                if open {
                    w.reader().map(Some).map_err(|e| e.to_string())
                } else {
                    Ok(lock(&w.reader).clone())
                }
            }));
        }
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

    /// Waits for a seeded start's damage scan to end.
    #[cfg(test)]
    pub(crate) fn wait_scan(&self) {
        if let Some(s) = lock(&self.scan).as_mut() {
            s.wait();
        }
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
        // The deliverer takes what is acknowledged, so that the series directory the shutdown writes covers it (A.18
        // Cadence).
        if let Some(h) = &self.hub {
            h.run(|d, c| d.pump(c));
        }
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
        // The loss watch gives up its reader; its reports stay (Q-926). A damage scan still running stops.
        lock(&self.watch).close();
        drop(lock(&self.scan).take());
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
        // A call offered before the log shut down is published under the decision lock: once it is free, every
        // such call is in the ring before the streams are told to close.
        drop(lock(&self.decider));
        self.fast.close();
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

/// What a seeded start built before the registry check: the writer's state, the series directory hook and the
/// deliverer, which still has to read the cursor files.
struct Seeded {
    kept: start::Kept,
    hook: Arc<derived::Hook>,
    deliverer: deliver::Deliverer,
    /// The regions the scan below the seed met, when it ran before the open (Q-1115 (a)).
    scanned: Option<Regions>,
}

/// A seeded start (`spec.md` §3 Order of a start): the writer and the deliverer start from the registry file and the
/// series directory at `seed`, then share one pass over the log above it. `None` when it gives way to the start from
/// the whole log ([`start::GiveWay`]); what it noted in `latch` that start finds again.
fn start_seeded(
    config: &StoreConfig,
    writer: &LogWriter,
    env: &Env,
    clocks: &Arc<clock::Clocks>,
    latch: &Arc<Latch>,
    store_id: [u8; 16],
    seed: Seed,
) -> Option<Seeded> {
    let dir = &config.dir;
    let mut scanned = None;
    if scan::scan_order(dir) == scan::ScanOrder::BeforeOpen {
        let never = std::sync::atomic::AtomicBool::new(false);
        let found = scan::scan_below(dir, env, seed.through, latch, &never).ok()??;
        if found.below {
            #[cfg(test)]
            seed::probe(|p| p.gave_way = Some(start::GiveWay::Below));
            return None;
        }
        scanned = Some(found.regions);
    }
    #[cfg(test)]
    seed::probe(|p| {
        p.recoveries_naming = seed
            .recoveries()
            .filter(|(_, r)| !r.consumers.is_empty())
            .count();
    });
    let budget = window_budget(&config.limits);
    let hook = derived::seeded_hook(writer, &seed);
    let mut d = deliver::Deliverer::new(
        dir,
        env,
        store_id,
        config.limits.consumer_queue,
        config.limits.max_consumers,
        budget,
        latch.clone(),
        Some(hook.fold.clone()),
    )
    .ok()?;
    d.seed(&seed);
    let mut tail = start::Tail::new(&seed, budget);
    let reader = start::log_reader(dir, env).ok();
    let passed = reader.and_then(|mut r| {
        let ctx = deliver::Deliverer::loading_ctx(dir, env, clocks);
        let pass = (|| {
            if !r.start_after(seed.reader_after()).unwrap_or(false) {
                return Err(start::GiveWay::Seed);
            }
            d.seed_lost(r.lost_ranges());
            let mut first = true;
            loop {
                let (u, more) = r
                    .refresh_some(start::PART)
                    .map_err(|_| start::GiveWay::Read)?;
                let mut events = u.events;
                if first && !events.is_empty() {
                    let e = events.remove(0);
                    tail.at_seed(&e)?;
                    d.seed_commit(&e);
                    first = false;
                }
                tail.take(&events, u.state.durable, latch)?;
                d.take_shared(events, u.state.durable, &ctx);
                if !more {
                    return if first {
                        Err(start::GiveWay::Seed)
                    } else {
                        Ok(())
                    };
                }
            }
        })();
        let resumed = pass.is_ok() && d.resume_after_pass();
        #[cfg(test)]
        seed::probe(|p| {
            p.gave_way = pass.err();
            p.resume_failed = pass.is_ok() && !resumed;
        });
        if resumed {
            Some(r)
        } else {
            let _ = r.close();
            None
        }
    });
    let Some(reader) = passed else {
        d.close();
        return None;
    };
    let kept = match tail.finish(reader, || start::log_reader(dir, env), latch) {
        Ok(k) => k,
        Err(_) => {
            d.close();
            return None;
        }
    };
    derived::settle(&hook, latch, &kept.recorded);
    Some(Seeded {
        kept,
        hook,
        deliverer: d,
        scanned,
    })
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
