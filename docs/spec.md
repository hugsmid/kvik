# kvik specification, v0.2 (design; not yet implemented)

This repository contains this specification and a reference model of its semantics (`model/`). The engine
described here does not exist yet. "Must" is normative: it states what an implementation is required to do. §12
records the choices decided between alternatives. The byte-level format will be specified in a separate document. Open questions found by modelling are in `model/OPEN-QUESTIONS.md`; those still open at v0.2, and the roadmap item
each is deferred to, are listed there under "Open at v0.2".

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
| `id` | u32, assigned once by the registry, never reused among the entries the store holds (after a restore, §8 Restore step 1) |
| `name` | `<instance>/<device>/<metric>`; a rename adds an alias, the id is unchanged; a name maps to its active series |
| `kind` | `gauge`, `counter` (cumulative), `state` (named or boolean), `text` (UTF-8, ≤ 255 bytes), `interval` (an amount over an interval of fixed or calendar length) |
| `value type` | gauge/counter/interval: `f64`, or `i64` with a declared `decimal scale` (exact); state: u16 index into declared names or bool |
| `unit` | fixed for the life of the series; a change of kind, type, scale or unit is a new series |
| `expected interval` | configuration, never learned. Default: gauge and counter 60 s; interval its interval length (calendar length: 24 h for `day`, 31 days for `month`); state and text none (valid until next reading) |
| `interval length`, `interval phase`, `interval zone`, `day start` | interval series only, all fixed for the life of the series (a change is a new series). The length is either a duration d > 0 with a phase, 0 ≤ phase < d (default 0), or a **calendar length**, `day` or `month`, with an `interval zone` (an IANA name) and no phase; `day` may declare a **day start**, a local time of day with 00:00 ≤ day start < 24:00 (default 00:00), so that a day from 05:00 to 05:00 local is one interval, and a day start on any other length is a definition error. The interval zone is independent of the site zone, is not effective-dated and is compared by canonical name after resolving IANA links; a definition naming a zone the tzdata version in force does not know is an error. A reading's `t` is the start of its interval. Its **end** is `t + d`, or for a calendar length the next **interval start** after `t`: the starts are, in the interval zone, the instants of the day start on each date (for `month`, the start of the first day of each month, §5.7); a day start that falls in a gap is placed where the gap ends, as §5.7 places a midnight that does not exist, and one that occurs twice as a midnight that occurs twice (open, Q-77). All starts are taken under the **tzdata version in force**, the one version that the phase check at commit, displacement (§5.4) and calendar results (§5.7) all use. Each interval thus runs from one start to the next, whatever the rule for an unusual day start. A reading carries no length: lengths are exact nanoseconds (a local day is usually 23–25 h). `t` must be an interval start (fixed length: `t ≡ phase (mod d)`, Euclidean, on UTC nanoseconds), or the reading is refused and counted; its value is the amount over `[t, end)`. Intervals of current readings that are not displaced (§5.4) therefore never overlap. A source whose periods are neither of one fixed length nor local days (from one day start to the next) or months of one zone, such as irregular billing periods, cannot be an interval series in this version, and the engine never splits an amount into finer intervals. Record on change (§2.6) does not apply to an interval series |
| `staleness limit` S | `3 × expected interval` unless set; none if no interval |
| `record on change` | optional; see §2.6 |
| `wrap_at`, `reset_ceiling` | counters only; §5.5. `reset_ceiling` default 10 % |
| `lateness bound` | default 5 min; §2.2 |
| `versioned` | optional, for sources that revise readings they have already published; fixed for the life of the series (a change is a new series, and history may be re-ingested with `changed_at`). Every reading of a versioned series carries `changed_at` (§2.2); readings of other series never do |
| `changed_at kind` | versioned series only: `time` (UTC nanoseconds) or `version` (an opaque counter); fixed for the life of the series; used to type exports, and must be `time` for a forecast series |
| `retention class` | `long-term` (default) or `diagnostic` with a retention period (default 30 days), for series kept mainly for short-term diagnosis, such as a machine's own resource metrics. It decides only where raw detail is kept (§8 Retention class), never which facts exist. A change of class is a registry entry that takes effect at its position, not by event time; it moves no detail back |
| `timing` | `observed` (default), `scheduled` or `forecast`; fixed for the life of the series. **Scheduled:** values known in advance that take effect at their `t`, such as published prices; a scheduled source that revises its values must be versioned. **Forecast:** values about future times, re-issued over time; the series must be versioned with `changed_at kind` `time`, `changed_at` being the issue time and `t` the valid time. The engine does not report a silent forecast source: its adapter emits an observed series with an expected interval on each successful fetch, to which stale-for applies |

Series definitions, parameter changes (with an effective event time, except a change of retention class, which
takes effect at its position), aliases and retirements are log entries. A parameter other than the retention class
applies to readings with `t` at or after its effective time. A definition entry always has a lower
position than the series' first reading (the same batch is allowed). Registry entries are also compacted into a
registry file that stays on the hot tier (tree checkpoints carry a copy, §8).

**Retirement** is effective at `t_r`: facts with `t < t_r` are still accepted; facts with `t ≥ t_r` are refused
and counted. The old id stays reachable by id.

### 2.2 Log entries, positions and facts
- Every log entry (fact, definition, parameter, alias, retirement, routing change) has a **position**
  `(generation: u32, sequence: u64)`, ordered lexicographically. Every generation starts at sequence 1; `(g, 0)`
  means "before the first entry of generation g" and `(0, 0)` means none. Sequence is strictly increasing and
  gap-free within a generation across restarts, except for lost ranges recorded in the manifest (§3); a restore
  starts a new generation (§8). Consumers see only the positions of facts routed to them, so gaps between delivered
  positions are normal.
- Fact: `(series, t, position, received_at, value, quality, flags)`, and `changed_at` in a versioned series.
- `changed_at`: a u64 from the source, its revision time or version. It must not decrease between revisions of a
  reading and should rise with each, and it is the same on every replay of one revision: the adapter must not take a
  time at replay. It is compared as an unsigned
  integer and never with `t`, `received_at` or any clock. A reading on a versioned series without it, or with it on
  any other series, is refused and counted. A source whose versions are coarser than its revisions can still have a
  correction with the same `changed_at` reverted (§2.4).
- **Rank:** a fact's position, or in a versioned series `(changed_at, position)`, compared lexicographically.
- `t`: event time, UTC nanoseconds, from the adapter. The adapter declares `t_source: device | host`.
- `received_at`: host clock at commit. `commit_time`: per batch, `max(received_at, previous commit_time)`.
- `quality`: good / estimated / suspect / bad, from best to worst. The adapter supplies it; the engine may lower
  it, never raise it (`future` below; invalid values, §2.5). The adapter-supplied quality is kept with the fact for
  the duplicate rule (§2.4).
- Fact flags (immutable): `future` (observed series only: `t > received_at + 60 s`; quality lowered to suspect if
  better), `late` (on an observed series, for a fact not flagged `future`, `t` earlier than the series' lateness
  reference by more than 2 × S; on any series, `received_at − t` above the lateness bound), `revise` (§2.4),
  `host_time` (`t_source = host`), `unsynced_clock` (§2.5). For a reading of an observed interval series, its end
  (§2.1) stands for `t` in `future`, in the second clause of `late`, and wherever a reading's readiness against `now`
  is tested (live state §2.3, future facts §4, live staleness §5.4, cadence §6); a scheduled or forecast interval
  reading takes effect at `t`. Delivery flags are separate (§4).
