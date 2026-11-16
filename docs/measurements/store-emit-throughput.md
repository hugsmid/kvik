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

