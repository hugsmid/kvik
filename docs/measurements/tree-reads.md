# Tree reads: charts from stored nodes (7.1b step 5, budget S7)

Chart reads (`buckets_of`, `Resolution::Auto { max_points: 2_000 }`) served by the summary tree against the fact
path they replace, at one, two and ten years. Measured with
[`kvik/examples/tree_reads.rs`](../../kvik/examples/tree_reads.rs):

```sh
cargo run --release -p kvik --example tree_reads -- --dir <scratch> --shape years --years 10 [--merge]
cargo run --release -p kvik --example tree_reads -- --dir <scratch> --shape day37 --days 1
```

## Method

- `years`: one gauge with a reading every 60 s (so it stores the 15-minute level, not the 1-minute one), event time
  ending at the build's wall clock; 0.5, 1.1 and 5.3 million facts. `day37`: 37 series (gauges, counters, states) at
  a reading a second for one day, 3.2 million facts. Built once on the local disk, log only (no compaction); the
  writer is shut down with its reader open, so that the clean shutdown checkpoints every quarter-hour.
- `--merge` then runs the tree's merge planner to the end (day parts with their 2-hour and 1-day roll-ups, then
  weeks, 13 and 52 weeks), as the writer's scheduled merges will (step 4). A year is then 6 parts, ten years 15;
  unmerged, one delta part a day (365 and 3,650).
- Each chart on a fresh `Reader` in the measuring process (the second-process case; no writer runs): its open and
  first refresh, the first chart (cold reader, warm page cache), then the best of three more (one at ten years
  before). Bytes read are the process's `rchar`; peak is its `VmHWM`, the whole process, not a per-query count.
- Before is the base commit of this step (587c7ce), the same example with `--merge` left out, on the same stores.
  After is this step's head (unmerged figures from an earlier commit of this step, before facts were read in one pass
  for all series). Machine: shared cloud virtual machine, four cores, 15 GB. Single runs; two runs of one build
  differed by up to 25 % on first reads and 10 % warm.

## Results

One gauge, `years` (milliseconds; first read / warm; MB read by the first and by a warm read):

| Store | Chart | Before | After, unmerged | After, merged |
|---|---|---:|---:|---:|
| 1 year | year | 101 / 75 (24.5 / 13.8) | 99 / 32 (14.4 / 0.6) | 83 / 18 (14.5 / 0.3) |
| | week | 24 / 2.4 | 30 / 2.8 | 24 / 2.3 |
| 2 years | two years | 192 / 170 (49.4 / 27.6) | 169 / 65 (28.7 / 0.5) | 185 / 38 (29.1 / 0.3) |
| | year | 141 / 87 | 101 / 27 | 117 / 23 |
| | week | 49 / 2.6 | 52 / 3.0 | 55 / 3.2 |
| 10 years | ten years | 1,146 / 957 (241 / 131) | 887 / 259 (143 / 0.7) | 894 / 181 (145 / 0.3) |
| | two years | 313 / 219 | 187 / 52 | 279 / 48 |
| | year | 187 / 102 | 132 / 30 | 183 / 32 |
| | week | 111 / 3.2 | 97 / 6.5 | 107 / 12.9 |

Every first read also pays the reader's open and refresh (17 to 120 ms, the same before and after) and the facts of
the chart's edges, which open the reader's fact index over the whole log (11, 22 and 40 MB of the first read's bytes
above). Day and ten-minute charts take the fact path in both builds (a step under 15 minutes, Q-2071) and are
unchanged (0.2 to 0.4 ms warm). Peak memory after: 40, 72 and 236 MiB merged (unmerged ten years: 1,023 MiB, every
day part decoded whole); before: 26, 49 and 82 MiB.

37 series, one day at a second (`day37`), every series in one `buckets_of` call:

| Chart | Before | After |
|---|---:|---:|
| year | 718 / 520 ms (156 / 87 MB) | 304 / 77 ms (74 / 0.8 MB) |
| week | 715 / 464 ms (153 / 83 MB) | 246 / 30 ms (74 / 0.8 MB) |

## What it means

- Before, every long chart read the whole log each time: warm reads grew with the log (957 ms and 131 MB for ten
  years, 520 ms for a 37-series year chart of one day). After, a warm chart reads under 1 MB whatever the age; its
  time is the tree's.
- S7 desktop-class (≤ 50 ms warm, ≤ 150 ms cold): met at one and two years (18 to 48 ms warm); missed at ten years
  (181 ms warm merged, 894 ms first). Minimal class (≤ 200 ms warm, ≤ 1 s cold): met at every age measured here.
- The ten-year warm time is not the merges (the roll-ups make about 40 a bucket, 80,000 a chart, about 14 ms at the
  measured 0.17 µs): it is forming the series' visible entries over the whole range, about 400,000 keys, every call.
  Looking entries up in the parts directly, or keeping the visible entries per root, removes it (Q-2074). The nested
  ladder of Q-1424 would cut merges, which are not the cost, so it stays unbuilt.
- The first read is dominated by the fact index the edges open (a whole-log read, the fact path's own cost, Q-1000)
  and by decoding parts whole (Q-2072). Charts ending at now always have a partial last bucket, so the edges are
  always read; compaction's chunks (6.2b) take the whole-log term away.
- Merging matters for memory more than time: ten unmerged years decode 3,650 parts on the first ten-year chart.
