# kvik interface, v1 outline (design; not yet implemented)

The shape of the public Rust interface. Names and signatures are proposals to be checked for implementability
before any code depends on them; the rules they must obey are in [`spec.md`](spec.md). Code blocks are
illustrative and are not compiled.

## Principles
- **One writer handle per store directory;** any number of read-only handles, in the same or another process.
- **No hidden threads the caller cannot see or stop.** The engine runs its tasks on a runtime the caller
  provides; shutdown is explicit and waits for a final sync.
- **Nothing blocks the writer.** Consumer code, queries and compaction never run on the commit path.
- **Errors are typed.** A refusal (invalid value, retired series, back-pressure) is distinct from a fault
  (failed write or sync), and a fault makes the store read-only.
- **Everything the engine depends on from outside is a trait** with a production and a test implementation:
  clock, file system, backup sink, boot identity, tzdata release.

## Opening
```rust
let store = Store::open(StoreConfig {
    dir: path,
    time_zone: tz,                 // required: calendar levels use it
    log: LogConfig::default(),     // segment size, seal interval, linger
    limits: Limits::default(),     // queue sizes, series ceiling, disk marks
    tiers: TierConfig::default(),  // cold path, age and fill thresholds
    env: Env::system(),            // clock, file system, boot identity, tzdata release
}).await?;
let reader = Reader::open(path).await?;      // read-only; no lock; may be another process
store.shutdown().await?;                     // final sync, checkpoint, release the lock
```
`Store::open` runs recovery and returns only when the log is consistent. It fails if another writer holds the
directory, if the store id of a file does not match, or if a file has a newer major format version. A writer whose
tzdata release is not the version in force writes a tzdata entry before it accepts ingest (spec §5.7); a reader, a
restore drill or a writer that cannot write fails with `TzdataMismatch` instead, and an open reader stops with it when
a later entry names another version.

## Series
```rust
let id: SeriesId = store.define_series(SeriesSpec {
    name: "plant/line-1/temperature".into(),
    kind: Kind::Gauge { value: ValueType::Decimal { scale: 1 } },
    unit: "°C".into(),
    expected_interval: Some(Duration::from_secs(1)),
    ..Default::default()
}).await?;
store.set_parameters(id, effective_from, ParameterChange { .. }).await?;
store.alias(id, "plant/line-1/temp").await?;
store.retire(id, t_r).await?;
let info = store.series(id)?;                 // also by name
```
These calls, and route changes, resolve only after the durable mark has been synced past their entry (spec §3
Commit). The writer forces that mark sync at once rather than waiting for `mark_sync_interval`, so each call waits
for one mark sync, not for up to the interval. Defining a series that already exists with the same specification
returns its id. A different kind, value type,
scale or unit under the same name is an error; the caller retires and defines anew.

An interval series (spec §2.1) has a fixed length or a calendar length, a local day or month in a zone fixed for the
series; a day may start at a declared local time instead of midnight:
```rust
kind: Kind::Interval { value: ValueType::Decimal { scale: 3 }, length: IntervalLength::Calendar {
    unit: CalendarUnit::Day { start: LocalTime::from_hms(5, 0, 0) },   // default midnight
    zone: zone_name.into(),                                           // the interval zone
} },
enum IntervalLength { Fixed { d: Duration, phase: Duration }, Calendar { unit: CalendarUnit, zone: String } }
enum CalendarUnit { Day { start: LocalTime }, Month }
```
`SeriesInfo` carries `latest_t` and the optional `lateness_reference` (spec §2.2).

## Writing
```rust
let ack: Ack = store.emit(&[Reading { series: id, t, value, quality, source: TimeSource::Device }]).await?;
// Ack { first: Position, last: Position, duplicates_dropped: u32, refused: Vec<(usize, Refusal)> }
store.try_emit(&readings)?;                   // never waits; Err(Busy) under back-pressure
store.emit_with(&readings, Intent::Revise).await?;
```
- `emit` resolves only after the batch is synced and the durable mark has moved. Batches resolve in commit order, so
  a batch of facts committed after a batch holding any other entry also waits for that batch's mark sync, which is
  forced at once (spec §3 Commit). It waits under back-pressure.
