# kvik

The public Rust interface of the kvik store, as settled at gate 3.3 in [`docs/api.md`](../docs/api.md) and governed by
the [specification](../docs/spec.md). Two backends stand behind it, chosen with `StoreConfig::backend` and
`ReaderConfig::backend`.

## Backends

> [!NOTE]
> **Migration.** The default is now the durable backend. Choose `Backend::Model` explicitly
> (`StoreConfig::backend`, and `ReaderConfig::backend` for its readers) for in-memory use. The `model-backend`
> feature is empty and deprecated; it goes in the next release.

**Durable (`Backend::Durable`, the default).** The log engine of [`kvik-log`](../log/): `Store::open` runs the log's
start (recovery, `spec.md` §3), rebuilds the series registry from the kept log and starts the `kvik-writer` and
`kvik-maint` threads; every write is committed to the log and acknowledged only once it is durable; a `Reader`, in
the same process or another, reads up to the durable mark. It supports series definitions (defining again, aliases,
retirement, a change of retention class, site-zone and tzdata entries), writing (`emit`, `emit_with`, `try_emit`
with every outcome the model backend gives) and reading facts in every view, the fact at a time, the interval of a
fixed-length interval series and series lookup, with `refresh`, health, statistics and losses, and live state,
windows, chart buckets and calendar results computed from the kept facts (roadmap 5.4). Differential property tests
compare its outcomes, facts, live state and window, bucket and calendar results with the model backend's. Some of
the read path is the model's own code, so those tests cannot catch a fault in it, which only the model's own tests
cover: the node arithmetic and aggregates (`kvik_model::summary`), calendar spans (`kvik_model::calendar`), and the
rules in `src/rules.rs` (a window's result from its node, refused aggregates, window spans, bucket steps, spans and
contents). What is the durable backend's own is which facts are read, from the log, for each call. Every start rebuilds the registry from the kept
log and writes the registry file (`format.md` A.16) again where it does not match; clean shutdown writes it too. **Not supported yet**, each until its roadmap
item: consumers and routing (5.2; `ConsumerError::NotSupportedYet`), and what the model backend lacks too (below).

**Model (`Backend::Model`).** The executable [reference model](../model/): exact, slow and entirely in memory. Nothing
is written to disk: `Store::open` only uses the directory as a name, and every store lives in the memory of the
current process. Within one process, opening the same directory again after `Store::shutdown` (or after dropping the
last `Store` clone, which acts as a crash) restarts the store as it was left, so restarts, catch-up, replay and
resyncs can be exercised in tests. `Reader::open` finds a store opened in the same process. It has no threads: its
calls complete at once, except `Consumer::next`, which waits for a delivery, and `Store::emit`, which waits under
back-pressure. Timers (window expiry, held-for and stale-for conditions, scheduled values taking effect) advance only
when the store is called: before every emit and delivery poll, and on `Store::tick`, which is temporary and goes when
the engine runs its own timers. A waiting call is woken by the next emit, routing change, confirmation or tick.

Only std types appear in signatures. The optional feature `jiff` adds conversions between the interface's time types
and jiff 0.2's; the default feature `bundled-tzdb` embeds the time-zone database (the model backend embeds it in any
case).

## Example

```sh
cargo run -p kvik --example quickstart
```

[`examples/quickstart.rs`](examples/quickstart.rs) opens a store, defines a gauge and a counter, emits readings,
receives and confirms a threshold condition's transition, and queries a rolling window and a calendar day; as the
durable backend has no consumers yet, it runs on the model backend.

## Coverage

What the model backend supports, each with a test in [`tests/`](tests/) or the crate's unit tests. The tests in
[`tests/api.rs`](tests/api.rs) run on both backends wherever the durable backend supports the feature, and check
that it says so where it does not; [`tests/durable.rs`](tests/durable.rs) covers durability (acknowledged readings
after a power loss), a reader in another handle, the registry and its file after crashes and damage, ids never
assigned twice, orphaned facts, writers on several threads, a store on the system environment, and the differential
test of the durable backend against the model.

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

Not supported by either backend (the durable backend refuses the same definitions and calls as the model, with the
same errors):

- **Decimal series** (`ValueType::Decimal`, `ValueType::decimal()`), so also `raise_scale`, the excess-precision
  policy, `set_excess_precision` and `Refusal::ExcessPrecision`: defining one, raising a scale or changing a policy
  returns `DefineError::NotSupportedYet`. The reference model has
  no exact decimal values, and storing decimals as `f64` would not give the exact sums the specification promises.
  `Value::Decimal` on an `f64` series is refused `WrongType`; `Outcome::Recorded.rounded`, `Outcome::Recorded.out_of_range` and the
  `rounded` fact flag are never set, and the `out_of_range` count stays 0.
- **Calendar-length interval series** (a local day or month, with a day start): `DefineError::NotSupportedYet`.
- **Parameter changes** other than the retention class: `DefineError::NotSupportedYet`.
- **Backup, restore, export of set-aside facts and rebuild** (`BackupError::NotSupportedYet`).
- `SystemTz` reports the embedded tzdata as the release `"embedded"`; `StdFs::free_space` returns `Unsupported` on
  Windows; `SystemBootId` is `None` outside Linux (the durable backend then syncs the mark at every commit).

Model backend:

- **A new consumer starting at a position or a time**, and **a consumer that emits** (`emits` other than
  `Select::None`): `ConsumerError::NotSupportedYet`. `Consumer::try_emit` therefore refuses every reading
  `NotDeclared`, and `RouteCycle` is never returned.
- **A funnel with an operator but no condition**, and a held-for condition over an operator:
  `ConsumerError::NotSupportedYet`.
