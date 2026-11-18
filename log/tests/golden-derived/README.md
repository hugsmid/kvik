# Golden files, derived files, format v1

Small v1 derived files (`docs/format.md` A.18) that guard their layout against accidental change. They are
hand-built: each body is laid out field by field from the A.18 tables in
[`../directory_golden.rs`](../directory_golden.rs), independently of the encoders, under the A.1 header and A.2
block framing. The store id is `01 02 … 10`, generation 0, written at 2026-01-01T00:10:00Z.

The tests check that each file equals the layout, decodes to what it shows, and encodes back to the same bytes.
Regenerate only after a deliberate format change: `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-log --test directory_golden`.

| File | Shows |
|---|---|
| `logdir.root` | A root (kind 15, file id 5): manifest epoch 4 ending at offset 312 (record CRC32C `0x12345678`), through position 90, two segments in one group, reset stamp (12, `0x5eed`), no C(E), one lost range (20 to 22 in segment 1), no quarantined or compacted files, no restored ends |
| `logdir.grp` | Group 0 (kind 15, file id 3): segment 1 (positions 1 to 40, with entries) and segment 2 (41 to 90); the root names its body's CRC32C |
| `series.sdir` | A series directory (kind 16, file id 2) through position 95, one lost position at or below it, judged at 00:10:01: series 1 with an `f64` candidate; series 7 orphaned, with an unsure text candidate and a pending fact with more after it |
| `status.stat` | A status file (kind 17, file id 9): 60 s interval, degraded (disk low) and partial, pressure clear, no slots, two counters |
