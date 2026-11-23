# Seeded start: reopen time against store age

Spec §3 Order of a start (seeded start), Q-1110 to Q-1112. A probe builds the home workload W0 (37 series, about 37
readings a second, accelerated event time, 60 s of arrivals a call, durable defaults) to 1 or 7 days, or W0x2 (74
series) to 7 days, then times `Store::open` and the first `latest` read after it; release builds, page cache warm,
a shared 4-core cloud machine, three opens each. Before is the base of this change, after is its head; each store
was built by the build it is opened with.

| Store | Before: open | After: open | Before: first `latest` | After: first `latest` |
|---|---:|---:|---:|---:|
| W0, 1 day (129 MiB, replayed, nothing sealed) | 2.03–2.08 s | 0.53–0.59 s | 0.85 s | 0.37 s |
| W0, 7 days (512 MiB, replayed) | 7.13–7.64 s | 0.66–0.74 s | 0.77 s | 0.33 s |
| W0x2, 7 days (960 MiB, replayed) | 15.4–18.4 s | 0.63–0.66 s | 0.94 s | 0.16 s |
| W0, 7 days, then 16.5 min at live rate, clean stop | 6.69–7.23 s | 0.10–0.12 s | – | – |
| W0, 7 days, then 16.7 min at live rate, process killed | 6.60–6.88 s | 0.18–0.19 s | – | – |

- After, the open is flat with age: the stores differ by a factor of seven in size and the open by 0.2 s. The first
  open of a 7-day store after other work had pushed it out of the page cache took 1.3–1.4 s.
- Of a replayed store's 0.55–0.69 s, recovery takes 0.40–0.56 s: it decodes the batches of the unsealed segment,
  which a replayed build fills to 64 MiB, to rebuild that segment's index for its seal. Scanning only to the mark
  (the research's F6) would save the read of the zero-filled rest, which is small here; it was not built.
- At live rates a seal every 5 minutes and a series directory every 15 keep the log after the seed short; a clean
  stop writes a directory through the mark, so the next open reads almost nothing of the log.
- The first `latest` overlaps the damage scan of the log below the seed, which runs after the open returns.
- Probe: the reopen research's probe (`probe build <dir> <days> [scale]`, `probe open <dir> [scale]`), with a
  `live <dir> <minutes>` step that emits the arrivals after the build each second at wall time.
