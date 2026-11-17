# Store emit throughput: the write path above the log writer

Readings per second and CPU of `Store::emit` on the durable backend, before and after four changes to the path
above the log writer: the delivery reader reads only a segment's new bytes at a refresh, and skips a refresh at the
writer's durable position (Q-704); live staleness is refreshed with one clock read and one lock per call; the
decision maps skip the version lookup without `changed_at` and move a call's history in. The harness is
[`kvik/examples/emit_throughput.rs`](../../kvik/examples/emit_throughput.rs). The figures are **indicative**: a 4-core
shared virtual machine with other work running, not the Linux NVMe target.

```sh
cargo run --release -p kvik --example emit_throughput -- --dir <scratch dir> --writers 1 --per-call 2000
cargo run --release -p kvik --example emit_throughput -- --dir <scratch dir> --writers 16 --per-call 2
```

## Method
- 2,000 series (gauges, every twentieth a boolean state, expected interval 1 s), `log.min_sync_interval = 0`, no
  consumer, the default system environment on a virtual disk. Each writer thread holds its own `Store` clone and
  emits 1,500 calls, each at one new timestamp, series taken in turn from its share; every reading is recorded.
- Cores are the process's user and system CPU over the wall time of the emits (`/proc/self/stat`).
- Before is the tree just before the changes, with the harness added; after is the tree with them. The two builds
  alternated three times, each run started only once the one-minute load average was under 2.0. Ranges are min–max
  of the three runs.

## Results
| Shape | Build | Calls/s | Readings/s | Cores | CPU per reading |
|---|---|---|---|---|---|
| 1 writer × 2,000 readings per call | before | 188–212 | 375–423 k | 0.97–1.05 | 2.3–2.8 µs |
| | after | 239–270 | 478–540 k | 1.02–1.03 | 1.9–2.2 µs |
| 16 writers × 2 readings per call | before | 3,560–5,110 | 7.1–10.2 k | 1.22–1.31 | 119–173 µs |
| | after | 20,850–21,190 | 41.7–42.4 k | 1.06–1.35 | 25–32 µs |

- **One writer, 2,000 readings per call:** 1.3× the readings per second (medians 376 k and 489 k), CPU per reading
  down about a fifth. The store still uses one core: the caller's read-back after each acknowledgement and the
  `kvik-deliver` thread take turns on the deliverer's lock. Leaving the read-back to that thread when no consumer is
  registered roughly doubled this shape in a prototype; it changes when the delivery counts in `Health` are updated
  and is not built (Q-706).
- **16 writers, 2 readings per call:** 4–6× the calls per second. With one batch sync acknowledging many calls,
  only one refresh of the delivery reader now runs per sync instead of one per caller plus one on `kvik-deliver`.

## Memory
A reader now keeps the bytes of the segment it follows up to the mark between refreshes, instead of allocating and
zeroing that prefix at every refresh: at most one segment's bytes per reader handle (64 MiB at the default
`segment_size`) while it follows a segment. A whole-file read of an earlier segment is freed when its refresh ends,
and the kept bytes are freed when the reader follows another segment, so after a rollover only the new segment's
prefix is held. This applies to the store's delivery reader and to every reader process handle.

## After freeing the kept bytes
Re-measured once that change was in, the same way (three alternated runs, load under 2.0):

| Shape | Build | Calls/s | Readings/s | Cores | CPU per reading |
|---|---|---|---|---|---|
| 1 writer × 2,000 readings per call | before | 168–257 | 335–514 k | 0.90–1.08 | 2.1–2.7 µs |
| | after | 232–261 | 464–521 k | 0.97–1.01 | 1.9–2.1 µs |
| 16 writers × 2 readings per call | before | 3,330–4,810 | 6.7–9.6 k | 1.29–1.32 | 137–197 µs |
| | after | 18,260–21,300 | 36.5–42.6 k | 1.21–1.48 | 33–35 µs |

The gain holds: the ranges for one writer overlap here because of one fast run of the old build, while the medians
(436 k and 519 k) and CPU per reading keep the earlier picture; 16 writers stay at 4–6×.


## Bounded decision history (roadmap 11.0)
The writer's decision state and the deliverer's facts are bounded by the recent window (spec §5.1) and the
revised-key index; keys outside them are read from the local log (the slow path, spec §2.4). Harness:
[`kvik/examples/history_memory.rs`](../../kvik/examples/history_memory.rs), one writer, one reading per series per call,
each call one second after the last, `log.min_sync_interval = 0`, no consumer. Before is the tree the change is based
on, with the harness added; after is the tree with it. The builds alternated, each run started once the one-minute
load average was under 2.0, on the same 4-core shared virtual machine. **Indicative.**

