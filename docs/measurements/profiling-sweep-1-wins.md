# The first profiling sweep's quick wins: before and after

Instructions and CPU of a start, a clean shutdown, delivery, the duplicate rule's slow path and `emit` on the
durable backend, before and after the quick wins of the first profiling sweep: one search for a segment's last non-
zero byte, done a block at a time; one decode per batch; whole-batch checksums only for the reader's last 64
batches; no delivery records built while the deliverer loads; series looked up by id in a vector at a start and on
the writer thread; the registry file of a routing change and of the shutdown, and `series`, `losses` and
`series_stats`, answered from the deliverer's held state; the writer's lookup reader freeing its start bytes; the
reader's first refresh in parts; a boxed entry variant in log items; condition timers and confirmations without per-
call maps; lookups reading batches into buffers of their own length and keeping their map across a recovery entry
(Q-831); and a call's own facts kept in maps only for the keys and series it repeats.

## Method
- **Builds.** Before is the bounded-history tree with the small store-path wins merged; after is the tree with the
  quick wins. Release builds of the same harness, a scratch program that calls the public API as the examples do.
- **Instructions** are Valgrind's callgrind counts of the whole process (every thread), exact for the run; one
  run per figure. A cost per reading is the run's count less that of the same run with nothing in it, divided by
  the readings. The slow path is counted inside `emit` on the caller's thread only.
- **CPU seconds** are the process's user and system time (`/proc/self/stat`, 10 ms ticks), peak resident memory
  `VmHWM`; six runs per build, the builds alternated, on a 4-core shared virtual machine. **Indicative.** Ranges
  are min–max.
- **Stores.** Gauges with a 1 s expected interval, one reading per series per call, each call a second after the
  last, ending a minute before now, `log.min_sync_interval = 0`, default limits. "1 M": 2,000 series × 500 calls
  (one open segment of 20.5 MB). The slow-path store: 500 series × 2,000 calls, so that early keys lie below the
  windows.

| Measure | Shape |
|---|---|
| Start | open the 1 M store and shut it down |
| Delivery | 200 series × 300 calls; one consumer on its own thread calling `next` and `confirm` per delivery, with a fact funnel on every series, and with 20 threshold funnels beside it |
| Slow path | open the slow-path store, then calls of 20 readings below the windows (10 replays of recorded keys, 10 new keys) |
| `emit` | 2,000 series × 300 calls of 2,000 readings; 200 series × 3,000 calls of 2 readings; then a clean shutdown |

## Results: instructions
| Measure | Before | After | Change |
|---|---|---|---|
| Start of the 1 M store, per reading (process, shutdown included) | 8,272 | 2,958 | −64 % |
| … the open (`Writer::open`) | 6,584 | 2,690 | −59 % |
| … of which the log writer's recovery | 2,330 | 440 | −81 % |
| … of which the writer's state (registry, counters, decision windows) | 1,654 | 1,121 | −32 % |
| … of which the deliverer's open | 2,579 | 1,108 | −57 % |
| … the clean shutdown | 1,374 | 32 | −98 % |
| Delivery per reading, one consumer with a fact funnel | 7,966 | 5,824 | −27 % |
| Delivery per reading, with 20 condition funnels beside it | 43,206 | 22,419 | −48 % |
| Slow path, first call after the start | 617 M | 450 M | −27 % |
| Slow path, each later call (mean of five) | 44.5 M | 2.8 M | −94 % |
| `emit`, 2,000 readings per call, per reading (process, shutdown included) | 9,557 | 5,812 | −39 % |
| … inside `Writer::emit` on the caller | 4,536 | 2,772 | −39 % |
| `emit`, 2 readings per call, per reading (process) | 60,449 | 53,455 | −12 % |

The shutdown of the `emit` runs read the whole log before (1,380 instructions a reading of the 9,557); after, it is
under 1 % of the run. The row inside `Writer::emit` is the one to compare for the write path itself.

A segment holding a recovery entry (Q-831), 22 to 32 M instructions per slow reading before by the sweep's profile,
was not measured again: its later lookups now read only the mapped batches that may hold a key, as a clean segment's
do (`a_segment_holding_a_recovery_entry_keeps_its_map_and_lookups_stay_exact` counts the decodes).

## Results: CPU and memory (indicative)
| Measure | Build | CPU | Peak resident |
|---|---|---|---|
| Start of the 1 M store (open and shutdown, process) | before | 2.44–2.96 s | 579 MiB |
| | after | 1.03–1.32 s | 229 MiB |
| … the open alone | before | 1.47–1.73 s | 268 MiB after the open |
| | after | 0.77–1.03 s | 229 MiB after the open |
| `series` and `series_stats` of 2,000 series and `losses`, just after the open | before | 0.74–0.90 s | 268 → 598 MiB |
| | after | 0.00–0.01 s | 229 → 229 MiB |
| Delivery, 20 condition funnels, per reading | before | 7.7–10.3 µs | 48–51 MiB |
| | after | 3.7–6.7 µs | 26–30 MiB |
| Slow path, later calls (calls 3 to 6), wall per call | before | 1.9–24.5 ms | 468 MiB |
| | after | 0.5–1.9 ms | 150 MiB |
| `emit`, 2,000 readings per call, CPU per reading | before | 1.85–2.13 µs | 365–375 MiB |
| | after | 1.60–1.88 µs | 165–166 MiB |

- Delivery to one plain consumer and `emit` at 2 readings per call change by less than the 10 ms tick resolution
  allows to see (0.1 s and 0.5 s a run); their instruction counts are above.
- The peak memory of a start no longer includes the shutdown's index of every fact; the running store holds 39 MiB
  less after the open (the boxed entry variant, no records while the deliverer loads, the lookup reader's start bytes
  freed). The first `series`, `series_stats` or `losses` call, and a routing change, no longer build an index of
  every fact for the life of the process; the figures above are for a store without consumers, and a routing
  change reads only what it appended (`a_routing_change_writes_the_registry_file_a_full_read_gives_without_reading_the_log`).
  When the log cannot be read, or the deliverer has not taken what the writer acknowledged, the store's own reader
  answers as before and builds that index.
- The second slow call took 16 to 21 ms in every run before and 13 to 16 ms in five of six after (1.2 ms once).
  After, the later calls average 2.8 M instructions, so that time is not compute.

## What is left
- **Delivery with condition funnels**: held-for and stale-for funnels are still evaluated at every poll (Q-830,
  decided: skipping one that is not due is not exactly equivalent).
- **The slow path's first call** after a start still scans every segment that may hold a key, decoding each batch
  once; a sealed segment is scanned to its end again by every lookup that reaches it.
- **A start** still reads the log three times (recovery, the writer's state, the deliverer); one read feeding all
  three is the sweep's first larger lever. Q-832 and Q-835 stay open.
