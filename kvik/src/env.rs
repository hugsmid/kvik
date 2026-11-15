//! Environment seams (`docs/api.md` Environment seams): everything the engine
//! depends on from outside is a trait with a production and a test
//! implementation.
//!
//! | Seam | Production | Test |
//! |---|---|---|
//! | [`Clock`] | [`SystemClock`] | [`ManualClock`] |
//! | [`Fs`] | [`StdFs`] | [`MemFs`] |
//! | [`BootId`] | [`SystemBootId`] | [`FixedBootId`] |
//! | [`TzSource`] | [`SystemTz`] | [`FixedTz`] |
//! | [`Entropy`] | [`SystemEntropy`] | [`FixedEntropy`] |
//!
//! The in-memory model backend uses the clock only: it ignores the file
//! system, the boot identity and the entropy, and computes calendars with the
//! reference model's own copy of the tzdata it embeds rather than through
//! [`Env::tz`]. The durable backend uses the file system, the boot identity,
//! the entropy (store and reader ids), the tzdata release's name and the wall
//! clock; the log's timers run on the process's own monotonic clock, so that a
//! manual clock that stands still does not hold a sync back. There is no
//! memory mapping: every byte the engine persists passes a seam call.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::time::{Span, Timestamp};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----- clock -----

/// A monotonic instant: nanoseconds from an origin fixed per process; never
/// stored, never compared across processes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Monotonic(pub u64);

/// The clocks the engine reads (spec §2.5, §4 Timers, §6 `now`).
pub trait Clock: Send + Sync + fmt::Debug {
    /// The system's wall clock, UTC. It may jump in either direction.
    fn wall(&self) -> Timestamp;
    /// A monotonic clock: never runs backwards and is not affected by
    /// corrections of the wall clock. Timers and live staleness use it.
    fn monotonic(&self) -> Monotonic;
    /// Whether the system reports clock synchronisation. Until it does,
    /// batches carry `unsynced_clock` and host-stamped readings wait (spec
    /// §2.5).
    fn synchronised(&self) -> bool;
}

type Probe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The system clocks.
///
/// There is no portable source of clock synchronisation without unsafe code,
/// so [`Clock::synchronised`] reports `true` unless the application supplies
/// a probe ([`SystemClock::with_sync_probe`]).
#[derive(Clone, Default)]
pub struct SystemClock {
    probe: Option<Probe>,
}

impl SystemClock {
    /// The system clocks, synchronised as `probe` reports.
    pub fn with_sync_probe(probe: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        SystemClock {
            probe: Some(Arc::new(probe)),
        }
    }
}

impl fmt::Debug for SystemClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemClock")
            .field("probe", &self.probe.is_some())
            .finish()
    }
}

fn process_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

impl Clock for SystemClock {
    fn wall(&self) -> Timestamp {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
    }
    fn monotonic(&self) -> Monotonic {
        let d = Instant::now().saturating_duration_since(process_origin());
        Monotonic(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }
    fn synchronised(&self) -> bool {
        self.probe.as_ref().is_none_or(|p| p())
    }
}

/// A clock that moves only when told to, for tests. Clones share the same
/// clock.
///
/// # Panics
/// [`ManualClock::advance`] and [`ManualClock::jump_wall`] panic when the wall
/// clock would leave the range of [`Timestamp`].
#[derive(Clone, Debug)]
pub struct ManualClock {
    state: Arc<Mutex<ManualState>>,
}

#[derive(Debug)]
struct ManualState {
    wall: Timestamp,
    elapsed: u64,
    synchronised: bool,
}

impl ManualClock {
    /// A synchronised clock whose wall clock reads `wall`.
    pub fn new(wall: Timestamp) -> Self {
        ManualClock {
            state: Arc::new(Mutex::new(ManualState {
                wall,
                elapsed: 0,
                synchronised: true,
            })),
        }
    }

    /// The wall clock now.
    pub fn wall(&self) -> Timestamp {
        lock(&self.state).wall
    }

    /// Advances both clocks by `d`.
    pub fn advance(&self, d: Duration) {
        let mut s = lock(&self.state);
        s.wall = s.wall + d;
        s.elapsed += u64::try_from(d.as_nanos()).expect("duration within range");
    }

    /// Moves the wall clock only, as a correction of the system clock does
    /// (spec §9 row 22).
    pub fn jump_wall(&self, by: Span) {
        let mut s = lock(&self.state);
        s.wall = s.wall + by;
    }

