//! Configuration (`docs/api.md` Opening, Configuration). Every default is the
//! specification's. The structs are `#[non_exhaustive]`: build them with
//! `default()` or `new` and set fields.

use std::path::PathBuf;
use std::time::Duration;

use crate::env::Env;
use crate::error::OpenError;

/// Which implementation stands behind a [`Store`](crate::Store) or a
/// [`Reader`](crate::Reader).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// The durable log engine (the default): every write goes through the
    /// log in the store directory, on the file system of
    /// [`Env::fs`](crate::env::Env). It does not support everything yet;
    /// the crate `README.md` lists what returns `NotSupportedYet`.
    #[default]
    Durable,
    /// The executable reference model: in memory, not durable, complete. The
    /// directory only names the store within the process.
    Model,
}

/// Configuration of [`Store::open`](crate::Store::open).
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct StoreConfig {
    /// The store directory. The model backend writes nothing to it: it only
    /// names the in-memory store within the process.
    pub dir: PathBuf,
    /// The backend (default [`Backend::Durable`]).
    pub backend: Backend,
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
    /// Gates' resume (spec §6 Gates, Resume).
    pub conditions: ConditionsConfig,
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

    /// Sets the backend.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
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

    /// Sets the gates' resume configuration.
    pub fn conditions(mut self, conditions: ConditionsConfig) -> Self {
        self.conditions = conditions;
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
        if self.limits.window_memory < 4 << 10 {
            return bad("limits.window_memory", "at least 4 KiB");
        }
        let c = &self.conditions;
        if c.replay_max.span.is_zero() || i64::try_from(c.replay_max.span.as_nanos()).is_err() {
            return bad(
                "conditions.replay_max",
                "a span above 0 and below 292 years",
            );
        }
        if c.checkpoint_interval.is_zero()
            || i64::try_from(c.checkpoint_interval.as_nanos()).is_err()
        {
            return bad(
                "conditions.checkpoint_interval",
                "above 0 and below 292 years",
            );
        }
        Ok(())
    }
}

/// How far a resume replays gate transitions (`conditions.replay_max`, spec §6 Gates, Resume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReplayMax {
    /// Most commit-clock time from the checkpoint to the resume (default 24 h).
    pub span: Duration,
    /// Most facts after the checkpoint (default 1,000,000).
    pub facts: u64,
}

impl ReplayMax {
    /// A bound of `span` and `facts`.
    pub fn new(span: Duration, facts: u64) -> Self {
        ReplayMax { span, facts }
    }
}

impl Default for ReplayMax {
    fn default() -> Self {
        ReplayMax {
            span: Duration::from_secs(24 * 3_600),
            facts: 1_000_000,
        }
    }
}

/// Gates' resume (spec §6 Gates, Resume; Q-1395). The model backend keeps the whole log and replays without bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConditionsConfig {
    /// Beyond this nothing is replayed: the gates are rebuilt from history (`transitions_not_replayed`).
    pub replay_max: ReplayMax,
    /// The longest a consumer's gate checkpoint lags its engine while the engine moves (default 60 s).
    pub checkpoint_interval: Duration,
}

impl ConditionsConfig {
    /// Sets `replay_max`.
    pub fn replay_max(mut self, replay_max: ReplayMax) -> Self {
        self.replay_max = replay_max;
        self
    }

    /// Sets `checkpoint_interval`.
    pub fn checkpoint_interval(mut self, d: Duration) -> Self {
        self.checkpoint_interval = d;
        self
    }
}

impl Default for ConditionsConfig {
    fn default() -> Self {
        ConditionsConfig {
            replay_max: ReplayMax::default(),
            checkpoint_interval: Duration::from_secs(60),
        }
    }
}

/// How often the mark file is synced (spec §3 Durable mark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkSync {
    /// On a timer (`mark_sync_interval`), and at once for a batch holding an entry that is not a fact or the first
    /// batch in a segment (Q-630).
    Interval(Duration),
    /// At every commit.
    EveryCommit,
}

/// Log configuration (spec §3). The model backend uses
/// [`LogConfig::seal_interval`] only; the durable backend uses every key.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogConfig {
    /// Segment size (default 64 MiB; 16 MiB to 1 GiB, a multiple of 4,096). Above the default, attaching a bucket
    /// warns (Q-970).
    pub segment_size: u64,
    /// A segment is sealed after this long at the latest (default 5 minutes).
    pub seal_interval: Duration,
    /// The ceiling of the batch-sync interval (default 10 ms; 0 disables the sync-rate budget; spec §3 Group
    /// commit, Q-981).
    pub min_sync_interval: Duration,
    /// The least batch-sync interval: batch syncs run at most one per floor over time, plus a burst of four
    /// (default 2 ms; at or above `min_sync_interval` the interval is fixed; spec §3 Group commit, Q-710, Q-981).
    pub sync_interval_floor: Duration,
    /// Largest batch (default 1 MiB; 64 KiB to 8 MiB).
    pub max_batch_bytes: u64,
    /// Mark sync (default every second).
    pub mark_sync: MarkSync,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            segment_size: DEFAULT_SEGMENT_SIZE,
            seal_interval: Duration::from_secs(300),
            min_sync_interval: Duration::from_millis(10),
            sync_interval_floor: kvik_log::writer::DEFAULT_SYNC_INTERVAL_FLOOR,
            max_batch_bytes: 1 << 20,
            mark_sync: MarkSync::Interval(Duration::from_secs(1)),
        }
    }
}

