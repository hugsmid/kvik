# kvik interface, v1 outline (design; not yet implemented)

The shape of the public Rust interface. Names and signatures are proposals to be checked for implementability
before any code depends on them; the rules they must obey are in [`spec.md`](spec.md). Code blocks are
illustrative and are not compiled. Sections marked as accepted at gate 3.3 are what roadmap section 4 builds;
the rest is filled in by the roadmap items named.

## Principles
- **One writer handle per store directory;** any number of read-only handles, in the same or another process.
- **No threads the caller cannot see or stop:** the engine's threads are the ones listed under Threads, started at
  open and joined at shutdown. The interface is runtime-neutral: the engine uses no async runtime and exposes none.
  Shutdown is explicit and waits for a final sync.
- **Nothing blocks the writer.** Consumer code, queries and compaction never run on the commit path.
- **Errors are typed.** A refusal (per reading: invalid value, retired series) is distinct from waiting
  (back-pressure) and from a fault (failed write or sync); a fault makes the store read-only.
- **Everything the engine depends on from outside is a trait** with a production and a test implementation:
  clock, file system, backup sink, boot identity, tzdata, entropy.
- **Stable shapes.** Every public enum and every configuration or result struct is `#[non_exhaustive]`; structs a
  caller builds have `Default` or a constructor with setters; no third-party type (runtime, time library, `bytes`)
  appears in a signature, only std types. Conversions to a time library's types sit behind an optional feature.

## Threads (accepted at gate 3.3)
- Futures returned by the interface are runtime-neutral: the engine's threads complete them, and they can be
  awaited on any executor or blocked on.
- The engine owns these named threads, started by `Store::open` and joined by `shutdown`; there are no others:
  `kvik-writer` (validate, batch, write, sync, mark update and mark timer, free-space check), `kvik-maint` (segment
  creation ahead of use, sealing; later compaction, tiering, checkpoints and uploads), `kvik-deliver` (from roadmap
  5.2: routing, funnel timers, cursor writes). A `Reader` owns no thread.
- `Store` calls that wait are `async`; `Reader` calls are blocking file I/O (Reader processes, Reading). The model
  backend may implement the same interface with no threads.

## Opening
```rust
let store = Store::open(
    StoreConfig::new(path, tz)        // tz: the site zone at creation; checked at every later open
        .create_if_missing(true)       // creates only in a missing or empty directory (format.md A.12)
        .log(LogConfig::default())     // segment size, seal interval, min_sync_interval, batch size, mark sync
        .limits(Limits::default())     // queue sizes, series ceiling, disk marks
        .tiers(TierConfig::default())  // cold path, age and fill thresholds
        .env(Env::system()),           // clock, file system, boot identity, tzdata, entropy
).await?;
let reader = Reader::open(ReaderConfig::new(path))?;   // read-only; no lock; may be another process; blocking
store.shutdown().await?;                               // final sync, checkpoint, release the lock
```
`Store::open` runs recovery and returns only when the log is consistent. It fails if another writer holds the
directory, also in the same process (`OpenError::Locked`), if the store id of a file does not match, or if a file has
a newer major format version or an unknown critical element. It also resumes a directory part-way through a restore
(roadmap 8.3).

`time_zone` is the site zone at creation; at every later open it must equal the zone in force after resolving links,
else `OpenError::SiteZoneMismatch`. The zone is changed by `store.set_site_zone(zone, effective_from).await` (a
site-zone entry, spec §5.7), and then in the configuration.

**tzdata** (spec §5.7). The writer reads the host's release through `Env::tz` at open, about once a minute and at
each local midnight. Whenever its release is not the version in force it writes a tzdata entry naming its own, at
open before it accepts ingest and later without a restart, and rebuilds the affected calendar days. No open is
refused for a tzdata release: a reader, or a restore drill, computes calendar results with its own release from the
UTC data, using a stored calendar node only where the node's recorded version is its own release. A writer that
cannot write, and values other than calendar results in a reader of another release, are open (Q-93 in
`model/OPEN-QUESTIONS.md`).

## Configuration (accepted at gate 3.3)
The keys roadmap section 4 needs; all may change between opens.