    /// Sets whether the clock reports synchronisation (spec §2.5).
    pub fn set_synchronised(&self, synchronised: bool) {
        lock(&self.state).synchronised = synchronised;
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> Timestamp {
        lock(&self.state).wall
    }
    fn monotonic(&self) -> Monotonic {
        Monotonic(lock(&self.state).elapsed)
    }
    fn synchronised(&self) -> bool {
        lock(&self.state).synchronised
    }
}

// ----- file system -----

pub use kvik_log::fs::{DirEntry, Fs, FsFile, MemFs, OpenMode, StdFs};

// ----- boot identity -----

/// The identity of the current boot of the machine (spec §3 Durable mark,
/// Recovery): it changes when the machine restarts.
pub trait BootId: Send + Sync + fmt::Debug {
    /// The current boot's identity, or `None` when the platform has none.
    fn current(&self) -> Option<[u8; 16]>;
}

/// The operating system's boot identity: on Linux the kernel's `boot_id`;
/// `None` elsewhere for now (other platforms' sources arrive with recovery,
/// roadmap 4.5).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBootId;

impl BootId for SystemBootId {
    fn current(&self) -> Option<[u8; 16]> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        parse_uuid(s.trim())
    }
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// A boot identity set by the test, which can change it to simulate a
/// reboot. Clones share the same identity.
#[derive(Clone, Debug)]
pub struct FixedBootId {
    id: Arc<Mutex<Option<[u8; 16]>>>,
}

impl FixedBootId {
    /// A boot identity.
    pub fn new(id: Option<[u8; 16]>) -> Self {
        FixedBootId {
            id: Arc::new(Mutex::new(id)),
        }
    }

    /// Changes the identity, as a reboot does.
    pub fn set(&self, id: Option<[u8; 16]>) {
        *lock(&self.id) = id;
    }
}

impl BootId for FixedBootId {
    fn current(&self) -> Option<[u8; 16]> {
        *lock(&self.id)
    }
}

// ----- tzdata -----

/// The host's tzdata release now (spec §5.7). It returns one release as a
/// whole, so that a computation never mixes two.
pub trait TzSource: Send + Sync + fmt::Debug {
    /// The current release.
    fn current(&self) -> Arc<dyn TzRelease>;
}

/// One tzdata release.
pub trait TzRelease: Send + Sync + fmt::Debug {
    /// The release name.
    fn version(&self) -> &str;
    /// The canonical name of a zone after resolving links; `None` if the
    /// release does not know it.
    fn canonical(&self, zone: &str) -> Option<String>;
    /// The offset in seconds east of UTC at `t`.
    fn offset_at(&self, zone: &str, t: Timestamp) -> Option<i32>;
    /// The next transition of the zone after `t`.
    fn next_transition(&self, zone: &str, after: Timestamp) -> Option<Timestamp>;
}

/// A release backed by the tzdata this crate embeds.
#[derive(Debug)]
struct EmbeddedRelease {
    version: String,
}

impl EmbeddedRelease {
    fn zone(zone: &str) -> Option<jiff::tz::TimeZone> {
        jiff::tz::TimeZone::get(zone).ok()
    }

    fn ts(t: Timestamp) -> Option<jiff::Timestamp> {
        jiff::Timestamp::from_nanosecond(i128::from(t.0)).ok()
    }
}

impl TzRelease for EmbeddedRelease {
    fn version(&self) -> &str {
        &self.version
    }
    fn canonical(&self, zone: &str) -> Option<String> {
        // The embedded database resolves links when a zone is loaded; the
        // name it reports is the one asked for, or the canonical one.
        let tz = Self::zone(zone)?;
        Some(tz.iana_name().unwrap_or(zone).to_string())
    }
    fn offset_at(&self, zone: &str, t: Timestamp) -> Option<i32> {
        Some(Self::zone(zone)?.to_offset(Self::ts(t)?).seconds())
    }
    fn next_transition(&self, zone: &str, after: Timestamp) -> Option<Timestamp> {
        let tz = Self::zone(zone)?;
        let next = tz.following(Self::ts(after)?).next()?;
        i64::try_from(next.timestamp().as_nanosecond())
            .ok()
            .map(Timestamp)
    }
}

/// The tzdata this crate embeds, reported as the release `"embedded"`.
/// Reading the host's own release arrives with the engine.
#[derive(Clone, Debug)]
pub struct SystemTz {
    release: Arc<EmbeddedRelease>,
}

impl Default for SystemTz {
    fn default() -> Self {
        SystemTz {
            release: Arc::new(EmbeddedRelease {
                version: "embedded".into(),
            }),
        }
    }
}

impl TzSource for SystemTz {
    fn current(&self) -> Arc<dyn TzRelease> {
        self.release.clone()
    }
}

/// A tzdata source for tests, whose release name can be changed to simulate
/// a host update; the rules are the embedded ones. Clones share the source.
#[derive(Clone, Debug)]
pub struct FixedTz {
    release: Arc<Mutex<Arc<EmbeddedRelease>>>,
}

impl FixedTz {
    /// A source reporting the release `version`.
    pub fn new(version: impl Into<String>) -> Self {
        FixedTz {
            release: Arc::new(Mutex::new(Arc::new(EmbeddedRelease {
                version: version.into(),
            }))),
        }
    }