- A batch is all-or-nothing for durability, but individual readings may be refused (invalid series, retired
  series, a non-finite value recorded as bad quality is not a refusal). Refusals are reported per index.
- Readings in one call keep their order; calls from different tasks are ordered by commit.
- A reading of an interval series whose `t` is not an interval start is refused with `Refusal::OffPhase`.

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
        DeliveryKind::Resync(s) => { /* drop positions above s.forget_above, durably; then confirm */ }
    }
    consumer.confirm(d.id).await?;            // advances the durable cursor
}
// Resync { forget_above: Option<Position>, funnels: Option<Vec<FunnelState { funnel, live: Live, condition: Option<ConditionState> }>> }
// funnels is Some only on the resync that ends a resume (spec §4 Resync)
enum ConditionState { True, False, Unknown }
enum Live { Value(Fact), None, Unknown }
```
- During a restore a condition evaluation with no local detail, and live state whose fact may not be local yet (spec §2.3), are
  `Unknown` (spec §2.3, §6 Conditions): the evaluation is skipped and counted (`evaluations_unknown`), the latch is
  unchanged, `Resync` carries `Unknown` for that funnel, and the first evaluation with detail is delivered as a
  `Trigger` if it differs from the latched state, or there is none.
- A delivery carries `id` (a position for a fact delivery; funnel and sequence for a condition delivery, on a fact
  or on the timer, valid until the next `Resync`), `flags` (`replayed`,
  `future`; on a trigger also `approximate_edges` and `restoring`) and the times needed to judge its age.
- `Start::Position(p)` begins with a `Resync` whose `forget_above` is `p`; how `p` is checked against a truncation or
  restore after it was read is open (Q-46 in `model/OPEN-QUESTIONS.md`).
- `confirm` is cheap and batched; the cursor is written at most once a second, on shutdown and when a `Resync`
  carrying `forget_above` is confirmed.
- A `Resync` may carry `forget_above` = X: the consumer must drop every position above X from its de-duplication
  memory, its own last confirmation included, because such a position can now name a different fact (spec §3
  Recovery rule 3, §4 At-least-once). It confirms that `Resync` only when the forgetting is durable wherever that
  memory is kept; nothing further is delivered until it does. Records it keeps outside the engine at those positions
  are copies of facts the store may no longer hold; the engine never asks for them to be deleted and does not protect
  them. A `Resync` without `forget_above` requires no forgetting of positions.
- A consumer that emits readings uses its own handle, `consumer.try_emit(&readings)`, or its own task, never the
  delivery callback. A reading on a series outside the consumer's `emits` is refused with `Refusal::NotDeclared` and
  counted.
- Routes may be added and removed while running; each change is a log entry and takes effect at its position. A
  route, a change of `emits` or an alias (`store.alias`) that closes a cycle between consumers is refused with the
  error `RouteCycle`, naming the cycle (spec §4 Routing). Whether two name patterns overlap must be decidable from
  the two patterns alone; the pattern grammar is not yet defined (`model/OPEN-QUESTIONS.md`, Q-88).

## Reading
```rust
let now = reader.live(id)?;                                   // latest valid value, with staleness; Live::Unknown (spec §2.3)
let w = reader.window(id, Window::rolling(Duration::from_secs(600)), Aggregate::Avg)?;
// WindowResult { value, coverage, from, to, flags }
let series = reader.buckets(id, range, Resolution::Auto { max_points: 2_000 })?;   // chart query
let facts = reader.facts(id, range, View::Current)?;         // also View::AsOf(position), View::History
let day = reader.calendar(id, date, Aggregate::Integral)?;   // site time zone
let day = reader.calendar_in(id, date, zone, Aggregate::Integral)?;   // a named IANA zone for the whole span
let (start, end) = reader.interval(id, t)?;                 // the interval of an interval series' reading at t
```
- `Resolution::Auto` picks the coarsest stored level that yields at most `max_points`, and raw facts when the
  range is short enough. Every bucket carries count, min, max and coverage so a chart can show gaps and spikes.
- Data is stored in UTC; a zone only defines calendar days and months. `calendar` takes site-zone days as spec §5.7
  defines them, also across a zone change; `calendar_in` and calendar windows that name a zone use that zone
  throughout, evaluated as a window over the UTC data (spec §5.7).
- `interval` is an error for a displaced reading (spec §5.4).
- Every result type carries the flag `restoring` while a restore is bringing detail back (spec §8 Restore);
  `WindowResult.flags` also carries `approximate_edges`, with `from` and `to` giving the span evaluated (spec §6
  Old spans). A window with no span over local detail is the error `DetailNotLocal`, never an empty result. On an
  interval series it also carries `apportioned` and `displaced` (spec §5.4, §6).
  `facts` over detail that is not local, also during a restore, fetches it on demand.
- All reads see only durable data. A reader in another process sees data up to the durable mark.
- A reader that reads a recovery entry drops what it took above the entry's `after` (spec §7 Readers). A call that
  checks a position kept for later use against a later truncation or restore is open (Q-46 in `model/OPEN-QUESTIONS.md`).
- A query engine integration (a table provider) is layered on these calls and is not part of the core crate.

## Health and statistics
```rust
let h: Health = store.health();     // Ok, Degraded { reasons }, ReadOnly { cause }
let s: Stats = store.stats();       // per series and per consumer counters, commit latency, queue depths,
                                    // time behind, disk marks, backup lag