- **Lateness reference** (observed series): the greatest `t` among the series' recorded facts of lower position that
  are flagged neither `future` nor `unsynced_clock` (§2.5), whatever their quality or rank, earlier facts of the same
  batch included; none until there is such a fact. Only the first clause of `late` uses it. A fact flagged `future` or
  `unsynced_clock` never raises it, neither at commit nor when its time comes, the flags being immutable, so a device
  clock far ahead at boot does not raise it either; a fact flagged `future` is never `late` by the first clause. A
  reading up to 60 s ahead is not `future` and does raise it, so on-time readings in the next 60 s − 2 × S of event
  time can be flagged `late` (none when S ≥ 30 s), an accepted limit. It is not the series' **latest recorded `t`**,
  the greatest `t` over all its recorded facts, which the duplicate fast path (§2.4), Restore step 2 (§8) and
  per-series statistics keep using. Both are functions of the log (positions set aside, §8 Restore, and lost ranges,
  §3, excepted).

### 2.3 Views
| View | Definition |
|---|---|
| Current | for each `(series, t)`, the fact of highest rank (§2.2) |
| As-of P | the current view over facts with position ≤ P |
| As-of time T | as-of P, where P is the last position whose batch `commit_time` ≤ T |
| History | all facts for `(series, t)` in position order, each with its `changed_at` in a versioned series |
| Live state | per series, the current fact with the greatest `t ≤ now` and quality not bad; a forecast series has none. While a restore brings detail back, a series has live state **unknown**, not none, when its live-state fact may be in a file not yet local: that is, unless the fact with its latest recorded `t` (§8 Restore step 1) is local or in a stored node and is itself the live-state fact (`t ≤ now`, quality not bad), such as a state or text series with no stored level whose last fact predates the log replayed in step 1 (open: what ends it when that fact stays in deep or is lost, Q-84) |

A fact that is not current for its key is a **revision**. In a versioned series a fact can be a revision from the
moment it is recorded: it is **outranked** when a fact of higher rank for its key is already recorded.

### 2.4 Duplicates
- An arriving fact is a **duplicate** if its `(series, t, value)` and adapter-supplied quality equal those of *any*
  fact already in the history of `(series, t)`, including facts earlier in the same batch. Flags and quality assigned
  by the engine are not part of identity. Values compare bitwise (`0.0` ≠ `-0.0`). Duplicates are not recorded and
  consume no position; they are counted per series (`duplicates_dropped`). Adapters may replay freely: a replay that the
  check can run on never reverts a correction (for the unchecked case, see below).
- In a versioned series identity also includes `changed_at`. A fact whose `changed_at` is above that of every fact
  in its key's history is never a duplicate, even when its value returns to an earlier one.
- A fact submitted with `revise` intent bypasses the check (deliberate return to an earlier value). On a versioned
  series `revise` is refused and counted; a return to an earlier value there is a fact with a higher `changed_at`.
- A reading recorded without a value (§2.5) is compared as "no value", with its adapter-supplied quality.
- Identity includes `t`: the same value one nanosecond later is a different fact and is recorded.
- **Revised-key index** (hot tier, never moves; carried by each tree checkpoint, §8): for every `(series, t)` with
  more than one recorded fact, the exact value, adapter-supplied quality and `changed_at` of each. For any other key,
  history holds at most the current fact, which is compared directly.
- **Version index** (hot tier, never moves; carried by each tree checkpoint, §8): for every key of a versioned series,
  its highest recorded `changed_at`. The rank of an arriving fact is therefore known whenever the index is local.
- **Fast path:** a fact with `t` later than the series' latest recorded `t` (over all recorded facts, `future` ones
  included; not the lateness reference, §2.2) cannot be a duplicate; no lookup. Other facts are checked against the
  in-memory recent window (§5.1) before any index probe.
- If the current fact for a key is on no local tier (its detail moved out under §8 fill pressure or retention class),
  the fact is recorded and counted `duplicates_unchecked` (§12 D5), unless its `t` is a displaced interval start
  (§5.4), where it is refused. The check never reads off-machine data. In a
  versioned series an arriving fact whose `changed_at` matches none in its key's history is not a duplicate and needs
  no check; one whose `changed_at` equals that of a fact off every local tier is recorded, counted
  `duplicates_unchecked` and takes the tie by position. An unchecked repeat therefore never reverts a correction with
  a higher `changed_at` in a versioned series. In any other series it can revert a correction that differs in value,
  so a source that revises readings must be declared versioned.

### 2.5 Quality, invalid values, clock
- Bad-quality facts stay in views but are ignored by summaries and do not refresh staleness (§12 D6).
- NaN or ±∞ is recorded as quality bad with no value, flagged and counted (§12 D1). A negative counter value
  (§5.5) is recorded the same way; D1 concerns NaN and ±∞ only.
- Until the system reports clock synchronisation, batches carry `unsynced_clock`, `future`/`late` are not set, and
  facts with `t_source = host` wait in the ingest queue (back-pressure) rather than being stamped with a wrong time.
  Device-stamped facts are accepted.

### 2.6 Record on change
A record-on-change series records a fact when the value changes **and** at least once per expected interval while
it does not (a keep-alive fact). For such series the expected interval is the keep-alive interval, not the poll
interval. A healthy unchanged series is therefore never stale, and every derived value stays a function of facts.
The adapter leaves out unchanged readings between keep-alive facts; the engine records every reading it is given
(§2.4).

