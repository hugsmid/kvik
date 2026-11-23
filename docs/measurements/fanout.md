# Fan-out of push delivery

What push delivery costs as subscribers are added, before and after selective fan-out (Q-1118, Q-1119).

## Method

- `cargo run --release -p kvik --example fanout -- --dir <scratch>`: the W0 home load, 37 series at 1 Hz with
  unaligned phases from 8 calling threads, each call one reading, acknowledged before the next; default store
  configuration. Every subscriber watches 2 to 6 series (the same draw in every run): a live session
  (`min_batch_interval` 20 ms) with a `Tail` of them waiting in `next(1 s)`, or a consumer routed to them that
  confirms each delivery. 5 s warm-up, then a 30 s window per cell, one cell at a time.
- CPU from `/proc` (whole process; the `kvik-writer` thread), as % of one core. Wakes: voluntary and involuntary
  context switches a second over every thread of the process. Reaction: from a reading's timestamp to its delivery.
- Machine: 4 cores, 16 GB, a VM, nothing else running. Before: the parent commit of this change (train-4 with the
  fast path and the seeded start), with the same example copied in. After: this change.

## Results

| mode | subscribers | process CPU % before | after | writer CPU % before | after | wakes/s before | after | reaction p50 / p99 ms before | after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| live | 5 | 3.23 | 2.80 | 1.07 | 1.07 | 565 | 329 | 1.25 / 16.8 | 1.36 / 17.0 |
| live | 20 | 4.67 | 3.10 | 1.13 | 1.03 | 1,597 | 393 | 1.23 / 16.2 | 1.15 / 16.1 |
| live | 50 | 7.47 | 3.70 | 1.33 | 1.10 | 3,612 | 553 | 1.38 / 15.0 | 1.37 / 15.5 |
| live | 200 | 24.40 | 6.87 | 2.90 | 1.17 | 15,204 | 1,625 | 2.15 / 17.4 | 1.53 / 17.2 |
| consumer | 20 | 8.13 | 3.07 | 1.07 | 1.10 | 3,520 | 493 | 1.58 / 6.8 | 1.44 / 5.8 |
| consumer | 50 | 23.73 | 4.73 | 1.00 | 1.07 | 9,983 | 947 | 3.18 / 12.2 | 1.68 / 8.1 |

Every cell delivered the same readings before and after (723 / 2,534 / 6,092 / about 24,091 live; about 2,560 and
6,170 to consumers, which start as their routes come into force).

- **Per added live session** (5 to 200): 0.109 % of a core before, 0.021 % after; wakes about 75 a second per
  session before, 6.7 after (each session's readings, about 4 a second, plus its own 1 s timeout).
- **Consumers** are no longer quadratic: 20 to 50 cost 15.6 points before, 1.7 after (0.055 % per consumer).
- **The writer thread** no longer pays the herd: 2.90 % at 200 sessions before, 1.17 % after, flat in sessions.
- **Reaction** is unchanged: p50 about 1.3 ms, p99 bounded by the 20 ms batch interval for live sessions.

## Against the research's targets

The research (fan-out CPU, 23 November) estimated about 2.5 % at 50 live sessions at box scale, with a harness that
does not poll, and about 0.011 % per added session, measured on an in-process probe outside kvik.

- At 50 the change gives 3.70 % here. The floor at 5 sessions is 2.80 %: the writer (1.07), the `kvik-fanout`
  thread's one round per commit (about 0.7), the `kvik-deliver` thread (about 0.5) and the callers. The research's
  probe floor at 5 was 2.17 % on this machine.
- Per session the change costs about 0.021 %, about twice the probe's: a woken session runs a full turn (the hub
  take, the walk of the round's pieces, the clone of its reads, its reach recomputed), about 45 µs a delivery against
  the probe's channel receive. The research's step B (one batch per distinct selection, no clones, the walk limited
  to the session's series) is the remaining lever; it was not part of this change.
- One cursor-file sync per tick for all consumers (the research's step D, second half) is not built: each consumer's
  file is still synced up to once a second (Q-1119).

## Changes measured

- A `kvik-fanout` thread in the writer's process, woken by the writer's turns (`Watch::on_progress`) and by refreshes
  of live staleness, runs the refresh round; once its view is published it rings only the sessions the round reaches.
  A session that took every earlier round passes over a round holding nothing it selects. Sessions wait on their own
  bell, with no 100 ms re-poll, and inside their batch interval wait on the bell rather than sleep while nothing rang.
- A view at the writer's durable position is taken without the reader's work lock, so sessions woken by one round
  take it side by side.
- Consumers: wakers per consumer; a confirmation wakes only its own consumer; an `emit` no longer reads its batch back
  or waits for wakes, the `kvik-deliver` thread does, taking the batch as of the emit's acknowledgement.
