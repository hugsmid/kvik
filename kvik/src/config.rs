//! Configuration (`docs/api.md` Opening, Configuration). Every default is the
//! specification's. The structs are `#[non_exhaustive]`: build them with
//! `default()` or `new` and set fields.

use std::path::PathBuf;
use std::time::Duration;

use crate::env::Env;
use crate::error::OpenError;

/// Configuration of [`Store::open`](crate::Store::open).
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct StoreConfig {
    /// The store directory. The model backend writes nothing to it: it only
    /// names the in-memory store within the process.
    pub dir: PathBuf,
    /// The site zone at creation, an IANA name; at every later open it must
    /// equal the zone in force (changed with
    /// [`Store::set_site_zone`](crate::Store::set_site_zone)).
    pub time_zone: String,
    /// Create the store if the directory holds none (default off).
    pub create_if_missing: bool,
    /// Log configuration.
    pub log: LogConfig,
    /// Queue sizes, ceilings and disk marks.
    pub limits: Limits,
    /// Tier configuration.
    pub tiers: TierConfig,
    /// Clock, file system, boot identity, tzdata and entropy.
    pub env: Env,
}

impl StoreConfig {
    /// A configuration with every default and the system environment.
    pub fn new(dir: impl Into<PathBuf>, time_zone: impl Into<String>) -> Self {
        StoreConfig {
            dir: dir.into(),
            time_zone: time_zone.into(),
            ..StoreConfig::default()
        }
    }

    /// Sets `create_if_missing`.
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.create_if_missing = create;
        self
    }

    /// Sets the environment.
    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    /// Sets the log configuration.
    pub fn log(mut self, log: LogConfig) -> Self {
        self.log = log;
        self
    }

    /// Sets the limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the tier configuration.
    pub fn tiers(mut self, tiers: TierConfig) -> Self {
        self.tiers = tiers;
        self
    }

    /// Checks every key against its limits (`docs/api.md` Configuration).
    pub(crate) fn validate(&self) -> Result<(), OpenError> {
        let bad = |key, reason: &str| {
            Err(OpenError::Config {
                key,
                reason: reason.into(),
            })
        };
        let l = &self.log;
        if !(16 << 20..=1 << 30).contains(&l.segment_size) || !l.segment_size.is_multiple_of(4_096)
        {
            return bad("log.segment_size", "16 MiB to 1 GiB, a multiple of 4,096");
        }
        if l.seal_interval.is_zero() {
            return bad("log.seal_interval", "must be positive");
        }
        if !(64 << 10..=8 << 20).contains(&l.max_batch_bytes) {
            return bad("log.max_batch_bytes", "64 KiB to 8 MiB");
        }
        if matches!(l.mark_sync, MarkSync::Interval(d) if d.is_zero()) {
            return bad("log.mark_sync", "an interval must be positive");
        }
        if self.limits.ingest_queue < 1_024 {
            return bad("limits.ingest_queue", "at least 1,024");
        }
        if !(0.0..1.0).contains(&self.limits.disk_low.fraction) {
            return bad("limits.disk_low", "a fraction below 1");
        }
        if self.limits.consumer_queue == 0 {
            return bad("limits.consumer_queue", "at least 1");
        }
        Ok(())
    }
}

/// How often the mark file is synced (spec §3 Durable mark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkSync {
    /// On a timer (`mark_sync_interval`).
    Interval(Duration),
    /// At every commit.
    EveryCommit,
}

/// Log configuration (spec §3). The model backend uses
/// [`LogConfig::seal_interval`] only.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogConfig {
    /// Segment size (default 64 MiB; 16 MiB to 1 GiB, a multiple of 4,096).
    pub segment_size: u64,
    /// A segment is sealed after this long at the latest (default 5 minutes).
    pub seal_interval: Duration,
    /// Log syncs start at least this far apart (default 10 ms; 0 disables).
    pub min_sync_interval: Duration,
    /// Largest batch (default 1 MiB; 64 KiB to 8 MiB).
    pub max_batch_bytes: u64,
    /// Mark sync (default every second).
    pub mark_sync: MarkSync,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            segment_size: 64 << 20,
            seal_interval: Duration::from_secs(300),
            min_sync_interval: Duration::from_millis(10),
            max_batch_bytes: 1 << 20,
            mark_sync: MarkSync::Interval(Duration::from_secs(1)),
        }
    }
}

/// The low disk mark: free space below either value.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct DiskLow {
    /// Fraction of the file system (default 0.05).
    pub fraction: f64,
    /// Bytes (default 2 GiB).
    pub bytes: u64,
}

impl Default for DiskLow {
    fn default() -> Self {
        DiskLow {
            fraction: 0.05,
            bytes: 2 << 30,
        }
    }
}

/// Queue sizes, ceilings and disk marks (spec §3, §4, §9 row 30).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Limits {
    /// Readings the ingest queue holds (default 65,536; at least 1,024). A
    /// call is at most `min(65,536, ingest_queue)` readings.
    pub ingest_queue: usize,
    /// Ceiling on the number of series (default 100,000).
    pub series_ceiling: u32,
    /// Low mark: compaction pauses.
    pub disk_low: DiskLow,
    /// Critical mark: ingest waits (default 512 MiB).
    pub disk_critical: u64,
    /// A reading further ahead than this is `future` (default 60 s).
    pub future_tolerance: Duration,
    /// Lateness bound of a series that declares none (default 5 minutes).
    pub default_lateness_bound: Duration,
    /// Deliveries a consumer's queue holds before it switches to catch-up
    /// (default 4,096; a key of roadmap 5.2, used by the model backend).
    pub consumer_queue: usize,
    /// Consumers a store can have; the model backend reserves this many
    /// consumer slots when the store is created (default 64).
    pub max_consumers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            ingest_queue: 65_536,
            series_ceiling: 100_000,
            disk_low: DiskLow::default(),
            disk_critical: 512 << 20,
            future_tolerance: Duration::from_secs(60),
            default_lateness_bound: Duration::from_secs(300),
            consumer_queue: 4_096,
            max_consumers: 64,
        }
    }
}

/// Tier configuration (spec §8). Not used by the model backend.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TierConfig {
    /// The cold tier's path, if any.
    pub cold: Option<PathBuf>,
    /// Age after which revision chunks and sealed log segments leave the hot
    /// tier once verified elsewhere (default 90 days).
    pub eviction_age: Duration,
    /// Fill ratio above which verified files leave the hot tier (default
    /// 0.7).
    pub fill_threshold: f64,
}

impl Default for TierConfig {
    fn default() -> Self {
        TierConfig {
            cold: None,
            eviction_age: Duration::from_secs(90 * 24 * 3_600),
            fill_threshold: 0.7,
        }
    }
}

/// Configuration of [`Reader::open`](crate::Reader::open).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ReaderConfig {
    /// The store directory.
    pub dir: PathBuf,
    /// The environment.
    pub env: Env,
    /// How often the lease is renewed at the latest (default 30 minutes).
    pub lease_refresh: Duration,
}

impl ReaderConfig {
    /// A reader configuration with the system environment.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ReaderConfig {
            dir: dir.into(),
            env: Env::system(),
            lease_refresh: Duration::from_secs(30 * 60),
        }
    }

    /// Sets the environment.
    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    /// Sets the lease refresh interval.
    pub fn lease_refresh(mut self, d: Duration) -> Self {
        self.lease_refresh = d;
        self
    }
}