| Key | Type | Default | Limits |
|---|---|---|---|
| `log.segment_size` | `u64` bytes | 64 MiB | 16 MiB – 1 GiB, a multiple of 4,096; applies to new segments |
| `log.seal_interval` | `Duration` | 5 min | > 0 |
| `log.min_sync_interval` | `Duration` | 10 ms | spec §3 Group commit: log syncs start at least this far apart, and a batch takes in what arrives meanwhile; the first reading after a quiet period is synced at once; 0 disables the bound. Mark syncs, timed or forced, are not bounded by it |
| `log.max_batch_bytes` | `u64` | 1 MiB | 64 KiB – 8 MiB |
| `log.mark_sync` | `MarkSync::Interval(Duration)` or `MarkSync::EveryCommit` | `Interval(1 s)` | `Interval(d)` is spec §3's `mark_sync_interval` (`format.md` A.10), the same key; forced to `EveryCommit` without a boot identity; `EveryCommit` closes the remount limit of spec §3 Recovery rule 1. Batches holding an entry that is not a fact force a mark sync at once either way |
| `limits.ingest_queue` | `usize` readings | 65,536 | ≥ 1,024; a call is at most `min(65,536, ingest_queue)` readings |
| `limits.series_ceiling` | `u32` | 100,000 | |
| `limits.disk_low` | fraction and bytes | 5 %, 2 GiB | |
| `limits.disk_critical` | `u64` bytes | 512 MiB | |
| `limits.future_tolerance`, `limits.default_lateness_bound` | `Duration` | 60 s, 5 min | |

Keys of later sections (consumer queue, tiers, tree, leases, backup, `mirror_path`) are listed by their items.

## Core types (accepted at gate 3.3)
```rust
pub struct Timestamp(pub i64);     // ns since the Unix epoch, UTC
pub struct Span(pub i64);          // signed ns; window offsets relative to now, either sign
pub struct Position { pub generation: u32, pub sequence: u64 }     // Ord; (g, 0) = before generation g
pub struct SeriesId(pub u32);
pub enum Quality { Good, Estimated, Suspect, Bad }
pub enum Value { F64(f64), Decimal { units: i64, scale: u8 }, State(u16), Bool(bool), Text(String), Absent }   // Decimal = units × 10^−scale
pub struct TimeRange { pub from: Timestamp, pub to: Timestamp }    // half-open [from, to), event time
pub struct LocalDate { pub year: i16, pub month: u8, pub day: u8 }
pub struct LocalTime(/* ns since local midnight */);
pub struct ZoneName(String);       // an IANA name; checked against the tzdata release where used
```
- A decimal series takes `Value::Decimal` or `Value::F64`. An `F64` is first converted to its shortest decimal
  representation (0.1 + 0.2 gives 0.3 at scale 4); a value with more decimal places than the series' current scale is
  then handled by its `ExcessPrecision` policy (spec §2.1): rounded half to even and flagged `rounded` (the default),
  refused with `Refusal::ExcessPrecision { scale_needed }`, or the scale raised up to a maximum. Writers should round
  to the precision they actually need; raw sensor floats belong in an `f64` series, which keeps the value bit-exact.
  `Absent` is "no value" (spec §2.5): it is recorded with quality bad whatever quality is supplied, and the supplied
  quality is kept for the duplicate rule; to record that a state is unknown, declare a state name for it. NaN and
  ±∞ are recorded as quality bad with no value, flagged and counted (spec §2.5), on a decimal series as on any
  other. A finite value too large for `i64` at the series' scale is open
  (Q-102 in `model/OPEN-QUESTIONS.md`).
- Configuration durations are `std::time::Duration`; `Span` is used only where a sign is needed.