## 3. Log
| Rule | |
|---|---|
| Batches | Append-only segments of arrival batches. A batch ends with a **commit block** (last position, checksum of the batch's blocks). One batch = one commit = one sync. A batch never spans segments. |
| Commit | Write batch, sync, update the durable mark, then (and only then) acknowledge adapters, update tree and live state, and deliver. |
| Durable mark | After each successful sync the writer updates a memory-mapped mark file: segment id, byte offset, last position, boot id, sequence lock. The mark file itself is synced on a timer (`mark_sync_interval`, default 1 s) and at clean shutdown, so after a power loss the mark on disk lags acknowledged batches by about one interval. Readers read log segments only up to the mark. |
| Group commit | While a sync is in flight, arriving readings queue and form the next batch. No added delay when idle (optional linger, default 0). |
| Segments | Preallocated at creation (64 MiB). The directory is synced after creating, sealing or renaming a file. |
| Sealing | At 64 MiB or 5 minutes (both configurable), whichever first; an empty segment is not sealed. Sealed segments are immutable and eligible for backup. Compaction never removes a log segment (§8). |
| Recovery | Below this table. |
| Write or sync failure | On the log, manifest, mark, cursor or registry files: stop accepting writes, mark read-only, fatal health; never retry on the same handle; recovery by restart. After a failed sync the batch's fate is unknown: a restart on the same boot truncates it; after a reboot recovery may keep it (row 7a). It was never acknowledged, so adapters may replay it, which the duplicate rule absorbs. A failed compaction, tiering or tree write abandons that job, deletes its partial output and degrades health. |
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
   truncated. The same applies when the mark is missing or unreadable. Because the mark on disk can lag acknowledged
   batches, a truncated tail may have held acknowledged facts whose bytes were damaged: it is reported as
   `possible_loss`, with its length in bytes up to the last non-zero byte and the number of commit blocks still
   readable in it; an empty tail is not reported. New positions then continue above the highest sequence of the
   current generation among the last kept batch, any local cursor file and any local manifest record; positions
   skipped that way are recorded as a lost range whose cause is a truncated tail. So no position named by a synced
   cursor or manifest record is reused. A delivered position that no synced cursor names can be reused, so after
   every recovery under this rule, whether or not anything was reported, every consumer receives `resync` before any
   position above the last kept batch and forgets the positions it has seen above its recovered cursor (§4
   At-least-once, Resync). A missing or unreadable cursor file is reported with `possible_loss`, and its consumer is treated as
   having no cursor.
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
| At-least-once | After a restart a consumer resumes after its cursor and may see a delivery again. Consumers de-duplicate fact deliveries by position and timer deliveries by `(funnel, sequence)`; commands carry ids. On `resync` a consumer must forget every position it has seen above its durable cursor as recovered from its file, which can lag its confirmations by up to the cursor sync interval (Consumer state): after a truncated tail such a position can name a different fact (§3 Recovery rule 3), which must not be dropped as a repeat. |
| Overflow | A full queue switches the consumer to catch-up: it reads the log from after the last position already enqueued for it in the current session (not from its confirmed cursor, so order holds within a session) up to the durable mark, then re-attaches to live at the next position under the writer's delivery lock (no gap, no repeat). It never blocks the log or other consumers. Time behind is measured. |
| Replay | Fact deliveries produced in catch-up are flagged `replayed` and keep their original times. They are for record-keeping and are never fed to a condition operator a second time (§6). A consumer that acts on devices must not act on old deliveries (default: `received_at` more than 60 s before now; event time does not count). |
| Resync | A `resync` delivery carries, per funnel, the current live state and condition state evaluated now. Sent on resume after downtime (§6), on `generation_changed` (§8), when the cursor is older than the oldest local log segment, when the cursor, or the `start` position of a newly registered consumer, is below or inside a lost range (§3), when it is at or above the start of a tail truncated by §3 Recovery rule 3, and to every consumer after every recovery under §3 Recovery rule 3, whether or not anything was reported, before any position above the last kept batch is delivered to it, since any consumer may have been delivered positions that are now reused. With no latch to compare, as after a restore, it delivers each condition's present state, true or false, as a transition. During a restore a condition or live state may be **unknown** (§2.3, §6 Conditions): `resync` reports it so for that funnel, which is no transition and leaves the latch as it is. It is not flagged `replayed`; acting on it is acting on the present. |
| Future facts | Observed series: delivered at commit, flagged `future`; ignored by live state, staleness and windows until `t ≤ now`; it never raises the lateness reference (§2.2). Scheduled and forecast facts are delivered once, at commit, in position order, not flagged `future`: the delivery is a notice of a value for `t`, and a consumer acts on live state or a condition, never on the delivery itself. A scheduled value taking effect is seen through live state or a condition funnel's timer evaluation; there is no second delivery. A stale-for condition on a forecast series is rejected when funnels are compiled (§6 Cadence), and `resync` for a funnel over a forecast series carries condition state only. |
| Outranked facts | A fact outranked at commit (§2.3) is delivered in position order, flagged `outranked`, and counted per series (`outranked_on_arrival`). It does not change live state or staleness, is never fed to an operator or condition and must not be acted on. |
| Timers | Window expiry, "held for N s" and staleness of an observed series use the monotonic clock; system clock corrections never fire or suppress them. A future fact matures, a scheduled value takes effect and a scheduled series falls stale by `now` (§6), so a forward correction of the system clock can bring any of them forward. Timer deliveries that fell due while the engine was down are not replayed; `resync` covers them. |

## 5. Summary tree
### 5.1 Levels and storage
| Level | Stored | Where |
|---|---|---|
| 1 second | **Never.** Computed from facts on demand (window edges, drill-down below 1 minute) | — |
| 1 minute | Yes, aligned to UTC, only where there is data | per-series tree files on hot; never moves (§12 D7) |
| 15 minutes | Yes, aligned to UTC, only where there is data | per-series tree files on hot; never moves |
| Local calendar day/month | Yes, for spans whose edges fall on 15-minute boundaries (§5.7) and throughout which the series' expected interval in force qualifies it for a stored 15-minute level (density rule; interval series §5.4), whether or not a given quarter-hour has data, assembled from 15-minute nodes in the site zone in force; an input to calendar results, not the result (§5.7) | per-series calendar file on hot |

- **Density rule:** a level is stored for a series only where its nodes summarise many facts: the 1-minute
  level only for series whose expected interval is 5 s or shorter, the 15-minute level only for 60 s or shorter.
  A sparser series answers those levels from its facts directly, so a stored level is never larger than the
  data it summarises. Such a series stores no calendar nodes either: its calendar results are window evaluations
  over its facts (§5.7).
- Memory holds only open nodes per series, a node cache, and a **recent window** of raw facts per series
  (default 15 minutes, at most 1 024 facts, and for an interval series at least its interval length, for a calendar
  length at least its latest two intervals) for the newest
  window edge and the duplicate fast path.
- Tree files record the position up to which they are complete (the **checkpoint**, written at most every 60 s and
  on clean shutdown). On start the engine loads them and replays log entries after the checkpoint. Tree files are
  derived and are backed up as such (§8).

### 5.2 Node contents (current view; bad quality ignored)
A node over span [a, b) is either **empty** (the identity) or holds, over the sequence of readings
r₁ … rₙ (in `t` order) inside its span:

| Kind | Per reading | Per consecutive pair (rᵢ, rᵢ₊₁) inside the span |
|---|---|---|
| all | count, first `(t, v)`, last `(t, v)` | coverage += hᵢ (interval: see its row) |
| gauge | sum, min, max | integral += vᵢ × hᵢ |
| counter | — | increase += step(vᵢ, vᵢ₊₁) (§5.5), resets, regressions |
| state | — | time[vᵢ] += hᵢ; transitions += [vᵢ ≠ vᵢ₊₁] |
| text | — | (count, first, last and coverage only) |
| interval | sum, min, max; coverage is the total length of the intervals of the readings starting in the span. This node form is used only for spans whose edges no interval crosses (§5.4); any other span, drill-down included, is evaluated as a window (§6) | none (no hold, no S) |

For every kind but interval, the **hold** hᵢ = min(tᵢ₊₁, tᵢ + Sᵢ) − tᵢ, Sᵢ being the staleness limit in force at tᵢ
(∞ if none). Time is held in nanoseconds and reported in seconds. **Nothing is held past a node's last reading inside
the node.** Min and max are over readings only.

### 5.3 Merge
`merge(L, R)` for adjacent L before R: if either is empty, the other; otherwise per-reading fields combine
(count and sum add, min/max, first from L, last from R), per-pair fields add, and the boundary pair
(L.last, R.first) contributes its pair terms once. The pairs of a concatenation are the pairs of each part plus
one boundary pair whatever the bracketing, so merge is associative with the empty node as identity. The reference
model must prove this against direct computation (invariant 6), including empty nodes, holds spanning several
nodes and late facts inserted between existing readings.

### 5.4 Staleness and coverage
A reading is valid for at most S (an interval reading for its interval only). An interval series stores a tree level
only where every node edge of that level is a multiple of d from the phase (for the UTC levels: d divides the node
length and the phase is 0), so that no interval crosses a node edge; any other level is answered from its facts. A
calendar-length interval series stores no level and no calendar node, whatever its expected interval; every result is
a window evaluation over its facts (§5.1 density rule, row 20a). The density rule (§5.1) applies as well, so an
interval series stores a calendar level only where it stores the 15-minute level. For an observed series, **live
staleness** is measured on the monotonic clock: a series is stale when more than S has passed since its last commit of
a fact that is not bad quality (§2.5) and not a future fact still awaiting its time or an outranked fact (§4), so a
forward jump of the system clock cannot make an observed series stale. A scheduled
series is stale when `now` is later than the `t` of its live-state fact by more than S, or when it has no live-state
fact; commits do not refresh it. A forecast series has no live staleness. **Event-time staleness** applies to history
and to every window evaluation (§6): a series is stale at event time x when x > (`t` of its latest reading before x
that is not bad quality) + S; for an interval series, x is covered if and only if it lies in the interval `[t, end)`
of a current reading that is neither bad quality nor displaced (below). Coverage stops at that point and
time-weighted results exclude the gap. Every windowed result carries coverage in seconds and as a fraction of the
window. Live staleness applies only to live state and stale-for conditions.

**Displaced readings.** After a tzdata change, a recorded reading of a calendar-length series whose `t` is no longer
an interval start is **displaced**. It is kept (current, as-of and history views show it), and is ignored by live
state, staleness and every window, as a bad-quality fact is (§2.5). A window result whose span contains the `t` of a
displaced current reading is flagged `displaced`; a per-series statistic, `interval_displaced`, counts the readings
found displaced, monotone and persisted on a best-effort basis like the health counters (§11). The flag stays while the rules displace the
reading, also after the source has repaired its interval; a window inside the affected interval that does not contain
the old `t` shows the missing amount only as missing coverage. A reading whose `t` is still a start keeps its place
and its end follows the version in force. Displacement is a function of `t`, the interval zone, the day start and the
tzdata version in force, so a later release can undo it. The source repairs by sending the amount at the new start,
which is a new key; nothing needs withdrawing. An identical replay at the displaced `t` is a duplicate and is dropped
(§2.4) when the check can run on a local current fact; any other reading there, a revision included, or a replay
whose current fact is on no local tier, is refused off its phase (row 40), never recorded unchecked. A tzdata change invalidates
cached windows (§6 Cache) of calendar-length series and rebuilds nothing, there being no stored node.

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
- Each node carries a version incremented on change. An outranked fact (§2.3) changes no node.
- If the facts of an affected minute are on no local tier, the fact is still recorded, the node is flagged
  `summary_stale` and recomputed when the detail is fetched (§8). Chunk indexes always stay hot, so the engine
  knows whether a minute has facts off the machine.

### 5.7 Calendar
- Facts and the UTC levels are in UTC; a zone only defines calendar days and months. The site zone is a log entry
  with an effective time; each local day uses the zone in force at its start. A zone change rebuilds calendar nodes
  from its effective time only (§12 D3). The tzdata version is recorded per calendar file; a tzdata change rebuilds
  the affected days and is logged.
- A calendar window or query may name an IANA zone instead; its days and months are then taken in that zone over
  the whole span, with no effective dating. Its result is a window evaluation over the UTC data like any other. A
  stored calendar node is an input to it only when every local day the node covers was taken in the named zone
  (zones compared by canonical name, after resolving IANA links; so not a month spanning a site-zone change) and
  the node's recorded tzdata version is the current one; elsewhere the result is evaluated as for a span off
  15-minute boundaries (last item).
- A calendar day or month **result** is a window evaluation (§6) over its span, edge stretches included. Stored day
  nodes are an input to that evaluation, not the result.
- A local day usually has 92–100 quarter-hours. A day whose local midnight does not exist starts at its first
  instant. Because results are window evaluations, the rule also covers a skipped date (an empty span) and historical
  offsets that are not multiples of 15 minutes.
- A calendar node is stored only for a span whose edges both fall on 15-minute boundaries and throughout which the
  series' expected interval in force qualifies it for a stored 15-minute level (§5.1), whether or not a given
  quarter-hour has data. For any other day or month none is stored; its result is evaluated from the stored nodes
  inside it, the facts at its edges and, where no level is stored, its facts.
- The interval zone and day start of a calendar-length interval series (§2.1) define only its readings' intervals.
  Calendar windows and results on it are taken in the site zone, or in a zone the window names, as for any series, and
  their days start at local midnight, never at a day start. Where every edge falls on an interval start the result is
  a sum of whole intervals, exact for a decimal scale and not flagged: so for a day or month of a `day` series whose
  day start is 00:00, or a month of a `month` series, taken in the interval zone, and for any other span whose edges
  happen to fall on starts (a UTC day on a Europe/London `day` series in January). Elsewhere (another zone, a UTC
  span, a day of a `month` series, a day or month of a `day` series with another day start, so a site-zone day against
  a day from 05:00) an edge may lie inside an interval, and where one does the result is apportioned (§6). No zones
  are compared: `apportioned` follows from the edges alone. The amount of one whole interval is its reading.

## 6. Windowed operators
| Rule | |
|---|---|
| Aggregates | avg (gauge: integral ÷ covered seconds), sum, integral, min, max, count, increase, rate (increase ÷ elapsed seconds between the readings that bound it; the result carries coverage and is flagged when a step it includes spans more than S, in force at the step's earlier reading; empty when no step falls in the window), last, time-in-state. With no reading in the window but a valid held value (not for interval series): `last`, avg, integral and time-in-state use it; `min`, `max`, `count`, `sum` and rate are empty; increase counts only a step credited to the window (Edge stretches). **Interval series:** sum is the amount in the window; rate is sum ÷ covered seconds; count, min, max and last are over readings whose `t` lies in the window; avg, integral, increase and time-in-state are refused. Edge stretches do not apply: a window edge inside an interval credits that interval's amount pro rata, amount × overlap ÷ length (end − `t`, §2.1) computed in f64, and the result is flagged `apportioned`; sums of whole intervals stay exact for a decimal scale, and apportioned results are compared within the §5.6 tolerance. An edge lookup reads the one reading whose interval can contain the edge: the reading at the interval start at or before it (fixed length: at most d back; calendar length: the start of the interval containing the edge). On an observed interval series a reading enters live state and windows only once its end ≤ `now`. A displaced reading (§5.4) is ignored and the result flagged `displaced`. |
| Windows | Rolling, `[now + a, now + b)` with a < b of either sign, so that a window may lie in the future, and calendar (since local midnight, this month, or relative to the current one, such as the next local day or month) in the site zone, or in an IANA zone the window names (§5.7) ("since local midnight" and "this month" end at `now`; a named or relative day or month ends at its end); a window wholly after `now` on an observed series is empty, with zero coverage; a calendar result is a window over its span (§5.7). Every window, rolling or calendar, is evaluated in event time against `now`: its bounds are event times, and no allowance is made for a jump of the system clock. Health reports such jumps (§11). |
| `now` | The window clock: an engine UTC clock that never runs backwards: max(system clock, previous value), advanced by the monotonic clock. |
| Evaluation | Whole stored nodes in the middle; at each edge, the partial minute is computed from raw facts (the recent window when the edge lies within it, the edge chunk otherwise). Exact to the nanosecond. Nodes whose span extends past the credit limit are never used whole. |
| Edge stretches | Leading: from the window start to the first reading inside, credited with the last reading before the start if within its S. Whatever S, the counter step and the state transition from the last reading before the start to the first reading inside are credited to this window. Trailing: from the last reading to min(window end, credit limit, last.t + S). Time is never credited past the **credit limit**: `now` for an observed series, the window end for a scheduled or forecast series, whose values are known in advance. |
| Old spans | A window never reads off-machine data. Where an edge's raw detail is not local (moved under §8, or during a restore held by a listed file not yet local, §8 Restore), the edge moves inward to the nearest node boundary of the finest level stored for the series there, or, where it stores none, to the nearest instant from which all its detail to the other edge is local. The result gives the span evaluated (`from`, `to`) and is flagged `approximate_edges`. Where no span remains the result is the error `detail_not_local`, never an empty result. |
| Cache | The merged middle is cached with the versions of its nodes and invalidated when any changes (late data included). |
| Cadence | A windowed funnel is evaluated on each routed fact that is not outranked (§4) and on a timer every 1 s (configurable per funnel). A fact of a scheduled series is fed to a condition, or to an operator that is not windowed, only once `t ≤ now` (a windowed operator sees it at commit), and a condition funnel over a scheduled series is also evaluated on the timer, so a value taking effect is seen. A condition over a forecast series needs a windowed operator; any other is rejected when funnels are compiled. |
| Conditions | Threshold with a separate clear level, on change (optional dead-band), held for N seconds, stale for N seconds. During a restore (§8 Restore step 2), an evaluation with no local detail (the error `detail_not_local`, Old spans, or live state unknown, §2.3) makes the state **unknown**, not false: the evaluation is skipped and counted per funnel (`evaluations_unknown`), the condition keeps its latched state, `resync` reports "unknown" for that funnel (§4), and the first evaluation with detail is delivered as a transition if it differs from the latched state, or there is none (open: a state that stays unknown, Q-84). |
| Late data | A trigger acts on what is known when it is evaluated and never fires retroactively. |
| Latch | Each condition funnel persists, with its consumer's cursor, the last emitted state and the position at which it was emitted. |
| Restart and resume | The operator is rebuilt from the tree and recent window as of now; it is never replayed. Facts after the cursor are delivered flagged `replayed`, not fed to the operator; after a §3 Recovery rule 3 recovery this replay stops at the last kept batch, and the one `resync` that follows serves both rules. The funnel then sends `resync` with the condition state now; if that differs from the latch it is a transition and is acted on (an unknown state, Conditions, is not). A condition that became true while the consumer was down is therefore delivered, never suppressed. |
| Multi-series | Conditions over several series are composed by the consumer in the first version. |

## 7. Compaction and files
- Compaction rearranges facts from whole sealed log segments into per-series chunks. It is **lossless**: every fact,
  including revisions, with its position and `received_at`. Facts are identified by position.
- Current facts and revisions go to **separate chunks**. A revision chunk holds only facts outranked by a fact within
  the same compaction's inputs, so the current view never needs revision chunks. A current chunk may hold facts
  outranked later or by a fact in another file; readers resolve the current view across current chunks by rank.
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
| Hot | fast local disk | log segments < 90 days, current detail, revised-key index, version index, chunk indexes, registry, the whole stored tree |
| Cold (optional) | another local path | older revision chunks and sealed log segments; moved current detail |
| Deep | S3-compatible bucket, via a pluggable backup sink | every sealed file, every manifest record, tree checkpoints (with the registry, latest recorded `t` and lateness reference, revised-key index and version index), generation claims |

| Rule | |
|---|---|
| Continuous backup | Each sealed file is queued for upload once; upload never blocks ingest or delivery. After its files, each manifest record is uploaded; a tree checkpoint at least daily, and only after all log up to its position is backed up. Each tree checkpoint carries the registry file, each series' latest recorded `t` and lateness reference, the revised-key index and the version index, as of its position. Cursor files are not backed up. |
| Encryption | On the machine before upload, once per file; key supplied by the embedding application. |
| Hashes | The manifest records SHA-256 of the plaintext at seal (hot and cold verification) and, per upload, SHA-256 of the ciphertext, sent with a single-part upload. The bucket must reject a mismatch and its reported hash must equal the manifest's; only then is the file backed up. Scheduled check: re-read reported hashes; sampled download-decrypt-verify proves key and content. |
| Bucket | The engine never deletes from it. Versioning or object lock required. |
| Eviction at 90 days | Revision chunks (age per series, "never" allowed) and sealed log segments leave hot only after they are verified in cold or deep. Current detail and the stored tree stay, except as the retention class allows. |
| Retention class | For a diagnostic series (§2.1), raw detail (current and revision chunks) whose span ends more than the retention period before `now` leaves hot once verified in cold or deep; for revision chunks, the earlier of this and their own age applies. The stored tree stays hot. Nothing is deleted: such detail is fetched on demand like moved current detail, so, as with any moved detail, the duplicate check for it is unchecked (§2.4) and window edges over it are approximate (§6). |
| Fill pressure | Above 70 % fill, verified sealed log segments leave first, then the oldest current raw detail (verified first, fetched on demand); the tree, chunk indexes, revised-key index and version index never move (§12 D4). |
| Behind consumers | A consumer whose cursor is older than the oldest local log segment receives `resync`, not a replay. The engine never downloads from deep for catch-up. |
| Corrupt block | File quarantined; the engine fetches the copy from cold, else deep, verifies the plaintext hash and swaps it in by a manifest record. If no other tier holds a verified copy, the file is compacted again from its kept inputs, verified by the §7 procedure, recorded with its new identity and hash, and swapped in by one manifest record that retires the quarantined file. Queries over the range return an error until then. Never repaired in place. Because inputs are kept until a file is verified elsewhere (§7), the only single copies are the open log segment and a sealed log segment not yet uploaded and not yet compacted: corruption there loses only the damaged region, from the last valid batch before it to the next valid batch after it or, with none after it, to the position before the next segment's first (in the open segment, to the mark's last position, as in §3 Recovery rule 2), recorded when found and reported as in §3 Recovery rule 2. For a sealed segment the valid batches are copied unchanged to a new segment, verified by their commit checksums, and one manifest record adds it with its new identity and hash, retires the quarantined file and records the lost range; the error ends with that record and the new segment is backed up like any sealed file. An optional local mirror path for sealed log segments (configuration `mirror_path`, off by default) closes this window for sealed segments. A restore reads the files it lists from deep alone: a listed file that the bucket has lost or corrupted is handled as here with deep as the failed tier (Restore, Failures and end). A loss in the bucket found outside a restore is open (Q-85). |
| Upload stalled | Files queue locally, health degraded, nothing evicted. |
| No deletion | Every fact is kept forever in at least the deep tier; the engine has no purge. An exception is a fact held only by a listed file that the bucket has lost or corrupted when a restore reads it, which becomes part of a lost range (Restore, Failures and end); a loss in the bucket found outside a restore is open (Q-85). |
| Re-ingest | Exported or foreign data goes through the normal ingest path, in batches. |
| Restore drill | The restore procedure is testable end to end; operators should exercise it regularly. A drill restores read-only and claims and uploads nothing. Its restore record is kept locally only, for the generation a restore would claim (one above the highest claimed), which stays unclaimed; the claims are not read again and the drill is never superseded. |
| Generations | The first store of a bucket writes generation 0, which counts as claimed. Before accepting ingest, a restore claims the generation one above the highest claimed, by a create-if-absent marker in the bucket; if the marker already exists it takes the next. A claim is never withdrawn, also when its restore fails. Manifest records are keyed by generation. Every generation above 0 begins with its **restore record**: its generation, checkpoint and restored end, the restored ends carried from the restore record it read (each with its generation), and every file it restores from, set-aside segments included, each with its hash, tier and position range. The records of one generation are therefore complete without those before it, and a restore reads the records of the highest generation below its own that has any in the bucket. The restore record is synced locally, then uploaded before any other file of its generation; the restore then reads the claims again and, if a generation above its own is claimed, stops before accepting ingest (superseded, fatal health). A writer that finds a higher generation claimed stops uploading and becomes read-only (fatal health, §3); what it uploads after the later restore has read its records stays in the bucket, in no later generation's records, and is lost to them as in a disaster. A restore keeps the store id of the files it restores (`format.md` file header), and backup is refused on a bucket that holds files of another store id. A restore from a bucket with no manifest records is refused. |

**Restore** (onto an empty disk):
1. Claim a generation and read the manifest records it restores from (Generations above). Each restore record, the
   carried ones and the new one, **sets aside** the positions above its restored end in the generations below its own;
   a position set aside once stays so in every later restore. The **restored log** is the positions held by the listed
   files that are not set aside. The **restored end** is its last position before the first gap that is not a recorded
   lost range (§3). The scan for that gap covers only the generation whose records are read, from its sequence 1; if
   that generation holds no position, the restored end is that of the restore record read. Positions of earlier
   generations at or below a recorded restored end were settled by that restore, and a file missing there is a failure
   of this restore (below), not a new restored end. Write and upload the restore record (Generations above) before
   ingest is accepted. Download the latest tree checkpoint at or below the restored end (labelled derived-unverified),
   with its registry file, latest recorded `t` and lateness reference per series, revised-key index and version
   index, and the sealed log segments after its position; replay them into the tree, which brings the registry, latest
   recorded `t`, lateness reference and version index up to the restored end. With no checkpoint, download the whole
   sealed log. Positions set aside are kept, not served: the log segments holding them are downloaded in this step
   and are then files like any other (compaction and tiering move
   them losslessly), and they are excluded by position from views, trees, indexes, the registry, delivery, catch-up
   and compaction's ranking (compaction ranks facts and splits current from revision chunks without them, so a
   set-aside fact never makes a restored fact a revision), in the writer and in reader processes, which take the
   restored ends from the restore record. Queries do not see them and return no error for them. A definition entry set
   aside keeps its id reserved in the registry and nothing else: a new series never takes that id, and its name is
   free. Health reports each restore record's set-aside range (first and last position, number of facts still held, commit times
   at both ends), derived from the record at every start. The engine never re-ingests them: the operator exports them,
   each fact under the name its own definition gives, and submits what is wanted through Re-ingest, where the
   duplicate rule drops what is already recorded. An id defined only in files that were never backed up, or uploaded
   by a superseded writer after the restore read its records, is in no file the store holds and can be assigned again.
   Positions of the new generation start at sequence 1.
