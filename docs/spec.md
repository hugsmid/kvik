# kvik specification, v0.1 (design; not yet implemented)

This repository contains this specification and a reference model of its semantics (`model/`). The engine
described here does not exist yet. "Must" is normative: it states what an implementation is required to do. Items
in §12 are open; the defaults written here apply until they are decided. The byte-level format will be specified
in a separate document. Open questions found by modelling are in `model/OPEN-QUESTIONS.md`.

## 1. Principles
1. **Facts are immutable.** A recorded reading is never edited or deleted by the engine.
2. **Everything else is derived** (summary tree, indexes, compacted layout, funnel state) and is a **pure
   function of the log entries and configuration recorded in them**. No learned or wall-clock state enters a
   derived value. Rebuild-equals-live is a CI test.
3. **React after sync.** Nothing is visible to any consumer or reader process before it is durable.
4. **Data moves, it is never dropped.** A file leaves a tier only after a hash-verified copy exists elsewhere.
5. **No silent loss.** Anything refused, flagged, repeated or delayed is counted and visible.
6. **One writer process; any number of reader processes.**

## 2. Data model
### 2.1 Series
| Field | Rule |
|---|---|
| `id` | u32, assigned once by the registry, never reused |
| `name` | `<instance>/<device>/<metric>`; a rename adds an alias, the id is unchanged; a name maps to its active series |
| `kind` | `gauge`, `counter` (cumulative), `state` (named or boolean), `text` (UTF-8, ≤ 255 bytes) |
| `value type` | gauge/counter: `f64`, or `i64` with a declared `decimal scale` (exact); state: u16 index into declared names or bool |
| `unit` | fixed for the life of the series; a change of kind, type, scale or unit is a new series |
| `expected interval` | configuration, never learned. Default: gauge and counter 60 s; state and text none (valid until next reading) |
| `staleness limit` S | `3 × expected interval` unless set; none if no interval |
| `record on change` | optional; see §2.6 |
| `wrap_at`, `reset_ceiling` | counters only; §5.5. `reset_ceiling` default 10 % |
| `lateness bound` | default 5 min; §2.2 |

Series definitions, parameter changes (with an effective event time), aliases and retirements are log entries.
A parameter applies to readings with `t` at or after its effective time. A definition entry always has a lower
position than the series' first reading (the same batch is allowed). Registry entries are also compacted into a
registry file that never leaves the hot tier.

**Retirement** is effective at `t_r`: facts with `t < t_r` are still accepted; facts with `t ≥ t_r` are refused
and counted. The old id stays reachable by id.

### 2.2 Log entries, positions and facts
- Every log entry (fact, definition, parameter, alias, retirement, routing change) has a **position**
  `(generation: u32, sequence: u64)`, ordered lexicographically. Every generation starts at sequence 1; `(g, 0)`
  means "before the first entry of generation g" and `(0, 0)` means none. Sequence is strictly increasing and
  gap-free within a generation across restarts, except for lost ranges recorded in the manifest (§3); a restore
  starts a new generation (§8). Consumers see only the positions of facts routed to them, so gaps between delivered
  positions are normal.
- Fact: `(series, t, position, received_at, value, quality, flags)`.
- `t`: event time, UTC nanoseconds, from the adapter. The adapter declares `t_source: device | host`.
- `received_at`: host clock at commit. `commit_time`: per batch, `max(received_at, previous commit_time)`.
- `quality`: good / estimated / suspect / bad, from best to worst. The adapter supplies it; the engine may lower
  it, never raise it (`future` below; invalid values, §2.5). The adapter-supplied quality is kept with the fact for
  the duplicate rule (§2.4).
- Fact flags (immutable): `future` (`t > received_at + 60 s`; quality lowered to suspect if better), `late`
  (`t` earlier than the series' latest recorded `t` by more than 2 × S, or `received_at − t` above the lateness
  bound), `revise` (§2.4), `host_time` (`t_source = host`), `unsynced_clock` (§2.5). Delivery flags are separate
  (§4).

### 2.3 Views
| View | Definition |
|---|---|
| Current | for each `(series, t)`, the fact with the highest position |
| As-of P | the current view over facts with position ≤ P |
| As-of time T | as-of P, where P is the last position whose batch `commit_time` ≤ T |
| History | all facts for `(series, t)` in position order |
| Live state | per series, the current fact with the greatest `t ≤ now` and quality not bad |

A fact superseded in the current view is a **revision**.

### 2.4 Duplicates
- An arriving fact is a **duplicate** if its `(series, t, value)` and adapter-supplied quality equal those of *any*
  fact already in the history of `(series, t)`, including facts earlier in the same batch. Flags and quality assigned
  by the engine are not part of identity. Values compare bitwise (`0.0` ≠ `-0.0`). Duplicates are not recorded and
  consume no position; they are counted per series (`duplicates_dropped`). Adapters may replay freely: a replay can
  no longer revert a correction.
