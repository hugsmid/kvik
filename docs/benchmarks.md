# Comparative benchmarks: plan

> [!IMPORTANT]
> **This is a plan. Nothing here has been run, and there are no results.** The engine does not exist yet
> ([roadmap](roadmap.md)). Tool versions, settings and hardware are fixed when the benchmarks run, and recorded
> with the results. Roadmap item 10.2.

## Purpose
Someone choosing a store for device data should be able to see, on workloads like their own, where kvik is the
better choice and where it is not. This plan sets out the workloads, the measurements, the tools to compare with
and the rules that keep the comparison fair. It measures the uses named in the README under
[Who it is for](../README.md#who-it-is-for); it is not a general database benchmark.

kvik's own budgets are in [`design.md`](design.md#budgets-to-be-measured) and are measured by roadmap items 4.8,
5.5, 9.2 and 10.1. Those show whether kvik meets its targets. This plan shows how it compares.

## Fairness rules
In brief (the numbered rules below give the detail):
- **Equivalent durability.** Every system is compared with durability settings equivalent to kvik's: a write is
  acknowledged only after it is synced. The settings that give this are stated per system. A default
  configuration that does less is reported separately, in its own class (rules 2 and 3, Durability classes).
- **Configured from its own documentation.** Each system is set up as its documentation recommends, and the
  versions, configuration, hardware and file system are recorded for every run (rules 1, 2 and 8).
- **Published harness.** The harness is published so that anyone can run it again (rule 4).
- **Claims limited to the workloads.** Results are stated for the named workloads only, small-batch durable
  ingest and window and calendar reads, not as general claims about any system (rule 10).
- **Preview offered.** Each project compared may be offered a preview of its results before publication
  (rule 11).
- **Open-source builds only.** Licence terms are checked before anything is published (rule 12).

1. **Same hardware.** Every tool runs on the same machines, the same drive and the same file system, one tool at
   a time, with the drive brought to the same starting state before each run. The hardware, kernel, file system,
   mount options and whether the drive honours a flush are recorded.
2. **Two configurations per tool.** *Default*: as installed, with nothing changed. *Durable*: tuned from the
   tool's own documentation so that an acknowledged write survives a power loss (for example synchronous commits).
   Both are published in full. Where a tool's documentation recommends further tuning for the workload, that
   tuning is applied to the durable configuration and recorded.
3. **Durability equivalence.** Latency and throughput are compared only between configurations in the same
   durability class (table below). A tool that acknowledges before a sync is reported in its own class, never
   beside one that syncs first as if the two did the same work.
4. **Reproducible.** The generator, the driver for every tool, the configurations and the analysis are in this
   repository. A run is one command on a prepared machine, and the data is generated from a recorded seed.
5. **Raw results published.** Every run's raw samples are kept, not only the summaries, including runs that failed
   or were stopped.
6. **No cherry-picking.** Every measurement in the plan is reported for every tool it applies to, including those
   where kvik is slower, larger or less capable. No workload or metric is dropped after the results are seen;
   a change to the plan is made in this file, with its reason, before the run it affects.
7. **Missing capabilities are shown, in both directions.** Where a tool cannot do something (keep revisions,
   answer as-of queries, evaluate conditions, report a loss), the row says "not supported" or describes the
   application code the driver needed. The same holds for what kvik leaves out of v1, such as SQL, a network
   server and multiple writers ([`api.md`](api.md#not-in-v1)).
8. **Idiomatic use.** Each driver uses its tool as that tool's documentation recommends for time series (schema,
   indexes, batching, partitioning). Review of a driver or configuration by that tool's maintainers or
   users is welcome; a suggested change is applied or the reason for not applying it is recorded.
9. **Warm-up and repetition.** Each measurement is repeated at least five times after a warm-up; the spread is
   reported with the median. Latency is measured from the intended send time, so a stalled tool cannot hide the
   stall by slowing the generator.
10. **Stated for the workload measured.** Each result is stated for the workload and hardware it was measured on
    (small-batch durable ingest, window and calendar reads, W1–W4). Results are not summarised as a general
    ranking or presented as claims about a tool beyond those workloads.
11. **Preview before publication (optional).** Each project compared may be offered its results, driver and
    configuration before publication. A misuse it points out is corrected and the run repeated; a disagreement
    that remains is recorded with the results.
12. **Open-source builds and licence terms.** Only open-source builds are tested. Before any results are
    published, each tool's licence terms are checked for restrictions on publishing benchmark results, and the
    check is recorded with the results. A tool whose terms forbid publication is left out.

### Durability classes
| Class | An acknowledged write is | Compared on latency with |
|---|---|---|
| A | synced to stable storage before the acknowledgement | class A only |
| B | synced within a stated bound after the acknowledgement (a timer or a group interval) | class B only, with the bound stated |
| C | in the operating system's cache or the process's memory, with no stated bound | class C only |

kvik acknowledges after the sync ([spec §3](spec.md#3-log)), so it is in class A. Each configuration of each
tool is placed in a class from its documentation when the benchmarks run, and the placement is then checked by the
power-cut loop below: a configuration that loses acknowledged writes in the loop is moved down a class and the
move is reported. A filled-in copy of this table, one row per tool and configuration with the settings that put
it there, is published with the results.

## Workloads
Each workload is a deterministic generated data set and a script of writes, reads and faults. Sizes are starting
points and are fixed before the first full run.

### W1. Home and building automation
- **Data.** 300–500 series at mixed rates: per-second power and state changes, per-minute temperatures and
  humidity, sparse events and text. Record-on-change sources with long quiet periods. Two years of history.
- **Writes.** Many small batches from many sources; occasional late readings from devices that reconnect.
- **Alarms.** Threshold conditions with a separate clear level, held-for and stale-for conditions, delivered to
  an in-process consumer. The consumer is stopped and restarted while conditions change.
- **Reads.** Current state of every series; charts of one series from two years down to ten minutes.

### W2. Industrial edge gateway
- **Data.** 2,000 and 10,000 series at 1 s, mostly numeric, some boolean; a year of history for the storage and
  query measurements.
- **Writes.** Steady per-second batches for days; a slow consumer; the uplink to the object store down for hours
  to days, then restored.
- **Faults.** Power cuts at random points; a full disk; a drive that fails a sync.
- **Reads.** Charts over any span; a second process reading while the writer runs.

### W3. Energy and settlement
- **Data.** Half-hourly and daily interval amounts for a few hundred meters over three years, with days that
  start at a fixed local time and the 23- and 25-hour days at clock changes. Scheduled prices published a day
  ahead. Forecasts re-issued several times a day.
- **Writes.** Revisions that arrive days or weeks later; the whole history replayed from the source after the
  revisions were applied.
- **Reads.** Exact sums per local day and month, checked against direct computation in decimal; the same sums as
  they stood at an earlier date; each forecast issue against what was later observed.

### W4. Laboratory and test rig
- **Data.** A few dozen series, idle for long periods, with bursts of 1–10 kHz per series for seconds to minutes.
- **Writes.** The burst rate ramped until the tool falls behind; the point where back-pressure starts and what
  the writer sees then.
- **Reads.** A chart of a whole burst and of a millisecond inside it, during and after the burst.

## Measurements
Each metric names the workloads it is run on. Latency is reported as p50, p99, p99.9 and the maximum.

| # | Metric | What is measured | Workloads |
|---|---|---|---|
| M1 | Ingest to durable acknowledgement | time from sending a batch to its acknowledgement, at several rates from idle to near the limit; class stated | W1, W2, W4 |
| M2 | Ingest to consumer | time from sending a reading to in-process code receiving it (for a server tool, to a subscribed client on the same machine) | W1, W2, W4 |
| M3 | Sustained throughput | highest rate held for an hour with p99 of M1 within a stated limit; behaviour past that rate | W2, W4 |
| M4 | Durability under power loss | the power-cut loop: acknowledged readings missing after recovery; whether the tool reported a loss; whether it started at all | W1, W2 |
| M5 | Late, out-of-order and corrected data | whether earlier values are kept; whether a replay of older data reverts a correction; as-of queries; cost of a late write | W1, W3 |
| M6 | Missed alarms | conditions that became true while the consumer was down: delivered on resume, delivered twice, or lost; replayed history told apart from live | W1 |
| M7 | Storage per reading | bytes per reading after compaction or the tool's equivalent, for the data alone and with indexes and summaries | all |
| M8 | Host write volume | bytes written to the drive per day at the workload's steady rate, from the block device's counters | W1, W2 |
| M9 | Chart query latency | a chart of fixed width over spans from years to seconds, cold and warm cache; correctness against direct computation | all |
| M10 | Restart | time from start to serving after a clean stop and after a power cut | W1, W2 |
| M11 | Restore | time to restore from an object-store backup onto an empty disk, and whether the restored data equals the source up to a stated point | W2, W3 |
| M12 | Footprint | binary size, resident memory and CPU time at the steady rate, on a small ARM machine and on a server-class machine | W1, W2 |
| M13 | Operations | processes and services to install and keep alive; steps to back up and to restore; what an unattended site needs after a power cut | all |

Notes on individual metrics:

- **M4, the power-cut loop.** A client outside the machine under test records every acknowledgement it receives.
  Power is cut at a random time, the machine restarts, and the store's contents are compared with the client's
  record. A run is several hundred cuts per tool and configuration. Reported: acknowledged readings missing;
  readings present that were never acknowledged (allowed, but counted); whether the tool said anything was lost
  and how exactly (kvik's equivalents are `possible_loss` and `data_lost`); corruption found later by a full
  read. The loop builds on kvik's own kill-loop harness (roadmap 4.5).
- **M5.** The script writes a value, corrects it, then replays the original from the source, as a re-import
  would. Reported per tool: the value a plain read returns afterwards; whether the superseded value can still be
  read; whether "what did the store show on date D" can be answered, and by what means. For tools without a
  built-in rule, the driver implements the usual pattern from that tool's documentation (for example a version
  column and an upsert), and its extra storage and latency are counted as that tool's cost.
- **M6.** Where a tool has no conditions or delivery, the driver uses the pairing listed under Candidates, and
  the result is that of the pair.
- **M7 and M8.** Sizes are taken after each tool has finished its background work. M8 is measured at the block
  device, so it includes journals, write-ahead logs and compaction rewrites.
- **M9.** Each tool may use the summaries its documentation recommends (materialised views, continuous
  aggregates, roll-up tables); their storage counts in M7 and their upkeep in M1, M3 and M8. A query that returns
  a wrong or unflagged approximate answer is reported as such, not timed as a success.
- **M11.** Uses a local S3-compatible fake for timing and one real bucket for the final run. Tools without a
  built-in backup use the method their documentation recommends.
- **M13** is descriptive: a table, not a score.

## Candidate comparisons
Open-source builds only (rule 12), chosen per workload. Exact versions and settings are fixed when the benchmarks run; a tool may
be replaced by a closer equivalent then, with the reason recorded.

| Candidate | Workloads | Why it is included |
|---|---|---|
| SQLite, WAL mode, `synchronous=FULL` (and its defaults) | W1, W2, W3 | the usual embedded choice; no service to run; a well-understood durability baseline |
| DuckDB | W2, W3, W4 | embedded analytical database; the reference for scan and aggregation speed in-process |
| Parquet files, written in batches and queried in place | W2, W4 | the common "files on disk" design; the baseline for size, and kvik's own export format |
| An embedded key-value store with a time-series schema (RocksDB or a similar log-structured store) | W2, W4 | what an application often builds itself for high-rate ingest |
| An embedded or edge time-series database (candidates: an edge build of InfluxDB, QuestDB, TDengine; one or two chosen at run time) | W1, W2, W4 | purpose-built for this data, though usually as a service |
| A server time-series database (TimescaleDB on PostgreSQL) | W1, W2, W3 | for contrast: mature SQL, continuous aggregates and tooling, at the cost of a service to run |
| A message log with a database (an MQTT broker with persistent sessions, or a message log such as NATS JetStream, feeding SQLite or the server database) | W1 | the usual way to get durable delivery and alarms beside storage; compared on M2, M4, M6 and M13 |

Not every tool runs every workload: a tool is left out of a workload it is plainly not meant for, and the table
published with the results says so. Licence terms are checked before publication (rule 12).

## Harness
Planned as a workspace crate, `bench/`, kept out of the engine's dependencies.

- **Generator.** Deterministic from a seed, producing the same readings, arrival order, late arrivals,
  revisions and outages for every tool. It starts from the generator of the encoding spike
  ([`spikes/encodings`](../spikes/encodings/), roadmap 2.1) and the paced load generator of the commit-latency
  spike ([`spikes/commit`](../spikes/commit/), roadmap 2.2, [results](measurements/2.2-commit-latency.md)),
  extended with the interval, scheduled and forecast shapes of W3 and the bursts of W4. The generated data set has a recorded hash.
- **Drivers.** One per tool behind a small common interface: open, define series, write a batch and wait for
  the acknowledgement, subscribe, query a chart, read as-of, close. Each driver states its schema and settings
  in one file. Capabilities a tool lacks are declared, not emulated silently.
- **Load control.** Open-loop sending at a set rate with intended send times recorded, so queueing delay is
  counted. The load generator and the recording client run on a second machine or pinned cores, stated per run.
- **Power-cut rig.** Three stages, each stricter than the last:
  1. a process kill loop, which checks recovery logic but not sync behaviour;
  2. a virtual machine or a loop device with fault injection that drops writes not yet flushed, cuts at chosen
     points and can fail a sync or fill the disk;
  3. for the final run, real hardware with a switched power supply and the acknowledgement record kept on
     another machine, on at least one consumer drive and one with power-loss protection.
- **Correctness checks.** Sums, charts and as-of answers are compared with direct computation over the generated
  data, the same way kvik's engine is tested against its [reference model](../model/).
- **Result format.** One directory per run: the environment (hardware, kernel, file system, tool versions,
  configurations, seed, commit of this repository), raw samples as CSV or Parquet, and a summary generated from
  them. Summaries are never edited by hand.
- **Where results live.** A page per run under [`measurements/`](measurements/), named for the roadmap item,
  with the tables, the filled-in durability table, the command that reproduces the run and a section listing
  where kvik did worse. Raw samples too large for the repository are attached to a release and linked.

## When
| Stage | After | Scope |
|---|---|---|
| Preview 1 | roadmap 4.8 (log benchmarks) | M1, M3, M4 (kill loop and fault injection only) and M8 on W2, against SQLite and one other tool |
| Preview 2 | roadmap 5.5 (delivery benchmarks) | M2 and M6 on W1, against the message-log pairing |
| Full run | roadmap 10.1 (scale test) | every workload and metric, every candidate, real power cuts; roadmap 10.2 |

Previews are published as previews: partial, on an unfinished engine, and replaced by the full run. Each uses the
same harness and the same fairness rules, so the drivers are exercised early.

## Limits of this plan
- Synthetic data. The generators imitate device data; they are not recordings. Compression and query results on
  real installations will differ, and the generator's parameters are published so others can vary them.
- One or two machines. Results hold for the hardware tested; the harness is meant to be run elsewhere.
- Embedded and server tools are not like for like. M13 and the notes say what each tool gives that the numbers
  do not show: SQL, concurrent writers, replication and an ecosystem on one side; no service on the other.
- The comparison is run by the project being compared. Rules 4, 5, 8 and 11 are there so that others can check the work.
