# One-series chart reads against one read of every series

A dashboard that charts each series with its own `buckets` call made one pass over the whole log per call once the
fact index was gone (roadmap 9.0h, Q-1130): 37 series, 37 passes. Measured with
[`kvik/examples/series_reads.rs`](../../kvik/examples/series_reads.rs):

```sh
cargo run --release -p kvik --example series_reads -- --dir <scratch dir> --days 1 [--shape w0|slice]
```

## Method
- 37 series (gauges, counters and states in turn), a reading a second each, event time ending at the build's wall
  clock; one writer, default configuration apart from syncs left out while building (and no sync-rate budget). The
  store is shut down, then a fresh reader (default 64 MiB cap) charts the last 24 hours at
  `Resolution::Auto { max_points: 1_500 }`.
- Shape `w0` is the home load (bench W0): each series on its own phase with ±100 ms jitter, 0.5% late by 1 to 5 s,
  0.1% missing, the arrivals of each 10 ms in one call, so most batches hold one reading, as the home run's syncs
  follow the arrival rate. Segments are sealed about every five minutes of event time, as at home: 288 a day.
  Shape `slice` is 60-second slices of every series in one call, so that every batch holds every series (the 9.0h
  harness).
- Queries: one series alone; every series one call at a time; every series in one `buckets_of` call; three dashboards
  at once, each on its own thread through the same reader, one call per series or one `buckets_of` each. Each figure
  is the best of the runs after the first on the reader, with the bytes the process read (`rchar`, page cache warm).
  Warm runs grew the process's peak resident memory by at most 2 MiB alone and 31 MiB for three at once.
- Before is `655ec25`; the index path is `3cd6011`, before roadmap 9.0h, whose first read builds the whole store's fact
  index. Machine: a shared cloud virtual machine with four cores; runs of one query vary by up to 10%.

## Before

| Store | Query | Before | Index path |
|---|---|---:|---:|
| W0, 1 day (641 MiB, 2.7 million batches) | one series | 1.49 s, 712 MB | 11.5 ms |
| | 37 series, one call each | 55.8 s, 26.3 GB | 0.32 s |
| | 37 series, one `buckets_of` | 2.46 s, 624 MB | |
| | 3 dashboards at once, one call each | 178 s, 83 GB | 0.32 s |
| | 3 dashboards at once, `buckets_of` | 6.91 s, 1.9 GB | |
| W0, 7 days | one series | 2.57 s, 1.14 GB | 10.2 ms |
| | 37 series, one call each | 89.5 s, 42 GB | 0.32 s |
| | 37 series, one `buckets_of` | 2.91 s, 632 MB | |
| | 3 dashboards at once, one call each | 275 s, 127 GB | 0.32 s |
| | 3 dashboards at once, `buckets_of` | 8.30 s, 1.65 GB | |
| slice, 1 day (193 MiB) | one series | 104 ms, 68 MB | 11.9 ms |
| | 37 series, one call each | 3.81 s, 2.5 GB | 0.30 s |
| | 37 series, one `buckets_of` | 0.54 s, 74 MB | |
| | 3 dashboards at once, one call each | 10.9 s, 7.5 GB | 0.50 s |
| | 3 dashboards at once, `buckets_of` | 1.22 s, 254 MB | |
| slice, 7 days | one series | 113 ms, 68 MB | 11.5 ms |
| | 37 series, one call each | 3.90 s, 2.5 GB | 0.31 s |
| | 37 series, one `buckets_of` | 0.56 s, 74 MB | |
| | 3 dashboards at once, one call each | 10.8 s, 7.5 GB | 0.33 s |

The index path's first read builds its index: 2.5 s and 441 MiB on the 1-day W0 store, 20 s and 2.9 GiB on the 7-day
one; the warm figures leave that out.

### Reading
- At the home load a one-series call reads the whole log more than once: most batches hold one reading of one series,
  every batch of a day's range is read and checked whatever series it holds, and a pass of a day of one series
  (86,400 facts) passes its grant and is read again in halves. Calls one at a time cost 23 to 31 times one
  `buckets_of`, and `buckets_of` itself reads the log about once.
- In the slice shape every batch holds every series, so a one-series call reads the log once: 7 times `buckets_of`.
