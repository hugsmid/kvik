# Tree parts: entry sizes and the cost of a decode and merge

What a stored node costs in a tree part (`format-tree.md` T.8 to T.11) and what a reader pays to decode a node and
merge it into a running result: the step 1 measurements of the 7.1b design, whose risk was a decode and merge above
about 1 µs.

## Method

- `cargo run --release -p kvik-tree --example fold_cost`: one gauge read every 5 s at 0.01 resolution, a UTC day of
  it (17,280 readings); the 1,440 minute nodes folded, then merged up the levels of `LEVELS` (15 minutes, 2 hours,
  1 day), all four runs in one day part.
- Bytes an entry: the part with one level's run, less the same part with no runs, divided by the run's entries; so
  it counts the entry fields, the nodes block heads and framing, and the directory records.
- Decode and merge: the encoded 15-minute nodes of the day (T.2 bytes, as a part holds them), each decoded with every
  check of T.6 and merged left to right into a running node; 200 rounds of the 96 nodes. Merge alone: the same over the
  decoded nodes.
- Whole-part decode: `Part::decode` of the day part, every check of T.11 including the comparison with a re-encoding,
  per entry.
- Machine: 4 cores, 16 GB, a shared cloud VM. Three runs; the figures are the middle one, the others within 3 %.

## Results

| Level | Entries a day | Bytes an entry in a part | of which node bytes |
|---|---:|---:|---:|
| 1 minute | 1,440 | 92.2 | 85.9 |
| 15 minutes | 96 | 96.2 | 88.9 |
| 2 hours | 12 | 109.3 | 92.0 |
| 1 day | 1 | 248.0 | 93.0 |

| Operation | Cost |
|---|---:|
| decode and merge of a 15-minute gauge node | 0.30 µs |
| of which merge | 0.17 µs |
| whole-part decode, every check | 0.73 µs an entry |
| fold | 0.08 µs a reading |

## What it means

- Entry overhead is about 6 to 8 B a node in a long run, as the design assumed (7 B). The day level's 248 B is one
  entry alone in its run, paying a block head and a directory record; in a 52-week part its run holds 364 entries
  and the overhead falls to that of the other levels.
- A series-year of the 5 s gauge: 3.4 MB at 15 minutes, 0.5 MB for both roll-ups, 48 MB at 1 minute (Q-1420).
- A decode and merge costs 0.30 µs, below the 1 µs at which the plan changes. A year chart of 37 series from
  15-minute nodes alone would take about 0.39 s (37 × 35,040 merges); with the roll-ups, one node a day plus the edges,
  about 5 ms. The roll-ups (Q-1421) still turn a year from 35,040 merges into about 400, but they are no longer what
  keeps such a chart under a second.
- Node bytes stay T.2 verbatim (Q-1426): stripping the start (8 B) and width (6 B) would save about 14 B of the 96,
  15 %, at the price of a node form that differs from a chunk summary's.