**Memory as readings accumulate** (2,000 series, 2,000 readings per call, 4 million readings; resident memory in MiB,
the same in both runs of each build):

| Readings | 0.5 M | 1 M | 2 M | 3 M | 4 M |
|---|---|---|---|---|---|
| before | 284 | 546 | 1,084 | 1,532 | 2,089 |
| after | 125 | 233 | 424 | 443 | 400 |

Before grows about 520 MiB per million readings. After levels off once 15 minutes of readings are held (1.8 million
readings here): about 1.8 million facts in the writer's window and as many in the deliverer's, some 400–440 MiB in
all, whatever the store's age. Readings per second (mean over the run): before 456 and 509 k, after 458 and 464 k.

**A workload with late readings** (500 series, 6,000 calls, 3 million readings; after the first hour every tenth call
also repeats ten readings of an hour earlier, duplicates, and records ten new readings two hours earlier, all outside
the window): 4,800 readings took the slow path (0.16 %), in 240 calls, one read of the log per call.

| Build | Resident at 3 M | Call with late readings | Other calls | Readings/s (mean) |
|---|---|---|---|---|
| before | 1,532 MiB | 1.98–2.26 ms | 1.55–1.80 ms | 258–291 k |
| after | 159–160 MiB | 6.3–6.4 ms | 1.63–1.66 ms | 289–291 k |

**Cost of the slow path on the ingest path**, after: the extra time of a call whose 20 late readings are read in one
pass, over the other calls of the same run, and per slow-path reading. The read keeps, per segment, each batch's place
and range of event times, so a later read decodes only the batches that may hold a key and those appended since; the
first read after a start maps every segment it needs. A first version without that map is shown for comparison.

| Store | Readings | Extra per call | Per slow-path reading | Without the map, per reading |
|---|---|---|---|---|
| small: 100 series | 0.5 M | 1.0 ms | 0.05 ms | 1.4 ms |
| middle: 500 series | 3 M | 4.7 ms | 0.23 ms | 6.8 ms |
| large: 2,000 series | 10 M | 12 ms | 0.60 ms | 16 ms |

The cost grows with the bytes of the segments a key's event time may be in (the open segment is scanned for new
batches at every read); a replay of an hour across many series is a stated slow path (spec §13 Duplicate check on
replay).

**Start of a store holding 4 million readings** (2,000 series, 193 MB on disk; each open in a new process):

| Build | Open | Resident after the open |
|---|---|---|
| before | 9.0–10.1 s | 3,509 MiB |
| after | 7.3–9.6 s | 428 MiB |

The start still reads the whole log once (Q-752): it keeps only the window, the revised keys met in it and the
counters, so its memory is bounded; its time stays proportional to the log until tree checkpoints carry that state.

## With the store-wide bound (`limits.window_memory`, Q-751)
After the review: the windows are bounded by `limits.window_memory` as well as per series, the revised-key index and
the lookup's caches by bytes, and a segment is read a part at a time. Same harness and conditions, one or two runs
each, the builds alternated where both ran; this day the machine ran the old build about 40 % slower than in the runs
above, so compare within this section only. **Indicative.**

**Resident memory (MiB) as readings accumulate**, one reading per series per call:

| Series, build, `window_memory` | 0.5 M | 1 M | 2 M | 3 M | 4 M | 6 M | 10 M | 15 M |
|---|---|---|---|---|---|---|---|---|
| 2,000, before | 284 | 546 | 1,084 | 1,532 | 2,089 | | | |
| 2,000, after, 512 MiB (default) | 125–126 | 234 | 425 | 444 | 401 | | | |
| 2,000, after, 64 MiB (minimal class) | 98 | 139 | 158 | 177 | 134 | | | |
| 10,000, before | | 598 | 1,123 | 1,578 | 2,122 | 3,020 | | |
| 10,000, after, 512 MiB | | | | | | | 680 | 711 |
| 10,000, after, 64 MiB | | | | | | | 145 | 177 |

At 10,000 series the after build was sampled every 2.5 million readings: 568–727 MiB at 512 MiB, 145–193 MiB at
64 MiB, flat from the first sample. What remains above the bound is the log readers' kept segment bytes (up to one
64 MiB segment each, which is the saw-tooth in the rows: it falls at each segment's rollover), the ingest queue and
the process's own allocations. Readings per second at 2,000 series (mean over the run): before 302 and 307 k, after
at 512 MiB 258 and 273 k, at 64 MiB 358 and 355 k.