## Errors (accepted at gate 3.3)
```rust
pub enum Refusal {                 // per reading, in Ack; each counted per series
    UnknownSeries, Retired, WrongType, StateOutOfRange, TextTooLong, ChangedAtMissing, ChangedAtUnexpected,
    ReviseOnVersioned, OffPhase, NotDeclared, ExcessPrecision { scale_needed: u8 } }
pub enum Wait { QueueFull, DiskFull, UnsyncedClock }                // back-pressure: not a refusal, not a fault
pub enum EmitError {               // per call
    Busy(Wait),                    // try_emit only: nothing was queued
    TooLarge { readings: usize, bytes: usize },   // over the call limits (Writing); nothing was queued
    ReadOnly(Fault), Closed,       // nothing was queued
    OutcomeUnknown(Fault) }        // the call's batch may or may not be stored; never acknowledged; re-send after restart
pub enum OpenError { Locked, NotAStore, StoreIdMismatch { file }, UnsupportedFormat { file, found, supported },
    SiteZoneMismatch { configured, in_force }, UnknownZone, Corrupt { file }, UnsupportedPlatform { reason },
    MarkUnreadable, Config { key, reason }, Io { path, source } }     // MarkUnreadable: Reader::open only (format.md A.10)
pub enum Fault { Io { op, path, kind }, SyncFailed { path }, ManifestCorrupt { epoch }, NoSpace { path } }
pub enum WriteError { ReadOnly(Fault), Closed, OutcomeUnknown(Fault) }      // any call that appends an entry
pub enum DefineError { Ceiling { limit }, NameInUse, Conflict { field }, Invalid { field, reason }, UnknownZone,
    UnknownSeries, Retired, ScaleNotRaised { current: u8 }, ScaleOverflow { largest_magnitude: i64 }, Write(WriteError) }
pub enum QueryError { UnknownSeries, DetailNotLocal { local_from: Option<Timestamp> }, AggregateRefused { reason },
    UnknownZone, NotAvailable { reason }, LostRange { id: Position }, MarkUnreadable, UnsupportedFormat { file }, Corrupt { file },
    Io { path, source } }
```
Funnel compilation errors (`RouteCycle` among them) and backup errors are added by roadmap 5.2 and 8.2.

## Handles and life cycle (accepted at gate 3.3)
- `Store` is `Clone + Send + Sync`; clones share the one writer. `Consumer` is `Send`, not `Clone`, and owns a share
  of the store. `Reader` is `Send + Sync` and owns its files; `store.reader()` gives one in process.
- `shutdown(&self).await`: new calls on any clone return `Closed`; calls already queued are committed and
  acknowledged; then the final commit, mark sync and cursor sync; the threads are joined and the lock released.
  Consumers' `next()` returns `None`. On a read-only store nothing is synced; the threads are joined, the lock
  released and the fault returned.
- Dropping the last clone without `shutdown` asks the threads to stop and neither waits nor syncs; it is equivalent
  to a process crash (nothing acknowledged is lost; the lock is released when the writer thread ends).
- A second `Store::open` on the same directory, also in the same process, fails with `OpenError::Locked`; the engine
  keeps an in-process table of open store paths besides the operating-system lock.

## Series
```rust
let id: SeriesId = store.define_series(
    SeriesSpec::new("plant/line-1/temperature",
                    Kind::Gauge { value: ValueType::Decimal { scale: 1 } },   // ValueType::decimal() declares scale 4
                    "°C")
        .expected_interval(Duration::from_secs(1))
        .excess_precision(ExcessPrecision::Round),   // default; or Refuse, or AutoRaise { max_scale }
).await?;
store.set_parameters(id, effective_from, ParameterChange::default().staleness(s)).await?;
store.raise_scale(id, 3).await?;              // from this entry's position on; never lowered
store.alias(id, "plant/line-1/temp").await?;
store.retire(id, t_r).await?;
let info = store.series(id)?;                 // also by name
```
These calls, and route changes, resolve only after the durable mark has been synced past their entry (spec §3
Commit). The writer forces that mark sync at once rather than waiting for the timed interval, so each call waits for
one mark sync, not for up to the interval. Defining a series that already exists with the same specification
returns its id. A different kind, value type or unit, or a lower scale, under the same name is an error; the caller
retires and defines anew. A higher scale is set with `raise_scale`, which appends a scale raise entry (spec §2.1
`decimal scale`); it fails with `ScaleNotRaised` unless the new scale is higher, and with `ScaleOverflow` if the
largest magnitude stored so far would overflow `i64` at it. Earlier facts keep their scale and are returned rescaled
exactly to the current one.