2. The store serves queries, flagged `restoring`, and accepts ingest. Until the restore ends, detail held by a file
   the restore record lists that is not yet local counts as not local (Failures and end below). Window evaluations
   follow §6 Old spans, from the checkpoint's nodes and the local facts, for a series with no stored level as for any
   other: an edge moves to where its detail is local, and the result is the error `detail_not_local` only when no span
   is left. A condition evaluation with no local detail, and live state whose fact may not be local yet, are unknown (§2.3,
   §6 Conditions). Reads of facts in any view (current, as-of, history) over detail not yet local fetch it on demand, as for
   moved detail (§8 Retention class, Fill pressure). Until the current chunks that step 3 brings back are local, facts
   whose `t` is at or before the latest `t` known for their series (from the checkpoint or the restored log) and which
   match no local fact and no entry of the revised-key index are recorded and counted `duplicates_unchecked`. A key
   that gains a second fact meanwhile, in the replay or by ingest, has its index entry completed when its chunk is
   back. The rank of an arriving fact on a versioned series is known from step 1 on, the version index being exact at
   the restored end (from the checkpoint and the replay; with no checkpoint, from the whole log). A consumer without a
   cursor is treated as beyond the restored end; one that kept its last confirmed position registers with `start:
   position`. A cursor of an older generation at or below the restored end receives the facts after it up to the
   restored end through catch-up, flagged `replayed`, if their segments are local; otherwise §4 Resync applies. Every
   consumer then receives `generation_changed` and `resync`, and resumes at the new generation's first position.