**Cost of the slow path on the ingest path**, measured again (the same late-reading workload as above; extra time of a
call with 20 late readings over the other calls of the run, and per slow-path reading):

| Store | Readings | Call with late readings | Other calls | Per slow-path reading |
|---|---|---|---|---|
| small: 100 series | 0.5 M | 2.3 ms | 0.78 ms | 0.08 ms |
| middle: 500 series, 512 MiB | 3 M | 9.6 ms | 2.0 ms | 0.38 ms |
| middle: 500 series, 64 MiB | 3 M | 10.7 ms | 2.9 ms | 0.39 ms |
| large: 2,000 series | 10 M | 26 ms | 7.5 ms | 0.94 ms |

Worst case, not measured here: the first read of a segment after a start scans it, 64 MiB at the default
`segment_size`, a part of 4 MiB at a time (`docs/api.md` Configuration, Memory), under the decision lock that every
`emit` and entry call takes.

## CPU per reading, with the append path
At the re-check the windows' ordered insert (a binary search over the window for every fact, at about 900 entries a
series) was found to cost a third more CPU per reading than the unbounded store; a fact that sorts at or after a
window's last entry is now appended without a search. CPU time (user and system) of the process for 2,000 series ×
2,000 calls (4 million readings, no late reading), three alternated rounds, the one-minute load under 3 (shared
machine, so CPU time rather than wall time is the figure); the integration tests pass on every build. **Indicative.**

| Build | CPU seconds, each round | Mean | Per reading | Wall seconds | Resident at the end |
|---|---|---|---|---|---|
| before 11.0 (unbounded) | 15.7, 17.2, 17.6 | 16.8 | 4.2 µs | 16.8–18.6 | 2,089 MiB |
| bounded, before the append path | 21.7, 22.1, 22.9 | 22.2 | 5.6 µs | 18.3–19.1 | 400 MiB |
| bounded, with the append path | 13.3, 13.6, 14.4 | 13.8 | 3.4 µs | 13.7–14.4 | 400 MiB |

The append path takes 38 % off the bounded store's CPU and puts it 18 % below the unbounded store's; late facts
still search.

## No hub work in `emit` while no consumer is registered (Q-706)
With no consumer registered, `emit` neither runs the timers nor reads its batch back nor waits for wakes; the
`kvik-deliver` thread takes the batch shortly after. Same harness and shapes (2,000 series, 1,500 calls per writer,
`log.min_sync_interval = 0`, no consumer). Before is the tree just before the change, after is the tree with it; the
builds alternated three times, each run started once the one-minute load average was under 2.0, on the same 4-core
shared virtual machine. **Indicative.** Ranges are min–max of the three runs.

| Shape | Build | Calls/s | Readings/s | Cores | CPU per reading |
|---|---|---|---|---|---|
| 1 writer × 2,000 readings per call | before | 196–202 | 392–405 k | 1.35–1.39 | 3.4–3.5 µs |
| | after | 300–311 | 601–623 k | 1.93–1.96 | 3.1–3.2 µs |
| 16 writers × 2 readings per call | before | 24,470–25,910 | 48.9–51.8 k | 1.62–1.66 | 31–34 µs |
| | after | 31,220–32,780 | 62.4–65.6 k | 1.60–1.64 | 25–26 µs |

- **One writer, 2,000 readings per call:** about 1.5× the readings per second (medians 395 k and 613 k). The store
  now uses about two cores: the caller decides its next call while `kvik-deliver` reads the last batch back, where
  before they took turns on the deliverer's lock. CPU per reading barely changes; the gain is wall time.
- **16 writers, 2 readings per call:** about 1.3× the calls per second. The callers no longer queue on the
  deliverer's lock for the timers and the read-back after each acknowledgement.
- The gain needs a second core: the store used 1.93–1.96 cores for one writer after the change against 1.35–1.39
  before, so on a single core the caller and `kvik-deliver` would take turns again.
- How far `kvik-deliver` runs behind (a separate probe: four writers, 300 calls of 500 readings each, in-memory files,
  three runs; **indicative**): at most 3,500 to 6,000 readings behind the last acknowledged position of 600,000, 0 to
  2,000 behind when the writers stopped, and the first `take_consumer` after the burst took 0.1 to 3 ms. No backlog
  builds up.
- A store with a consumer registered behaves as before.
