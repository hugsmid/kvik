//! Store configuration (`docs/api.md` Opening). Every default is the
//! specification's.

use std::path::PathBuf;
use std::time::Duration;

use crate::env::Env;

/// Configuration of [`Store::open`](crate::Store::open).
#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// The store directory. The model backend writes nothing to it: it only
    /// names the in-memory store within the process.
    pub dir: PathBuf,
    /// The site zone, an IANA name: calendar days and months are taken in it
    /// (spec §5.7). A store keeps the zone it was created with; a change is
    /// a log entry, not a different value here.
    pub time_zone: String,
    /// Log configuration.
    pub log: LogConfig,
    /// Queue sizes, ceilings and disk marks.
    pub limits: Limits,
    /// Tier configuration.
    pub tiers: TierConfig,
    /// Clock, file system and boot identity.
    pub env: Env,
}

impl StoreConfig {
    /// A configuration with every default and the system environment.
    pub fn new(dir: impl Into<PathBuf>, time_zone: impl Into<String>) -> Self {
        StoreConfig {
            dir: dir.into(),
            time_zone: time_zone.into(),
            log: LogConfig::default(),
            limits: Limits::default(),
            tiers: TierConfig::default(),
            env: Env::system(),
        }
    }
}

/// Log configuration (spec §3). The model backend uses
/// [`LogConfig::seal_interval`] only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogConfig {
    /// Segment size; a segment is sealed when full (default 64 MiB).
    pub segment_size: u64,
    /// A segment is sealed after this long at the latest (default 5 minutes).
    pub seal_interval: Duration,
    /// Added delay before a commit when idle (default 0).
    pub linger: Duration,
    /// How often the mark file is synced (default 1 s).
    pub mark_sync_interval: Duration,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            segment_size: 64 << 20,
            seal_interval: Duration::from_secs(300),
            linger: Duration::ZERO,
            mark_sync_interval: Duration::from_secs(1),
        }
    }
}

/// Queue sizes, ceilings and disk marks (spec §3, §4, §9 row 30).
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    /// Readings the ingest queue holds before emits wait (default 65,536).
    pub ingest_queue: usize,
    /// Deliveries a consumer's queue holds before it switches to catch-up
    /// (default 4,096).
    pub consumer_queue: usize,
    /// Consumers a store can have. The model backend reserves this many
    /// consumer slots when the store is created (default 64).
    pub max_consumers: usize,
    /// Ceiling on the number of series (default 100,000).
    pub series_ceiling: usize,
    /// Low mark: free space below this many bytes, or below
    /// [`Limits::low_mark_fraction`] of the file system, pauses compaction
    /// (default 2 GiB).
    pub low_mark: u64,
    /// Low mark as a fraction of the file system (default 0.05).
    pub low_mark_fraction: f64,
    /// Critical mark: free space below this many bytes makes ingest wait
    /// (default 512 MiB).
    pub critical_mark: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            ingest_queue: 65_536,
            consumer_queue: 4_096,
            max_consumers: 64,
            series_ceiling: 100_000,
            low_mark: 2 << 30,
            low_mark_fraction: 0.05,
            critical_mark: 512 << 20,
        }
    }
}

/// Tier configuration (spec §8). Not used by the model backend.
#[derive(Clone, Debug, PartialEq)]
pub struct TierConfig {
    /// The cold tier's path, if any.
    pub cold: Option<PathBuf>,
    /// Age after which revision chunks and sealed log segments leave the hot
    /// tier once verified elsewhere (default 90 days).
    pub eviction_age: Duration,
    /// Fill ratio above which verified files leave the hot tier (default
    /// 0.7).
    pub fill_threshold: f64,
    /// A local mirror for sealed log segments (default none).
    pub mirror_path: Option<PathBuf>,
}

impl Default for TierConfig {
    fn default() -> Self {
        TierConfig {
            cold: None,
            eviction_age: Duration::from_secs(90 * 24 * 3_600),
            fill_threshold: 0.7,
            mirror_path: None,
        }
    }
}