/// The default `log.segment_size`, the size at which a start's cost on a crafted or damaged file is measured (Q-970).
const DEFAULT_SEGMENT_SIZE: u64 = 64 << 20;

impl LogConfig {
    /// The warning for a store that reads files from a bucket, which others may be able to write: `None` at or below
    /// the default segment size, the only size the cost of a crafted or damaged file is promised at (Q-970).
    pub(crate) fn bucket_warning(&self) -> Option<String> {
        (self.segment_size > DEFAULT_SEGMENT_SIZE).then(|| {
            format!(
                "log.segment_size is {} bytes, {:.1} times the default of 64 MiB: if others can write the bucket this \
                 store reads files from, a crafted or damaged file can cost a start about that many times the figure \
                 measured per file at the default (Q-970)",
                self.segment_size,
                self.segment_size as f64 / DEFAULT_SEGMENT_SIZE as f64
            )
        })
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
    /// (default 4,096; a key of roadmap 5.2).
    pub consumer_queue: usize,
    /// Consumers a store can have; the model backend reserves this many
    /// consumer slots when the store is created (default 64).
    pub max_consumers: usize,
    /// Bytes the recent windows may hold in all (spec §5.1, Q-751): half for the writer's decision windows and
    /// revised-key index, half for the deliverer's windows; beyond it the oldest entries are read from the log
    /// instead. Default [`WINDOW_MEMORY_DESKTOP`]; [`WINDOW_MEMORY_MINIMAL`] suits the minimal hardware class.
    pub window_memory: u64,
}

/// The default `limits.window_memory`, for the desktop class (4 or more cores, 8 GB or more).
pub const WINDOW_MEMORY_DESKTOP: u64 = 512 << 20;
/// `limits.window_memory` for the minimal class (1 to 3 GB of memory).
pub const WINDOW_MEMORY_MINIMAL: u64 = 64 << 20;

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
            window_memory: WINDOW_MEMORY_DESKTOP,
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
    /// The backend (default [`Backend::Durable`]); it must be the one the
    /// store's writer uses.
    pub backend: Backend,
    /// The environment.
    pub env: Env,
    /// How often the lease is renewed at the latest (default 30 minutes).
    pub lease_refresh: Duration,
    /// The longest a read holds its cut, counted from the last check that found its epoch current (spec §7
    /// Readers; default 30 minutes, at most an hour, zero refused): a read held longer ends with
    /// [`QueryError::CutExpired`](crate::QueryError::CutExpired).
    pub cut_max_age: Duration,
    /// The cap on what the reader's queries allocate together (the query design's §1 Memory; default 64 MiB, at least
    /// 32 MiB): a read that needs more works in more passes, with equal results (Q-1063).
    pub memory: u64,
}

impl ReaderConfig {
    /// A reader configuration with the system environment.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ReaderConfig {
            dir: dir.into(),
            backend: Backend::Durable,
            env: Env::system(),
            lease_refresh: Duration::from_secs(30 * 60),
            cut_max_age: Duration::from_secs(30 * 60),
            memory: 64 << 20,
        }
    }

    /// Sets the environment.
    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    /// Sets the backend.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// Sets the lease refresh interval.
    pub fn lease_refresh(mut self, d: Duration) -> Self {
        self.lease_refresh = d;
        self
    }

    /// Sets the longest a read holds its cut.
    pub fn cut_max_age(mut self, d: Duration) -> Self {
        self.cut_max_age = d;
        self
    }

    /// Sets the memory cap of the reader's queries.
    pub fn memory(mut self, bytes: u64) -> Self {
        self.memory = bytes;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_warning_is_given_above_the_default_segment_size_only_and_names_the_ratio() {
        let mut log = LogConfig::default();
        assert_eq!(log.bucket_warning(), None);
        log.segment_size = 16 << 20;
        assert_eq!(log.bucket_warning(), None);
        log.segment_size = (64 << 20) + 4_096;
        assert!(log.bucket_warning().is_some());
        log.segment_size = 1 << 30;
        let w = log.bucket_warning().unwrap();
        assert!(w.contains("1073741824 bytes, 16.0 times"), "{w}");
    }
}