3. Bring back the files the restore record places on the hot tier, and on the cold tier where one is configured:
   current chunks, then revision chunks and log segments. Other files stay in deep and are fetched on demand as
   before. A position present in two files is one fact and must have identical content.
4. When detail for a span is back, recompute its nodes from facts as of the checkpoint's position and compare them
   with the checkpoint's; any difference is reported, and the nodes are then recomputed from the current facts of
   their span (§5.6), which also include facts committed after the checkpoint. The checkpoint label is removed
   when the detail the restore brings back has been verified (Failures and end below).

A restart after the restore record is written resumes the restore in its generation, uploading its restore record if
that had not finished (a drill uploads nothing and reads no claims), reading the claims again before ingest is accepted if step 1 had not completed (Generations
above), and fetching the files its restore record lists that steps 1 and 3 bring back and are not yet local.
A crash before the record starts the restore again with the next generation.

**Failures and end.** The restore record is written before any file is downloaded, so a restore that stops before it
leaves nothing local; `restore` refuses a directory that holds files and no restore record. Until step 1 is complete
no ingest is accepted: a failure in it, other than a lost or corrupted file (below), leaves the engine down with fatal
health (`restore_failed`, naming the file) and a restart resumes it. After that, a file that cannot be fetched, or
whose plaintext hash fails, is retried, with degraded health naming it and the reason, while the others continue; the
restore stays in progress. A listed file that the bucket no longer holds, or has corrupted, in step 1 or later, is
treated as §8 Corrupt block with deep as the failed tier: what it held is compacted again from the other listed files,
and the positions that no other listed file holds become a lost range, recorded and reported as `data_lost` (§3
Recovery rule 2); the restore then completes without them. Facts of a log segment found lost in step 1 that other listed files hold
(such as compacted chunks) are fetched in step 1, before ingest is accepted, so that the version index and each
series' latest recorded `t` stay exact at the restored end (§2.4, step 2). A lost or corrupted tree checkpoint is
replaced by the latest earlier checkpoint the records list, or, with none, by the whole sealed log, as step 1 allows.
How the engine tells that the bucket has lost or corrupted a file, rather than a transient fault, a damaged download
or a wrong key, is open (Q-82). What a definition entry lost this way leaves in the registry is open (Q-33). A file
holding a position whose content differs from a local copy is not installed; health names both files and the restore
stays in progress until an operator acts. The store is `restoring` from the restore record until every file step 3
brings back is installed, or replaced or recorded lost as above; this follows from the record and the local files and
is not recorded. While `restoring`, every query result carries the flag, detail held by a listed file that is not yet
local counts as not local (§6 Old spans, §5.6), and no tree checkpoint is uploaded for backup (§8 Continuous backup);
tree files keep recording their own checkpoint (§5.1). The `derived-unverified` label goes when every span whose
detail the restore brings back is verified, which can be earlier; nodes over detail that stays in deep are not
verified by the restore. Progress (files and bytes remaining) is a statistic, not a health condition.