An interval series (spec §2.1) has a fixed length or a calendar length, a local day or month in a zone fixed for the
series; a day may start at a declared local time instead of midnight:
```rust
kind: Kind::Interval { value: ValueType::Decimal { scale: 3 }, length: IntervalLength::Calendar {
    unit: CalendarUnit::Day { start: LocalTime::from_hms(5, 0, 0) },   // default midnight
    zone: zone_name,                                                  // the interval zone, a ZoneName
} },
enum IntervalLength { Fixed { d: Duration, phase: Duration }, Calendar { unit: CalendarUnit, zone: ZoneName } }
enum CalendarUnit { Day { start: LocalTime }, Month }
```
`SeriesInfo` carries `latest_t` and the optional `lateness_reference` (spec §2.2).

## Writing (accepted at gate 3.3)
```rust
pub async fn emit(&self, readings: &[Reading]) -> Result<Ack, EmitError>;           // waits under back-pressure
pub fn try_emit(&self, readings: &[Reading]) -> Result<Pending, EmitError>;         // never waits; Err(Busy(w))
pub struct Pending;  /* impl Future<Output = Result<Ack, EmitError>> */              // may be dropped
pub struct Ack { pub outcomes: Vec<Outcome> }                                        // one per reading, by index
pub enum Outcome { Recorded { position: Position, outranked: bool, unchecked: bool, rounded: bool }, Duplicate, Refused(Refusal) }
impl Ack { pub fn first(&self) -> Option<Position>; pub fn last(&self) -> Option<Position>;
           pub fn duplicates_dropped(&self) -> u32; pub fn refused(&self) -> impl Iterator<Item = (usize, Refusal)>; }
pub struct Reading { pub series: SeriesId, pub t: Timestamp, pub value: Value, pub quality: Quality,
                     pub source: TimeSource, pub changed_at: Option<u64> }           // Reading::new(series, t, value)
store.emit_with(&readings, Intent::Revise).await?;
```
- **One call is one batch unit:** its readings are committed in one batch or not at all, in order. A call is at
  most `min(65,536, limits.ingest_queue)` readings and 8 MiB plain-encoded (`EmitError::TooLarge` otherwise; the
  caller splits). Several calls may share a batch. Calls from different tasks are ordered by commit.
- Outcomes are decided at commit (a retirement committed while a reading waits refuses it). `emit` and `Pending`
  resolve after the batch's sync and the mark update, and, for a batch holding an entry that is not a fact, after
  the mark sync, which the writer forces at once. Batches resolve in commit order, so a batch of facts committed after
  a batch holding any other entry also waits for that one mark sync (spec §3 Commit).
- Individual readings may be refused (`Refusal`, reported per index); a non-finite value is recorded, not refused.
  A reading of an interval series whose `t` is not an interval start is refused with `Refusal::OffPhase`.
- **Back-pressure** is a wait, with its reason: `try_emit` returns `Busy(Wait)`, including `UnsyncedClock` when the
  call holds a host-stamped reading; `emit` waits, and `stats().ingest.waiting` and health name the reason. There is
  no timeout parameter: a caller that must not wait uses `try_emit`.
- **Cancellation:** dropping an `emit` future or a `Pending` cancels nothing. Readings already queued still commit,
  and their outcomes are then visible only in statistics; the caller cannot tell whether they were queued and treats
  the call as unacknowledged (it re-sends; the duplicate rule absorbs the repeat).
- After a fault the store is read-only; a call whose batch may or may not be stored returns `OutcomeUnknown` and is
  never acknowledged (spec §3 Write or sync failure).