    /// Changes the release name, as a host tzdata update does.
    pub fn set_version(&self, version: impl Into<String>) {
        *lock(&self.release) = Arc::new(EmbeddedRelease {
            version: version.into(),
        });
    }
}

impl TzSource for FixedTz {
    fn current(&self) -> Arc<dyn TzRelease> {
        lock(&self.release).clone()
    }
}

// ----- entropy -----

/// Random bytes, for store and reader ids.
pub trait Entropy: Send + Sync + fmt::Debug {
    /// Fills `out` with random bytes.
    fn fill(&self, out: &mut [u8]);
}

/// Random bytes from the standard library's per-process random hash keys,
/// mixed with a counter and the clocks. Not for cryptographic use; ids only
/// need to be unlikely to collide.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn fill(&self, out: &mut [u8]) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for chunk in out.chunks_mut(8) {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
            h.write_u64(SystemClock::default().wall().0 as u64);
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
}

/// Deterministic bytes for tests: a counter from a seed. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct FixedEntropy {
    next: Arc<AtomicU64>,
}

impl FixedEntropy {
    /// Bytes counting up from `seed`.
    pub fn new(seed: u64) -> Self {
        FixedEntropy {
            next: Arc::new(AtomicU64::new(seed)),
        }
    }
}

impl Entropy for FixedEntropy {
    fn fill(&self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next.fetch_add(1, Ordering::Relaxed);
            chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
        }
    }
}

// ----- the environment -----

/// The environment a store or reader runs in.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Env {
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// File system.
    pub fs: Arc<dyn Fs>,
    /// Boot identity.
    pub boot: Arc<dyn BootId>,
    /// tzdata release.
    pub tz: Arc<dyn TzSource>,
    /// Entropy.
    pub entropy: Arc<dyn Entropy>,
    /// Whether the durable backend runs consumer timers (held-for and stale-for conditions, values coming into
    /// force) on its own thread when no call has run them for a second (default `true`). With `false` they run only
    /// when the store is called (emits, deliveries polled, [`crate::Store::tick`]), as on the model backend, so a
    /// test on a manual clock decides when they run.
    pub background_timers: bool,
}

impl Env {
    /// The production environment.
    pub fn system() -> Self {
        Env {
            clock: Arc::new(SystemClock::default()),
            fs: Arc::new(StdFs),
            boot: Arc::new(SystemBootId),
            tz: Arc::new(SystemTz::default()),
            entropy: Arc::new(SystemEntropy),
            background_timers: true,
        }
    }

    /// A test environment: the given manual clock, an in-memory file system,
    /// a fixed boot identity, a fixed tzdata release and deterministic
    /// entropy.
    pub fn test(clock: ManualClock) -> Self {
        Env {
            clock: Arc::new(clock),
            fs: Arc::new(MemFs::new()),
            boot: Arc::new(FixedBootId::new(Some([1; 16]))),
            tz: Arc::new(FixedTz::new("test")),
            entropy: Arc::new(FixedEntropy::new(1)),
            background_timers: true,
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::system()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(Timestamp::from_secs(1_000));
        c.advance(Duration::from_secs(5));
        assert_eq!(Clock::wall(&c), Timestamp::from_secs(1_005));
        assert_eq!(Clock::monotonic(&c), Monotonic(5_000_000_000));
        c.jump_wall(Span::from_secs(-100));
        assert_eq!(Clock::wall(&c), Timestamp::from_secs(905));
        assert_eq!(Clock::monotonic(&c), Monotonic(5_000_000_000));
        c.set_synchronised(false);
        assert!(!c.synchronised());
    }

    #[test]
    fn system_clock_probe() {
        let c = SystemClock::with_sync_probe(|| false);
        assert!(!c.synchronised());
        assert!(SystemClock::default().synchronised());
        let a = c.monotonic();
        assert!(c.monotonic() >= a);
        assert!(c.wall() > Timestamp::from_secs(1_700_000_000));
    }

    #[test]
    fn boot_ids() {
        assert_eq!(
            parse_uuid("00112233-4455-6677-8899-aabbccddeeff"),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
        assert_eq!(parse_uuid("not a uuid"), None);
        let b = FixedBootId::new(Some([2; 16]));
        b.set(None);
        assert_eq!(b.current(), None);
        if cfg!(target_os = "linux") {
            assert!(SystemBootId.current().is_some());
        }
    }

    #[test]
    fn tz_releases() {
        let tz = FixedTz::new("2026a");
        let r = tz.current();
        assert_eq!(r.version(), "2026a");
        let summer: Timestamp = "2026-07-01T00:00:00Z".parse().unwrap();
        assert_eq!(r.offset_at("Europe/London", summer), Some(3_600));
        let next = r
            .next_transition("Europe/London", summer)
            .expect("a transition");
        assert_eq!(next, "2026-10-25T01:00:00Z".parse().unwrap());
        assert!(r.canonical("Not/AZone").is_none());
        tz.set_version("2026b");
        assert_eq!(tz.current().version(), "2026b");
        assert_eq!(r.version(), "2026a", "a release is a whole");
        assert_eq!(SystemTz::default().current().version(), "embedded");
    }

    #[test]
    fn entropy() {
        let mut a = [0u8; 12];
        let mut b = [0u8; 12];
        SystemEntropy.fill(&mut a);
        SystemEntropy.fill(&mut b);
        assert_ne!(a, b);
        let f = FixedEntropy::new(7);
        let mut c = [0u8; 8];
        f.fill(&mut c);
        assert_eq!(u64::from_le_bytes(c), 7);
    }
}