- **A condition funnel over several series**: it must select exactly one, and a name pattern in it is resolved once,
  when routed, so `docs/api.md`'s `plant/*/power` condition example is refused (`ConsumerError::Route`) unless
  exactly one series matches.
- `export_orphaned` (`QueryError::NotAvailable`): the model has no orphaned facts.
- Never produced: the `Unknown` live and condition states, `approximate_edges`, `displaced`, a trigger's value and
  coverage, `forget_above`, the `ResetPoint` and `GenerationChanged` resync causes, `PossibleLoss`, `ManifestRebuilt` (provisional or not), `LossCause::Unknown`, `loss_identities_rebuilt`, `WriteError::NotFinal`,
  `Outcome::Recorded.unchecked` and `duplicates_unchecked` (the model's detail is always local),
  `SealBlocked`, `Wait::QueueFull` (a call never exceeds the queue, which is empty between calls), commit latency and
  time waited, `orphaned_facts`, `scale_raises`, `interval_displaced`, and `evaluations_unknown` and
  `transitions_not_delivered`, which are not reported at all.
- The seams other than the clock are not used: it writes no files, takes no lock file and computes calendars with
  the reference model's embedded tzdata rather than through `Env::tz`.
- `DiskStats.free` and the disk marks (`limits.disk_low`, `limits.disk_critical`) are measured against the reference
  model's simulated disk of 1 TiB, not a real file system; `IngestStats.syncs` is 0, as nothing is synced.

- **`ReaderState.stamp`** is always the zero stamp: the model has no resets. The durable backend gives the mark's.

Durable backend, besides consumers:

- `Reader::facts` collects a query's facts before it returns, as the model backend does, rather than streaming
  block by block; every process holds the facts of the kept log in memory (the indexes and tree of roadmap 6 and 7
  replace this).
- Live state, windows, chart buckets and calendar results are evaluated from the kept facts at every call, in the
  model's exact arithmetic; no summary is stored yet (the tree of roadmap 7 serves the same calls). Calendar results
  take local days from the time-zone rules the model embeds, not from the release `Env::tz` reports (Q-93 (c)).
- Live staleness of an observed series is measured on the monotonic clock since its last refreshing commit in the
  writer's process, taken when the call's acknowledgement is (a call whose acknowledgement is never taken refreshes
  nothing); a reader process, and a writer before its first such commit after opening, measure it in event
  time (`now − t` of the live-state fact), as the model backend does after a restart (Q-16, Q-140).
- The log's timers (group commit, mark sync, sealing) run on the process's monotonic clock, not on `Env::clock`,
  whose wall clock stamps `received_at`: a manual clock that stands still would otherwise hold a sync back.
- A call that defines, aliases, retires or changes a series waits for its mark sync on the calling thread, holding
  the decision lock, so no later outcome depends on an entry that is not yet durable.
- `acknowledge_losses` holds acknowledgements in memory until the store closes: the manifest's acknowledgement
  record is not written yet. `losses()` reads the manifest's lost-range records and every recovery entry of the kept
  log.
- Per-series counts of recorded, late and future facts are rebuilt from the log at every open; duplicates,
  refusals, `outranked_on_arrival` and `absent_supplied` count since the store was opened. Clock jumps are not
  counted.
- Not produced: `Wait::UnsyncedClock` in `stats().ingest.waiting` and health (the call waits, and `waits` counts
  it), `ManifestRebuilt`, `loss_identities_rebuilt`, `WriteError::NotFinal`, `Outcome::Recorded.unchecked` and
  `duplicates_unchecked` (all detail is local), `ConditionState`s and deliveries of any kind, `scale_raises`,
  `interval_displaced`, `counter_regressions` and `SeriesStats.orphaned_facts` (orphaned facts are left out of every
  view and returned by `export_orphaned`, but not counted yet).

Different from `docs/api.md`:

- **`CounterRules::reset_ceiling` must be a whole number of parts per million**, on both backends: the log keeps it
  so (`format.md` A.6), and a ceiling it cannot keep exactly is refused at definition (Q-142).
- **Every seam trait requires `fmt::Debug`** besides `Send + Sync`, so that `Env` can be printed.
- `Funnel` and `Reading` are plain structs, as `docs/api.md` builds them literally; every other configuration or
  result struct is `#[non_exhaustive]`, built with `new`, the setters, or `default()` and field assignment.
- **Errors**: the backend adds `NotSupportedYet` to `DefineError`, `ConsumerError::NotSupportedYet`,
  `QueryError::Invalid` (a malformed query) and `QueryError::Closed`. `ConsumerError` and `BackupError` stand in for
  the funnel-compilation and backup errors that roadmap 5.2 and 8.2 settle.
- **`Health::Closed`** after the writer shuts down, and **`Reason::ClockSkew`** (spec §9 row 5).
- **`Backend`**, `StoreConfig::backend` and `ReaderConfig::backend` choose the backend.
- **`Live::Value` carries a `LiveValue`** (the fact and whether it is stale), where `docs/api.md` shows the fact alone
  "with staleness".
- **`ConditionState::Value`** holds an on-change condition's last value; **`FunnelState.transition`** says whether a
  resync's condition state differs from the latch.
- **`Reader::buckets` returns `ChartData`**: raw facts when the range is short enough, else buckets, where
  `docs/api.md` shows `Vec<Bucket>` and also says raw facts. `Resolution::Auto` takes the finest of 1 minute,
  15 minutes and multiples of 15 minutes that fits `max_points` (`docs/api.md` says the coarsest), counted from the
  UTC-aligned start.
- `Start::Position` takes a `SavedPoint`
  (spec §7 Readers) and is `NotSupportedYet`; `Reader::check` and `PointCheck` (`docs/api.md`) are not in the crate
  yet (roadmap 5.3).
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
