# kvik

The public Rust interface of the kvik store, as settled at gate 3.3 in [`docs/api.md`](../docs/api.md) and governed by
the [specification](../docs/spec.md). Applications can write adapters and consumers against it today; the durable log
engine will replace the backend behind the same interface (roadmap section 4 onwards).

## Backend: in-memory, not durable, for development and tests

> [!WARNING]
> **Nothing is written to disk yet.** The only backend, behind the default feature `model-backend`, is the
> executable [reference model](../model/): exact, slow and entirely in memory. Every store lives in the memory of the
> current process and is lost when the process ends.

`Store::open` takes a directory but only uses it as a name. Within one process, opening the same directory again after
`Store::shutdown` (or after dropping the last `Store` clone, which acts as a crash) restarts the store as it was left,
so restarts, catch-up, replay and resyncs can be exercised in tests. `Reader::open` finds a store opened in the same
process.

The engine owns its threads; the model backend has none. Its calls complete at once, except `Consumer::next`, which
waits for a delivery, and `Store::emit`, which waits under back-pressure. Timers (window expiry, held-for and
stale-for conditions, scheduled values taking effect) advance only when the store is called: before every emit and
delivery poll, and on `Store::tick`, which is temporary and goes when the engine runs its own timers. A waiting call
is woken by the next emit, routing change, confirmation or tick.

Only std types appear in signatures. The optional feature `jiff` adds conversions between the interface's time types
and jiff 0.2's; the default feature `bundled-tzdb` embeds the time-zone database (the model backend embeds it in any
case).

## Example

```sh
cargo run -p kvik --example quickstart
```

[`examples/quickstart.rs`](examples/quickstart.rs) opens a store, defines a gauge and a counter, emits readings,
receives and confirms a threshold condition's transition, and queries a rolling window and a calendar day.

## Coverage

What the model backend supports, each with a test in [`tests/`](tests/) or the crate's unit tests:

| Area | Supported |
|---|---|
| Opening | `Store::open` with `create_if_missing`, one writer per directory (`OpenError::Locked`), configuration checked against its limits, site-zone check and `set_site_zone`, restart; `Reader::open(ReaderConfig)`, `refresh`, readers after the writer shuts down; `Store` clones and `shutdown(&self)` |
| Environment | `Clock` (with `Monotonic`), `Fs`, `BootId`, `TzSource`/`TzRelease` and `Entropy` seams, each with a production and a test implementation; the backend uses the clock only |
| Series | gauge, counter, boolean and named state, text, fixed-length interval series with a phase, versioned series; defining again, `Conflict` naming the differing field, `alias`, `retire`, `series` by id or name, a change of retention class. Other definition fields (record on change, counter rules, lateness bound, timing) are passed to the model unchanged |
| Writing | `emit`, `emit_with` (`Intent::Revise`), `try_emit` returning a `Pending`; one `Outcome` per reading (recorded with its position and `outranked`, duplicate, refused); `Value::Absent` (of any supplied quality, which the duplicate rule keeps) and non-finite values recorded without a value; back-pressure as `Wait` (unsynchronised clock for host-stamped readings, critical disk), named in `stats().ingest.waiting` and health while it lasts; `EmitError::TooLarge` over the call limits |
| Consuming | consumers by name; funnels by ids, name pattern (re-routed on definition and alias) or all; `unroute`; threshold, on-change, held-for and stale-for conditions, with or without a rolling operator; `next`, `try_next`, `confirm`; the `replayed`, `future` and `outranked` flags; overflow and catch-up; a consumer taken again replays then resyncs; resyncs in parts of at most 256 funnel states, confirmed by the last part, with nothing delivered until then; condition deliveries on the timer; `Consumer::try_emit` |
| Reading | live state with staleness; rolling, relative, absolute and calendar windows in the site zone or a named zone; calendar day and month results; facts in the current, as-of (position or time) and history views, as a `Facts` iterator; the fact at a time (`Reader::at`), also for a versioned or forecast series; chart buckets (automatic and fixed steps) whose min and max are over readings only while the mean can come from a carried-in value; the interval containing a time |
| Health | `Health` with `Reason`s and a `Fault`; `Stats` (ingest, log, disk, clock jumps); `series_stats`; `losses`, `acknowledge_losses`, `acknowledge_manifest_rebuild` |

## Differences from `docs/api.md`

Not supported by the model backend:

- **Decimal series** (`ValueType::Decimal`, `ValueType::decimal()`), so also `raise_scale`, the excess-precision
  policy, `set_excess_precision` and `Refusal::ExcessPrecision`: defining one, raising a scale or changing a policy
  returns `DefineError::NotSupportedYet`. The reference model has
  no exact decimal values, and storing decimals as `f64` would not give the exact sums the specification promises.
  `Value::Decimal` on an `f64` series is refused `WrongType`; `Outcome::Recorded.rounded`, `Outcome::Recorded.out_of_range` and the
  `rounded` fact flag are never set, and the `out_of_range` count stays 0.
