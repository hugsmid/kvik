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
  clock, file system, backup sink, boot identity.

## Opening
```rust
let store = Store::open(StoreConfig {
    dir: path,
    time_zone: tz,                 // required: calendar levels use it
    log: LogConfig::default(),     // segment size, seal interval, linger
    limits: Limits::default(),     // queue sizes, series ceiling, disk marks
    tiers: TierConfig::default(),  // cold path, age and fill thresholds
    env: Env::system(),            // clock, file system, boot identity
}).await?;
let reader = Reader::open(path).await?;      // read-only; no lock; may be another process
store.shutdown().await?;                     // final sync, checkpoint, release the lock
```
`Store::open` runs recovery and returns only when the log is consistent. It fails if another writer holds the
directory, if the store id of a file does not match, or if a file has a newer major format version.

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
Defining a series that already exists with the same specification returns its id. A different kind, value type,
scale or unit under the same name is an error; the caller retires and defines anew.

## Writing
```rust
let ack: Ack = store.emit(&[Reading { series: id, t, value, quality, source: TimeSource::Device }]).await?;
// Ack { first: Position, last: Position, duplicates_dropped: u32, refused: Vec<(usize, Refusal)> }
store.try_emit(&readings)?;                   // never waits; Err(Busy) under back-pressure
store.emit_with(&readings, Intent::Revise).await?;
```
- `emit` resolves only after the batch is synced and the durable mark has moved. It waits under back-pressure.
- A batch is all-or-nothing for durability, but individual readings may be refused (invalid series, retired
  series, a non-finite value recorded as bad quality is not a refusal). Refusals are reported per index.
- Readings in one call keep their order; calls from different tasks are ordered by commit.

## Consuming (funnels)
```rust
let mut consumer = store.consumer("load-shedding", Start::Now).await?;
consumer.route(Funnel {
    series: Select::Names("plant/*/power".into()),
    operator: Some(Operator::rolling(Aggregate::Avg, Duration::from_secs(600))),
    condition: Some(Condition::above(3_000.0).clear_below(2_500.0)),
}).await?;

while let Some(d) = consumer.next().await {
    match d.kind {
        DeliveryKind::Fact(f) => { /* f.position, f.series, f.t, f.value */ }
        DeliveryKind::Trigger(t) => { /* t.funnel, t.state, t.value, t.coverage */ }
        DeliveryKind::Resync(s) => { /* current live state and condition state */ }
    }
    consumer.confirm(d.id).await?;            // advances the durable cursor
}
```
- A delivery carries `id` (position, or funnel and sequence for timer deliveries), `flags` (`replayed`,
  `future`) and the times needed to judge its age.
- `confirm` is cheap and batched; the cursor is written at most once a second and on shutdown.
- A consumer that emits readings uses `try_emit` or its own task, never the delivery callback.
- Routes may be added and removed while running; each change is a log entry and takes effect at its position.

## Reading
```rust
let now = reader.live(id)?;                                   // latest valid value, with staleness
let w = reader.window(id, Window::rolling(Duration::from_secs(600)), Aggregate::Avg)?;
// WindowResult { value, coverage, from, to, flags }
let series = reader.buckets(id, range, Resolution::Auto { max_points: 2_000 })?;   // chart query
let facts = reader.facts(id, range, View::Current)?;         // also View::AsOf(position), View::History
let day = reader.calendar(id, date, Aggregate::Integral)?;   // site time zone
```
- `Resolution::Auto` picks the coarsest stored level that yields at most `max_points`, and raw facts when the
  range is short enough. Every bucket carries count, min, max and coverage so a chart can show gaps and spikes.
- All reads see only durable data. A reader in another process sees data up to the durable mark.
- A query engine integration (a table provider) is layered on these calls and is not part of the core crate.

## Health and statistics
```rust
let h: Health = store.health();     // Ok, Degraded { reasons }, ReadOnly { cause }
let s: Stats = store.stats();       // per series and per consumer counters, commit latency, queue depths,
                                    // time behind, disk marks, backup lag
```
Every refusal, drop, repeat and delay the specification says is counted appears here.

## Backup and restore
```rust
trait BackupSink {
    async fn put(&self, key: &str, bytes: Bytes, sha256: [u8; 32]) -> Result<Stored>;
    async fn stat(&self, key: &str) -> Result<Option<Stored>>;   // reported hash and size
    async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Bytes>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}
store.attach_backup(sink, Encryption::Key(key)).await?;
Store::restore(dir, sink, Encryption::Key(key), RestoreMode::SummariesFirst).await?;
store.rebuild(Rebuild::Derived).await?;       // recompute every derived structure from facts
```
The sink has no delete. An S3-compatible implementation lives in a separate crate.

## Environment seams (for tests and for unusual platforms)
```rust
trait Clock { fn wall(&self) -> Timestamp; fn monotonic(&self) -> Instant; fn synchronised(&self) -> bool; }
trait Fs { /* open, preallocate, write_at, sync, rename, sync_dir, mmap, free_space */ }
trait BootId { fn current(&self) -> Option<[u8; 16]>; }
```
The test implementations inject crashes at every write and sync, failed syncs, a full disk, clock jumps and a
changed boot identity. The engine is tested against the reference model through these seams.

## Not in v1
A network server, SQL, multi-writer access, clustering, percentiles, conditions over several series, and any
form of deletion.
