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
5. **No silent loss.** Anything refused, flagged, repeated or delayed is counted and visible, except one accepted
   case: a write after an adopted recovery entry that left no readable commit block, never acknowledged or delivered,
   may go unreported (§3 Recovery rule 3, Adoption).
6. **One writer process; any number of reader processes.**

## 2. Data model
### 2.1 Series
| Field | Rule |
|---|---|
| `id` | u32, assigned once by the registry, never reused among the entries the store holds (after a restore, §8 Restore step 1), nor at or below an id floor (below) |
| `name` | `<instance>/<device>/<metric>`; a rename adds an alias, the id is unchanged; a name maps to its active series |
| `kind` | `gauge`, `counter` (cumulative), `state` (named or boolean), `text` (UTF-8, ≤ 255 bytes), `interval` (an amount over an interval of fixed or calendar length) |
| `value type` | gauge/counter/interval: `f64`, or `i64` with a declared `decimal scale` (exact); state: u16 index into declared names or bool |
| `decimal scale` | decimal series only: the number of decimal places, 0–18 (value = integer × 10^−scale); 4 unless declared. It may be **raised**, never lowered: a raise is a registry entry that applies from its position onwards. Each fact records the scale it was stored at (`format.md` A.5) and keeps it; readers rescale exactly to the current scale. A raise is refused if the largest magnitude stored so far for the series would overflow `i64` at the new scale (where that magnitude is kept is open, Q-100; a lost raise: open, Q-100) |
| `excess precision` | decimal series only: what happens to a value with more decimal places than the current scale. `round` (default): rounded half to even at the current scale, and the fact is flagged `rounded`, so the change is never silent; `refuse`: the reading is refused, naming the scale needed; `auto-raise` with a maximum scale: the scale is raised as far as needed by ordinary raise entries, each subject to the range check, and beyond the maximum or the range the value is rounded and flagged. A value given as `f64` is first converted to its shortest decimal representation, then this applies (0.1 + 0.2 becomes 0.3 at scale 4). A batch holding a raise is acknowledged after the mark sync (§3 Commit). Whether the policy may change after definition is open (Q-100) |
| `unit` | fixed for the life of the series; a change of kind, type or unit, or a lower decimal scale, is a new series; a raised scale is not (`decimal scale`) |
| `expected interval` | configuration, never learned. Default: gauge and counter 60 s; interval its interval length (calendar length: 24 h for `day`, 31 days for `month`); state and text none (valid until next reading) |
| `interval length`, `interval phase`, `interval zone`, `day start` | interval series only, all fixed for the life of the series (a change is a new series). The length is either a duration d > 0 with a phase, 0 ≤ phase < d (default 0), or a **calendar length**, `day` or `month`, with an `interval zone` (an IANA name) and no phase; `day` may declare a **day start**, a local time of day with 00:00 ≤ day start < 24:00 (default 00:00), so that a day from 05:00 to 05:00 local is one interval, and a day start on any other length is a definition error. The interval zone is independent of the site zone, is not effective-dated and is compared by canonical name after resolving IANA links; a definition naming a zone the tzdata version in force does not know is an error. For an existing series the name is resolved by the version in force each time, and the definition keeps the name as given. Where that version makes it a link, the starts follow the link's target from then on, like any change of rules (§5.4). Where that version does not know the name, the series has no interval starts: every recorded reading is displaced (§5.4), every new reading is refused off phase and counted (row 40), and health is degraded, naming the series and the zone, until a version that knows the name is in force, which undoes it. A reading's `t` is the start of its interval. Its **end** is `t + d`, or for a calendar length the next **interval start** after `t`: the starts are, in the interval zone, the instants of the day start on each date (for `month`, the start of the first day of each month, §5.7); a day start that falls in a gap is placed where the gap ends, as §5.7 places a midnight that does not exist, and one that occurs twice is placed at the earlier instant, as §5.7 places a midnight that occurs twice; a reading at the later instant is refused off phase (row 40). All starts are taken under the **tzdata version in force**, the one version that the phase check at commit, displacement (§5.4) and calendar results (§5.7) all use. Each interval thus runs from one start to the next, whatever the rule for an unusual day start. A reading carries no length: lengths are exact nanoseconds (a local day is usually 23–25 h). `t` must be an interval start (fixed length: `t ≡ phase (mod d)`, Euclidean, on UTC nanoseconds), or the reading is refused and counted; its value is the amount over `[t, end)`. Intervals of current readings that are not displaced (§5.4) therefore never overlap. A source whose periods are neither of one fixed length nor local days (from one day start to the next) or months of one zone, such as irregular billing periods, cannot be an interval series in this version, and the engine never splits an amount into finer intervals. Record on change (§2.6) does not apply to an interval series |
| `staleness limit` S | `3 × expected interval` unless set; none if no interval. An S that was set is kept when the expected interval changes |
| `record on change` | optional; see §2.6 |
| `wrap_at`, `reset_ceiling` | counters only; §5.5. `reset_ceiling` default 10 % |
| `lateness bound` | default 5 min; §2.2 |
| `versioned` | optional, for sources that revise readings they have already published; fixed for the life of the series (a change is a new series, and history may be re-ingested with `changed_at`). Every reading of a versioned series carries `changed_at` (§2.2); readings of other series never do |
| `changed_at kind` | versioned series only: `time` (UTC nanoseconds) or `version` (an opaque counter); fixed for the life of the series; used to type exports, and must be `time` for a forecast series |
| `retention class` | `long-term` (default) or `diagnostic` with a retention period (default 30 days), for series kept mainly for short-term diagnosis, such as a machine's own resource metrics. It decides only where raw detail is kept (§8 Retention class), never which facts exist. A change of class is a registry entry that takes effect at its position, not by event time; it moves no detail back |
| `timing` | `observed` (default), `scheduled` or `forecast`; fixed for the life of the series. **Scheduled:** values known in advance that take effect at their `t`, such as published prices; a scheduled source that revises its values must be versioned. **Forecast:** values about future times, re-issued over time; the series must be versioned with `changed_at kind` `time`, `changed_at` being the issue time and `t` the valid time. The engine does not report a silent forecast source: its adapter emits an observed series with an expected interval on each successful fetch, to which stale-for applies |

Series definitions, parameter changes (with an effective event time, except a change of retention class, which
takes effect at its position), aliases, retirements, scale raises, site-zone entries and tzdata entries (§5.7) are log entries. A parameter other than the retention class
applies to readings with `t` at or after its effective time. A definition entry always has a lower
position than the series' first reading (the same batch is allowed). Registry entries are also compacted into a
registry file that stays on the hot tier (tree checkpoints carry a copy, §8). Site-zone entries and tzdata entries
(§5.7) and routing changes (§4) are registry entries in this sense: the registry file holds each with its position,
so they are in force wherever the registry is, whether or not the log segment that holds them is local. Recovery
entries (§3) are compacted into it too, each with its position.

After a lost range (§3) only the entries of the kept log apply; a lost definition, parameter change, alias,
retirement, scale raise, routing change or zone entry is never appended again by the engine and is issued again through the
normal calls; a definition issued again is a new series. A kept fact whose series has no definition in the kept log
is an **orphaned fact**: kept in the log and through compaction, outside views, trees, indexes, routing and
delivery, counted per id (`orphaned_facts`) and exported for re-ingest. New ids are assigned above every id floor,
every id defined in the kept log and every series id in a kept fact. An **id floor** is the highest id known, at
the start that writes the record or entry carrying it, to the mark (highest series id assigned), the registry
file, the local checkpoint, the kept log and any definition still readable in the torn tail. On a disk that loses
synced data, an id defined only in the lost bytes can be assigned again. This paragraph covers lost ranges of the
log; when a log segment whose facts are already compacted is lost, which definitions count as lost (one may still be
held by the registry file or a chunk) is open, as is how derived files are brought back to a function of the kept
log (Q-33).

**Retirement** is effective at `t_r`: facts with `t < t_r` are still accepted; facts with `t ≥ t_r` are refused
and counted. The old id stays reachable by id.

### 2.2 Log entries, positions and facts
- Every log entry (fact, definition, parameter, alias, retirement, scale raise, site zone, tzdata, routing change, recovery
  entry, §3) has a **position** `(generation: u32, sequence: u64)`, ordered lexicographically. Every generation starts
  at sequence 1; `(g, 0)` means "before the first entry of generation g" and `(0, 0)` means none. Sequence is strictly
  increasing and gap-free within a generation across restarts, except for lost ranges, each recorded in the manifest
  or described by a recovery entry (§3); a restore starts a new generation (§8). Consumers see only the positions of
  facts routed to them, so gaps between delivered positions are normal.
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
  `host_time` (`t_source = host`), `unsynced_clock` (§2.5), `rounded` (a decimal value rounded to the series' scale,
  §2.1 `excess precision`). For a reading of an observed interval series, its end
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
  by the engine are not part of identity. Values compare bitwise (`0.0` ≠ `-0.0`); a decimal value compares as
  stored (a replay across a scale raise is open, Q-101). Duplicates are not recorded and
  consume no position; they are counted per series (`duplicates_dropped`). Adapters may replay freely: a replay that the
  check can run on never reverts a correction (for the unchecked case, see below).