## Consuming (funnels)
```rust
let mut consumer = store.consumer("load-shedding", Start::Now, Select::None).await?;   // emits: the series it may emit
consumer.route(Funnel {
    series: Select::Names("plant/*/power".into()),
    operator: Some(Operator::rolling(Aggregate::Avg, Duration::from_secs(600))),
    condition: Some(Condition::above(3_000.0).clear_below(2_500.0)),
}).await?;

while let Some(d) = consumer.next().await {
    match d.kind {
        DeliveryKind::Fact(f) => { /* f.position, f.series, f.t, f.value */ }
        DeliveryKind::Trigger(t) => { /* t.funnel, t.state, t.value, t.coverage */ }
        DeliveryKind::Resync(s) => { /* drop positions above s.forget_above, durably; then confirm the last part */ }
        DeliveryKind::GenerationChanged { from, to } => { /* after a restore */ }
    }
    consumer.confirm(d.id).await?;            // advances the durable cursor
}
pub struct ResyncPart { pub cause: ResyncCause /* Resume | ResetPoint | GenerationChanged | CursorBehindLog */,
    pub forget_above: Option<Position>,       // Some at a reset point: drop every position above it
    pub part: u32, pub last: bool,            // a resync is 1..n deliveries
    pub funnels: Vec<FunnelState> }           // at most 256 per part; empty unless it ends a resume
pub struct FunnelState { pub funnel: FunnelId, pub series: SeriesId, pub live: Live, pub condition: Option<ConditionState> }
enum ConditionState { True, False, Unknown }
enum Live { Value(Fact), None, Unknown }
```
- A resync is bounded per delivery: state for many series arrives as several parts, all evaluated at one position
  and all carrying the same `cause` and `forget_above`. The consumer confirms the delivery id of the `last` part
  (confirming an earlier part does nothing); nothing further is delivered until then. `funnels` carries one
  `FunnelState` per funnel and series on the resync that ends a resume (spec §4 Resync). Details in roadmap 5.3.
- During a restore a condition evaluation with no local detail, and live state whose fact may not be local yet (spec §2.3), are
  `Unknown` (spec §2.3, §6 Conditions): the evaluation is skipped and counted (`evaluations_unknown`), the latch is
  unchanged, the resync carries `Unknown` for that funnel, and the first evaluation with detail is delivered as a
  `Trigger` if it differs from the latched state, or there is none.
- A delivery carries `id` (a position for a fact delivery; funnel and sequence for a condition delivery, on a fact
  or on the timer, valid until the next resync), `flags` (`replayed`,
  `future`; on a trigger also `approximate_edges` and `restoring`) and the times needed to judge its age.
- `Start::Position(p)` begins with a resync whose `forget_above` is `p`; how `p` is checked against a truncation or
  restore after it was read is open (Q-46 in `model/OPEN-QUESTIONS.md`).
- `confirm` is cheap and batched; the cursor is written at most once a second, on shutdown and when a resync
  carrying `forget_above` is confirmed.
- A resync may carry `forget_above` = X: the consumer must drop every position above X from its de-duplication
  memory, its own last confirmation included, because such a position can now name a different fact (spec §3
  Recovery rule 3, §4 At-least-once). It confirms that resync only when the forgetting is durable wherever that
  memory is kept; nothing further is delivered until it does. Records it keeps outside the engine at those positions
  are copies of facts the store may no longer hold; the engine never asks for them to be deleted and does not protect
  them. A resync without `forget_above` requires no forgetting of positions.
- A consumer that emits readings uses its own handle, `consumer.try_emit(&readings)`, or its own task, never the
  delivery callback. A reading on a series outside the consumer's `emits` is refused with `Refusal::NotDeclared` and
  counted.
- Routes may be added and removed while running; each change is a log entry and takes effect at its position. A
  route, a change of `emits` or an alias (`store.alias`) that closes a cycle between consumers is refused with the
  error `RouteCycle`, naming the cycle (spec §4 Routing). Whether two name patterns overlap must be decidable from
  the two patterns alone; the pattern grammar is not yet defined (`model/OPEN-QUESTIONS.md`, Q-88).

## Reader processes (accepted at gate 3.3)
```rust
pub fn Reader::open(cfg: ReaderConfig) -> Result<Reader, OpenError>;   // { dir, env, lease_refresh: 30 min }; blocking
pub fn refresh(&self) -> Result<ReaderState, QueryError>;              // re-reads mark, manifest tail and new entries; renews the lease
pub struct ReaderState { pub durable: Position, pub epoch: u64, pub generation: u32, pub lease_renewed_at: Timestamp }
```
- Every read call first reads the mark and the entries appended since the reader's last call, so it sees data up to
  the mark at call time. The lease is written at open and renewed by any call once half of `lease_refresh` has
  passed, or by `refresh()`; a long iteration renews it as it advances. A reader idle past the expiry takes a new
  epoch on its next call. There is no background task.