A disaster loses facts in files not backed up, which may already have been acted upon, and the positions set aside
(step 1) until they are re-ingested.

## 9. Edge cases
| # | Case | Rule | Test |
|---|---|---|---|
| 1 | Same `(series, t)`, different value | New fact; unless outranked (row 2d), becomes current, the old one becomes a revision and the minute path is recomputed | model + property |
| 2 | Same `(series, t)`, identical to any fact in its history | Dropped and counted (§2.4) | unit |
| 2a | Series not versioned: A, then B, then a replay of A | Replay dropped; B stays current. With `revise`, A is recorded and current | property |
| 2b | Identical repeat whose current fact is off every local tier | Recorded, `duplicates_unchecked` | unit |
| 2c | Versioned series: revision A (`changed_at` 1), correction B (2), then a replay of A, also with A's detail off every tier | Replay is a duplicate or recorded unchecked; B stays current. A later revision (3) with A's value is recorded and current | property |
| 2d | Versioned series: a fact with a lower `changed_at` than the current one and a new value | Recorded, delivered flagged `outranked`, counted; no node, live-state or condition change | property |
| 3 | Reading for last week arrives now | Flagged `late`; minute path updated; no retroactive trigger | model |
| 4 | Reading > 60 s in the future, on an observed series | Recorded, `future`, suspect (or worse if supplied); delivered flagged; ignored by live state, staleness, windows until due; does not raise the lateness reference | model |
| 4a | Observed series: one reading a day ahead, then on-time readings | The on-time readings are not `late`; the reference follows them; duplicate checks still see the future reading | model |
| 5 | Device clock stuck or far in the past | Late data; adapter health reports skew when `received_at − t` is persistently large | unit |
| 6 | Two readings in one second | Both facts; the minute node folds both | unit |
| 7 | Process crash after write, before sync | Same boot: truncated to the durable mark on restart; never delivered. With the mark lost, recovery treats it as a different boot and may keep the batch, which was never acknowledged; an invalid tail is truncated and reported as `possible_loss` | kill loop |
| 7a | Power loss after write, before sync | New boot: torn tail truncated by checksum and reported as `possible_loss`; complete batches synced on recovery before use; positions named by the last kept batch, a synced cursor or a manifest record are not reused; every consumer receives `resync` before any later position and forgets the positions it has seen above its recovered cursor | power-loss rig |
| 8 | Crash after sync, before delivery | Delivered on restart from each consumer's cursor | kill loop |
| 9 | Crash during compaction | Output unpublished and discarded; inputs intact | kill loop |
| 10 | Crash around the manifest record | Before: output discarded. After: inputs retired. Never both live | kill loop |
| 11 | Torn or corrupt block in a sealed file | Quarantine; fetch from cold/deep, else compact again from kept inputs; verify, swap by manifest; errors meanwhile. Log segment with no other copy: valid batches copied to a new segment, the damaged region recorded as lost, `data_lost` | fault injection |
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
| 20a | Calendar day or month of a series with no stored 15-minute level (expected interval above 60 s; an interval series with d = 30 min; a calendar-length interval series), or of a span in which the expected interval crosses the threshold | No calendar node; the result is a window over its facts and any stored nodes inside the span | property |
| 21 | Site zone changed, or tzdata changes its rules | Calendar nodes rebuilt from the effective time / affected days only; logged | integration |
| 21a | Calendar query in a named zone whose offset is not a multiple of 15 minutes (historical offsets); or, on an interval series, a multiple of 15 minutes but not of d (+05:45 with d = 30 min) | Days taken in that zone for the whole span; no stored calendar node used; evaluated as a window over the UTC data, with facts at its edges; an interval crossing a day edge is apportioned (§6) | model (roadmap 1.6) |
| 22 | System clock jumps | Timers of observed series monotonic; future-fact maturity and scheduled values follow `now`; window `now` never runs backwards; `commit_time` monotonic | unit |
| 22a | Host clock not yet synchronised at boot | `unsynced_clock`; host-stamped facts wait | unit |
| 23 | Series renamed | Alias entry; id and data unchanged | unit |
| 24 | Unit, kind or scale changes | New series; old one retired | unit |
| 25 | Series retired at `t_r` | Facts with `t < t_r` accepted; later refused and counted | integration |
| 26 | Reader holds an old epoch | Its files stay while its lease names that epoch and its counter keeps changing; leases of a different boot are void | integration |
| 27 | Bucket unreachable for days | Queue grows, no eviction, degraded health | fault injection |
| 28 | Restore onto an empty disk | New generation, claimed in the bucket; windows from the checkpoint's stored levels and local facts under §6 Old spans (`detail_not_local` only when no span is left); fact reads fetch on demand; detail back-fills and verifies the checkpoint; positions beyond the restored end set aside, reported, ids reserved | restore drill |
| 28a | Crash or reboot during a restore | Resumes in its generation from the restore record; before the record, starts again with the next generation | kill loop |
| 28b | Two restores from one bucket, or a drill beside a live store | Each restore claims its own generation; a writer that finds a higher one stops uploading; of two concurrent restores, the one that finds the higher claim after uploading its record never accepts ingest; a drill claims nothing | fault injection |
| 29 | Late fact for a minute whose detail is not local | Recorded; node `summary_stale` until fetched | model |
| 30 | Very many series | Memory per series bounded and documented; ceiling (default 100,000) refuses new series clearly | load test |
| 31 | Value NaN or infinite, or a negative counter value | Recorded as bad quality without a value; counted | unit |
| 32 | Format version newer than the reader | Refused with a clear error | golden files |
| 33 | Consumer emits readings from a delivery | Non-blocking emit off the writer thread; cyclic routes rejected | integration |
| 34 | Route added while a consumer is catching up | Applies from its position only | model |
| 35 | Window with no new data, or edge inside the open minute | Timer evaluation; edge from the recent window; nothing credited past the credit limit or S | property |
| 36 | Scheduled value for tomorrow | Recorded unflagged, quality kept; delivered at commit as notice; in live state from its `t`, with no second delivery; stale S after the last value takes effect | model |
| 37 | Forecast re-issued | The higher `changed_at` is current; earlier issues stay in history; a replay of an old issue is a duplicate or outranked | property |
| 37b | Forecast source falls silent | No live staleness; reported by stale-for on the adapter's companion series, and later by shrinking lookahead coverage | integration |
| 38 | Window over tomorrow on a scheduled or forecast series | Credited up to the window end; coverage shows where the values run out | property |
| 39 | Window ending after `now` on an observed series | Nothing credited past `now` | property |
| 40 | Interval reading off its phase; on a calendar-length series, not at an interval start in the interval zone (a day start, or the start of a month), including an irregular period or a day starting at another hour than its day start | Refused and counted; an identical replay of a reading already recorded at that `t` (now displaced, row 45) is dropped as a duplicate first (§2.4) when its current fact is local, and otherwise refused | unit |
| 41 | Window edge inside an interval | That interval's amount credited pro rata by time; result flagged `apportioned` | property |
| 42 | Interval missing | A gap in coverage; nothing held across it | property |
| 43 | Daily amount on a clock-change day (`day`, 23 or 25 h), with a day start of 00:00 or another (05:00) | Accepted at the day start; its interval runs to the next day start, a day start in the gap or the repeated hour placed as §2.1 says (a day start of 01:30 in Europe/London on 29 March 2026 falls in the gap and is placed at its end, 01:00Z; a reading labelled 01:30Z is refused, row 40); with a day start of 00:00, day and month results in the interval zone are exact and unflagged; rate uses the true length | property |
| 44 | Window edge inside an interval of a calendar-length series (possible for a UTC day, another zone, a day of a `month` series, a calendar day or month of a `day` series whose day start is not 00:00) | Credited pro rata by the reading's exact length; `apportioned`. A window whose edges fall on starts is not flagged, whatever its zone (a UTC day on a Europe/London `day` series in January) | property |
| 45 | tzdata change moves a boundary of a calendar-length series | Readings still on a start keep their place, their end following the new rules; a reading no longer on one is displaced: kept in views, ignored by live state and windows, result flagged `displaced`, counted; re-sent at the new start it is a new key | model |
| 46 | Host tzdata older than the source's rules | Reading at the true start refused off phase and counted until tzdata is updated; the replay is then recorded | integration |