- In a versioned series identity also includes `changed_at`. A fact whose `changed_at` is above that of every fact
  in its key's history is never a duplicate, even when its value returns to an earlier one.
- A fact submitted with `revise` intent bypasses the check (deliberate return to an earlier value). On a versioned
  series `revise` is refused and counted; a return to an earlier value there is a fact with a higher `changed_at`.
- A reading recorded without a value (§2.5) is compared as "no value", with its adapter-supplied quality.
- Identity includes `t`: the same value one nanosecond later is a different fact and is recorded.
- **Revised-key index** (hot tier, never moves; carried by each tree checkpoint, §8): for every `(series, t)` with
  more than one recorded fact, the exact value, adapter-supplied quality and `changed_at` of each, and whether the
  entry is incomplete (last item). For any other key, history holds at most the current fact, which is compared
  directly.
- **Version index** (hot tier, never moves; carried by each tree checkpoint, §8): for every key of a versioned series,
  its highest recorded `changed_at`. The rank of an arriving fact is therefore known whenever the index is local.
- **Fast path:** a fact with `t` later than the series' latest recorded `t` (over all recorded facts, `future` ones
  included; not the lateness reference, §2.2) cannot be a duplicate; no lookup. Other facts are checked against the
  in-memory recent window (§5.1) before any index probe.
- The check compares an arriving fact with the local facts of its key and with the key's entry in the revised-key
  index; a match with either is a duplicate, whatever tier holds the facts. Only where there is no match, and a fact
  of the key is on no local tier and not in the index, is the arriving fact recorded and counted
  `duplicates_unchecked` (§12 D5), unless its `t` is a displaced interval start (§5.4), where it is refused. That is
  the case for a key with a single fact that has moved out (§8 fill pressure or retention class), and for a key whose
  entry is **incomplete**: an entry started by an unchecked fact, or rebuilt during a restore (§8 Restore steps 1 and
  2) while an earlier fact of its key is not local, holds only the facts recorded since, until the detail is fetched
  and completes it. The check never reads off-machine data. In a versioned series an arriving fact whose `changed_at`
  matches none in its key's history is not a duplicate and needs no check; one whose `changed_at` equals that of a
  fact off every local tier and not in the index is recorded, counted `duplicates_unchecked` and takes the tie by
  position. An unchecked repeat therefore never reverts a correction with a higher `changed_at` in a versioned series.
  In any other series it can revert a correction whose key's entry is incomplete: after a restore, for example, an
  entry rebuilt in step 1 or 2 while the key's earlier fact is in deep lacks that fact, so a replay of the earlier
  value is recorded unchecked and becomes current; completing the entry later does not demote it. So a source that
  revises readings must be declared versioned.

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
| Commit | Write batch, sync, update the durable mark, then (and only then) acknowledge adapters, update tree and live state, and deliver. A batch holding any entry that is not a fact is acknowledged and delivered only after the mark file has been synced past it, so a truncated tail holds no acknowledged definition, parameter, alias, retirement, scale raise, routing or zone entry. For such a batch the writer forces the mark sync as soon as the batch's own sync has returned, rather than waiting for the timed `mark_sync_interval` (Durable mark). Batches are acknowledged and delivered in commit order, so a later batch holding only facts waits for that one mark sync too, not for up to an interval, and every entry still takes effect at its position. |
| Durable mark | After each successful sync the writer updates the mark file in place by positional writes (`format.md` A.10): segment id, byte offset, last position, boot id, highest series id assigned. The file holds two checksummed slots, so a torn update falls back to the other slot. The writer writes a batch only after the previous batch's sync has returned and the mark has moved. The mark file itself is synced on a timer (`mark_sync_interval`, default 1 s), at once after a batch holding an entry that is not a fact (Commit), at clean shutdown and by recovery, so after a power loss the mark on disk lags acknowledged batches by about one interval. It is updated in place and never replaced while it exists. Readers read log segments only up to the mark. No cursor file or derived file is written naming a position above the last synced mark, nor any manifest record except a lost-range record written by a start (§3 Order of a start, step 4), which names only positions in segments that start has synced; a write that would name one waits for the next mark sync or forces one. |
| Sync primitive | "Sync" means the bytes are on stable storage with the device cache flushed. A failed sync is never retried, on any handle: the file's contents since its last successful sync are unknown. Linux: `fdatasync` or `fsync` on the file, `fsync` on the directory after a create or rename. macOS: `F_FULLFSYNC` for every sync named in this document (`fsync` alone does not flush the device), also for directories. Windows: `FlushFileBuffers` on the file; there is no directory sync, so a create or rename is made durable by flushing the created or renamed file (`format.md` A.13); a file that is open cannot always be replaced by rename, which is why the mark is updated in place and a manifest record is repaired in place (§7 Manifest), a rename being the fallback. The mark is synced like any other file; no file is memory-mapped. A platform or file system that cannot give these guarantees is refused at open, except Windows, which is a development platform like macOS (`design.md` Budgets): it builds, runs and is tested, runs with the mark synced at every commit, and is not held to these durability guarantees or to the budgets. (A platform without a boot identifier syncs the mark after every update, `format.md` A.10; that is a separate case.) These are requirements; the roadmap 2.2 spike measured their costs on macOS only, and roadmap 4.8 measures them on Linux and Windows. |
| Group commit | While a sync is in flight, arriving readings queue and form the next batch. A batch is synced at once if no log sync is in flight and at least `min_sync_interval` (default 10 ms; 0 disables the bound) has passed since the previous log sync started; otherwise it waits until both hold, taking in whatever arrives meanwhile. So the first reading after a quiet period has no added delay, and log syncs run at most 1/`min_sync_interval` per second (100 at the default). Mark syncs (Durable mark), timed or forced, are separate and not bounded by it. |
| Segments | Created ahead of use and zero-filled (64 MiB, `format.md` A.4). If creating one fails for lack of space, the partial file is deleted, health is critical, ingest waits, and the creation is retried when a free-space check passes; lack of space at a batch write or sync is a write failure (below). The directory is synced after creating, sealing or renaming a file. |
| Sealing | At 64 MiB or 5 minutes (both configurable), whichever first; an empty segment is not sealed. Sealed segments are immutable and eligible for backup. Compaction never removes a log segment (§8). |
| Recovery | Below this table. |
| Write or sync failure | On the log, manifest, mark, cursor or registry files: stop accepting writes, mark read-only, fatal health; a failed sync is never retried, on any handle; recovery by restart. After a failed sync the batch's fate is unknown: a restart on the same boot truncates it; after a reboot recovery may keep it (row 7a). It was never acknowledged, so adapters may replay it, which the duplicate rule absorbs. A failed compaction, tiering or tree write abandons that job, deletes its partial output and degrades health. |
| Disk space | Low mark: free space below 5 % of the filesystem or 2 GiB, whichever is larger → compaction pauses, health degraded. Tiering and eviction, which free space, continue at every mark. Critical: below 512 MiB free → ingest waits (back-pressure). Reads continue. Space is never freed by deleting facts. |
| Back-pressure | Ingest is a bounded queue (default 65 536 readings); a full queue makes adapter emits wait; the wait is measured. |
| Single writer | An exclusive operating-system lock on the file `LOCK` in the store directory (`format.md` A.12), held for the life of the writer and released when the process ends; readers never take it. Network file systems are not supported. |

**Recovery.** Every unsealed segment is scanned to its end. A missing or unreadable mark (no valid slot, `format.md`
A.10) is treated as a different boot. The rules apply in this order:
1. **Same boot id as the mark:** truncate everything after the mark. It may have reached the disk, but the mark did
   not cover it, so it was never acknowledged or delivered; adapters replay. A batch is written only after the
   previous batch's sync and mark update, so beyond a current mark lies at most one complete batch. If two or more
   complete batches lie beyond the mark on the same boot, the mark is **stale** (the store's file system was
   unmounted or detached within the boot and the page cache lost): rule 3 applies, as for a different boot. Accepted
   limit: when exactly one batch was committed since the mark's last sync, a remount within the boot truncates it
   without a report. A store on media that may be remounted within a boot closes this by syncing the mark at every
   commit (`api.md` `log.mark_sync`).