- Roadmap 4.6 ships `facts`, series lookup and `refresh` for reader processes. `live` and windows in a reader process
  (they need a tree checkpoint plus the log since it) are specified with roadmap 5.4 and 7.1; until then they return
  `QueryError::NotAvailable { reason }` rather than a stale value.
- `reader.health()` reports what the files hold (loss reports), not the writer's counters.

## Reading
```rust
let now = reader.live(id)?;                                   // latest valid value, with staleness; Live::Unknown (spec §2.3)
let w = reader.window(id, Window::rolling(Duration::from_secs(600)), Aggregate::Avg)?;   // also Window::between(Span, Span)
// WindowResult { value, coverage, from, to, flags }
let series = reader.buckets(id, range, Resolution::Auto { max_points: 2_000 })?;   // chart query: Vec<Bucket>
// Bucket { from, to, count: u64, min: Option<Value>, max: Option<Value>, mean: Option<f64>, coverage, flags }
let day = reader.calendar(id, date, Aggregate::Integral)?;   // site time zone
let day = reader.calendar_in(id, date, zone, Aggregate::Integral)?;   // a named IANA zone for the whole span
let (start, end) = reader.interval(id, t)?;                 // the interval of an interval series' reading at t

pub fn facts(&self, id: SeriesId, range: TimeRange, view: View) -> Result<Facts, QueryError>;   // accepted at gate 3.3
pub struct Facts;   /* impl Iterator<Item = Result<FactRow, QueryError>> */   // streams block by block, bounded memory
pub enum View { Current, AsOf(Position), AsOfTime(Timestamp), History }
pub struct FactRow { pub series, pub t, pub position, pub received_at, pub value, pub quality,
                     pub supplied_quality, pub flags: FactFlags, pub changed_at: Option<u64> }
pub fn at(&self, id: SeriesId, t: Timestamp, view: View) -> Result<Option<FactRow>, QueryError>;   // the fact in force at t
```
- `at` returns the fact with the greatest event time ≤ `t` that a window can credit (not bad quality, not displaced),
  as `view` selects it (for a versioned series, the version the view selects), with its `changed_at`; `None` if there
  is none. It lets a client show, for example, the issue time of the value a window's leading stretch carries in.
  The leading-edge credit (spec §6 Edge stretches) uses that fact only while `t` is within its S; `at` returns it
  regardless, and the caller compares `t` with the fact's `t` + S (S from `series()`).
- Read calls are synchronous and blocking; an asynchronous caller runs them on its blocking pool. Ranges are
  half-open in event time.
- `Resolution::Auto` picks the coarsest stored level that yields at most `max_points`, and raw facts when the
  range is short enough. Every bucket carries count and coverage so a chart can show gaps. `min` and `max` are over
  the readings in the bucket only (spec §5.2), so they are `None` when no reading starts in it, while `mean` (for a
  gauge, time-weighted: integral ÷ covered seconds, spec §6 Aggregates) can still be present from a value carried in
  across the bucket's start.
- Data is stored in UTC; a zone only defines calendar days and months. `calendar` takes site-zone days as spec §5.7
  defines them, also across a zone change; `calendar_in` and calendar windows that name a zone use that zone
  throughout, evaluated as a window over the UTC data (spec §5.7).
- `interval` is an error for a displaced reading (spec §5.4).
- Every result type carries the flag `restoring` while a restore is bringing detail back (spec §8 Restore);
  `WindowResult.flags` also carries `approximate_edges`, with `from` and `to` giving the span evaluated (spec §6
  Old spans). A window with no span over local detail is the error `DetailNotLocal`, never an empty result. On an
  interval series it also carries `apportioned` and `displaced` (spec §5.4, §6).
  Whether `facts` fetches detail that is not local itself is decided with roadmap 8.2; in section 4 all detail is
  local.
- All reads see only durable data. A reader in another process sees data up to the durable mark.
- A reader that reads a recovery entry drops what it took above the entry's `after` (spec §7 Readers). A call that
  checks a position kept for later use against a later truncation or restore is open (Q-46 in `model/OPEN-QUESTIONS.md`).