- **Calendar-length interval series** (a local day or month, with a day start): `DefineError::NotSupportedYet`.
- **Parameter changes** other than the retention class: `DefineError::NotSupportedYet`.
- **A new consumer starting at a position or a time**, and **a consumer that emits** (`emits` other than
  `Select::None`): `ConsumerError::NotSupportedYet`. `Consumer::try_emit` therefore refuses every reading
  `NotDeclared`, and `RouteCycle` is never returned.
- **A funnel with an operator but no condition**, and a held-for condition over an operator:
  `ConsumerError::NotSupportedYet`.
- **A condition funnel over several series**: it must select exactly one, and a name pattern in it is resolved once,
  when routed, so `docs/api.md`'s `plant/*/power` condition example is refused (`ConsumerError::Route`) unless
  exactly one series matches.
- **Backup, restore, export of set-aside facts and rebuild** (`BackupError::NotSupportedYet`), and
  `export_orphaned` (`QueryError::NotAvailable`).
- Never produced: the `Unknown` live and condition states, `approximate_edges`, `displaced`, a trigger's value and
  coverage, `forget_above`, the `ResetPoint` and `GenerationChanged` resync causes, `PossibleLoss`, `ManifestRebuilt` (provisional or not), `LossCause::Unknown`,
  `Outcome::Recorded.unchecked` and `duplicates_unchecked` (the model's detail is always local),
  `SealBlocked`, `Wait::QueueFull` (a call never exceeds the queue, which is empty between calls), commit latency and
  time waited, `orphaned_facts`, `scale_raises`, `interval_displaced`, and `evaluations_unknown` and
  `transitions_not_delivered`, which are not reported at all.
- The seams other than the clock are not used: the backend writes no files, takes no lock file and computes calendars
  with the reference model's embedded tzdata rather than through `Env::tz`. `SystemTz` reports the embedded tzdata as
  the release `"embedded"`; `StdFs::free_space` returns `Unsupported`; `SystemBootId` is `None` outside Linux.
- `DiskStats.free` and the disk marks (`limits.disk_low`, `limits.disk_critical`) are measured against the reference
  model's simulated disk of 1 TiB, not a real file system; `IngestStats.syncs` is 0, as nothing is synced.

Different from `docs/api.md`:

- **Every seam trait requires `fmt::Debug`** besides `Send + Sync`, so that `Env` can be printed.
- `Funnel` and `Reading` are plain structs, as `docs/api.md` builds them literally; every other configuration or
  result struct is `#[non_exhaustive]`, built with `new`, the setters, or `default()` and field assignment.
- **Errors**: the backend adds `NotSupportedYet` to `DefineError`, `ConsumerError::NotSupportedYet`,
  `QueryError::Invalid` (a malformed query) and `QueryError::Closed`. `ConsumerError` and `BackupError` stand in for
  the funnel-compilation and backup errors that roadmap 5.2 and 8.2 settle.
- **`Health::Closed`** after the writer shuts down, and **`Reason::ClockSkew`** (spec §9 row 5).
- **`Live::Value` carries a `LiveValue`** (the fact and whether it is stale), where `docs/api.md` shows the fact alone
  "with staleness".
- **`ConditionState::Value`** holds an on-change condition's last value; **`FunnelState.transition`** says whether a
  resync's condition state differs from the latch.
- **`Reader::buckets` returns `ChartData`**: raw facts when the range is short enough, else buckets, where
  `docs/api.md` shows `Vec<Bucket>` and also says raw facts. `Resolution::Auto` takes the finest of 1 minute,
  15 minutes and multiples of 15 minutes that fits `max_points` (`docs/api.md` says the coarsest), counted from the
  UTC-aligned start.
- **`Reader::interval`** returns the interval that contains `t`, whether or not `t` is its start.
- **`Consumer::next`** ends with `None` on any error, not only after a shutdown; `try_next` returns the error.
- `BackupSink` takes and returns `Vec<u8>` until roadmap 8.2 settles its signatures.
- **Added**: `Store::tick` (temporary), `Store::now`, `Store::is_durable`, `Store::consumer_stats`,
  `Consumer::try_next`, `Consumer::unroute`, `Consumer::routes`, `Reader::series`, `Ack::recorded`, `Start::Time`,
  `Trigger.cause`, `DeliveryFlags.outranked`, `WindowResult.count` and `coverage_fraction`,
  `ResultFlags.summary_stale` and `rate_flagged`, `Resolution::Step`, `SeriesStats.latest_t` and
  `counter_regressions`, `IngestStats.waits`, and `Limits.consumer_queue` and `max_consumers` (keys of later
  sections that the backend needs).

Name patterns in a funnel's selection work, but their grammar is provisional (`*` within one `/`-separated segment),
as the specification does not define one yet ([Q-88](../model/OPEN-QUESTIONS.md#q-88--the-grammar-of-name-patterns)).