- A fact submitted with `revise` intent bypasses the check (deliberate return to an earlier value).
- A reading recorded without a value (§2.5) is compared as "no value", with its adapter-supplied quality.
- Identity includes `t`: the same value one nanosecond later is a different fact and is recorded.
- **Revised-key index** (hot tier, never moves): for every `(series, t)` with more than one recorded fact, the exact
  value and adapter-supplied quality of each. For any other key, history holds at most the current fact, which is
  compared directly.
- **Fast path:** a fact with `t` later than the series' latest recorded `t` cannot be a duplicate; no lookup. Other
  facts are checked against the in-memory recent window (§5.1) before any index probe.
- If the current fact for a key is on no local tier (its detail moved out under §8 fill pressure), the fact is
  recorded and counted `duplicates_unchecked` (§12 D5). The check never reads off-machine data.

### 2.5 Quality, invalid values, clock
- Bad-quality facts stay in views but are ignored by summaries and do not refresh staleness (§12 D6).
- NaN or ±∞ is recorded as quality bad with no value, flagged and counted (§12 D1). A negative counter value
  (§5.5) is recorded the same way whatever D1 decides; D1 concerns NaN and ±∞ only.
- Until the system reports clock synchronisation, batches carry `unsynced_clock`, `future`/`late` are not set, and
  facts with `t_source = host` wait in the ingest queue (back-pressure) rather than being stamped with a wrong time.
  Device-stamped facts are accepted.

### 2.6 Record on change
A record-on-change series records a fact when the value changes **and** at least once per expected interval while
it does not (a keep-alive fact). For such series the expected interval is the keep-alive interval, not the poll
interval. A healthy unchanged series is therefore never stale, and every derived value stays a function of facts.