- A query engine integration (a table provider) is layered on these calls and is not part of the core crate.

## Health and statistics (types accepted at gate 3.3)
```rust
pub enum Health { Ok, Degraded { reasons: Vec<Reason> }, ReadOnly { cause: Fault, reasons: Vec<Reason> } }
pub enum Reason { DataLost(LossReport), IngestWaiting(Wait), DiskLow { free: u64 }, DiskCritical { free: u64 },
    SealBlocked { file_id: u64 }, ManifestRebuilt { epoch: u64 /* the rebuild record's own epoch */ } }
pub struct LossReport { pub identity: Position, pub kind: LossKind /* DataLost | PossibleLoss */,
    pub cause: LossCause /* Corruption | TruncatedTail | LostInBucket | InvalidContent */,
    pub first: Option<Position>, pub last: Option<Position>, pub file_id: Option<u64>, pub bytes: u64,
    pub readable_commit_blocks: u32, pub mark_state: MarkState /* Readable | Missing | Unreadable | Stale */,
    pub boot_id: Option<[u8; 16]>, pub commit_time_before: Option<Timestamp>, pub commit_time_after: Option<Timestamp>,
    pub cursors_missing: Vec<String>, pub recorded: bool, pub acknowledged: bool }
pub fn losses(&self) -> Vec<LossReport>;                    // every loss, for ever; Store and Reader
pub async fn acknowledge_losses(&self, identities: &[Position]) -> Result<(), WriteError>;
pub async fn acknowledge_manifest_rebuild(&self, epoch: u64) -> Result<(), WriteError>;   // the rebuild record's epoch; spec §7 Manifest
pub struct ClockJump { pub at: Timestamp, pub by: Span }
pub struct Stats { pub ingest: IngestStats, pub log: LogStats, pub disk: DiskStats, pub clock: ClockStats }
// IngestStats { queue_depth, waiting: Option<Wait>, waited: Duration, batches, syncs, commit_latency: Latency { p50, p99, max } }
// LogStats { durable: Position, mark_synced: Position, mark_sync: MarkSyncMode /* Interval | EveryCommit { reason } */,
//            open_file_id, sealed_segments, bytes, manifest_rebuilds }   ClockStats { jumps, last: Option<ClockJump> }
pub fn series_stats(&self, id: SeriesId) -> Option<SeriesStats>;   // recorded, duplicates_dropped, duplicates_unchecked,
                                                                   // refused (per Refusal), outranked_on_arrival, late, future, orphaned_facts,
                                                                   // rounded, scale_raises, absent_supplied
let facts = store.export_orphaned(range)?;     // orphaned facts (spec §2.1), for re-ingest
```
- A loss's identity is the first position of its range when it was first reported. Reports are read from the
  manifest and the recovery entries at every start. A `DataLost` holds health `Degraded` until it is acknowledged; a
  `PossibleLoss` is listed by `losses()`, counted, never degrades and needs no acknowledgement (spec §8 Acknowledging
  a loss, §11 Health). `acknowledge_losses` is refused while read-only and in a drill. A corrupt manifest record with
  no copy is rebuilt from the local files and reported as `ManifestRebuilt` until `acknowledge_manifest_rebuild`,
  which is refused while read-only and in a drill as `acknowledge_losses` is, and whose torn acknowledgement record is
  given again as for `acknowledge_losses` (spec §7 Manifest);
  while deep cannot be reached to tell whether a copy exists, it is the fault `ManifestCorrupt` (spec §7 Manifest).
  `SealBlocked` lasts until the seal succeeds. A mark synced at every commit (no boot identity, or configured) is
  reported in `LogStats`, not as a health reason, so a platform without a boot identity is not degraded for ever.
- Clock jumps are statistics, each with its size; they are not a health reason.
- Every refusal, drop, repeat and delay the specification says is counted appears here. Statistics include, per
  calendar-length interval series, `interval_displaced`, the readings found displaced (spec §5.4), and, per funnel,
  `evaluations_unknown`, the evaluations skipped during a restore for lack of local detail (spec §6 Conditions), and
  per condition funnel `transitions_not_delivered` while its consumer caught up (spec §4 Overflow), and restore
  progress (files and bytes remaining), which is not a health condition (spec §8 Restore). Health reasons added by
  their roadmap items (the enums are non-exhaustive): `restore_failed` (naming the file), the set-aside range of each
  restore record, a tzdata change with both versions and the days rebuilt, a series whose interval zone the tzdata
  version in force does not know (spec §2.1, §5.7), per-consumer and backup reasons.

