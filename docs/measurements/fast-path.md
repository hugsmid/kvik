# Provisional streams: scaling and reaction with a 1 s gathering window

Spec §4 Provisional streams, Q-1109, Q-1110. `cargo run --release -p kvik --example fast_path -- --dir <scratch dir>`
(options in the example's header): a durable store on a local file system with `log.min_sync_interval` and
`log.sync_interval_floor` at 1 s; 37 series; one thread emits 370 calls a second, each one reading, for 30 s (11,100
calls, so p99.9 rests on 11 samples per run and p99 on 111); each subscriber is a thread reading a provisional
stream. A shared 4-core cloud machine, otherwise idle. CPU is in cores: the whole process, the emitting thread (the
`try_emit` caller, which decides, queues and publishes each call) and the log's writer thread; from `/proc`, at a
resolution of 10 ms. Emit is the `try_emit` call's own time. Provisional is the time from the call to the
subscriber, over every subscriber.

## Every series per subscriber

| Subscribers | Process | Emitting thread | Log writer | RSS | Emit p50 / p99 / p99.9 / max | Provisional p50 / p99 / p99.9 / max | Durable after p50 / max |
|---|---|---|---|---|---|---|---|
| 0 | 0.041 | 0.022 | 0.013 | 9.7 MiB | 0.03 / 0.08 / 0.45 / 4.5 ms | – | – |
| 1 | 0.057 | 0.028 | 0.014 | 17.0 MiB | 0.04 / 0.11 / 0.59 / 3.0 ms | 0.11 / 0.30 / 2.3 / 4.8 ms | 502 / 999 ms |
| 5 | 0.076 | 0.033 | 0.014 | 20.6 MiB | 0.06 / 0.15 / 1.0 / 4.3 ms | 0.12 / 0.33 / 1.9 / 4.5 ms | 501 / 1,000 ms |
| 20 | 0.120 | 0.037 | 0.011 | 33.3 MiB | 0.07 / 0.25 / 1.6 / 6.7 ms | 0.14 / 0.57 / 2.7 / 10.3 ms | 501 / 1,000 ms |
| 50 | 0.212 | 0.052 | 0.009 | 50.0 MiB | 0.14 / 0.54 / 1.3 / 4.3 ms | 0.19 / 0.83 / 2.4 / 5.7 ms | 504 / 1,003 ms |
| 200 | 0.810 | 0.214 | 0.014 | 123.7 MiB | 0.62 / 2.3 / 3.6 / 6.0 ms | 0.42 / 2.2 / 4.4 / 13.6 ms | 511 / 1,009 ms |

- The ring holds one copy of each call for every stream, and the log writer's cost stays flat. The emitting thread's
  cost does grow with subscribers: 0.028 cores with one, 0.214 with 200. With 200 threads waiting for every call,
  the emitting thread is woken back onto a crowded machine (4 cores, 200 runnable threads per call). The process's
  cost is mostly the subscribers' own wake-ups, about 10 µs of CPU per delivery.
- The herd. Each call wakes every subscriber that selects one of its series. Streams of every series share one
  bell, rung once per call. Before that change each stream had its own bell, and at 200 subscribers the emitting
  thread used 0.48 cores and the log writer 0.42 (emit p99 25 ms, max 84 ms), because the writer rang 200 bells
  per call and per settlement.
- Reaction stays within a few milliseconds while durability follows the 1 s window (half a window on average).
- RSS is the whole process, so it includes the subscriber threads and their stacks.

## One series per subscriber

| Subscribers | Process | Emitting thread | Log writer | RSS | Emit p50 / p99 / p99.9 / max | Provisional p50 / p99 / max |
|---|---|---|---|---|---|---|
| 1 | 0.044 | 0.024 | 0.014 | 13.6 MiB | 0.03 / 0.10 / 0.43 / 2.5 ms | 0.15 / 0.75 / 7.8 ms (300 deliveries) |
| 20 | 0.051 | 0.025 | 0.015 | 15.9 MiB | 0.04 / 0.12 / 0.51 / 1.7 ms | 0.11 / 0.29 / 2.7 ms |
| 200 | 0.124 | 0.041 | 0.030 | 22.3 MiB | 0.08 / 0.51 / 7.0 / 12.3 ms | 0.12 / 0.49 / 6.5 ms |

A call wakes only the streams whose selection holds its series, so 200 one-series subscribers cost about what 20
whole-store ones do.

## A slow and a failing subscriber

20 subscribers of every series, plus one that sleeps 10 ms per event and one that panics after 100 events (its
stream is dropped as it unwinds): the emitting thread used 0.037 cores (0.037 without them), emit 0.08 / 0.26 / 0.70 /
2.9 ms, and the other subscribers' provisional latency 0.13 / 0.43 / 1.7 / 4.4 ms. Neither extra subscriber moved the
writer or the others.

## A stream stalled for 60 s, then woken (the ring's hold)

5,000 calls a second for 70 s (350,000 calls), one subscriber of every series, and with it, in the second run, a
stream with `max_behind` at `PROVISIONAL_MAX_BEHIND` that reads nothing for 60 s and then catches up on about
300,000 calls:

| Run | Emit p50 / p99 / p99.9 / max | Emit in the 2 s after the wake, p99.9 / max |
|---|---|---|
| No stalled stream | 0.03 / 0.07 / 0.31 / 8.2 ms | – |
| Stalled stream | 0.02 / 0.06 / 0.32 / 6.6 ms | 0.82 / 4.0 ms |

The take itself, measured on the ring with a full backlog of 1,048,576 one-fact calls in a release build
(`take_fresh_hold_of_the_ring_with_a_full_backlog`, ignored by default): 256 holds of at most 4,096 calls, the longest
0.13 ms (median 0.08 ms). The same take in one hold, as before the bound, held the ring for 30.8 ms. So a stream
catching up makes the writer wait at most about 0.13 ms per call it hands to the ring. Emit maxima of a few
milliseconds appear with or without a stream, and come from scheduling on the shared machine.
