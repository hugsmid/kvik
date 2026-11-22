# Provisional streams: scaling and reaction with a 1 s gathering window

Spec §4 Provisional streams, Q-1109. `cargo run --release -p kvik --example fast_path -- --dir <scratch dir>`: a
durable store on a local file system with `log.min_sync_interval` and `log.sync_interval_floor` at 1 s; 37 series,
one reading of each a second, each its own call (37 calls a second); each subscriber a thread reading a provisional
stream of every series; 10 s per count, on a shared 4-core cloud machine.

| Subscribers | Cores (process) | RSS | Provisional p50 | p99 | Max | Durable after, p50 / max |
|---|---|---|---|---|---|---|
| 0 | 0.005 | 5.9 MiB | – | – | – | – |
| 1 | 0.005 | 7.4 MiB | 0.09 ms | 0.29 ms | 4.0 ms | 513 / 1,004 ms |
| 5 | 0.008 | 7.6 MiB | 0.13 ms | 0.25 ms | 3.0 ms | 507 / 999 ms |
| 20 | 0.012 | 9.3 MiB | 0.14 ms | 1.54 ms | 5.4 ms | 512 / 999 ms |
| 50 | 0.037 | 10.9 MiB | 0.25 ms | 1.26 ms | 5.4 ms | 512 / 1,007 ms |
| 200 | 0.111 | 16.7 MiB | 0.52 ms | 2.08 ms | 5.8 ms | 511 / 994 ms |

- One copy of each call serves every stream; the writer's cost does not grow with them. What grows is waking each
  subscriber's thread for each call, about 14 µs of CPU per delivery here, and each thread's stack, about 55 KiB.
- Reaction stays within a few milliseconds while durability follows the 1 s window (half a window on average).
- RSS is the whole process, so it includes the subscriber threads the benchmark starts.