2. **Corruption:** an invalid region that lies below a valid mark (a valid slot, `format.md` A.10) of any boot, or that is
   followed by a complete batch, was synced (the mark is written only after a sync returns, and a batch only after
   the previous batch's sync), so it is corruption, not a torn tail. Later complete batches are kept and nothing is
   overwritten. The byte and position range, from the last valid batch to the next valid batch or, at the tail, to
   the mark's last position, is reported as `data_lost` when found and recorded in the manifest as a **lost range**
   by step 4 below; neither makes the store read-only. Its positions are not reused. A lost range is a reset point
   for consumers (§4 Resync).
3. **Different boot id** (the machine rebooted, the page cache is gone), or the mark missing, unreadable or stale
   (rule 1): any remaining invalid tail is torn. Let K be the **last kept position**: the higher of the last kept
   batch's last position and the end of a lost range found at the tail under rule 2. Let F = 1 + the highest
   sequence of the current generation among K and every position named by a local cursor file, manifest record or
   derived file. Recovery writes a **recovery entry** at F, alone in its batch, at the next write position (after
   the kept bytes or after the lost region at the tail; in a new segment when the open one is full): `after` = K, the
   boot id, the tail's length in bytes up to the last non-zero byte, the readable commit blocks in it, the mark's state, an id floor (§2.1) and the consumers whose cursor file was missing or unreadable.
   Positions between K and F, if any, are a lost range whose cause is a truncated tail. Positions above K may have
   been acknowledged and delivered and are assigned again (F to the entry, then from F + 1), so a recovery entry is
   a reset point for every consumer (§4 Resync). A consumer whose cursor file was missing or unreadable is treated
   as having no cursor. **Adoption:** if the last kept batch is a recovery entry, a readable mark does not cover it,
   and the bytes after it show no later write (none lies beyond the end of the tail it reported, and no readable
   commit block among them has a commit time at or after the entry's), no recovery since has resumed: that entry is
   adopted, whether or not the mark is readable, no second entry is written, and the invalid bytes after it are the
   rest of the tail it reported (a mark update torn by a crash in step 6 is no new loss). Otherwise a new entry
   is written, and the bytes after the old one are a newly truncated tail that the new entry reports as
   `possible_loss`, so a doubtful case costs a second report rather than a reused position without a reset point.
   Accepted limit: a write after the entry that left no readable commit block inside the reported tail cannot be
   detected, and the entry is adopted. Such a batch was never synced, so it was never acknowledged or delivered (a
   synced batch survives as a complete batch, and the entry is then not the last kept batch), and the consumers that
   had confirmed the entry's reset point received nothing above it from that write; at worst one `possible_loss` goes
   unreported. On Linux a page whose writeback failed is marked clean and kept in the page cache, so if the mark file
   is deleted, or damaged in both slots, on the same boot after a failed sync, this rule keeps the batch whose sync
   failed although the disk may not hold it; how recovery tells this apart is open (Q-87). The first start of a new store (no mark file and an empty log) writes no entry and creates the mark.

**Order of a start.** Every start runs these steps; none depends on state left by an earlier attempt.
1. Sync the log directory. Read the manifest (§7) and the mark; scan every unsealed segment to its end.
2. Rule 1: truncate after the mark; sync every segment holding a lost range found (rule 2) and the directory, with
   step 3's rule for a failed sync; go to step 4.
3. Sync the kept segments and the directory. If that sync fails the engine stays read-only, leaves the old mark and
   the manifest as they are and writes nothing; what it found is reported in health as found and recorded by the
   next start that can write.
4. Repair the manifest's tail (§7 Manifest): drop a torn last record and truncate the file after the last valid
   record, write the last valid record's bytes again in place, and sync. Then, for every lost range found (rule 2
   regions, recovery-entry gaps) none of whose positions a lost-range record holds, append a lost-range record; for
   one that overlaps a recorded loss and extends it, upward or downward, append a record that extends that loss and
   names its identity (one loss, counted once). A loss's **identity** is the first position of its range when it
   was first reported, so a loss reported before any record holds it can be acknowledged at once, and the record,
   when written, carries that identity. Sync the
   manifest.
5. Rule 3: write or adopt the recovery entry and sync; zero the rest of the torn tail and sync.
6. Rule 3: write the mark (current boot id, last position F, highest series id assigned) and sync the
   mark file and the directory. An existing mark file is updated in place, whatever its content (except a slot of unknown version,
   `format.md` A.10); a new file is created only when none exists. Then record the entry's gap, if any, as in step 4.
7. Resume. Each consumer's deliveries are computed from its cursor file and the log (§4 Resync, §6).

A failed sync in any step leaves the engine read-only (§3 Write or sync failure); the next start begins at step 1.

## 4. Delivery (funnels)
| Rule | |
|---|---|
| Funnel | Declared in configuration: series selection (ids or name patterns), optional operator (§6), condition, consumer. |
| Routing | Array indexed by series id → funnels, built at start from the registry (§2.1); a funnel declared in configuration that the registry does not hold, or holds differently, is first written as a routing change, so the routes in force are always those the log names. Changes are log entries, kept in the registry file with their positions, and take effect at their position; catch-up applies the routes in force at each position. Patterns are re-evaluated when a definition entry commits. A consumer declares the series it may emit (`emits`, ids or name patterns, default none), recorded with its routing changes, and emits through its own handle, which refuses and counts a reading on any other series. Compilation, at start and on every routing change, draws an edge from consumer A to consumer B where a series A may emit can be selected by a funnel of B: two patterns that one name could match, an id against a pattern through any name of its series, or the same id. A change that closes a cycle, a consumer's own included, is refused, naming the cycle. The test is on the selections, whether or not a matching series exists yet, so a definition entry cannot close a cycle; an alias that would is refused. Readings an application sends through the store's own `emit` are outside the check. |
| Concurrency | Delivery and consumer code never run on the writer thread. A consumer emits readings through its own handle (Routing), with the non-blocking `try_emit` or from its own task. |
| Consumer state | Bounded queue (default 4 096); a durable cursor (last confirmed position) and one latch per condition funnel (§6). Written to a per-consumer file, synced at most every 1 s, on clean shutdown and when a `resync` carrying `forget_above` is confirmed; never ahead of the consumer's confirmation. |
| New consumer | Declares `start: now | position | time` (default now). |
| Order | A consumer sees fact deliveries in position order (invariant 3). A **condition delivery**, from an evaluation on a routed fact or on the timer, is identified by `(funnel, sequence)`; it carries the last committed position at evaluation (on a routed fact, that fact's) as data, not as identity. The sequence is per funnel, kept in memory, and starts at 1 at every engine start; within one run it continues when a funnel is removed and added again under the same id. A consumer that resumes at an engine start receives no condition delivery before the `resync` that ends its resume at that start (§6 Restart and resume; §8 Restore step 2); a funnel added at run time follows §6 Latch. |
| At-least-once | After a restart a consumer resumes after its cursor and may see a delivery again. Consumers de-duplicate fact deliveries by position; commands carry ids. A condition delivery is never delivered twice: one not confirmed before a stop is not sent again, and the `resync` that ends the next resume covers it. Its identity holds only until the consumer's next `resync`, on which the consumer forgets the condition deliveries it has seen; a consumer that derives command ids from it adds something of its own. A `resync` may carry `forget_above` = X: the consumer must drop every position above X from its de-duplication memory, its own last confirmation included, because such a position can now name a different fact (§3 Recovery rule 3). It confirms that `resync` only when the forgetting is durable wherever that memory is kept. Records it keeps outside the engine at those positions, and series ids it first saw there, are copies of facts the store may no longer hold; the engine never asks for them to be deleted and does not protect them. A `resync` without `forget_above` requires no forgetting of positions. |
| Overflow | A full queue switches the consumer to catch-up: it reads the log from after the last position already enqueued for it in the current session (not from its confirmed cursor, so order holds within a session) up to the durable mark, then re-attaches to live at the next position under the writer's delivery lock (no gap, no repeat). It never blocks the log or other consumers. Time behind is measured. Catch-up applies reset points as a resume does (Resync). While a consumer catches up, its condition funnels are still evaluated, on live facts and on the timer, but send nothing, and the last state emitted to it does not change; a condition delivery that finds the queue full is not queued and switches the consumer to catch-up in the same way. On re-attaching, before any live delivery, each condition funnel whose state differs from the last state emitted to the consumer (on change: by more than the dead-band) sends it as one condition delivery; a `resync` delivered during the catch-up counts as emitting the states it carries (none, unless it ends a resume), so the same transition is not sent twice. Transitions in between are not delivered and are counted per funnel (`transitions_not_delivered`): a condition true only during the catch-up is not seen, as one true only during downtime is not (Timers). Catch-up reads from hot and cold, never from deep: where the next entry it needs is on no local tier, it ends, the consumer receives `resync`, which supersedes the positions passed over, and it re-attaches at the next position. |
| Replay | Fact deliveries produced in catch-up are flagged `replayed` and keep their original times. They are for record-keeping and are never fed to a condition operator a second time (§6). A consumer that acts on devices must not act on old deliveries (default: `received_at` more than 60 s before now; event time does not count). |
| Resync | The `resync` that ends a resume (§6 Restart and resume; after `generation_changed`, §8) carries, per funnel, the current live state and condition state evaluated now; any other `resync` carries no funnel state. Sent on resume after downtime (§6), on `generation_changed` (§8), when the cursor is older than the oldest local log segment, when catch-up after overflow needs an entry that is on no local tier (Overflow; `forget_above` = the last position enqueued for the consumer, id = the mark's last position), and at every **reset point**. Reset points are: a recovery entry (`forget_above` = its `after`, id = its position), and a lost range not described by a recovery entry (`forget_above` = the position before it, id = its last position, since it may have held a recovery entry). Consecutive reset points with nothing routed to the consumer between them are delivered as one, with the lowest `forget_above` and the highest id. A reset point applies to a consumer whose position (its cursor on resume, the last position enqueued for it in a session or in catch-up) is below the reset point's id; it is delivered in position order, carrying `forget_above`, and nothing further is delivered to that consumer, live or by catch-up, until it has confirmed it and its cursor file, naming the id, is synced. A consumer that never confirms receives nothing more; the wait is measured as time behind. A cursor at or above the id is past the reset point. When a lost range is recorded at run time, consumers past it receive a plain `resync`. A reset point at the last position is the same delivery as the `resync` that ends the resume. A consumer whose cursor is older than the oldest local log segment receives no replay and one `resync` with `forget_above` = its cursor and id = the mark's last position. A consumer with no cursor file receives, before its first delivery, a `resync` with `forget_above` = the position before its start: the mark's last position for `start: now`, the position before the first fact at or after T for `start: time`, and P for `start: position` P, after which reset points above P apply as to a cursor at P. How P is checked against a truncation or restore that happened after it was read is open (Q-46). With no latch to compare, as after a restore, the `resync` that ends a resume delivers each condition's present state, true or false, as a transition. During a restore a condition or live state may be **unknown** (§2.3, §6 Conditions): `resync` reports it so for that funnel, which is no transition and leaves the latch as it is. It is not flagged `replayed`; acting on it is acting on the present. |
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
| Local calendar day/month | Yes, for spans whose edges fall on 15-minute boundaries (§5.7), for a series that stores the 15-minute level (density rule; interval series §5.4), whether or not a given quarter-hour has data, assembled from 15-minute nodes in the site zone in force; an input to calendar results, not the result (§5.7) | per-series calendar file on hot |

- **Density rule:** a level is stored for a series only where its nodes summarise many facts: the 1-minute
  level only for series whose definition gives an expected interval of 5 s or shorter, the 15-minute level only
  for 60 s or shorter. A sparser series answers those levels from its facts directly, so a stored level is never
  larger than the data it summarises. Such a series stores no calendar nodes either: its calendar results are
  window evaluations over its facts (§5.7). The levels a series stores are therefore fixed for its life: a later
  change of the expected interval (§2.1) does not move them. A series whose rate changes for good is retired and
  defined anew to change its levels.
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
(§2.4) when the check finds it among the local facts of its key or in the revised-key index; any other reading there,
a revision included, or a replay whose match is on no local tier and not in the index, is refused off its phase
(row 40), never recorded unchecked. A tzdata change invalidates
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
  Where a series stores the 15-minute level and not the 1-minute level, its 1-minute nodes are computed for the
  merge and not kept.
  Series with a decimal scale sum in exact `i128`. Results assembled in other orders (windows, coarse queries) are
  compared with direct computation to a relative tolerance of 1e-9 (invariants 6, 7).
- Each node carries a version incremented on change. An outranked fact (§2.3) changes no node.
- If the facts of an affected minute are on no local tier, the fact is still recorded, the node is flagged
  `summary_stale` and recomputed when the detail is fetched (§8). Chunk indexes always stay hot (after a restore, from
  step 1, §8), so the engine knows whether a minute has facts off the machine.

### 5.7 Calendar
- Facts and the UTC levels are in UTC; a zone only defines calendar days and months. The site zone is a log entry with
  an effective time (a registry entry, §2.1); of two entries with the same effective time the later in the log holds.
  The store descriptor written at creation (`format.md` A.12) records the first site zone and tzdata version; it
  counts as the first site-zone and tzdata entries, in force from the beginning of time, so positions start with the
  application's entries. The **local date** at an instant is its date in the zone
  in force at that instant. The local day of date D starts at the first instant whose local date is D or later and
  ends where the day of D + 1 starts; a month runs from the start of its first day to the start of the next month's
  first day. Local days therefore tile the time line whatever the entries, and months tile with their days. Away from
  a change the start is the local midnight (below for one that does not exist or occurs twice). Across a change to a
  zone that is behind, the day in progress runs on until the new zone reaches the next date (up to about 50 hours);
  across a change to a zone that is ahead, the day ends at the effective time if the new zone is already in a later
  date, and a date can be empty. "Today" is the latest date whose day has started. A zone change rebuilds calendar
  nodes from the start, taken under the old zone, of the local day that contains its effective time, so that day's
  node and the node of the month containing it are rebuilt too (§12 D3); earlier days keep the old zone. A day or
  month that contains a zone change is taken in no single zone, so its stored node is no input to a named-zone result.
  A site-zone entry, the latest or an earlier one, naming a zone that the tzdata version in force does not know is
  handled as an unknown interval zone is (§2.1): it gives no local dates, so the span in which it is in force belongs
  to no local day (the one exception to tiling), and health is degraded, naming the zone, until a version that knows
  it is in force, which undoes it; a new site-zone entry naming such a zone is refused.
- The **tzdata version in force** is the one named by the latest **tzdata entry** in the log (a registry entry,
  §2.1), or before the first, by the store descriptor. The writer reads the host's tzdata release through the
  environment (`api.md`) at open, about once a minute and at each local midnight. A writer whose release is not the
  version in force writes a tzdata entry naming its own, at open before it accepts ingest and later without a
  restart, whether its release is newer or older; the change is logged. The entry takes effect at its position, for
  every derived value at once; no as-of view brings an earlier version back. It rebuilds the calendar days and months
  whose spans or zones differ, moves interval starts (§2.1, §5.4) and is reported in health with both versions and
  the days rebuilt. **Readers never refuse a tzdata release:** a reader process, or a restore drill, computes calendar
  results with its own release from the UTC data, using a stored calendar node only where the node's recorded
  version is its own release, and otherwise evaluating from the 15-minute nodes and the edges, as for a span off
  15-minute boundaries (below). Which rules a writer that cannot write (read-only after a failed sync, §3) uses, and
  how a reader of another release gives the values other than calendar results that depend on tzdata, are open
  (Q-93). The version is also recorded per calendar file; the writer rebuilds a file of another version.
- A calendar window or query may name an IANA zone instead; its days and months are then taken in that zone over
  the whole span, with no effective dating. Its result is a window evaluation over the UTC data like any other. A
  stored calendar node is an input to it only when every local day the node covers was taken in the named zone
  (zones compared by canonical name, after resolving IANA links; so not a month spanning a site-zone change) and
  the node's recorded tzdata version is the release of the process evaluating it (the writer's is the version in
  force); elsewhere the result is evaluated as for a span off
  15-minute boundaries (below).
- A calendar day or month **result** is a window evaluation (§6) over its span, edge stretches included. Stored day
  nodes are an input to that evaluation, not the result.
- A local day usually has 92–100 quarter-hours. A day whose local midnight does not exist starts at its first instant.
  A day whose local midnight occurs twice starts at the earlier instant, so the repeated time belongs to the new day
  (America/Havana, 2 November 2025: the day starts at 04:00Z and has 25 hours). Because results are window
  evaluations, the rule also covers a skipped date (an empty span) and historical offsets that are not multiples of 15
  minutes.
- A calendar node is stored only for a span whose edges both fall on 15-minute boundaries, for a series that stores
  the 15-minute level (§5.1), whether or not a given quarter-hour has data. For any other day or month none is stored; its result is evaluated from the stored nodes
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
| Aggregates | avg (gauge: integral ÷ covered seconds), sum, integral, min, max, count, increase, rate (increase ÷ elapsed seconds between the readings that bound it; the result carries coverage and is flagged when a step it includes spans more than S, in force at the step's earlier reading; empty when no step falls in the window), last, time-in-state. With no reading in the window but a valid held value (not for interval series): `last`, avg, integral and time-in-state use it; `min`, `max`, `count`, `sum` and rate are empty; increase counts only a step credited to the window (Edge stretches). **Interval series:** sum is the amount in the window; rate is sum ÷ covered seconds; count, min, max and last are over the readings whose `t` lies in the window, each with its whole amount, never pro rata: they describe the readings that start in the window, so each reading is counted in exactly one of adjacent windows. Where a window edge lies inside the interval of a current reading that is neither bad quality nor displaced, their result is flagged `apportioned`, as sum is: the readings do not line up with the window; avg, integral, increase and time-in-state are refused. Edge stretches do not apply: a window edge inside an interval credits that interval's amount pro rata, amount × overlap ÷ length (end − `t`, §2.1) computed in f64, and the result is flagged `apportioned`; sums of whole intervals stay exact for a decimal scale, and apportioned results are compared within the §5.6 tolerance. An edge lookup reads the one reading whose interval can contain the edge: the reading at the interval start at or before it (fixed length: at most d back; calendar length: the start of the interval containing the edge). On an observed interval series a reading enters live state and windows only once its end ≤ `now`. A displaced reading (§5.4) is ignored and the result flagged `displaced`. |
| Windows | Rolling, `[now + a, now + b)` with a < b of either sign, so that a window may lie in the future, and calendar (since local midnight, this month, or relative to the current one, such as the next local day or month) in the site zone, or in an IANA zone the window names (§5.7) ("since local midnight" and "this month" end at `now`; a named or relative day or month ends at its end); a window wholly after `now` on an observed series is empty, with zero coverage; a calendar result is a window over its span (§5.7). Every window, rolling or calendar, is evaluated in event time against `now`: its bounds are event times, and no allowance is made for a jump of the system clock. Health reports such jumps (§11). |
| `now` | The window clock: an engine UTC clock that never runs backwards: max(system clock, previous value), advanced by the monotonic clock. |
| Evaluation | Whole stored nodes in the middle; at each edge, the partial minute is computed from raw facts (the recent window when the edge lies within it, the edge chunk otherwise). Exact to the nanosecond. Nodes whose span extends past the credit limit are never used whole. |
| Edge stretches | Leading: from the window start to the first reading inside, credited with the last reading before the start if within its S. Whatever S, the counter step and the state transition from the last reading before the start to the first reading inside are credited to this window. Trailing: from the last reading to min(window end, credit limit, last.t + S). Time is never credited past the **credit limit**: `now` for an observed series, the window end for a scheduled or forecast series, whose values are known in advance. |
| Old spans | A window never reads off-machine data. Where an edge's raw detail is not local (moved under §8, or during a restore held by a listed file not yet local, §8 Restore), the edge moves inward to the nearest node boundary of the finest level stored for the series there, or, where it stores none, to the nearest instant from which all its detail to the other edge is local. The result gives the span evaluated (`from`, `to`) and is flagged `approximate_edges`. Where no span remains the result is the error `detail_not_local`, never an empty result. |
| Cache | The merged middle is cached with the versions of its nodes and invalidated when any changes (late data included). |
| Cadence | A windowed funnel is evaluated on each routed fact that is not outranked (§4) and on a timer every 1 s (configurable per funnel). A fact of a scheduled series is fed to a condition, or to an operator that is not windowed, only once `t ≤ now` (a windowed operator sees it at commit), and a condition funnel over a scheduled series is also evaluated on the timer, so a value taking effect is seen. A condition over a forecast series needs a windowed operator; any other is rejected when funnels are compiled. |
| Conditions | Threshold with a separate clear level, on change (optional dead-band), held for N seconds, stale for N seconds. During a restore (§8 Restore step 2), an evaluation with no local detail (the error `detail_not_local`, Old spans, or live state unknown, §2.3) makes the state **unknown**, not false: the evaluation is skipped and counted per funnel (`evaluations_unknown`), the condition keeps its latched state, `resync` reports "unknown" for that funnel (§4), and the first evaluation with detail is delivered as a transition if it differs from the latched state, or there is none (open: a state that stays unknown, Q-84). |
| Late data | A trigger acts on what is known when it is evaluated and never fires retroactively. |
| Latch | Each condition funnel persists, with its consumer's cursor, the last emitted state (on change: the last emitted value) and the position at which it was emitted; a confirmed condition delivery sets it, and so does a confirmed `resync`, for a true or false state it carries, a transition included (an unknown state leaves the latch as it is). A latch belongs to one incarnation of a funnel: removing the funnel drops it, and a funnel added at run time, also under an id used before, has none, sends no `resync` and starts false (on change: with no reference value). It is evaluated at its cadence from its entry's position, so a condition already true is delivered as a transition at its first evaluation, and an on-change funnel delivers its first value. |
| Restart and resume | The operator is rebuilt from the tree and recent window as of now; it is never replayed. Facts after the cursor are delivered flagged `replayed`, not fed to the operator; at each reset point above the cursor the replay pauses for a `resync` carrying `forget_above` (§4 Resync). The funnel then sends `resync` with the condition state now; if that differs from the latch it is a transition and is acted on (an unknown state, Conditions, is not). A condition that became true while the consumer was down is therefore delivered, never suppressed. **State now:** a threshold is true when the operator value now is above its activation level, false when below its clear level, and between them, or with no value, keeps the latched state (false with no latch); the crossing it came from is not looked for. Held-for is true when the value was above throughout the last N, judged on the facts as of now; stale-for follows Q-16 (open). On change gives the value now, a transition when there is no latch or it differs from the latched value by more than the dead-band. A threshold transition emitted but not confirmed before a stop, and any threshold after a restore, therefore resumes false between its levels until the value next leaves the band. |
| Multi-series | Conditions over several series are composed by the consumer in the first version. |

## 7. Compaction and files
- Compaction rearranges facts from whole sealed log segments into per-series chunks. It is **lossless**: every fact,
  including revisions, with its position and `received_at`. Facts are identified by position. Compaction takes
  sealed log segments in position order. A segment whose positions are at or below the last position held by a live
  compacted file is compacted and is not compacted again, except to replace a quarantined file (§8 Corrupt block);
  after a restore the same test runs on the files the restore record lists.
- Current facts and revisions go to **separate chunks**. A revision chunk holds only facts outranked by a fact within
  the same compaction's inputs, so the current view never needs revision chunks. A current chunk may hold facts
  outranked later or by a fact in another file; readers resolve the current view across current chunks by rank.
- Procedure: write new file → verify (fact count, per-series checksum of facts, each chunk's summary equals the
  summary recomputed from the input facts assigned to it) → publish.
- **Manifest:** append-only, checksummed records, each with an epoch. One record may add files, retire files and
  move files between tiers **atomically**. An epoch never lists two live compacted files holding the same position. A log segment holds the same positions as
  the chunks compacted from it until it leaves (§8). Tier
  moves keep the file's identity and change its location; readers resolve locations through the manifest.
  A record is appended only after the previous record's sync has returned. At every start the writer writes the
  bytes of the last valid record again in place and syncs, so that a record kept from the page cache after a failed
  sync is on disk before a successor is appended (§3 Order of a start, step 4). On reading, an invalid record is passed by searching forward, at
  8-byte boundaries, for a valid record with a higher epoch. An invalid record followed by a valid one was synced
  and is **corrupt**. An invalid last record is a **torn tail**: the writer drops it at start and truncates the
  file after the last valid record (§3 Order of a start, step 4). So that dropping is always safe, the last record is neither uploaded nor acted
  on irreversibly (no file it retires, moves or quarantines is removed) until a later record has been synced; the
  writer appends an empty record when it needs one, and at once after a restore record. What a dropped record
  described is found again: an unlisted compaction output is discarded (§9 row 10), an unlisted sealed log segment
  is listed again, a lost range is found again by the scan or the scrub, an acknowledgement is given again. New
  epochs continue above every epoch named by the last valid record and by any lease file. A corrupt record is
  fetched from deep (by generation and epoch) or from the mirror path, verified, written again in place over the
  corrupt bytes and synced; where that cannot be done, the manifest is rewritten to a new file and renamed while no
  handle holds the old one open (§3 Sync primitive). Every file is self-describing: its header and trailer carry everything a publish record says
  about it (`format.md` A.14). **If no copy exists** (the mirror path holds none and deep has answered that it holds
  none, or none is configured), the writer **rebuilds the manifest** from the local files by itself, with the
  procedure of the operator call `rebuild_manifest` (roadmap 6.3), and continues; the files are kept, so no fact is
  lost. Steps 1–3 below are ruled only where the local files settle the case; the rest is open (Q-103). The rebuild:
  1. Lists every valid local file (hot and cold paths; nothing under `quarantine/` is live) and verifies each by its
     own checksums and plaintext hash.
  2. Takes every log segment as live. Of compacted files whose position ranges overlap, an output that names the
     others as its inputs and verifies against them by the §7 procedure is live and its inputs are retired (it holds
     exactly their facts, so either choice holds the same facts); an output that does not verify is discarded, as an
     unlisted output is (§9 row 10).
  3. Records again every lost range the files show (positions missing between the batches of a segment, between
     segments, or below the mark, as found by the scan), with the first missing position as its identity; their
     acknowledgements are given again.
  4. Writes the new manifest as `MANIFEST.new` (`format.md` A.12), opened by a rebuild record, with epochs above
     every epoch named by a valid record and by any lease file; syncs it; renames it over the old file while no handle
     holds that open (§3 Sync primitive); syncs the directory. A `MANIFEST.new` left by a crash is removed at the
     next start.
  A rebuilt manifest takes no file as verified in another tier until it has been verified there again (§7 inputs
  kept, §8).
  The rebuild is reported: health is degraded (`manifest_rebuilt`) until the rebuild is acknowledged, and rebuilds are
  counted. What a rebuild cannot recover from local files (files held only in deep or the mirror, records of a
  generation above 0, loss causes and identities, the `file_id` floor, deep that cannot be reached, uploading the
  rebuilt records) is open (Q-103), as is what the writer does meanwhile; for now, until deep answers, the store opens
  read-only with fatal health (`manifest_corrupt`).
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
  segments up to the durable mark. A reader that reads a recovery entry (§3) voids what it took above the entry's
  `after`. How a reader that keeps a position for later use learns of a truncation or a restore that happened after
  it read the position is open (Q-46). A reader opens the mark by path for every read of it and
  reads files by position, never mapping them (`format.md` A.10, A.13), so it never reads past a mark the writer no
  longer maintains (invariant 11).

## 8. Tiers, backup and rebuild
| Tier | Location | Holds |
|---|---|---|
| Hot | fast local disk | log segments < 90 days, current detail, revised-key index, version index, chunk indexes, registry, the whole stored tree |
| Cold (optional) | another local path | older revision chunks and sealed log segments; moved current detail |
| Deep | S3-compatible bucket, via a pluggable backup sink | every sealed file, every manifest record, tree checkpoints (with the registry, latest recorded `t` and lateness reference, revised-key index and version index), generation claims, and the store descriptor (`format.md` A.12), uploaded once before any other file of the store. Until that upload the local descriptor is the only copy, an accepted limit; an operator repairs a damaged local copy from the bucket's (roadmap 8.2) |

| Rule | |
|---|---|
| Continuous backup | Each sealed file is queued for upload once; upload never blocks ingest or delivery. After its files, each manifest record is uploaded (the last record only once a later record has been synced, §7 Manifest); a tree checkpoint at least daily, and only after all log up to its position is backed up. Each tree checkpoint carries the registry file, each series' latest recorded `t` and lateness reference, the revised-key index and the version index, as of its position. Cursor files are not backed up. |
| Encryption | On the machine before upload, once per file; key supplied by the embedding application. |
| Hashes | The manifest records SHA-256 of the plaintext at seal (hot and cold verification) and, per upload, SHA-256 of the ciphertext, sent with a single-part upload. The bucket must reject a mismatch and its reported hash must equal the manifest's; only then is the file backed up. Scheduled check: re-read reported hashes; sampled download-decrypt-verify proves key and content. |
| Bucket | The engine never deletes from it. Versioning or object lock required. |
| Eviction at 90 days | Revision chunks (age per series, "never" allowed) and sealed log segments leave hot only after they are verified in cold or deep. Current detail and the stored tree stay, except as the retention class allows. |
| Retention class | For a diagnostic series (§2.1), raw detail (current and revision chunks) whose span ends more than the retention period before `now` leaves hot once verified in cold or deep; for revision chunks, the earlier of this and their own age applies. The stored tree stays hot. Nothing is deleted: such detail is fetched on demand like moved current detail, so, as with any moved detail, the duplicate check for it can be unchecked (§2.4) and window edges over it are approximate (§6). |
| Fill pressure | Above 70 % fill, verified sealed log segments leave first, then the oldest current raw detail (verified first, fetched on demand); the tree, chunk indexes, revised-key index and version index never move (§12 D4). |
| Behind consumers | A consumer whose cursor, or whose catch-up position after overflow, is older than the oldest local log segment receives `resync`, not a replay. The engine never downloads from deep for catch-up. Its `resync` carries `forget_above` and an id as §4 Resync gives for the cursor case and the catch-up case; recovery entries in segments that are no longer local are not needed. |
| Corrupt block | File quarantined; the engine fetches the copy from cold, else deep, verifies the plaintext hash and swaps it in by a manifest record. If no other tier holds a verified copy, the file is compacted again from its kept inputs, verified by the §7 procedure, recorded with its new identity and hash, and swapped in by one manifest record that retires the quarantined file. Queries over the range return an error until then. Never repaired in place. Because inputs are kept until a file is verified elsewhere (§7), the only single copies are the open log segment, a sealed log segment not yet uploaded and not yet compacted, and a manifest record not yet uploaded (§7 Manifest: its corruption loses no fact; the manifest is rebuilt from the local files; open part: Q-103). Corruption of such a log segment loses only the damaged region, from the last valid batch before it to the next valid batch after it or, with none after it, to the position before the next segment's first (in the open segment, to the mark's last position, as in §3 Recovery rule 2), reported when found and recorded as in §3 Recovery rule 2: for the open segment by the next start that can write (§3 Order of a start, step 4), for a sealed segment by the record below. For a sealed segment the valid batches are copied unchanged to a new segment, verified by their commit checksums, and one manifest record adds it with its new identity and hash, retires the quarantined file and records the lost range; the error ends with that record and the new segment is backed up like any sealed file. An optional local mirror path for sealed log segments and the manifest (configuration `mirror_path`, off by default) closes this window for sealed segments and manifest records. A restore reads the files it lists from deep alone: a listed file that the bucket has lost or corrupted is handled as here with deep as the failed tier (Restore, Failures and end). A loss in the bucket found outside a restore is open (Q-85). |
| Acknowledging a loss | `acknowledge_losses` appends one manifest record naming, for each `data_lost` acknowledged, its identity (the first position of its range when it was first reported, §3 Order of a start, step 4; a loss reported as found but not yet recorded can be acknowledged) and its first and last position as reported then; a `possible_loss` holds no degraded state and needs no acknowledgement (§11 Health). The list of losses is kept for ever; an acknowledgement only ends the degraded state. A loss is acknowledged while a record names its identity with a range that covers its present range, so a range that grows, upward or downward, needs a new acknowledgement. Acknowledging an acknowledged loss, or none, writes nothing. A torn acknowledgement record (§7 Manifest) is dropped and the loss is degraded again. The call is refused while read-only and in a drill. A restore record carries the lost ranges, acknowledgements and recovery-entry loss reports of the generations it restores from, each with its generation, so a loss whose entry or record the restore sets aside or does not restore stays listed; a lost range the restore itself records is unacknowledged. |
| Upload stalled | Files queue locally, health degraded, nothing evicted. |
| No deletion | Every fact is kept forever in at least the deep tier; the engine has no purge. An exception is a fact held only by a listed file that the bucket has lost or corrupted when a restore reads it, which becomes part of a lost range (Restore, Failures and end); a loss in the bucket found outside a restore is open (Q-85). |
| Re-ingest | Exported or foreign data goes through the normal ingest path, in batches. |
| Restore drill | The restore procedure is testable end to end; operators should exercise it regularly. A drill restores read-only and claims and uploads nothing. Its restore record is kept locally only, for the generation a restore would claim (one above the highest claimed), which stays unclaimed; the claims are not read again and the drill is never superseded. |
| Generations | The first store of a bucket writes generation 0, which counts as claimed. Before accepting ingest, a restore claims the generation one above the highest claimed, by a create-if-absent marker in the bucket; if the marker already exists it takes the next. A claim is never withdrawn, also when its restore fails. Manifest records are keyed by generation. Every generation above 0 begins with its **restore record**: its generation, checkpoint and restored end, the restored ends carried from the restore record it read (each with its generation), the lost ranges, acknowledgements and recovery-entry loss reports of the generations it restores from (each with its generation, Acknowledging a loss), and every file it restores from, set-aside segments included, each with its hash, tier and position range. The records of one generation are therefore complete without those before it, and a restore reads the records of the highest generation below its own that has any in the bucket. The restore record is synced locally, then uploaded before any other file of its generation; the restore then reads the claims again and, if a generation above its own is claimed, stops before accepting ingest (superseded, fatal health). A writer that finds a higher generation claimed stops uploading and becomes read-only (fatal health, §3); what it uploads after the later restore has read its records stays in the bucket, in no later generation's records, and is lost to them as in a disaster. A restore keeps the store id of the files it restores (`format.md` file header), and backup is refused on a bucket that holds files of another store id. A restore from a bucket with no manifest records is refused. |

**Restore** (onto an empty disk):
1. Claim a generation and read the manifest records it restores from (Generations above). Each restore record, the
   carried ones and the new one, **sets aside** the positions above its restored end in the generations below its own;
   a position set aside once stays so in every later restore. The **restored log** is the positions held by the listed
   files that are not set aside. The **restored end** is its last position before the first gap that is not a recorded
   lost range (§3) and is not followed directly by a recovery entry whose `after` is the position before the gap. The
   scan for that gap covers only the generation whose records are read, from its sequence 1; if
   that generation holds no position, the restored end is that of the restore record read. Positions of earlier
   generations at or below a recorded restored end were settled by that restore, and a file missing there is a failure
   of this restore (below), not a new restored end. Write and upload the restore record (Generations above) before
   ingest is accepted. Download the latest tree checkpoint at or below the restored end (labelled derived-unverified),
   with its registry file, latest recorded `t` and lateness reference per series, revised-key index and version
   index, and the sealed log segments after its position, and the chunk index of every compacted file the restore
   record lists, whatever tier the record gives the file, so that chunk indexes are on hot before ingest is accepted
   (§5.6); replay the log segments into the tree, which brings the registry, latest
   recorded `t`, lateness reference and version index up to the restored end. With no checkpoint, download the whole
   sealed log and those chunk indexes. Positions set aside are kept, not served: the log segments holding them are downloaded in this step
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
   Positions of the new generation start at sequence 1. A drill appends no recovery entry and writes no mark after
   its first.
2. The store serves queries, flagged `restoring`, and accepts ingest. Until the restore ends, detail held by a file
   the restore record lists that is not yet local counts as not local (Failures and end below). Window evaluations
   follow §6 Old spans, from the checkpoint's nodes and the local facts, for a series with no stored level as for any
   other: an edge moves to where its detail is local, and the result is the error `detail_not_local` only when no span
   is left. A condition evaluation with no local detail, and live state whose fact may not be local yet, are unknown (§2.3,
   §6 Conditions). Reads of facts in any view (current, as-of, history) over detail not yet local fetch it on demand, as for
   moved detail (§8 Retention class, Fill pressure). Until the current chunks that step 3 brings back are local, facts
   whose `t` is at or before the latest `t` known for their series (from the checkpoint or the restored log), which
   match no local fact of their key and no entry of the revised-key index, while a fact of their key is on no local
   tier and not in the index (§2.4; the chunk indexes are on hot from step 1), are recorded and counted
   `duplicates_unchecked`. A key
   that gains a second fact meanwhile, in the replay or by ingest, has an incomplete index entry (§2.4), completed
   when its chunk is back. The rank of an arriving fact on a versioned series is known from step 1 on, the version index being exact at
   the restored end (from the checkpoint and the replay; with no checkpoint, from the whole log). A consumer without a
   cursor is treated as beyond the restored end; one that kept its last confirmed position registers with `start:
   position`. A cursor of an older generation at or below the restored end receives the facts after it up to the
   restored end through catch-up, flagged `replayed`, if their segments are local; otherwise §4 Resync applies. Every
   consumer then receives `generation_changed` and `resync`, and resumes at the new generation's first position.
3. Bring back the files the restore record places on the hot tier, and on the cold tier where one is configured:
   current chunks, then revision chunks and log segments. Other files stay in deep, their chunk indexes on hot since
   step 1, and are fetched on demand as before. A position present in two files is one fact and must have identical content.
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
or a wrong key, is open (Q-82). A definition entry lost this way is lost as §2.1 says (only the kept log applies; its facts are orphaned); how the registry file, trees and indexes are brought back after it is open (Q-33). A file
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
| 2a | Series not versioned: A, then B, then a replay of A | Replay dropped; B stays current, except where the key's revised-key index entry is incomplete (§2.4), where the replay is recorded unchecked and becomes current. With `revise`, A is recorded and current | property |
| 2b | Identical repeat whose current fact is off every local tier and not in the revised-key index | Recorded, `duplicates_unchecked` | unit |
| 2c | Versioned series: revision A (`changed_at` 1), correction B (2), then a replay of A, also with A's detail off every tier | Replay is a duplicate (the index holds A), or recorded unchecked where the entry is incomplete; B stays current. A later revision (3) with A's value is recorded and current | property |
| 2d | Versioned series: a fact with a lower `changed_at` than the current one and a new value | Recorded, delivered flagged `outranked`, counted; no node, live-state or condition change | property |
| 3 | Reading for last week arrives now | Flagged `late`; minute path updated; no retroactive trigger | model |
| 4 | Reading > 60 s in the future, on an observed series | Recorded, `future`, suspect (or worse if supplied); delivered flagged; ignored by live state, staleness, windows until due; does not raise the lateness reference | model |
| 4a | Observed series: one reading a day ahead, then on-time readings | The on-time readings are not `late`; the reference follows them; duplicate checks still see the future reading | model |
| 5 | Device clock stuck or far in the past | Late data; adapter health reports skew when `received_at − t` is persistently large | unit |
| 6 | Two readings in one second | Both facts; the minute node folds both | unit |
| 7 | Process crash after write, before sync | Same boot: truncated to the durable mark on restart; never delivered. With the mark lost, recovery treats it as a different boot and may keep the batch, which was never acknowledged; an invalid tail is truncated and described by a recovery entry, which reports the lost mark and the tail as `possible_loss` | kill loop |
| 7a | Power loss after write, before sync | New boot: torn tail truncated by checksum and reported as `possible_loss`; complete batches synced on recovery before use; a recovery entry at the first new position; every consumer below it receives `resync` with `forget_above` before any later position, on this start and on every later one until its synced cursor has passed the entry | power-loss rig |
| 7b | Export continued by position across a power loss | Nothing taken above the last kept position is kept as valid; how the export learns that position is open (Q-46) | power-loss rig |
| 7c | Crash on the same boot after a rule 3 recovery, before a consumer received its `resync` | Rule 1 start; the replay meets the recovery entry before any reused position | kill loop |
| 8 | Crash after sync, before delivery | Delivered on restart from each consumer's cursor | kill loop |
| 9 | Crash during compaction | Output unpublished and discarded; inputs intact | kill loop |
| 10 | Crash around the manifest record | Before: output discarded. After: inputs retired. Never both live | kill loop |
| 11 | Torn or corrupt block in a sealed file | Quarantine; fetch from cold/deep, else compact again from kept inputs; verify, swap by manifest; errors meanwhile. Log segment with no other copy: valid batches copied to a new segment, the damaged region recorded as lost, `data_lost` | fault injection |
| 11a | Corrupt block inside the open segment, or below an old boot's mark after a power loss | Later complete batches kept; range recorded as lost, `data_lost`; positions not reused; a reset point (§4 Resync) for cursors below its end | fault injection |
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
| 20a | Calendar day or month of a series with no stored 15-minute level (expected interval above 60 s; an interval series with d = 30 min; a calendar-length interval series) | No calendar node; the result is a window over its facts and any stored nodes inside the span | property |
| 21 | Site zone changed, or tzdata changes its rules | Calendar nodes rebuilt from the start, under the old zone, of the local day containing the effective time (that day's and its month's nodes included) / affected days only; each change is a log entry | integration |
| 21a | Calendar query in a named zone whose offset is not a multiple of 15 minutes (historical offsets); or, on an interval series, a multiple of 15 minutes but not of d (+05:45 with d = 30 min) | Days taken in that zone for the whole span; no stored calendar node used; evaluated as a window over the UTC data, with facts at its edges; an interval crossing a day edge is apportioned (§6) | model (roadmap 1.6) |
| 22 | System clock jumps | Timers of observed series monotonic; future-fact maturity and scheduled values follow `now`; window `now` never runs backwards; `commit_time` monotonic | unit |
| 22a | Host clock not yet synchronised at boot | `unsynced_clock`; host-stamped facts wait | unit |
| 23 | Series renamed | Alias entry; id and data unchanged | unit |
| 24 | Unit or kind changes, or a lower decimal scale | New series; old one retired. A higher scale is a raise entry on the same series; earlier facts keep their scale and are rescaled exactly when read | unit |
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
| 33 | Consumer emits readings from a delivery | Non-blocking emit off the writer thread, through the consumer's handle and within its `emits`; a route, `emits` or alias that closes a cycle is refused | integration |
| 34 | Route added while a consumer is catching up | Applies from its position only | model |
| 35 | Window with no new data, or edge inside the open minute | Timer evaluation; edge from the recent window; nothing credited past the credit limit or S | property |
| 36 | Scheduled value for tomorrow | Recorded unflagged, quality kept; delivered at commit as notice; in live state from its `t`, with no second delivery; stale S after the last value takes effect | model |
| 37 | Forecast re-issued | The higher `changed_at` is current; earlier issues stay in history; a replay of an old issue is a duplicate or outranked | property |
| 37b | Forecast source falls silent | No live staleness; reported by stale-for on the adapter's companion series, and later by shrinking lookahead coverage | integration |
| 38 | Window over tomorrow on a scheduled or forecast series | Credited up to the window end; coverage shows where the values run out | property |
| 39 | Window ending after `now` on an observed series | Nothing credited past `now` | property |
| 40 | Interval reading off its phase; on a calendar-length series, not at an interval start in the interval zone (a day start, or the start of a month), including an irregular period or a day starting at another hour than its day start | Refused and counted; an identical replay of a reading already recorded at that `t` (now displaced, row 45) is dropped as a duplicate first (§2.4) when the check finds it among the local facts of its key or in the revised-key index, and otherwise refused | unit |
| 41 | Window edge inside an interval | That interval's amount credited pro rata by time; result flagged `apportioned`; count, min, max and last take the readings starting in the window and carry the same flag | property |
| 42 | Interval missing | A gap in coverage; nothing held across it | property |
| 43 | Daily amount on a clock-change day (`day`, 23 or 25 h), with a day start of 00:00 or another (05:00) | Accepted at the day start; its interval runs to the next day start, a day start in the gap or the repeated hour placed as §2.1 says (a day start of 01:30 in Europe/London on 29 March 2026 falls in the gap and is placed at its end, 01:00Z; a reading labelled 01:30Z is refused, row 40); with a day start of 00:00, day and month results in the interval zone are exact and unflagged; rate uses the true length | property |
| 44 | Window edge inside an interval of a calendar-length series (possible for a UTC day, another zone, a day of a `month` series, a calendar day or month of a `day` series whose day start is not 00:00) | Credited pro rata by the reading's exact length; `apportioned`. A window whose edges fall on starts is not flagged, whatever its zone (a UTC day on a Europe/London `day` series in January) | property |
| 45 | tzdata change moves a boundary of a calendar-length series, or drops or links its zone | Readings still on a start keep their place, their end following the new rules; a reading no longer on one is displaced: kept in views, ignored by live state and windows, result flagged `displaced`, counted; re-sent at the new start it is a new key | model |
| 46 | Host tzdata older than the source's rules | Reading at the true start refused off phase and counted until tzdata is updated; the replay is then recorded | integration |

## 10. Invariants (checked by the reference model and property tests)
1. Sequences are strictly increasing and gap-free within a generation across restarts, except for lost ranges, each
   recorded in the manifest or described by a recovery entry (§3); only a restore starts one.
2. Every delivered position is durable, except where invariant 10 allows a loss (a lost range under §3 Recovery rule 2 or §8 Restore, or a tail truncated by rule 3); every durable position routed to a consumer (an orphaned fact, §2.1, is routed to
   none) is delivered to it, or superseded by a `resync` covering it.
3. For any consumer, fact deliveries are in position order.
4. The current view equals "highest rank per `(series, t)`" (§2.2) over all facts in the files the store holds,
   positions set aside (§8 Restore) and orphaned facts (§2.1) excepted.
5. Every stored node is bitwise equal to its canonical computation over the current view of its span (nodes flagged
   `summary_stale` excepted; lost ranges excepted, open, Q-33).
6. `merge` is associative with the empty node as identity and agrees with direct computation (tolerance §5.6).
7. A window result equals direct computation over the current view at evaluation time (tolerance §5.6; results
   flagged `approximate_edges` or `summary_stale` excepted).
8. Rebuilding derived structures from log entries yields exactly the live structures, under the tzdata version the log
   names (lost ranges excepted, open, Q-33).
9. Compaction, tiering and backup preserve the set of positions outside lost ranges, each with identical content;
   restore preserves the positions of the restored log (§8) outside lost ranges it records (§8 Restore, Failures and
   end) and keeps the positions set aside, except those in a set-aside segment recorded lost.
10. No operation removes a durable fact. The only losses short of a disaster are corruption of a log segment, open
    or sealed, with no other copy (§3 Recovery, §8 Corrupt block) or a listed file that the bucket has lost or
    corrupted when a restore reads it (§8 Restore, Failures and end), each recorded as a lost range and
    reported as `data_lost`, and a tail truncated by §3 Recovery rule 3 after a reboot or with the mark lost, reported as
    `possible_loss`; after a reboot with a readable mark it can hold about one mark-sync interval of acknowledged
    batches of facts (§3 Commit), and with the mark lost it is unbounded. On a store whose file system is unmounted
    within one boot, one batch committed since the last mark sync can be truncated by §3 Recovery rule 1 unreported
    (accepted limit). Corruption of a manifest record with no other copy loses no fact; the manifest is rebuilt
    from the local files (§7 Manifest; open part: Q-103).
11. No reader or consumer observes a position beyond the durable mark as the writer holds it in memory.
12. No file leaves a tier unless a hash-verified copy exists in another tier.
13. A cursor or latch on disk is never ahead of the consumer's confirmation.
14. No manifest epoch lists two live compacted files holding the same position.
15. An arriving fact is dropped as a duplicate iff it is identical (§2.4, including `changed_at` in a versioned
    series) to a fact in the history of its `(series, t)` and lacks `revise` (except `duplicates_unchecked`, §2.4);
    `revise` applies to series that are not versioned.
16. After resume, every condition true now (§6 Restart and resume) is delivered once, in the `resync` that ends the
    resume, except one whose state is unknown during a restore (§6 Conditions), which is delivered as a transition at
    its first evaluation with detail if that differs from the latched state, or there is none.
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
22. For an interval series, the sums over adjacent windows add up to the sum over their union and their counts to its count (within the §5.6
    tolerance where an interval is apportioned; results flagged `approximate_edges` excepted); displaced readings
    contribute to no window.
23. On a calendar-length interval series the intervals of current readings that are neither bad quality nor displaced
    are pairwise disjoint, each running from one interval start to the next under the tzdata version in force; no
    displaced reading contributes to live state or to any window result.
24. On an observed series, a fact committed with a synchronised clock is `late` by the first clause if and only if it
    is not flagged `future` and a fact of its series with a lower position, flagged neither `future` nor
    `unsynced_clock`, has a `t` greater than its own by more than 2 × S; no fact flagged `future` or `unsynced_clock`
    changes the `late` flag of any other fact (positions set aside and lost ranges excepted).
25. No position above a reset point's `forget_above` is delivered to a consumer whose position is below the reset
    point's id before the `resync` carrying it has been confirmed and its cursor synced (§4 Resync).
26. No cursor file or derived file is written naming a position above the last synced mark, nor any manifest record
    except a lost-range record written by a start (§3 Order of a start, step 4) for positions in segments that start
    has synced.
27. The list of losses never loses a loss, across restarts and restores, and each loss keeps one identity however its
    range grows; acknowledging changes only the health state (§8 Acknowledging a loss).

## 11. Interfaces and configuration (outline; signatures follow in the API document)
| Interface | Contents |
|---|---|
| Ingest | `emit(reading, t_source, intent: normal | revise, changed_at)` → recorded(position) / duplicate / refused(reason); waits under back-pressure. Exports carry `changed_at` |
| Query | current, as-of P or T, history, tree level for a span, window evaluation; each result carries coverage and flags |
| Consumers | register(start, emits), deliveries, confirm(position), latches; `resync` (with `forget_above` at a reset point, §4 Resync) and `generation_changed` events |
| Health | counters (duplicates, unchecked, refused, regressions, outranked on arrival, evaluations unknown, orphaned facts, back-pressure wait, lag), persisted with the checkpoint on a best-effort basis; each jump of the system clock measured against the monotonic clock, with its size; degraded/fatal states; `data_lost`, one report per lost range caused by corruption (§3 Recovery rule 2, §8 Corrupt block) or by a listed file lost or corrupted in the bucket (§8 Restore, Failures and end) (a record that extends a range is the same loss); `possible_loss`, one report per recovery entry (§3 Recovery rule 3) that names a non-empty tail, a gap, a mark that was not readable or stale, or a missing or unreadable cursor file. Both are read from the manifest and the recovery entries at every start: a restart never clears them and each loss is counted once. A `possible_loss` whose recovery entry is later lost stays listed (the registry file and each tree checkpoint keep every recovery entry, §2.1, §8), and the range that took it is a further `data_lost`. Neither makes the store read-only. A `data_lost` holds health degraded until it is acknowledged (§8 Acknowledging a loss); a `possible_loss` is listed and counted and does not hold health degraded. A tail written after an adopted recovery entry with no readable commit block may go unreported (§3 Recovery rule 3, Adoption). A corrupt manifest record with no other copy is rebuilt from the local files and degrades health until the rebuild is acknowledged (`manifest_rebuilt`); while deep cannot be reached to tell whether a copy exists it is fatal (`manifest_corrupt`, §7 Manifest). A seal blocked by another program holding the file (`format.md` A.13) degrades health until it succeeds. Whether the mark is synced at every commit, and why (no boot identity, or configured), is reported in statistics, not as a health reason. Each report carries what is known of its range: positions, bytes and the commit times of the batches on either side. Query results carry no flag for a lost range: the engine knows its positions, not the series and times it held. Each restore record's set-aside range, and `restore_failed` (§8 Restore) |

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
| D5 | Identical repeat whose current fact is off the machine and not in the revised-key index | Recorded, counted unchecked | Ingest of that fact waits for a fetch |
| D6 | Bad-quality reading in a gauge | Ignored; previous value's hold continues up to S | Ends the previous hold (gap) |
| D7 | 1-minute level for old spans | Stays hot forever (≈ 38 MB per series-year with data every minute) | Follows raw detail to cold |

D4 and D7 are looked at again once the §13 measurements behind them exist (hot disk during log retention and tree
size, which need compaction and the tree: roadmap sections 6, 7 and 10), and may change then.

## 13. Risks to measure
| Risk | How measured | If it fails |
|---|---|---|
| Sync rate and SSD wear | Syncs/s and bytes written on a representative SSD with N unbatched 1 Hz series (N stated in the benchmark), segments created ahead of use and zero-filled, `fdatasync`, plus the timed mark sync | A longer `min_sync_interval` (§3 Group commit); a longer `mark_sync_interval` |
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