## 3. Log
| Rule | |
|---|---|
| Batches | Append-only segments of arrival batches. A batch ends with a **commit block** (last position, checksum of the batch's blocks). One batch = one commit = one sync. A batch never spans segments. |
| Commit | Write batch, sync, update the durable mark, then (and only then) acknowledge adapters, update tree and live state, and deliver. |
| Durable mark | After each successful sync the writer updates a memory-mapped mark file: segment id, byte offset, last position, boot id, sequence lock. Not synced itself. Readers read log segments only up to the mark. |
| Group commit | While a sync is in flight, arriving readings queue and form the next batch. No added delay when idle (optional linger, default 0). |
| Segments | Preallocated at creation (64 MiB). The directory is synced after creating, sealing or renaming a file. |
| Sealing | At 64 MiB or 5 minutes (both configurable), whichever first; an empty segment is not sealed. Sealed segments are immutable and eligible for backup. Compaction never removes a log segment (§8). |
| Recovery | Below this table. |
| Write or sync failure | On the log, manifest, cursor or registry files: stop accepting writes, mark read-only, fatal health; never retry on the same handle; recovery by restart. After a failed sync the batch's fate is unknown: a restart on the same boot truncates it; after a reboot recovery may keep it (row 7a). It was never acknowledged, so adapters may replay it, which the duplicate rule absorbs. A failed compaction, tiering or tree write abandons that job, deletes its partial output and degrades health. |
| Disk space | Low mark: free space below 5 % of the filesystem or 2 GiB, whichever is larger → compaction pauses, health degraded. Tiering and eviction, which free space, continue at every mark. Critical: below 512 MiB free → ingest waits (back-pressure). Reads continue. Space is never freed by deleting facts. |
| Back-pressure | Ingest is a bounded queue (default 65 536 readings); a full queue makes adapter emits wait; the wait is measured. |
| Single writer | Exclusive `flock` on the store directory; readers never take it. |

**Recovery.** Every unsealed segment is scanned to its end. A missing, unreadable or half-updated mark is treated as
a different boot. The rules apply in this order:
1. **Same boot id as the mark:** truncate everything after the mark. It may have reached the disk, but the mark did
   not cover it, so it was never acknowledged or delivered; adapters replay.
2. **Corruption:** an invalid region that lies below a valid mark (even sequence lock) of any boot, or that is
   followed by a complete batch, was synced (the mark is written only after a sync returns, and a batch only after
   the previous batch's sync), so it is corruption, not a torn tail. Later complete batches are kept and nothing is
   overwritten. The byte and position range, from the last valid batch to the next valid batch or, at the tail, to
   the mark's last position, is recorded in the manifest as a **lost range** and reported as `data_lost` for that
   range, which does not make the store read-only. Its positions are not reused. A consumer whose cursor is below a
   lost range receives `resync`.
3. **Different boot id** (the machine rebooted, the page cache is gone): any remaining invalid tail is torn and is
   truncated.
4. Whenever rule 1 did not apply, recovery syncs the segment and directory and writes the mark with the current boot
   id, then resumes. If that sync fails, the engine stays read-only and leaves the old mark.

## 4. Delivery (funnels)
| Rule | |
|---|---|
| Funnel | Declared in configuration: series selection (ids or name patterns), optional operator (§6), condition, consumer. |
| Routing | Array indexed by series id → funnels, built at start. Changes are log entries and take effect at their position; catch-up applies the routes in force at each position. Patterns are re-evaluated when a definition entry commits. Compilation rejects a cycle where a consumer's emitted series routes back to it. |
| Concurrency | Delivery and consumer code never run on the writer thread. A consumer emits readings through non-blocking `try_emit` or from its own task. |
| Consumer state | Bounded queue (default 4 096); a durable cursor (last confirmed position) and one latch per condition funnel (§6). Written to a per-consumer file, synced at most every 1 s and on clean shutdown; never ahead of the consumer's confirmation. |
| New consumer | Declares `start: now | position | time` (default now). |
| Order | A consumer sees fact deliveries in position order (invariant 3). Timer deliveries carry the last committed position at evaluation plus a per-funnel sequence. |
| At-least-once | After a restart a consumer resumes after its cursor and may see a delivery again. Consumers de-duplicate fact deliveries by position and timer deliveries by `(funnel, sequence)`; commands carry ids. |
| Overflow | A full queue switches the consumer to catch-up: it reads the log from after the last position already enqueued for it in the current session (not from its confirmed cursor, so order holds within a session) up to the durable mark, then re-attaches to live at the next position under the writer's delivery lock (no gap, no repeat). It never blocks the log or other consumers. Time behind is measured. |
| Replay | Fact deliveries produced in catch-up are flagged `replayed` and keep their original times. They are for record-keeping and are never fed to a condition operator a second time (§6). A consumer that acts on devices must not act on old deliveries (default: `received_at` more than 60 s before now; event time does not count). |
| Resync | A `resync` delivery carries, per funnel, the current live state and condition state evaluated now. Sent on resume after downtime (§6), on `generation_changed` (§8), when the cursor is older than the oldest local log segment, and when the cursor, or the `start` position of a newly registered consumer, is below a lost range (§3). With no latch to compare, as after a restore, it delivers each condition's present state, true or false, as a transition. It is not flagged `replayed`; acting on it is acting on the present. |
| Future facts | Delivered at commit, flagged `future`; ignored by live state, staleness and windows until `t ≤ now`. |
| Timers | Window expiry, "held for N s", staleness and future-fact maturity use the monotonic clock; system clock corrections never fire or suppress them. Timer deliveries that fell due while the engine was down are not replayed; `resync` covers them. |

## 5. Summary tree
### 5.1 Levels and storage
| Level | Stored | Where |
|---|---|---|
| 1 second | **Never.** Computed from facts on demand (window edges, drill-down below 1 minute) | — |
| 1 minute | Yes, aligned to UTC, only where there is data | per-series tree files on hot; never moves (§12 D7) |
| 15 minutes | Yes, aligned to UTC, only where there is data | per-series tree files on hot; never moves |
| Local calendar day/month | Yes, for spans whose edges fall on 15-minute boundaries (§5.7), assembled from 15-minute nodes in the site zone in force; an input to calendar results, not the result (§5.7) | per-series calendar file on hot |

- **Density rule:** a level is stored for a series only where its nodes summarise many facts: the 1-minute
  level only for series whose expected interval is 5 s or shorter, the 15-minute level only for 60 s or shorter.
  A sparser series answers those levels from its facts directly, so a stored level is never larger than the
  data it summarises.
- Memory holds only open nodes per series, a node cache, and a **recent window** of raw facts per series
  (default 15 minutes, at most 1 024 facts) for the newest window edge and the duplicate fast path.
- Tree files record the position up to which they are complete (the **checkpoint**, written at most every 60 s and
  on clean shutdown). On start the engine loads them and replays log entries after the checkpoint. Tree files are
  derived and are backed up as such (§8).

### 5.2 Node contents (current view; bad quality ignored)
A node over span [a, b) is either **empty** (the identity) or holds, over the sequence of readings
r₁ … rₙ (in `t` order) inside its span:

| Kind | Per reading | Per consecutive pair (rᵢ, rᵢ₊₁) inside the span |
|---|---|---|
| all | count, first `(t, v)`, last `(t, v)` | coverage += hᵢ |
| gauge | sum, min, max | integral += vᵢ × hᵢ |
| counter | — | increase += step(vᵢ, vᵢ₊₁) (§5.5), resets, regressions |
| state | — | time[vᵢ] += hᵢ; transitions += [vᵢ ≠ vᵢ₊₁] |
| text | — | (count, first, last and coverage only) |

where the **hold** hᵢ = min(tᵢ₊₁, tᵢ + Sᵢ) − tᵢ, Sᵢ being the staleness limit in force at tᵢ (∞ if none). Time is
held in nanoseconds and reported in seconds. **Nothing is held past a node's last reading inside the node.** Min and
max are over readings only.

### 5.3 Merge
`merge(L, R)` for adjacent L before R: if either is empty, the other; otherwise per-reading fields combine
(count and sum add, min/max, first from L, last from R), per-pair fields add, and the boundary pair
(L.last, R.first) contributes its pair terms once. The pairs of a concatenation are the pairs of each part plus
one boundary pair whatever the bracketing, so merge is associative with the empty node as identity. The reference
model must prove this against direct computation (invariant 6), including empty nodes, holds spanning several
nodes and late facts inserted between existing readings.

### 5.4 Staleness and coverage
A reading is valid for at most S. **Live staleness** is measured on the monotonic clock: a series is stale when
more than S has passed since its last commit of a fact that is not bad quality (§2.5) and not a future fact still
awaiting its time (§4), so a forward jump of the system clock cannot make a series stale. **Event-time staleness**
applies to history and to every window evaluation (§6); live staleness applies only to live state and stale-for
conditions: a series is stale at event time x when x > (`t` of its latest reading before x that is not
bad quality) + S. Coverage stops at that point and time-weighted results exclude the gap. Every windowed result
carries coverage in seconds and as a fraction of the window.

### 5.5 Counters
- Counter values are non-negative; a negative value is recorded as bad quality with no value (§2.5).
- step(p, n) for consecutive good readings, **never capped by staleness** (consumption across a gap is real;
  coverage still stops). The step is credited to whichever node or window contains n.
- n ≥ p: n − p. With `wrap_at` and n < p: `wrap_at − p + n`.
- Without `wrap_at`, n < p: a **reset** if n < `reset_ceiling` × p, contributing n; otherwise a **regression**:
  contributes 0, counted (`counter_regressions`), and the next step is measured from n (§12 D2).

### 5.6 Updates and determinism
- Updates are applied **once per batch** after commit and before delivery of that batch: each affected 1-minute
  node is recomputed from the current facts of that minute, then its 15-minute and calendar ancestors from their
  children. An in-order append extends the fold and gives the identical result.
- **Canonical order:** a 1-minute node is the left-to-right fold over its facts in `t` order; every higher node is
  the left-to-right merge of its children. Stored nodes are therefore bitwise reproducible (invariants 5, 8).
  Series with a decimal scale sum in exact `i128`. Results assembled in other orders (windows, coarse queries) are
  compared with direct computation to a relative tolerance of 1e-9 (invariants 6, 7).
- Each node carries a version incremented on change.
- If the facts of an affected minute are on no local tier, the fact is still recorded, the node is flagged
  `summary_stale` and recomputed when the detail is fetched (§8). Chunk indexes always stay hot, so the engine
  knows whether a minute has facts off the machine.

### 5.7 Calendar
- The site zone is a log entry with an effective time; each local day uses the zone in force at its start. A zone
  change rebuilds calendar nodes from its effective time only (§12 D3). The tzdata version is recorded per
  calendar file; a tzdata change rebuilds the affected days and is logged.
- A calendar day or month **result** is a window evaluation (§6) over its span, edge stretches included. Stored day
  nodes are an input to that evaluation, not the result.
- A local day usually has 92–100 quarter-hours. A day whose local midnight does not exist starts at its first
  instant. Because results are window evaluations, the rule also covers a skipped date (an empty span) and historical
  offsets that are not multiples of 15 minutes.
- A calendar node is stored only for a span whose edges both fall on 15-minute boundaries. For any other day or
  month none is stored; its result is evaluated from the 15-minute nodes inside it and the facts at its edges.

## 6. Windowed operators
| Rule | |
|---|---|
| Aggregates | avg (gauge: integral ÷ covered seconds), sum, integral, min, max, count, increase, rate (increase ÷ elapsed seconds between the readings that bound it; the result carries coverage and is flagged when a step it includes spans more than S, in force at the step's earlier reading; empty when no step falls in the window), last, time-in-state. With no reading in the window but a valid held value: `last`, avg, integral and time-in-state use it; `min`, `max`, `count`, `sum` and rate are empty; increase counts only a step credited to the window (Edge stretches). |
| Windows | Rolling (any length) and calendar (since local midnight, this month) in the site zone; a calendar result is a window over its span (§5.7). Every window, rolling or calendar, is evaluated in event time against `now`: its bounds are event times, and no allowance is made for a jump of the system clock. Health reports such jumps (§11). |
| Window end | `now` = an engine UTC clock that never runs backwards: max(system clock, previous value), advanced by the monotonic clock. |
| Evaluation | Whole stored nodes in the middle; at each edge, the partial minute is computed from raw facts (the recent window for the newest edge, the edge chunk otherwise). Exact to the nanosecond. Nodes whose span extends past `now` are never used whole. |
| Edge stretches | Leading: from the window start to the first reading inside, credited with the last reading before the start if within its S. Whatever S, the counter step and the state transition from the last reading before the start to the first reading inside are credited to this window. Trailing: from the last reading to min(window end, now, last.t + S). Time is never credited past `now`. |
| Old spans | If an edge's raw detail is not local, the edge is evaluated at 1-minute precision and the result is flagged `approximate_edges`. |
| Cache | The merged middle is cached with the versions of its nodes and invalidated when any changes (late data included). |
| Cadence | A windowed funnel is evaluated on each routed fact and on a timer every 1 s (configurable per funnel). |
| Conditions | Threshold with a separate clear level, on change (optional dead-band), held for N seconds, stale for N seconds. |
| Late data | A trigger acts on what is known when it is evaluated and never fires retroactively. |
| Latch | Each condition funnel persists, with its consumer's cursor, the last emitted state and the position at which it was emitted. |
| Restart and resume | The operator is rebuilt from the tree and recent window as of now; it is never replayed. Facts after the cursor are delivered flagged `replayed`, not fed to the operator. The funnel then sends `resync` with the condition state now; if that differs from the latch it is a transition and is acted on. A condition that became true while the consumer was down is therefore delivered, never suppressed. |
| Multi-series | Conditions over several series are composed by the consumer in the first version. |

## 7. Compaction and files
- Compaction rearranges facts from whole sealed log segments into per-series chunks. It is **lossless**: every fact,
  including revisions, with its position and `received_at`. Facts are identified by position.
- Current facts and revisions go to **separate chunks**. A revision chunk holds only facts superseded within the
  same compaction's inputs, so the current view never needs revision chunks (a current chunk may hold facts
  superseded later).
- Procedure: write new file → verify (fact count, per-series checksum of facts, each chunk's summary equals the
  summary recomputed from the input facts assigned to it) → publish.
- **Manifest:** append-only, checksummed records, each with an epoch. One record may add files, retire files and
  move files between tiers **atomically**. An epoch never lists two live files holding the same position. Tier
  moves keep the file's identity and change its location; readers resolve locations through the manifest.
- Compaction of compacted files (merging overlapping chunks created by late data) publishes outputs and retires
  inputs in one record. Policy: merge when a series' chunks overlapping any 15-minute span exceed 4.
- A sealed file's **inputs are kept** until the file is verified in another tier. Log segments are never retired by
  compaction (§8).
- **Retired files** are removed locally only after the grace period (1 hour of the writer's monotonic clock since the
  later of the retirement record and the writer's start), once the files that replaced them are verified in another
  tier, and when no live reader lease names an older epoch.
- **Leases:** a lease file holds the epoch, the boot id (from the same source as the mark's) and a refresh counter
  that the reader increments at every refresh. The writer expires a lease when its counter has not changed for
  1 hour of the writer's monotonic clock since the writer first observed that value; at writer start, every lease of
  the current boot counts as first observed then. A lease with a different boot id is void.
- **Readers** take an epoch, write a lease file naming it, and refresh both at least every 30 minutes. A reader
  needs nothing from the writer's memory: it reads via the manifest, the documented layout, tree files and log
  segments up to the durable mark.

## 8. Tiers, backup and rebuild
| Tier | Location | Holds |
|---|---|---|
| Hot | fast local disk | log segments < 90 days, current detail, revised-key index, chunk indexes, registry, the whole stored tree |
| Cold (optional) | another local path | older revision chunks and sealed log segments; moved current detail |
| Deep | S3-compatible bucket, via a pluggable backup sink | every sealed file, every manifest record, tree checkpoints, generation claims |

| Rule | |
|---|---|
| Continuous backup | Each sealed file is queued for upload once; upload never blocks ingest or delivery. After its files, each manifest record is uploaded; a tree checkpoint at least daily, and only after all log up to its position is backed up. Each tree checkpoint carries the registry file and each series' latest recorded `t`, as of its position. Cursor files are not backed up. |
| Encryption | On the machine before upload, once per file; key supplied by the embedding application. |
| Hashes | The manifest records SHA-256 of the plaintext at seal (hot and cold verification) and, per upload, SHA-256 of the ciphertext, sent with a single-part upload. The bucket must reject a mismatch and its reported hash must equal the manifest's; only then is the file backed up. Scheduled check: re-read reported hashes; sampled download-decrypt-verify proves key and content. |
| Bucket | The engine never deletes from it. Versioning or object lock required. |
| Eviction at 90 days | Revision chunks (age per series, "never" allowed) and sealed log segments leave hot only after they are verified in cold or deep. Current detail and the stored tree stay. |
| Fill pressure | Above 70 % fill, verified sealed log segments leave first, then the oldest current raw detail (verified first, fetched on demand); the tree, chunk indexes and revised-key index never move (§12 D4). |
| Behind consumers | A consumer whose cursor is older than the oldest local log segment receives `resync`, not a replay. The engine never downloads from deep for catch-up. |
| Corrupt block | File quarantined; the engine fetches the copy from cold, else deep, verifies the plaintext hash and swaps it in by a manifest record. If no other tier holds a verified copy, the file is compacted again from its kept inputs, verified by the §7 procedure, recorded with its new identity and hash, and swapped in by one manifest record that retires the quarantined file. Queries over the range return an error until then. Never repaired in place. Because inputs are kept until a file is verified elsewhere (§7), the only single copies are the open log segment and a sealed log segment not yet uploaded and not yet compacted: corruption there loses that range, recorded when found and reported as in §3 Recovery rule 2. An optional local mirror path for sealed log segments (configuration `mirror_path`, off by default) closes this window for sealed segments. |
| Upload stalled | Files queue locally, health degraded, nothing evicted. |
| No deletion | Every fact is kept forever in at least the deep tier; the engine has no purge. |
| Re-ingest | Exported or foreign data goes through the normal ingest path, in batches. |
| Restore drill | The restore procedure is testable end to end; operators should exercise it regularly. A drill restores read-only and claims nothing. |
| Generations | Before accepting ingest, a restore claims the generation one above the highest claimed, by a create-if-absent marker in the bucket; if the marker already exists it takes the next. Manifest records are keyed by generation, and a restore reads those of the highest generation claimed before its own. A writer that finds a higher generation claimed stops uploading and becomes read-only (fatal health, §3). A restore from a bucket with no manifest records is refused. |

**Restore** (onto an empty disk):
1. Claim a generation (Generations above) and read the manifest records of the highest generation claimed before it.
   The **restored end** is the last position before the first gap in the restored log that is not a recorded lost
   range (§3). Write one manifest record (generation, checkpoint, restored end) before ingest is accepted. Download
   the latest tree checkpoint at or below the restored end (labelled derived-unverified), with its registry file and
   latest `t` per series, and the sealed log segments after its position; replay them into the tree. With no
   checkpoint, download the whole sealed log. Segments beyond the restored end are downloaded and kept but
   quarantined: outside views, trees and delivery, reported in health like a lost range, and available for re-ingest.
   Positions of the new generation start at sequence 1.
2. The store serves queries at ≥ 1-minute resolution, flagged `restoring`, and accepts ingest. Until the revised-key
   index is back, facts whose `t` is at or before the latest `t` known for their series (from the checkpoint or the
   restored log) and which match no local fact are recorded and counted `duplicates_unchecked`. A consumer without a
   cursor is treated as beyond the restored end; one that kept its last confirmed position registers with
   `start: position`. A cursor of an older generation at or below the restored end receives the facts after it up to
   the restored end through catch-up, flagged `replayed`, if their segments are local; otherwise §4 Resync applies.
   Every consumer then receives `generation_changed` and `resync`, and resumes at the new generation's first
   position.
3. Download current chunks and the revised-key index, then revision chunks and log segments. A position present in
   two files is one fact and must have identical content.
4. When detail for a span is back, recompute its nodes from facts and compare with the checkpoint; any difference
   is reported and the recomputed value wins. The checkpoint label is removed when all detail has been verified.

A restart during a restore resumes it in its generation, fetching the files listed by the manifest records it
restores from that are not yet local.
A crash before the record starts the restore again with the next generation.

A disaster loses facts in files not backed up, which may already have been acted upon, and those beyond the first
gap until they are re-ingested.

## 9. Edge cases
| # | Case | Rule | Test |
|---|---|---|---|
| 1 | Same `(series, t)`, different value | New fact; becomes current; old one a revision; minute path recomputed | model + property |
| 2 | Same `(series, t)`, identical to any fact in its history | Dropped and counted (§2.4) | unit |
| 2a | A, then B, then a replay of A | Replay dropped; B stays current. With `revise`, A is recorded and current | property |
| 2b | Identical repeat whose current fact is off every local tier | Recorded, `duplicates_unchecked` | unit |
| 3 | Reading for last week arrives now | Flagged `late`; minute path updated; no retroactive trigger | model |
| 4 | Reading > 60 s in the future | Recorded, `future`, suspect (or worse if supplied); delivered flagged; ignored by live state, staleness, windows until due | model |
| 5 | Device clock stuck or far in the past | Late data; adapter health reports skew when `received_at − t` is persistently large | unit |
| 6 | Two readings in one second | Both facts; the minute node folds both | unit |
| 7 | Process crash after write, before sync | Same boot: truncated to the durable mark on restart; never delivered. With the mark lost, recovery treats it as a different boot and may keep the batch, which was never acknowledged | kill loop |
| 7a | Power loss after write, before sync | New boot: torn tail truncated by checksum; complete batches synced on recovery before use | power-loss rig |
| 8 | Crash after sync, before delivery | Delivered on restart from each consumer's cursor | kill loop |
| 9 | Crash during compaction | Output unpublished and discarded; inputs intact | kill loop |
| 10 | Crash around the manifest record | Before: output discarded. After: inputs retired. Never both live | kill loop |
| 11 | Torn or corrupt block in a sealed file | Quarantine; fetch from cold/deep, else compact again from kept inputs; verify, swap by manifest; errors meanwhile. Log segment with no other copy: range recorded as lost, `data_lost` | fault injection |
| 11a | Corrupt block inside the open segment, or below an old boot's mark after a power loss | Later complete batches kept; range recorded as lost, `data_lost`; positions not reused; `resync` for cursors below it | fault injection |
| 12 | Sync returns an error | Read-only, fatal; a restart on the same boot truncates to the durable mark; after a reboot the batch may be kept (never acknowledged) | fault injection |
| 13 | Disk fills | Degrade, then ingest waits; compaction ENOSPC abandons the job; nothing deleted | fault injection |
| 14 | Consumer slower than the stream | Catch-up from the log after the last position enqueued, hand-over to live without gap or repeat | load test |
| 15 | Consumer down for a day | Resumes from cursor; deliveries `replayed`; then `resync`. Cursor older than the oldest local log segment: `resync` only | model |
| 16 | Consumer restarts; a condition became true while it was down | Operator rebuilt as of now, not replayed; `resync` differs from the latch → acted on (equal → no new transition) | model |
| 17 | Series goes silent | Stale after S; coverage stops; stale-for-N funnels fire | model |
| 17a | Record-on-change series, value unchanged | Keep-alive fact each expected interval; never stale while healthy | model |
| 18 | Counter reset | Decrease below `reset_ceiling` × previous counts the new value | property |
| 18a | Counter wraps, glitches low, or is silent for hours | `wrap_at` rule; regression (0, counted); full step credited after the gap, coverage shows it and rate is flagged | property |
| 19 | Correction changes a node's min or max | Minute recomputed from facts, ancestors from children | property |
| 20 | DST changes | UTC levels unaffected; local days of 92–100 quarter-hours, evaluated as windows over their spans | property |
| 21 | Site zone changed, or tzdata changes its rules | Calendar nodes rebuilt from the effective time / affected days only; logged | integration |
| 22 | System clock jumps | Timers monotonic; window `now` never runs backwards; `commit_time` monotonic | unit |
| 22a | Host clock not yet synchronised at boot | `unsynced_clock`; host-stamped facts wait | unit |
| 23 | Series renamed | Alias entry; id and data unchanged | unit |
| 24 | Unit, kind or scale changes | New series; old one retired | unit |
| 25 | Series retired at `t_r` | Facts with `t < t_r` accepted; later refused and counted | integration |
| 26 | Reader holds an old epoch | Its files stay while its lease names that epoch and its counter keeps changing; leases of a different boot are void | integration |
| 27 | Bucket unreachable for days | Queue grows, no eviction, degraded health | fault injection |
| 28 | Restore onto an empty disk | New generation, claimed in the bucket; queries at ≥ 1 min from the checkpoint; detail back-fills and verifies the checkpoint; segments beyond the first gap quarantined | restore drill |
| 28a | Crash or reboot during a restore | Resumes in its generation from the restore record; before the record, starts again with the next generation | kill loop |
| 28b | Two restores from one bucket, or a drill beside a live store | Each restore claims its own generation; a writer that finds a higher one stops uploading; a drill claims nothing | fault injection |
| 29 | Late fact for a minute whose detail is not local | Recorded; node `summary_stale` until fetched | model |
| 30 | Very many series | Memory per series bounded and documented; ceiling (default 100,000) refuses new series clearly | load test |
| 31 | Value NaN or infinite, or a negative counter value | Recorded as bad quality without a value; counted | unit |
| 32 | Format version newer than the reader | Refused with a clear error | golden files |
| 33 | Consumer emits readings from a delivery | Non-blocking emit off the writer thread; cyclic routes rejected | integration |
| 34 | Route added while a consumer is catching up | Applies from its position only | model |
| 35 | Window with no new data, or edge inside the open minute | Timer evaluation; edge from the recent window; nothing credited past now or S | property |

## 10. Invariants (checked by the reference model and property tests)
1. Sequences are strictly increasing and gap-free within a generation across restarts, except for lost ranges
   recorded in the manifest (§3); only a restore starts one.
2. Every delivered position is durable; every durable position routed to a consumer is delivered to it, or
   superseded by a `resync` covering it.
3. For any consumer, fact deliveries are in position order.
4. The current view equals "highest position per `(series, t)`" over all facts in all tiers, quarantined segments
   (§8 Restore) excepted.
5. Every stored node is bitwise equal to its canonical computation over the current view of its span.
6. `merge` is associative with the empty node as identity and agrees with direct computation (tolerance §5.6).
7. A window result equals direct computation over the current view at evaluation time (tolerance §5.6).
8. Rebuilding derived structures from log entries yields exactly the live structures.
9. Compaction, tiering and backup preserve the set of positions outside lost ranges, each with identical content;
   restore preserves the positions held in sealed, backed-up files up to the restored end, and keeps later ones
   quarantined.
10. No operation removes a durable fact. The only loss short of a disaster is corruption of a log segment, open or
    sealed, with no other copy (§3 Recovery, §8 Corrupt block), which is recorded as a lost range and reported as
    `data_lost`.
11. No reader or consumer observes a position beyond the durable mark.
12. No file leaves a tier unless a hash-verified copy exists in another tier.
13. A cursor or latch on disk is never ahead of the consumer's confirmation.
14. No manifest epoch lists two live files holding the same position.
15. An arriving fact is dropped as a duplicate iff it is identical (§2.4) to a fact in the history of its
    `(series, t)` and lacks `revise` (except `duplicates_unchecked`, §2.4).
16. After resume, every condition true now is delivered once in `resync`.
17. For a counter, increases over adjacent windows sum to the increase over their union; for a state series, so do
    transitions (each step or transition credited once).
18. No time is credited past `now` or past a reading's staleness limit (counter steps excepted).

## 11. Interfaces and configuration (outline; signatures follow in the API document)
| Interface | Contents |
|---|---|
| Ingest | `emit(reading, t_source, intent: normal | revise)` → recorded(position) / duplicate / refused(reason); waits under back-pressure |
| Query | current, as-of P or T, history, tree level for a span, window evaluation; each result carries coverage and flags |
| Consumers | register(start), deliveries, confirm(position), latches; `resync`, `generation_changed`, `cursor` events |
| Health | counters (duplicates, unchecked, refused, regressions, back-pressure wait, lag), persisted with the checkpoint on a best-effort basis; each jump of the system clock measured against the monotonic clock, with its size; degraded/fatal states; `data_lost` for each lost range (§3), which does not make the store read-only |

Every default in this document is a configuration key, with its unit where it is defined. Byte-level format,
Parquet export schema, API signatures and memory figures follow in separate documents.

## 12. Open decisions (defaults above apply until decided)
| # | Choice | Default | Alternative |
|---|---|---|---|
| D1 | NaN/∞ from a device | Recorded as bad quality, no value | Refused and counted |
| D2 | Counter decrease that is not a plausible reset | Regression: contributes 0, counted | Every decrease is a reset (one glitch can add a whole counter value) |
| D3 | Site zone change | Effective-dated; past days keep the old zone | Rebuild all history in the new zone |
| D4 | Fill pressure order | Verified sealed log leaves before current detail | Only current detail moves; log stays 90 days |
| D5 | Identical repeat whose current fact is off the machine | Recorded, counted unchecked | Ingest of that fact waits for a fetch |
| D6 | Bad-quality reading in a gauge | Ignored; previous value's hold continues up to S | Ends the previous hold (gap) |
| D7 | 1-minute level for old spans | Stays hot forever (≈ 38 MB per series-year with data every minute) | Follows raw detail to cold |

## 13. Risks to measure
| Risk | How measured | If it fails |
|---|---|---|
| Sync rate and SSD wear | Syncs/s and bytes written on a representative SSD with N unbatched 1 Hz series (N stated in the benchmark), preallocated segments, `fdatasync` | Enable linger (≤ 1 ms) |
| Funnel budget ≤ 1 µs | Benchmark per-reading routing and a 24 h rolling average with cached middle | Budget excludes windowed conditions; cadence-only evaluation |
| Batch tree update cost | Backfill at a stated target rate through normal ingest, with late facts | Larger batches for re-ingest |
| Duplicate check on replay | 1 hour × 100 series replay; probes outside the recent window | Larger recent window |
| Tree size | Bytes per series-year per level on real data | D7 alternative |
| Hot disk during 90-day log retention | Log plus compacted bytes per day at expected load | D4 default; earlier move to cold |
| Restart ≤ 1 s | Start time with 100,000 series and 60 s of log after the checkpoint | Shorter checkpoint interval |
| Deep object count | Objects per year and restore time from deep | Larger seal size for sealed-log uploads |
| Bucket SHA-256 checksums | Confirm the bucket provider's S3 API rejects a mismatched single-part SHA-256 | Hash in object metadata plus sampled downloads |
| Memory per series | Resident bytes per series at the ceiling | Lower ceiling; smaller recent window |
| Overlapping chunks | Read amplification after a year of late corrections from upstream sources | Tune overlap threshold |