let l: Vec<Loss> = store.losses();  // every loss ever reported: kind (data_lost, possible_loss), range, bytes,
                                    // cause, commit times, boot id, acknowledged
store.acknowledge_losses(&[loss_id]).await?;   // LossId: first position of the range when first reported
let facts = store.export_orphaned(range)?;     // orphaned facts (spec §2.1), for re-ingest
```
Every refusal, drop, repeat and delay the specification says is counted appears here. Statistics include, per
calendar-length interval series, `interval_displaced`, the readings found displaced (spec §5.4), and, per funnel,
`evaluations_unknown`, the evaluations skipped during a restore for lack of local detail (spec §6 Conditions), and
per condition funnel `transitions_not_delivered` while its consumer caught up (spec §4 Overflow). Health reasons
include `restore_failed` (naming the file), the set-aside range of each restore record, a tzdata change with both
versions and the days rebuilt, and a series whose interval zone the tzdata version in force does not know (spec §2.1,
§5.7); statistics include restore progress (files and bytes remaining), which is not a health condition (spec §8
Restore). Statistics include `orphaned_facts` per id (spec §2.1). Health reasons include `unacknowledged_loss`, one
per `data_lost` not yet acknowledged, and `manifest_corrupt` (fatal, spec §7 Manifest). `Loss.acknowledged` applies
to `data_lost` only: a `possible_loss` is listed and counted, never holds health degraded and needs no
acknowledgement (spec §8 Acknowledging a loss, §11 Health). `acknowledge_losses` is refused while read-only and in a
drill.

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
The sink has no delete. An S3-compatible implementation lives in a separate crate.

## Environment seams (for tests and for unusual platforms)
```rust
trait Clock { fn wall(&self) -> Timestamp; fn monotonic(&self) -> Instant; fn synchronised(&self) -> bool; }
trait Fs { /* open, preallocate, write_at, sync, rename, sync_dir, mmap, free_space */ }
trait BootId { fn current(&self) -> Option<[u8; 16]>; }
trait Tzdata { fn version(&self) -> &str; /* rules by zone name */ }   // read once at open (spec §5.7)
```
The test implementations inject crashes at every write and sync, failed syncs, a full disk, clock jumps and a
changed boot identity. The engine is tested against the reference model through these seams.

## Not in v1
A network server, SQL, multi-writer access, clustering, percentiles, conditions over several series, interval
readings whose length is neither fixed nor a local day or month (irregular billing periods), a view as of an issue
time (`changed_at`), forecast-accuracy operators (computed by the reader from `View::History`), and any form of
deletion.