## 10. Invariants (checked by the reference model and property tests)
1. Sequences are strictly increasing and gap-free within a generation across restarts, except for lost ranges
   recorded in the manifest (§3); only a restore starts one.
2. Every delivered position is durable, except where invariant 10 allows a loss (a lost range under §3 Recovery rule 2 or §8 Restore, or a tail truncated by rule 3); every durable position routed to a consumer is delivered to it, or
   superseded by a `resync` covering it.
3. For any consumer, fact deliveries are in position order.
4. The current view equals "highest rank per `(series, t)`" (§2.2) over all facts in the files the store holds,
   positions set aside (§8 Restore) excepted.
5. Every stored node is bitwise equal to its canonical computation over the current view of its span (nodes flagged
   `summary_stale` excepted).
6. `merge` is associative with the empty node as identity and agrees with direct computation (tolerance §5.6).
7. A window result equals direct computation over the current view at evaluation time (tolerance §5.6; results
   flagged `approximate_edges` or `summary_stale` excepted).
8. Rebuilding derived structures from log entries yields exactly the live structures.
9. Compaction, tiering and backup preserve the set of positions outside lost ranges, each with identical content;
   restore preserves the positions of the restored log (§8) outside lost ranges it records (§8 Restore, Failures and
   end) and keeps the positions set aside, except those in a set-aside segment recorded lost.