## Backup and restore
```rust
trait BackupSink {
    async fn put(&self, key: &str, bytes: Bytes, sha256: [u8; 32]) -> Result<Stored>;
    async fn stat(&self, key: &str) -> Result<Option<Stored>>;   // reported hash and size
    async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Bytes>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}
store.attach_backup(sink, Encryption::Key(key)).await?;
let end: Position = Store::restore(dir, sink, Encryption::Key(key), RestoreMode::SummariesFirst).await?;
let facts = store.export_set_aside(range)?;   // set-aside facts, each with its series name, for re-ingest
store.rebuild(Rebuild::Derived).await?;       // recompute every derived structure from facts
```
The sink has no delete. An S3-compatible implementation lives in a separate crate. The sink's signatures are
settled with roadmap 8.2, under the stable-shapes and runtime-neutral principles.

## Environment seams (accepted at gate 3.3)
```rust
pub struct Env { pub clock: Arc<dyn Clock>, pub fs: Arc<dyn Fs>, pub boot: Arc<dyn BootId>,
                 pub tz: Arc<dyn TzSource>, pub entropy: Arc<dyn Entropy> }       // Env::system()
pub struct Monotonic(pub u64);       // ns from an origin fixed per process; never stored, never compared across processes
pub trait Clock: Send + Sync { fn wall(&self) -> Timestamp; fn monotonic(&self) -> Monotonic; fn synchronised(&self) -> bool; }
pub trait BootId: Send + Sync { fn current(&self) -> Option<[u8; 16]>; }
pub trait Entropy: Send + Sync { fn fill(&self, out: &mut [u8]); }               // store id, reader id
pub trait TzSource: Send + Sync { fn current(&self) -> Arc<dyn TzRelease>; }      // the host's release now
pub trait TzRelease: Send + Sync {
    fn version(&self) -> &str;                                                  // the release name
    fn canonical(&self, zone: &str) -> Option<String>;                          // resolves links; None = unknown
    fn offset_at(&self, zone: &str, t: Timestamp) -> Option<i32>;               // seconds east of UTC
    fn next_transition(&self, zone: &str, after: Timestamp) -> Option<Timestamp>;
}
pub enum OpenMode { Read, ReadWrite, CreateNew }
pub trait Fs: Send + Sync {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn FsFile>>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;               // DirEntry { name: String, len: u64 }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;                 // replaces `to`
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}
pub trait FsFile: Send + Sync {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;              // writes all of buf
    fn sync_data(&self) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
    fn set_len(&self, len: u64) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn try_lock_exclusive(&self) -> io::Result<bool>;                           // false = held elsewhere
}
```
- There is no memory mapping: every byte the engine persists passes a seam call, so the test file system can drop
  unsynced writes (the mark's included) at a simulated power loss and model Linux's failed writeback per page.
- `TzSource::current` returns one release as a whole, so a computation never mixes two releases. The writer calls
  it at open, about once a minute and at each local midnight (spec §5.7); a reader calls it only at open, so a
  long-lived `Reader` sees a host tzdata update only when it is opened again (accepted).
- `Clock::synchronised` has no portable source without unsafe code: the system clock reports `true` unless the
  application supplies a probe (`SystemClock::with_sync_probe(impl Fn() -> bool)`); an accepted limit, documented.
- The test implementations inject crashes at every write and sync, failed syncs, a full disk, clock jumps, a
  changed boot identity and a changed tzdata release. The engine is tested against the reference model through these
  seams.

## Not in v1
A network server, SQL, multi-writer access, clustering, percentiles, conditions over several series, interval
readings whose length is neither fixed nor a local day or month (irregular billing periods), a view as of an issue
time (`changed_at`), forecast-accuracy operators (computed by the reader from `View::History`), and any form of
deletion.
