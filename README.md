<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner.svg" width="100%" alt="kvik: durable time series for devices, in one Rust library. Design stage: specification v0.2 and an exact reference model; the engine is not built yet.">
  </picture>
</h1>

[![CI](https://github.com/hugsmid/kvik/actions/workflows/ci.yml/badge.svg)](https://github.com/hugsmid/kvik/actions/workflows/ci.yml)

**A planned embedded time-series store for device and sensor data, in Rust: a durable ordered log and an analytical store
in one library, with no database service.**

kvik is designed to sit inside the application that talks to the devices. Readings go into a checksummed, append-only
log and are synced to disk before anything reacts to them; the same library then delivers them to in-process
consumers, keeps per-series summaries for drill-down from years to seconds, and moves old files to cheaper storage
and an off-site bucket; the engine itself never deletes a fact. Typical uses are home and building automation, energy
monitoring, small industrial or laboratory installations and edge devices: places where per-second data from many
sources must be acted on quickly, kept for years and restored to a known, recorded point after a failure
([who it is for](#who-it-is-for)).

> [!IMPORTANT]
> **Status: design stage. The engine does not exist yet.**
>
> - The [specification](docs/spec.md) is tagged **v0.2**.
> - A slow, exact [reference model](model/) of its semantics exists, with property tests and edge-case tests; a few
>   newer rules are not yet modelled ([coverage table](model/README.md#coverage)).
> - Two measurements are recorded: [chunk encodings against Parquet](docs/measurements/2.1-chunk-encodings.md) and
>   [commit and sync latency](docs/measurements/2.2-commit-latency.md); gate 2.3 has looked at both.
>
> | [Roadmap](docs/roadmap.md) section | Progress |
> |---|---|
> | 1. Specification settled | 8 of 8 done |
> | 2. Measurements before building | 3 of 3 done; gate 2.3 passed |
> | 3–10. Format and interface, log, delivery, compaction, summary tree, tiers and backup, query, scale | not started |
>
> 11 of 46 roadmap items are done, including 2 of 9 gates.

## Principles
*What the specification requires of the engine, in brief:*

<p>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/card-durable-dark.svg">
    <img src="docs/assets/card-durable.svg" width="160" alt="Synced to disk before anything reacts">
  </picture>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/card-history-dark.svg">
    <img src="docs/assets/card-history.svg" width="160" alt="Nothing deleted; corrections kept as revisions">
  </picture>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/card-losses-dark.svg">
    <img src="docs/assets/card-losses.svg" width="160" alt="Losses reported, never silent">
  </picture>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/card-drilldown-dark.svg">
    <img src="docs/assets/card-drilldown.svg" width="160" alt="Drill down from years to seconds">
  </picture>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/card-embedded-dark.svg">
    <img src="docs/assets/card-embedded.svg" width="160" alt="Embedded library: no database service to run">
  </picture>
</p>

From [spec §1, §2.4 and §8](docs/spec.md#1-principles):

- **Durable before anything reacts.** A batch of readings is synced and the durable mark moved before adapters are
  acknowledged, summaries are updated or any consumer or reader process sees it.
- **Late and corrected data are recorded.** A reading is never edited in place; a correction is a new fact and the
  old one stays as a revision. Sources that revise what they have published declare a *versioned series*, so a
  replay cannot undo a correction that carries a higher `changed_at`.
- **Facts are immutable; everything else is derived.** The summary tree, indexes, compacted layout and funnel
  state are pure functions of the log entries and the configuration recorded in them, so each can be rebuilt from the facts. Rebuild-equals-live is a test.
- **Data moves; the engine never deletes it.** A file leaves fast local disk for cold or deep storage only after a
  hash-verified copy exists there, and there is no purge. Losses the engine cannot prevent, such as a corrupt block
  with no other copy or a file the bucket has lost when a restore reads it, are recorded as lost ranges and reported as `data_lost`.
- **No silent loss.** Anything refused, repeated, flagged or delayed is counted and visible.
- **Restore to a known point.** A restore from the bucket starts a new generation and is restored to a known,
  recorded point. Positions beyond it that reached the bucket are set aside: kept and exportable, never silently
  mixed in. Facts in files that were never backed up are lost.
- **One writer process, any number of readers**, including readers in other processes while the writer runs.

## Who it is for
kvik is meant for places where the [principles](#principles) above are requirements rather than preferences. The
engine does not exist yet, so each item below says what it is designed to do, not what has been shown.

- **Regulated data that needs a complete change history**, such as pharmaceutical and food production, water
  treatment or energy metering. Besides keeping every fact and every correction, the store answers as-of queries:
  the data as it held it at a given time, so what the system showed then. Losses found at recovery or restore are
  recorded and reported as `data_lost`. kvik makes no claim of regulatory compliance or certification.
- **Unattended, power-fragile edge sites**, such as pumping stations, telecom cabinets, wind turbines, remote
  substations and vessels. kvik is a library inside the application, so there is no database service to install or
  keep alive. After a power loss, recovery follows fixed, ordered rules and reports a truncated tail as
  `possible_loss` and a corrupt range as `data_lost`. Sealed files are encrypted and uploaded to a bucket whenever
  the link allows and queue locally while it is down; nothing is evicted meanwhile, so local disk must cover the
  outage.
- **Alarms that must not be missed.** Conditions (a threshold with a separate clear level, held for N seconds, stale
  for N seconds) are evaluated by the store itself. A condition that became true while a consumer was down, and
  still holds, is delivered when the consumer resumes, never suppressed (during a restore, once its detail is local
  again); history replayed during catch-up is flagged `replayed`, so a consumer can tell it from live data and not
  act on it. The log, delivery and conditions form one system, rather than a database and a message broker joined
  by hand.
- **Energy and settlement data.** Interval series hold half-hourly, daily or monthly amounts, including days that
  start at a fixed local time, such as a gas day, and 23- or 25-hour days at clock changes; sums of whole intervals
  are exact for a decimal scale. Scheduled series hold values known in advance, such as published prices, which
  take effect at their time; forecast series keep every issue with its issue time, so a reader can compare them
  with what was later observed.
- **Small industrial and laboratory installations**: per-second data from many sources, kept for years, with
  drill-down from years to seconds ([budgets](docs/design.md#budgets-to-be-measured), still to be measured).

### What it is not
kvik is a storage engine, not a complete system. **Not planned for v1:** a network server, SQL, multiple writers,
clustering, conditions over several series, interval readings over irregular billing periods, or any form of
deletion ([`docs/api.md`](docs/api.md#not-in-v1)); a Parquet export is planned for other tools. An application
built on it would add:

- adapters for the protocols its devices speak, such as OPC UA, Modbus or MQTT;
- user accounts, access control and the record of who made a change;
- validation documentation, if it is used for regulated data.

The file format is a proposal and may change before a first release ([`docs/format.md`](docs/format.md)).

## How it works
The intended shape, from [`docs/design.md`](docs/design.md) and the [specification](docs/spec.md):

```mermaid
flowchart TB
    A["Adapters (devices, APIs)"] -->|emit| I["Ingest: validate, stamp, de-duplicate"]
    I --> L[("Log: arrival batches, checksummed")]
    L -->|"sync, then move the durable mark"| M{{"Durable mark"}}
    M --> T["Summary tree and live state"]
    M --> D["Delivery to consumers"]
    D --> C1["Live"]
    D --> C2["Catch-up from the log (replayed)"]
    D --> C3["Resync: state now"]
    L -->|"sealed segments"| K["Compaction (lossless)"]
    K --> CH[("Per-series chunks: current and revisions")]
    T --> Q["Queries: windows, calendar results, charts"]
    CH --> Q
    L -->|"recent facts not yet compacted"| Q
```

Each batch is one commit and one sync; while a sync is in flight, new readings form the next batch. Only after the
sync and the durable mark do the summary tree and live state update and consumers receive the batch. A consumer that falls behind catches up
from the log without blocking the writer or other consumers; one that was away too long receives a `resync` with
the present state instead of a stale replay. Compaction later rewrites sealed log segments into per-series chunks,
keeping every fact with its position. Queries combine whole summary nodes in the middle of a window with raw facts
at its edges, so results are exact to the nanosecond where the detail is local, and carry their coverage. A window never reads
off-machine data: where an edge's detail is not local, the edge moves inward and the result is flagged
`approximate_edges`, or is the error `detail_not_local` if no span is left.

### A reading, from emit to delivery

```mermaid
sequenceDiagram
    participant A as Adapter
    participant W as Store
    participant F as Log file
    participant C as Consumer
    A->>W: emit(readings)
    W->>F: append batch with commit block
    W->>F: sync
    W->>W: move durable mark
    W-->>A: Ack(first and last position)
    W->>W: update summary tree and live state
    W->>C: delivery (position order)
    C->>W: confirm(id)
    Note over C,W: the durable cursor never runs ahead of confirmation
```

### Tiers, backup and restore

```mermaid
flowchart LR
    H[("Hot: local disk")] -->|"after a verified copy"| CO[("Cold: another local path (optional)")]
    H -->|"every sealed file, encrypted, hash-checked"| DP[("Deep: object-storage bucket")]
    DP -.->|"fact reads fetched on demand"| H
    DP ==>|"restore onto an empty disk: new generation"| R["Restored store"]
```

The stored summary tree, indexes and registry never leave hot. At 90 days, revision chunks and sealed log segments
leave hot once verified in cold or deep; current detail stays hot except under fill pressure or for a series with a
diagnostic retention class. Cold is another local path, read in place. Detail held only in the bucket is fetched on
demand for fact reads, never for windows or consumer catch-up. The engine never deletes from the bucket.

### Key concepts

| Concept | Meaning |
|---|---|
| Fact | One recorded reading: series, event time `t`, value, quality, flags, position. Never edited or deleted |
| Position | `(generation, sequence)`, gap-free within a generation except lost ranges, recorded or described by a recovery entry; orders every log entry |
| Series | A named stream with a fixed kind (gauge, counter, state, text, interval), value type and unit |
| Versioned series | A series whose source revises readings; each carries `changed_at`, and the highest revision wins |
| Window | A rolling or calendar span evaluated in event time; every result carries coverage and flags |
| Calendar result | A local day or month in a configured or named time zone, evaluated as a window over UTC data |
| Consumer | In-process code fed by funnels (selection, optional windowed operator, condition), with a durable cursor |
| Resync | A delivery sent where a replay would be wrong or impossible: at the end of a resume it carries the present live and condition state; after a power loss or a lost range it names the positions the consumer must forget |
| Tier | Hot local disk, optional cold path, deep bucket; files move between them only after a verified copy |

### Planned interface
*Subject to change: an outline from [`docs/api.md`](docs/api.md), not yet implemented.*

```rust
let store = Store::open(config).await?;   // StoreConfig: directory, time zone, limits, tiers, ...

let id = store.define_series(SeriesSpec {
    name: "plant/line-1/temperature".into(),
    kind: Kind::Gauge { value: ValueType::Decimal { scale: 1 } },
    unit: "°C".into(),
    ..Default::default()
}).await?;

// Resolves only after the batch is synced and the durable mark has moved.
let ack = store.emit(&[Reading { series: id, t, value, quality, source: TimeSource::Device }]).await?;

let reader = Reader::open(path).await?;   // read-only; may be another process
let w = reader.window(id, Window::rolling(Duration::from_secs(600)), Aggregate::Avg)?;
// WindowResult { value, coverage, from, to, flags }
```

## How it is being built
1. **Specification first.** [`docs/spec.md`](docs/spec.md) states the rules, edge cases and invariants. Gaps found
   along the way are recorded in [`model/OPEN-QUESTIONS.md`](model/OPEN-QUESTIONS.md), not settled silently.
2. **An exact reference model.** [`model/`](model/) implements the semantics in memory with exact rationals, so
   tree nodes, merges and windows are compared with direct computation for equality. Property tests check the
   invariants of spec §10 that the model covers; CI runs them with 10,000 cases on every change and 40,000 nightly.
3. **Measurements before building.** Roadmap item 2.1 compared candidate chunk encodings with Parquet on synthetic
   per-second data: the own encoding was 8–14 % smaller than Parquet with ALP for decimal readings stored as
   `f64`, but only about 1 % smaller when they are declared as scaled `i64`. On two non-decimal shapes it was 13 %
   smaller than the best Parquet variant on one and 23 % larger than Parquet's dictionary encoding on the other
   ([results](docs/measurements/2.1-chunk-encodings.md)). Gate 2.3 kept the own format for compacted data, with
   per-value decimal exceptions and a compressed XOR fallback; gate 10.2 confirms it against Parquet at scale, and
   Parquet is for export only. Roadmap item 2.2 measured commit and sync latency, group commit, a failed sync and a
   full disk, so far on macOS only ([results](docs/measurements/2.2-commit-latency.md)).
4. **The engine against the model.** Each part (log, delivery, compaction, summary tree, tiers, query) will be
   built behind injectable clock and file-system seams and tested differentially against the model, with crash points,
   failed syncs and full disks injected.

Design budgets, still to be met by the engine, cover delivery to an in-process consumer (sync included), chart
queries at any zoom level, restart to serving and size on disk; the figures and the platforms they apply to are in
[`docs/design.md`](docs/design.md#budgets-to-be-measured).

## Repository map

| Path | Contents |
|---|---|
| [`docs/spec.md`](docs/spec.md) | The specification (v0.2): the rules an implementation must follow |
| [`docs/design.md`](docs/design.md) | Design direction, performance budgets, build order |
| [`docs/roadmap.md`](docs/roadmap.md) | Roadmap and status, one pull request per item |
| [`docs/format.md`](docs/format.md) | Proposed byte-level format; not yet checked for implementability (roadmap 3.1) |
| [`docs/api.md`](docs/api.md) | Proposed public interface; not yet checked for implementability (roadmap 3.2) |
| [`docs/measurements/`](docs/measurements/) | Recorded measurements |
| [`model/`](model/) | `kvik-model`: a slow, exact, in-memory reference model of the specification, with property tests |
| [`model/OPEN-QUESTIONS.md`](model/OPEN-QUESTIONS.md) | Open questions in the specification, and where the model still differs from it |
| [`spikes/encodings/`](spikes/encodings/) | The encoding spike behind roadmap item 2.1; not part of the engine |
| [`spikes/commit/`](spikes/commit/) | The commit and sync spike behind roadmap item 2.2; not part of the engine |
| [`docs/assets/`](docs/assets/) | Logo, banner and other images |

## Running the checks

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                  # the reference model
cargo test --locked -p encodings     # the encoding spike
cargo test --locked -p commit        # the commit and sync spike
cargo run --example basic            # a small tour of the model
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for how changes are made and [SECURITY.md](SECURITY.md) for reporting
vulnerabilities.

## Licence
kvik is source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE.md). It is free for personal,
hobby, research, education and other non-commercial use, as the licence defines. Commercial use needs a separate
commercial licence: open an issue on this repository to ask. Everything published before this change,
including tag v0.2, was released under MIT OR Apache-2.0 and remains available under those terms.