10. No operation removes a durable fact. The only losses short of a disaster are corruption of a log segment, open
    or sealed, with no other copy (§3 Recovery, §8 Corrupt block) or a listed file that the bucket has lost or
    corrupted when a restore reads it (§8 Restore, Failures and end), each recorded as a lost range and
    reported as `data_lost`, and a tail truncated by §3 Recovery rule 3 after a reboot or with the mark lost, reported as
    `possible_loss`; after a reboot with a readable mark it can hold about one mark-sync interval of acknowledged
    batches, and with the mark lost it is unbounded.
11. No reader or consumer observes a position beyond the durable mark as the writer holds it in memory.
12. No file leaves a tier unless a hash-verified copy exists in another tier.
13. A cursor or latch on disk is never ahead of the consumer's confirmation.
14. No manifest epoch lists two live files holding the same position.
15. An arriving fact is dropped as a duplicate iff it is identical (§2.4, including `changed_at` in a versioned
    series) to a fact in the history of its `(series, t)` and lacks `revise` (except `duplicates_unchecked`, §2.4);
    `revise` applies to series that are not versioned.
16. After resume, every condition true now is delivered once in `resync`, except one whose state is unknown during a
    restore (§6 Conditions), which is delivered as a transition at its first evaluation with detail if that differs
    from the latched state, or there is none.
17. For a counter, increases over adjacent windows sum to the increase over their union; for a state series, so do
    transitions (each step or transition credited once; results flagged `approximate_edges` excepted).
18. No time is credited past the credit limit (§6) or past a reading's validity: its staleness limit, or for an
    interval reading its interval (counter steps excepted).
19. For a source whose `changed_at` rises with every revision, re-submitting readings already recorded on a versioned
    series never changes the value, adapter-supplied quality or `changed_at` of any current fact while the version
    index is local, whether or not the duplicate check could run.
20. No fact of a scheduled or forecast series carries `future`, or `late` by its first clause (§2.2).
21. A window result over a scheduled or forecast series is a function of the current view and the window's bounds
    alone (tolerance §5.6; results flagged `approximate_edges` or `summary_stale` excepted); for a calendar-length
    interval series also of the tzdata version in force.
22. For an interval series, the sums over adjacent windows add up to the sum over their union (within the §5.6
    tolerance where an interval is apportioned; results flagged `approximate_edges` excepted); displaced readings
    contribute to no window.
23. On a calendar-length interval series the intervals of current readings that are neither bad quality nor displaced
    are pairwise disjoint, each running from one interval start to the next under the tzdata version in force; no
    displaced reading contributes to live state or to any window result.
24. On an observed series, a fact committed with a synchronised clock is `late` by the first clause if and only if it
    is not flagged `future` and a fact of its series with a lower position, flagged neither `future` nor
    `unsynced_clock`, has a `t` greater than its own by more than 2 × S; no fact flagged `future` or `unsynced_clock`
    changes the `late` flag of any other fact (positions set aside and lost ranges excepted).

## 11. Interfaces and configuration (outline; signatures follow in the API document)
| Interface | Contents |
|---|---|
| Ingest | `emit(reading, t_source, intent: normal | revise, changed_at)` → recorded(position) / duplicate / refused(reason); waits under back-pressure. Exports carry `changed_at` |
| Query | current, as-of P or T, history, tree level for a span, window evaluation; each result carries coverage and flags |
| Consumers | register(start), deliveries, confirm(position), latches; `resync`, `generation_changed`, `cursor` events |
| Health | counters (duplicates, unchecked, refused, regressions, outranked on arrival, evaluations unknown, back-pressure wait, lag), persisted with the checkpoint on a best-effort basis; each jump of the system clock measured against the monotonic clock, with its size; degraded/fatal states; `data_lost` for each lost range caused by corruption (§3 Recovery rule 2, §8 Corrupt block) or by a listed file lost or corrupted in the bucket (§8 Restore, Failures and end), which does not make the store read-only; `possible_loss` for each tail truncated by §3 Recovery rule 3, after a reboot or with the mark lost. Each report carries what is known of its range: positions, bytes and the commit times of the batches on either side. Query results carry no flag for a lost range: the engine knows its positions, not the series and times it held. Each restore record's set-aside range, and `restore_failed` (§8 Restore) |

Every default in this document is a configuration key, with its unit where it is defined; the decided choices D1–D7
(§12) are rules, not configuration keys. Byte-level format,
Parquet export schema, API signatures and memory figures follow in separate documents.

## 12. Decided choices
| # | Choice | Rule | Rejected alternative |
|---|---|---|---|
| D1 | NaN/∞ from a device | Recorded as bad quality, no value | Refused and counted |
| D2 | Counter decrease that is not a plausible reset | Regression: contributes 0, counted | Every decrease is a reset (one glitch can add a whole counter value) |
| D3 | Site zone change | Effective-dated; past days keep the old zone; a calendar window or query may name another zone for its whole span (§5.7) | Rebuild all history in the new zone |
| D4 | Fill pressure order | Verified sealed log leaves before current detail | Only current detail moves; log stays 90 days |
| D5 | Identical repeat whose current fact is off the machine | Recorded, counted unchecked | Ingest of that fact waits for a fetch |
| D6 | Bad-quality reading in a gauge | Ignored; previous value's hold continues up to S | Ends the previous hold (gap) |
| D7 | 1-minute level for old spans | Stays hot forever (≈ 38 MB per series-year with data every minute) | Follows raw detail to cold |

D4 and D7 are looked at again once the §13 measurements behind them exist (hot disk during log retention and tree
size, which need compaction and the tree: roadmap sections 6, 7 and 10), and may change then.

## 13. Risks to measure
| Risk | How measured | If it fails |
|---|---|---|
| Sync rate and SSD wear | Syncs/s and bytes written on a representative SSD with N unbatched 1 Hz series (N stated in the benchmark), preallocated segments, `fdatasync`, plus the timed mark sync | Enable linger (≤ 1 ms); a longer `mark_sync_interval` |
| Funnel budget ≤ 1 µs | Benchmark per-reading routing and a 24 h rolling average with cached middle | Budget excludes windowed conditions; cadence-only evaluation |
| Batch tree update cost | Backfill at a stated target rate through normal ingest, with late facts | Larger batches for re-ingest |
| Duplicate check on replay | 1 hour × 100 series replay; probes outside the recent window | Larger recent window |
| Forecast series | Revised-key index size (on hot and in each tree checkpoint), chunk overlap and duplicate probes below the latest `t`, since every key of a forecast series is revised by design | Keep fewer issues hot; larger recent window |
| Tree size | Bytes per series-year per level on real data | D7 alternative |
| Hot disk during 90-day log retention | Log plus compacted bytes per day at expected load | D4 rule; earlier move to cold |
| Restart ≤ 1 s | Start time with 100,000 series and 60 s of log after the checkpoint | Shorter checkpoint interval |
| Calendar results from facts | Time for a year of monthly results on a series just above the 60 s threshold (about 525,000 facts) | Store day nodes for such series, folded from facts in `t` order |
| Version index size | Bytes per series-year on hot, and in each tree checkpoint, for versioned series at their real reading rates | Keep index entries only for keys that were revised or are recent |
| Deep object count | Objects per year and restore time from deep | Larger seal size for sealed-log uploads |
| Bucket SHA-256 checksums | Confirm the bucket provider's S3 API rejects a mismatched single-part SHA-256 | Hash in object metadata plus sampled downloads |
| Memory per series | Resident bytes per series at the ceiling | Lower ceiling; smaller recent window |
| Overlapping chunks | Read amplification after a year of late corrections from upstream sources | Tune overlap threshold |
