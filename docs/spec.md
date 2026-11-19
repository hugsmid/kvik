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
3. **React after acknowledgement.** Nothing is visible to a consumer, or to the writer's own queries and live state,
   before it is acknowledged at its durability level (§2.1, §3 Commit; Q-259): at the default level after the sync
   that covers it, at the faster level once written. A power loss or a kernel crash can withdraw a faster-level
   reading already delivered, which is then a reset point (§3 Recovery rule 3); a remount within the boot, or a
   failed sync whose pages are evicted before a restart on the same boot, can withdraw one with no reset point (open,
   Q-250, Q-254). Nothing is visible to a reader process before it is durable.
4. **Data moves, it is never dropped.** A file leaves a tier only after a hash-verified copy exists elsewhere.
5. **No silent loss.** Anything refused, flagged, repeated or delayed is counted and visible, except one accepted
   case: a write after an adopted recovery entry that left no readable commit block, never acknowledged or delivered,
   may go unreported (§3 Recovery rule 3, Adoption). Open: faster-level batches not yet synced that a remount within
   the boot removes, or that a failed sync leaves to be evicted from the page cache before a restart on the same
   boot, leave nothing beyond the mark, so their positions are given again with no reset point (Q-250, Q-254), as can
   those written after an adopted recovery entry when the mark's current slot is damaged (Q-251).
6. **One writer process; any number of reader processes.**

## 2. Data model
### 2.1 Series
| Field | Rule |
|---|---|
| `id` | u32, assigned once by the registry, from 1 (0 names no series, so a highest id assigned or an id floor of 0 means none), never reused among the entries the store holds (after a restore, §8 Restore step 1), nor at or below an id floor (below) |
| `name` | `<instance>/<device>/<metric>`: exactly three `/`-separated segments, none empty; a definition or an alias with a name of another shape is refused (Q-670). A rename adds an alias, the id is unchanged; a name maps to its active series. A log written before this rule may hold a name of another shape: the store opens with it unchanged, and the series stays readable and writable by id (and by that name); defining it again under that name is refused, an alias of the right shape gives it a name patterns can select (§4 Funnel) |
| `kind` | `gauge`, `counter` (cumulative), `state` (named or boolean), `text` (UTF-8, ≤ 255 bytes), `interval` (an amount over an interval of fixed or calendar length) |
| `value type` | gauge/counter/interval: `f64`, or `i64` with a declared `decimal scale` (exact); state: u16 index into declared names, which are distinct (Q-182), or bool |
| `decimal scale` | decimal series only: the number of decimal places, 0–18 (value = integer × 10^−scale); 4 unless declared. It may be **raised**, never lowered: a raise is a registry entry that applies from its position onwards. Each fact records the scale it was stored at (`format.md` A.5) and keeps it; readers rescale exactly to the current scale. Units are limited to ±(2^63 − 1), so every stored value fits `i64` (units of `i64::MIN` are out of range, §2.5). The **largest magnitude** of a series is the greatest absolute value, in units at the current scale, over all its recorded facts that have a value, whatever their quality or rank. It is a function of the log, like the latest recorded `t` (§2.2; positions set aside and lost ranges excepted, where a carried value may exceed that of the kept log, which only makes the check stricter): held in memory, carried by each tree checkpoint with the highest stored scale (§8) and brought up to date by replay. A raise by k places is refused when the largest magnitude exceeds (2^63 − 1) / 10^k, rounded down. The **scale in force** (the current scale) is the highest of the defined scale, the kept raise entries and the highest stored scale among the series' kept facts (orphaned facts and positions set aside excepted; a highest stored scale carried by a checkpoint from before a lost range is brought back to the kept log as other derived values are, Q-33). After a lost raise (§3) the kept facts therefore hold the scale, and no entry is written again; with no kept fact above the registry's scale, the scale is the registry's, and later values are handled by the policy as usual |
| `excess precision` | decimal series only: what happens to a value with more decimal places than the current scale. `round` (default): rounded half to even at the current scale, and the fact is flagged `rounded`, so the change is never silent; `refuse`: the reading is refused, naming the scale needed; `auto-raise` with a maximum scale: the scale is raised as far as needed by ordinary raise entries, each subject to the range check, and beyond the maximum or the range the value is rounded and flagged. A value given as `f64` is first converted to its shortest decimal representation, then this applies (0.1 + 0.2 becomes 0.3 at scale 4). A batch holding a raise is acknowledged after the mark sync (§3 Commit). A value that does not fit at the current scale is out of range (§2.5) whatever the policy, and `auto-raise` never raises the scale for it. The policy and its maximum scale may be changed by a parameter change that takes effect at its position, like a change of retention class (`api.md` `set_excess_precision`); the maximum is at least the current scale and may go up or down. The policy acts on a value when it is committed, so each value is handled by the policy in force at its position |
| `unit` | fixed for the life of the series; a change of kind, type or unit, or a lower decimal scale, is a new series; a raised scale is not (`decimal scale`) |
| `expected interval` | configuration, never learned. Default: gauge and counter 60 s; interval its interval length (calendar length: 24 h for `day`, 31 days for `month`); state and text none (valid until next reading) |
| `interval length`, `interval phase`, `interval zone`, `day start` | interval series only, all fixed for the life of the series (a change is a new series). The length is either a duration d > 0 with a phase, 0 ≤ phase < d (default 0), or a **calendar length**, `day` or `month`, with an `interval zone` (an IANA name) and no phase; `day` may declare a **day start**, a local time of day with 00:00 ≤ day start < 24:00 (default 00:00), so that a day from 05:00 to 05:00 local is one interval, and a day start on any other length is a definition error. The interval zone is independent of the site zone, is not effective-dated and is compared by canonical name after resolving IANA links; a definition naming a zone the tzdata version in force does not know is an error. For an existing series the name is resolved by the version in force each time, and the definition keeps the name as given. Where that version makes it a link, the starts follow the link's target from then on, like any change of rules (§5.4). Where that version does not know the name, the series has no interval starts: every recorded reading is displaced (§5.4), every new reading is refused off phase and counted (row 40), and health is degraded, naming the series and the zone, until a version that knows the name is in force, which undoes it. A reading's `t` is the start of its interval. Its **end** is `t + d`, or for a calendar length the next **interval start** after `t`: the starts are, in the interval zone, the instants of the day start on each date (for `month`, the start of the first day of each month, §5.7); a day start that falls in a gap is placed where the gap ends, as §5.7 places a midnight that does not exist, and one that occurs twice is placed at the earlier instant, as §5.7 places a midnight that occurs twice; a reading at the later instant is refused off phase (row 40). All starts are taken under the **tzdata version in force**, the one version that the phase check at commit, displacement (§5.4) and calendar results (§5.7) all use. Each interval thus runs from one start to the next, whatever the rule for an unusual day start. A reading carries no length: lengths are exact nanoseconds (a local day is usually 23–25 h). `t` must be an interval start (fixed length: `t ≡ phase (mod d)`, Euclidean, on UTC nanoseconds), or the reading is refused and counted; its value is the amount over `[t, end)`. Intervals of current readings that are not displaced (§5.4) therefore never overlap. A source whose periods are neither of one fixed length nor local days (from one day start to the next) or months of one zone, such as irregular billing periods, cannot be an interval series in this version, and the engine never splits an amount into finer intervals. Record on change (§2.6) does not apply to an interval series |
| `staleness limit` S | `3 × expected interval` unless set; none if no interval. An S that was set is kept when the expected interval changes |
| `record on change` | optional; see §2.6 |
| `wrap_at`, `reset_ceiling` | counters only; §5.5. `reset_ceiling` default 10 % |
| `lateness bound` | default 5 min; §2.2 |
| `versioned` | optional, for sources that revise readings they have already published; fixed for the life of the series (a change is a new series, and history may be re-ingested with `changed_at`). Every reading of a versioned series carries `changed_at` (§2.2); readings of other series never do |
| `changed_at kind` | versioned series only: `time` (UTC nanoseconds) or `version` (an opaque counter); fixed for the life of the series; used to type exports, and must be `time` for a forecast series |
| `durability` | `synced` (default) or `written`: when a reading of the series is acknowledged and delivered (§3 Commit). **Synced:** after the sync that covers it; an acknowledged reading survives a crash of the process, of the kernel and a power loss, within §3's limits. **Written:** once its batch is written (in the operating system's cache, before the sync), and while a default-level batch's sync is in flight it waits for that sync (§3 Commit; Q-255); a crash or restart of the process keeps it while the page cache holds it (§3 Recovery rule 1). A power loss or a kernel crash before its sync returns can remove it: a reset point reported as `possible_loss` (§3 Recovery rule 3), or, when a later batch reached the disk and it did not, a lost range reported as `data_lost` (rule 2; Q-257). Two removals go unreported (open): a remount within the boot (Q-250), and a failed sync whose pages are evicted before a restart on the same boot (Q-254). Such an event removes at most what was written since the last log sync that returned; the writer starts that sync at the latest `max_unsynced` after the first flagged batch (§3 Commit), so a slow device widens the window (Q-681). A change of level is a parameter change that takes effect at its position, like a change of retention class; it changes no fact (Q-256). Definitions and every other entry that is not a fact are always synced |
| `retention class` | `long-term` (default) or `diagnostic` with a retention period (default 30 days), for series kept mainly for short-term diagnosis, such as a machine's own resource metrics. It decides only where raw detail is kept (§8 Retention class), never which facts exist. A change of class is a registry entry that takes effect at its position, not by event time; it moves no detail back |
| `timing` | `observed` (default), `scheduled` or `forecast`; fixed for the life of the series. **Scheduled:** values known in advance that take effect at their `t`, such as published prices; a scheduled source that revises its values must be versioned. **Forecast:** values about future times, re-issued over time; the series must be versioned with `changed_at kind` `time`, `changed_at` being the issue time and `t` the valid time. The engine does not report a silent forecast source: its adapter emits an observed series with an expected interval on each successful fetch, to which stale-for applies |

Series definitions, parameter changes (with an effective event time, except a change of retention class or of the
excess-precision policy or the durability level, which take effect at their position), aliases, retirements, scale raises, label changes (Labels below), site-zone entries and tzdata entries (§5.7) are log entries. A parameter other than the retention class, the
excess-precision policy and the durability level applies to readings with `t` at or after its effective time. A definition entry always has a lower
position than the series' first reading (the same batch is allowed). Registry entries are also compacted into a
registry file that stays on the hot tier (tree checkpoints carry a copy, §8; `format.md` A.16). The registry file is a
derived file: every start rebuilds the registry from the kept log and writes the file again where it does not match,
carrying the id floor (Q-33), and the writer writes it again after each routing change, before the call that made it
returns (Q-130). Site-zone entries and tzdata entries
(§5.7) and routing changes (§4) are registry entries in this sense: the registry file holds each with its position,
so they are in force wherever the registry is, whether or not the log segment that holds them is local. Recovery
entries (§3) are compacted into it too, each with its position. **Label changes are the exception:** they are
registry entries for every other rule, but the registry file does not hold them and its Derived check wants exactly
the kept log's registry entries other than label changes; they are held in label parts (Labels below, Q-892).

After a lost range (§3) only the entries of the kept log apply; a lost definition, parameter change, alias,
retirement, scale raise, label change, routing change or zone entry is never appended again by the engine and is issued again through the
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

**Labels.** A series may carry key–value labels, versioned like facts (Q-880, Q-890, Q-891). Labels belong to the
id: an alias or a rename leaves them alone, a retired series keeps them and still takes label changes, and a new
series (a change of kind, type or unit) starts with none. Names stay the identity, the clustering order and the
export key; a label never replaces a segment.

| Part | Rule |
|---|---|
| Key | ASCII `[a-z][a-z0-9_]*`, 1–63 bytes |
| Value | UTF-8, 1–255 bytes, no control character (U+0000–U+001F, U+007F); compared byte for byte, never normalised. There is no empty value: absence is removal |
| Reserved keys | `site`, `area`, `device`, `metric`, `month`, `series` and every key starting `kvik_`; setting one is refused (Q-881) |
| Per series | at most 64 distinct keys ever used, every version counting its key; at most `limits.label_versions_per_series` versions (1,024) |
| Per store | at most `limits.label_versions` versions (1,000,000; 100,000 on the minimal class) and `limits.label_bytes` of label entries, each counted at its encoded length (`format.md` A.6; 32 MiB; 4 MiB on the minimal class), orphaned entries included (Q-1022) |

- **A label change** is one log entry (`format.md` A.6 type 10) naming a series, an **effective time** (`from`: UTC
  nanoseconds, or *always*, which sorts before every time) and 1 to 64 items with distinct keys, each a **set** (a
  value), a **remove** or a **withdraw**. Each item is a **label version** `(series, key, from, position, item)`. A
  change may take effect before or after it is recorded; a future `from` takes effect when event time reaches it.
  A label change shares the log's one position order with the series' facts; where a store keeps several logs, it
  is written to its series' log.
- **Calls.** `set_labels` and `set_labels_many` (`api.md` Labels) write one batch with a forced mark sync (§3
  Commit). One bad item refuses the whole call and appends nothing. A definition may carry labels: the call appends
  the definition and a label change effective *always* in one batch; defining an existing series again leaves its
  labels alone and says so (Q-884). `declare_labels` sets, effective at `now`, each listed key whose value in force
  now differs, and appends nothing for the others; a key it does not list is left alone. A change on an id with no
  definition is refused.
- **Duplicates.** An item is dropped only when the current version (§2.3 Label views) at exactly its
  `(series, key, from)`, earlier entries of the same batch included, is already that item (Q-1025). Whatever a caller
  was told succeeded is recorded, and the labels do not depend on the order of calls. A change left with no item is
  not written.
- **Limits are checked at the call only**, on the versions the call adds (Q-1024). They are ceilings that only
  configuration lifts: nothing is deleted, and a removal or a correction is itself a version. Replay, rebuild,
  restore and readers apply every kept label change also when the configured limit is now lower, and a start never
  fails on its own log. Health gives a notice at 80 % of a store limit and names each series at 80 % of its own
  (Q-1029).
- **Lost and orphaned.** A label change in a lost range is gone and is issued again through the calls. A kept label
  change whose series has no definition in the kept log is an **orphaned label entry**: kept in the log, through
  compaction and in the label parts, outside every view, the label index and routing, and counted
  (`orphaned_label_entries`).
- **Where they are held.** Besides the log and compacted files, which carry them as every entry that is not a fact,
  label changes are held in **label parts** (`format.md` A.19): immutable synced files under `registry/labels/`, each
  holding the label changes of the kept log of one generation in a range of sequences, written after a seal whose
  segment holds any and at a clean stop for those not yet sealed, and merged occasionally. A sealed segment records
  whether it holds any label change (`has_labels`, `format.md` A.8), so the label state is the label parts in force
  and the flagged segments above the last of them. Label parts are derived from the kept log: one that overlaps a
  lost range, holds positions set aside by a restore, is missing, fails its checksum or differs from the local log is
  rebuilt from what is kept, and until the rebuild ends label calls and selections fail with `LabelsUnavailable`
  while reads without them are unaffected. They are not in the registry file, the series directory, the chunk index
  or chunk headers (Q-893). A log segment that carries `has_labels` is removed locally only under §8 Eviction's
  condition for label parts.

**Retirement** is effective at `t_r`: facts with `t < t_r` are still accepted; facts with `t ≥ t_r` are refused
and counted. The old id stays reachable by id.

### 2.2 Log entries, positions and facts
- Every log entry (fact, definition, parameter, alias, retirement, scale raise, label change, site zone, tzdata, routing change, recovery
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
  any other series, is refused and counted, as is one above `i64::MAX` on a series of `changed_at kind` `time`, which
  must fit a timestamp as every other time does (Q-195). A source whose versions are coarser than its revisions can
  still have a correction with the same `changed_at` reverted (§2.4).
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

**Label views.** The labels of a series are a function of the kept log's label versions (§2.1 Labels) over two time
axes, as a fact has `t` and position. The versions **known at P** are those with position ≤ P. Among them, of the
versions with one `(series, key, from)` the one of highest position is **current**, the others **superseded** (the
fact rule with `(series, key, from)` in place of `(series, t)`). The labels **in force** at event time T, as known at
P, hold for each key the current version with the greatest `from ≤ T` among those that are not withdrawals, when
that version is a set: a current remove makes the key absent from its `from`, and a current withdraw makes its
`(key, from)` contribute nothing, so the labels read as if no change had been made at that time, whatever is later
corrected before it; a later set or remove at the same `from` reinstates it (Q-1026).

| Label view | Definition |
|---|---|
| Now | in force at `now`, as known at the read's cut; it depends on the clock once a future `from` exists |
| At T | in force at T, as known at the read's cut |
| As-of P | in force at T, as known at P |
| As-of time T′ | as-of P, where P is the last position whose batch `commit_time` ≤ T′ |
| History | every version in position order, each marked current or superseded |

A series with no definition in the kept log has no labels in any view (Q-1028).

A fact that is not current for its key is a **revision**. In a versioned series a fact can be a revision from the
moment it is recorded: it is **outranked** when a fact of higher rank for its key is already recorded.

### 2.4 Duplicates
- An arriving fact is a **duplicate** if its `(series, t, value)` and adapter-supplied quality equal those of *any*
  fact already in the history of `(series, t)`, including facts earlier in the same batch. Flags and quality assigned
  by the engine are not part of identity. Values compare bitwise (`0.0` ≠ `-0.0`). A decimal value compares by exact numeric value,
  whatever scale each was stored at (12000 at scale 4 equals 120000 at scale 5). An arriving value also equals a
  recorded fact flagged `rounded` when, rounded half to even to that fact's stored scale, it gives that fact's value:
  a replay after a raise is a duplicate of the reading it repeats. A correction that differs from the recorded
  reading only below that scale is therefore dropped as a duplicate; it is submitted with `revise`, or on a
  versioned series with a higher `changed_at`. The check runs on the value as submitted (an `f64` taken as its
  shortest decimal representation), before the excess-precision policy (§2.1): a duplicate is never rounded, refused
  or the cause of a raise; a value out of range (§2.5) compares as "no value". Duplicates are not recorded and
  consume no position; they are counted per series (`duplicates_dropped`). Adapters may replay freely: a replay that the
  check can run on never reverts a correction (for the unchecked case, see below).
- In a versioned series identity also includes `changed_at`. A fact whose `changed_at` is above that of every fact
  in its key's history is never a duplicate, even when its value returns to an earlier one.
- A fact submitted with `revise` intent bypasses the check (deliberate return to an earlier value). On a versioned
  series `revise` is refused and counted; a return to an earlier value there is a fact with a higher `changed_at`.
- A reading recorded without a value (§2.5) is compared as "no value", with its adapter-supplied quality.
- Identity includes `t`: the same value one nanosecond later is a different fact and is recorded.
- **Revised-key index** (hot tier, never moves; carried by each tree checkpoint, §8): for every `(series, t)` with
  more than one recorded fact, the exact value (on a decimal series with its stored scale and `rounded` flag),
  adapter-supplied quality and `changed_at` of each, and whether the entry is incomplete (last item). For any other key, history holds at most the current fact, which is compared
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
- **Local data that cannot be read** (Q-750). A local fact the check needs that cannot be read during a run (damage
  found after the start, or a file that cannot be read) is treated as on no local tier: an arriving fact that matches
  no fact the engine can read is recorded, flagged unchecked and counted `duplicates_unchecked`, with its rank judged
  on the facts that can be read; ingest never stops for it. A delivery decided without such data (whether a fact was
  outranked when committed, or the facts a condition or live state is evaluated over) is flagged `unchecked` and
  counted, as is an evaluation made without such data whether or not it delivers. Either holds health degraded
  (`local_data_unreadable`) until the next start. A damaged region the start passes over, or a consumer's read of the
  log meets, that no lost range records also holds it degraded, at every start, until the region is repaired or
  recorded as lost (Q-755); its positions are out of the log meanwhile (§3 Recovery rule 2).

### 2.5 Quality, invalid values, clock
- Bad-quality facts stay in views but are ignored by summaries and do not refresh staleness (§12 D6).
- NaN or ±∞ is recorded as quality bad with no value, flagged and counted (§12 D1). A negative counter value
  (§5.5) is recorded the same way; D1 concerns NaN and ±∞ only. On a decimal series a finite value that, rounded
  half to even to the series' current scale, does not fit ±(2^63 − 1) units (a decimal with units `i64::MIN`
  included) is **out of range**: recorded the same way, whatever the excess-precision policy, and counted
  (`out_of_range`). The writer is also told: the reading's outcome in the acknowledgement says it was recorded out of
  range, as it says a value was rounded (`api.md` Writing), and the call itself succeeds, so one value never fails a
  batch. The raw value is not kept.
- A reading submitted with no value (`Value::Absent`, `api.md`) is recorded the same way whatever quality was
  supplied, on a series of any kind: quality bad, no value, flagged and counted on its own (`absent_supplied`). Its
  supplied quality is kept for the duplicate rule (§2.4), so two such readings at one `t` are duplicates only when
  their supplied qualities are equal, and any two readings recorded without a value under this section at one `t`
  with the same supplied quality are duplicates (the stored row does not tell them apart, `format.md` A.5). It is
  never live state (§2.3), enters no summary and does not refresh staleness; it does not end the validity of the
  reading before it, whose hold runs on to its staleness limit (§5.2). At a `t` that already holds a reading it is
  a new fact of higher rank and becomes current (§2.3), as a NaN does, so that `t` then has no value in summaries or
  live state; the earlier reading stays as a revision. On a state or text series, which has none,
  the previous value therefore stays live: an application that must record that a state is unknown declares a
  state name for it.
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
| Batches | Append-only segments of arrival batches. A batch ends with a **commit block** (last position, checksum of the batch's blocks, and whether the batch is **flagged**: acknowledged once written, `format.md` A.7). A flagged batch holds only facts of series at the `written` level (§2.1); every other batch is unflagged and may hold readings of both levels. The queue is cut into batches by call, a call being one batch unit (`api.md` Writing): consecutive calls whose readings are all at the `written` level form flagged batches, and consecutive other calls form unflagged batches, so each call is acknowledged at one level, and a call holding readings of both levels at the default level (Q-255). An unflagged batch = one commit = one sync; flagged batches share the next sync. A batch never spans segments. |
| Commit | Unflagged batch: write, sync, update the durable mark, then (and only then) acknowledge adapters, update tree and live state, and deliver. Flagged batch: write, then acknowledge, update tree and live state, and deliver; its sync follows, and the mark moves after it as after any sync. The writer writes a batch only after the previous **unflagged** batch's sync has returned and the mark has moved past it; flagged batches follow one another, and an unflagged batch may follow them, with no sync between. So a faster-level reading that arrives while a default-level batch's sync is in flight waits for that sync (Q-255), and batches are still acknowledged and delivered in commit order. The writer syncs the log at the latest `max_unsynced` (default 1 s, at most `mark_sync_interval`) after the first flagged batch written since the last sync, and at once before a seal or a roll to the next segment, at clean shutdown, and before a write that would name a position above the durable mark (Durable mark). A batch holding any entry that is not a fact is acknowledged and delivered only after the mark file has been synced past it, so a truncated tail holds no acknowledged definition, parameter, alias, retirement, scale raise, routing or zone entry. For such a batch the writer forces the mark sync as soon as the batch's own sync has returned, rather than waiting for the timed `mark_sync_interval` (Durable mark). Batches are acknowledged and delivered in commit order, so a later batch holding only facts waits for that one mark sync too, not for up to an interval, and every entry still takes effect at its position. The first batch written into a segment, when it is unflagged, is likewise acknowledged only after the mark naming it is synced, the writer forcing that mark sync too (one mark sync more per segment), so a durable mark names a position in every segment that begins with an unflagged batch and holds acknowledged data, and losing such a segment whole is a reported loss (Recovery rule 2, Q-630). A segment that begins with flagged batches has no such anchor until the next mark sync: a flagged batch is acknowledged before any sync, so the anchor is another thing the `written` level gives up (§2.1 `durability`). |
| Durable mark | After each successful sync the writer updates the mark file in place by positional writes (`format.md` A.10): segment id, byte offset, last position, boot id, highest series id assigned, and the current reset stamp (§7 Readers). The file holds two checksummed slots; every update between two mark syncs goes to the slot that does not hold the last synced mark, so a torn update falls back to that mark, never an older one, and a reader that meets an update in progress sees a mark at most about one interval old. The writer writes a batch only after the previous unflagged batch's sync has returned and the mark has moved (Commit). The mark covers only synced batches: a flagged batch lies beyond it until its sync. The mark file itself is synced on a timer (`mark_sync_interval`, default 1 s), at once after a batch holding an entry that is not a fact or an unflagged first batch in a segment (Commit), at clean shutdown and by recovery, so after a power loss the mark on disk lags acknowledged batches by about one interval, and, except after a segment that begins with flagged batches, never names a segment below the last one holding an acknowledged batch. It is updated in place and never replaced while it exists. Readers read log segments only up to the mark. No cursor file or derived file is written naming a position above the last synced mark, nor any manifest record except a lost-range record written by a start (§3 Order of a start, step 4), which names only positions in segments that start has synced; a write that would name one waits for the next mark sync or forces one. |
| Sync primitive | "Sync" means the bytes are on stable storage with the device cache flushed. A failed sync is never retried, on any handle: the file's contents since its last successful sync are unknown. Linux: `fdatasync` or `fsync` on the file, `fsync` on the directory after a create or rename. macOS: `F_FULLFSYNC` for every sync named in this document (`fsync` alone does not flush the device), also for directories. Windows: `FlushFileBuffers` on the file; there is no directory sync, so a create or rename is made durable by flushing the created or renamed file (`format.md` A.13); a file that is open cannot always be replaced by rename, which is why the mark is updated in place and a manifest record is repaired in place (§7 Manifest), a rename being the fallback. The mark is synced like any other file; no file is memory-mapped. A platform or file system that cannot give these guarantees is refused at open, except Windows, which is a development platform like macOS (`design.md` Budgets): it builds, runs and is tested, runs with the mark synced at every commit, and is not held to these durability guarantees or to the budgets. (A platform without a boot identifier syncs the mark after every update, `format.md` A.10; that is a separate case.) These are requirements; the roadmap 2.2 spike and roadmap 4.8 measured their costs on macOS, 4.8's harness runs on Linux and Windows cloud runners, and the Linux figures on the target hardware are recorded there (`measurements/4.8-commit-latency.md`). |
| Group commit | While a batch sync is in flight, arriving readings queue and form the next batch: one batch sync covers all of them, and each is acknowledged only once that sync has returned (Commit). Batch syncs draw on a **sync budget** of four batch syncs, full at the start and refilled at one per **interval**: eight times the smoothed duration of a batch sync, clamped between min(`sync_interval_floor`, `min_sync_interval`) and `min_sync_interval` (defaults 2 ms and 10 ms), and `min_sync_interval` itself before the first sample; `min_sync_interval` at 0 disables the budget (Q-980, Q-981, Q-712); only batch syncs that succeed are timed. With no log sync in flight, a batch is synced at once while the budget holds a sync, and otherwise as soon as its refill reaches one, taking in whatever arrives meanwhile. So a writer under budget waits for no timer, and whatever the load, any window holds at most its length divided by the least interval, min(`sync_interval_floor`, `min_sync_interval`), plus four, batch syncs (Q-985), whatever interval is in force: at most 1/`sync_interval_floor` per second over time (500 at the default) on a fast device, and 1/`min_sync_interval` (100 at the default) on one whose sync takes an eighth of `min_sync_interval` (1.25 ms at the default) or more; in between, batch syncs keep the device busy at most about an eighth of the time. The budget refills by time, so a late start or a stall does not delay later syncs (Q-310). Mark syncs (Durable mark), timed or forced, and the sync that takes a segment into use at a timed seal spend nothing of it (Q-982). A flagged batch is written once formed, unless an unflagged batch's sync is in flight (Commit); the budget applies to the sync that follows it, which also carries any unflagged batch. |
| Segments | Created ahead of use and zero-filled (64 MiB, `format.md` A.4). If creating one fails for lack of space, the partial file is deleted, health is critical, ingest waits, and the creation is retried when free space is at least `disk_critical` + `segment_size`, checked every second; where free space is unknown it is retried every 30 seconds, as each try zero-fills until it fails. Any failure of a creation other than lack of space at the create or a write, a failed sync included, is a write failure on the log (below); so is lack of space at a batch write or sync. The directory is synced after creating, sealing or renaming a file. |
| Sealing | At 64 MiB or 5 minutes (both configurable), whichever first; an empty segment is not sealed. The 5 minutes run from when the segment is taken into use; the sync that takes the next segment into use at a timed seal carries no batch and spends nothing of the sync budget (§3 Group commit, Q-982). Sealed segments are immutable and eligible for backup. Compaction never removes a log segment (§8). |
| Recovery | Below this table. |
| Write or sync failure | On the log, manifest, mark, cursor or registry files: stop accepting writes, mark read-only, fatal health; a failed sync is never retried, on any handle; recovery by restart. (The read-only state of a corrupt manifest record while deep cannot be reached comes from no failed write and ends by itself, §7 Manifest.) After a failed sync the batch's fate is unknown: a restart on the same boot truncates it; after a reboot recovery may keep it (row 7a). It was never acknowledged, so adapters may replay it, which the duplicate rule absorbs. Flagged batches the failed sync covered were acknowledged: a restart on the same boot keeps every complete one the page cache still holds (Recovery rule 1); on Linux the failed write-back leaves their pages clean, so they may be evicted before the restart, which then finds nothing of them and reports nothing (open, Q-254); after a reboot those that did not reach the disk are a reset point (rule 3). A failed compaction, tiering or tree write abandons that job, deletes its partial output and degrades health. |
| Disk space | Low mark: free space below 5 % of the filesystem or 2 GiB, whichever is larger → compaction pauses, health degraded. Tiering and eviction, which free space, continue at every mark. Critical: below 512 MiB free → ingest waits (back-pressure). Reads continue. Space is never freed by deleting facts. |
| Back-pressure | Ingest is a bounded queue (default 65 536 readings); a full queue makes adapter emits wait; the wait is measured. |
| Single writer | An exclusive operating-system lock on the file `LOCK` in the store directory (`format.md` A.12), held for the life of the writer and released when the process ends; readers never take it. Network file systems are not supported. |

**Recovery.** Every unsealed segment is scanned to its end. A missing or unreadable mark (no valid slot, `format.md`
A.10) is treated as a different boot. The rules apply in this order:
1. **Same boot id as the mark:** keep every complete flagged batch beyond the mark, then truncate everything after
   the last batch kept (after the mark when none is). A flagged batch was acknowledged once written (§3 Commit), and
   on the same boot a crash of the process leaves it in the page cache (a remount, Q-250, or the eviction of pages
   whose write-back failed, Q-254, can remove it with nothing left to find): the start writes each kept batch again in
   place, unchanged, with its segment's header when it is the segment's first (a sync vouches only for bytes written
   by the start that calls it, rule 3), syncs, and only then moves the mark past the last one (Order of a start,
   step 2). An invalid region before a kept batch is rule 2's. What is truncated may have reached the disk, but no
   mark covered it, so it was never acknowledged or delivered unless a remount lost the mark's update (below);
   adapters replay. A batch is written only after the previous unflagged batch's sync and mark update, so beyond a
   current mark lie flagged batches and at most one complete unflagged batch, the last. If a complete unflagged
   batch beyond the mark is followed by another complete batch on the same boot (two unflagged ones included), the
   mark is **stale** (the store's file system was unmounted or detached within the boot and the page cache lost):
   rule 3 applies, as for a different boot. Flagged batches do not count towards this test. A sealed
   segment holding positions above the mark makes the mark stale too (the mark passes every batch of a segment
   before its seal starts). When the truncated bytes hold a complete batch or any non-zero byte (a batch damaged after its sync, or a write
   cut short, an unused segment's bytes after its header included, `format.md` A.4; Q-320), they may hold a batch that was acknowledged (a remount within the boot loses
   the mark's update since its last sync with the page cache) or not (its sync or mark update failed); recovery
   cannot tell, so the truncation is a **reset point**: after it, a recovery entry is written as under rule 3, with
   K = the last kept position (the last kept flagged batch's last position, else the mark's), mark state readable,
   the truncated bytes as its tail and a new reset stamp, and the loss is reported as `possible_loss`. If the one
   complete unflagged batch after the kept batches is such an entry, written by an earlier start that stopped before
   its mark (its `after` is K, it starts where the kept batches end and nothing after it shows a later write, as for
   Adoption under rule 3), it is adopted and written again in place, and no second entry is written. Accepted limit:
   an acknowledged unflagged batch can be truncated this way; it is reported and consumers and readers forget above
   K. A store on media that may be remounted within a boot avoids it by syncing the mark at every commit (`api.md`
   `log.mark_sync`). That does not cover flagged batches not yet synced, which a remount removes from the page cache
   and may leave nowhere on disk (open, Q-250).
2. **Corruption:** an invalid region that lies below a valid mark (a valid slot, `format.md` A.10) of any boot, or that is
   followed by a complete batch, was synced (the mark is written only after a sync returns, and a batch only after
   the previous unflagged batch's sync), so it is corruption, not a torn tail; or it held flagged batches, written
   with no sync between them, whose pages a power loss or a remount left unwritten while a later batch's reached the
   disk. Either way it may have been acknowledged and delivered, and it is handled as corruption. Later complete batches are kept and nothing is
   overwritten. The byte and position range, from the last valid batch (with none, from the last sealed segment's
   end or, with no sealed segment of the current generation, its first position: 1 in generation 0, the restore
   record's in a restored generation, Q-264) to the next valid batch or, at the tail, to the mark's last position, is reported as `data_lost` when found and recorded in the manifest as a **lost range**
   by step 4 below; neither makes the store read-only. Its positions are not reused. A lost range is a reset point
   for consumers (§4 Resync). The next valid batch is found by resynchronisation (`format.md` A.2) under the window
   rule of the scan bound (`format.md` A, common rules), which damage does not reach: in a crafted file, the rest of
   an 8 MiB window can be passed as damaged and lost. A region below the mark is also found while
   the store is open, by a read of the writer's process or of a reader process that has not read those bytes before, or that reads them again because
   the newest batch it has read no longer reads the same: that process reports it from then on, not yet recorded,
   and the next start finds it again and records it under the same identity (one that grew downward: Q-203). A
   region that overlaps a loss already found or recorded grows that loss under its identity, as in step 4; the
   grown loss is not recorded until a record covers it, and needs a new acknowledgement (§8 Acknowledging a loss).
   Once the writer's process has found it, the facts it held are in no key's history (§2.4), so a replay of one is
   recorded again. Accepted limit: damage to a batch the writer's process has already read is not found by `losses()`
   or `health()` unless it is the newest; it is found when a query of that process reads it (Q-928); until then a
   replay of a fact still in the recent window is dropped, and one outside it is recorded unchecked (Q-750). Other
   readers and the next start find it (Q-200).
   Because the first batch in a segment, when unflagged, is acknowledged only once a synced
   mark names it (Commit), such a segment holding acknowledged batches that is damaged, zeroed or missing whole leaves
   a valid mark naming a position in it or in a later segment, while the mark is readable, so its loss is a
   `data_lost` from that anchor at least up to the mark's position, also when it was the only unsealed segment and no
   other source names a position (Q-630); positions acknowledged above the durable mark and lost with it are a reset
   point under rule 1 or 3, as for any tail beyond the mark.
3. **Different boot id** (the machine rebooted, the page cache is gone), or the mark missing, unreadable or stale
   (rule 1): any remaining invalid tail is torn. Let K be the **last kept position**: the higher of the last kept
   batch's last position and the end of a lost range found at the tail under rule 2. Let F = 1 + the highest
   sequence of the current generation among K and every position named by a local cursor file, manifest record or
   derived file (the registry file, `format.md` A.16, and the files of A.18).
   Recovery writes a **recovery entry** at F, alone in its batch, at the next write position (after
   the kept bytes or after the lost region at the tail; in a new segment when the open one is full): `after` = K, a
   new **reset stamp** (the sequence F and a random nonce, never zero; the mark carries it from then on), the
   boot id, the tail's length in bytes up to the last non-zero byte, the readable commit blocks in it (the tail reported
   is that of the last segment in use, with the bytes after the header of any unused segment beyond the mark's segment,
   or of any unused segment with no readable mark, Q-320), the mark's state, an id floor (§2.1), the consumers whose cursor file was
   missing or unreadable, and whether a series is at the `written` level (§2.1) in the registry at K (that of the kept
   log, after the restore's checkpoint where there is one): flagged batches not yet synced can vanish without leaving
   a byte, so such an entry is a `possible_loss` whatever its tail (§11; Q-251). A change of level is synced with the
   mark before it is acknowledged (§3 Commit), so the registry at K holds every level in force when the lost batches
   were written.
   Positions between K and F, if any, are a lost range whose cause is a truncated tail. Positions above K may have
   been acknowledged and delivered and are assigned again (F to the entry, then from F + 1), so a recovery entry is
   a reset point for every consumer (§4 Resync). A consumer whose cursor file was missing or unreadable is treated
   as having no cursor. **Adoption:** if the last kept batch is a recovery entry, a readable mark does not cover it,
   and the bytes after it show no later write (none lies beyond the end of the tail it reported, no readable
   commit block among them has a commit time at or after the entry's, and no unused segment in a later file holds
   bytes after its header, which the writer may have written after the entry when the mark is lost or its current
   slot damaged, Q-320), no recovery since has resumed: that entry is
   adopted, whether or not the mark is readable, no second entry is written, and the invalid bytes after it are the
   rest of the tail it reported (a mark update torn by a crash in step 6 is no new loss). With the mark missing or
   unreadable while a series is at the `written` level in the registry at K, an entry is never adopted: the start
   that wrote it may have resumed and acknowledged flagged batches that left no byte. When it is not adopted, a new
   entry is written, and the bytes after the old one are a newly truncated tail that the new entry reports as
   `possible_loss`, so a doubtful case costs a second report rather than a reused position without a reset point.
   Accepted limit: a write after the entry that left no readable commit block inside the reported tail cannot be
   detected, and the entry is adopted. Such a batch was never synced, so it was never acknowledged or delivered unless
   it was flagged, which the exception above rules out except where the mark's current slot is damaged and an older
   one is read (Q-251) (a synced batch survives as a complete batch, and the entry is then not the last kept batch),
   and the consumers that
   had confirmed the entry's reset point received nothing above it from that write; at worst one `possible_loss` goes
   unreported. **Last batch written again:** on Linux a page whose writeback failed is marked clean and kept in the
   page cache, and a later sync of the file returns success without writing it, so a sync vouches only for bytes
   written by the start that calls it. Where this rule applies and no readable mark shows a different boot (the mark
   is missing, unreadable or stale), the machine may not have rebooted, and the last complete batch may be one whose
   sync failed. The same holds for a recovery entry that is adopted, whatever the mark: the start that wrote it may
   have failed its sync and left it in the page cache only (Q-117). Before the sync of step 3 the start writes the bytes of the last complete batch again, in place and
   unchanged (no damaged region is overwritten, rule 2), together with the flagged batches directly before it, back
   to the nearest unflagged batch, and with them the segment's header when the first of them is the first of its
   segment (the header rewrite that takes a segment into use is made durable by the same sync, `format.md` A.4).
   Only these can be in doubt: a batch is written only after the previous unflagged batch's sync has returned, a
   seal or a roll syncs first (§3 Commit), and each start writes these batches again before it writes after them,
   and an adopted entry is written again with them, so a recovery entry adopted from a start whose own sync failed
   is covered too. If that sync succeeds the batch is on disk and is kept
   (keeping a batch never acknowledged is allowed, row 12); if it fails the engine stays read-only (step 3) and
   nothing is delivered. This rests on the platform fact that the manifest's rewrite of its last record rests on
   (§7 Manifest): that writing identical bytes makes the next sync write them, checked on Linux, on ext4
   and XFS, at gate 4.9 (Q-52 (b); macOS and Windows still open). The first start of a new store (no mark file and an empty log) writes no
   entry and creates the mark. A mark with no valid slot beside a log with no segment in use and a manifest naming
   no position is a first start too, unless an unused segment holds bytes after its header (then rule 3 applies and
   its entry reports them, Q-320) (a first start cut short while it wrote the mark): the mark is written again in
   place.

**Order of a start.** Every start runs these steps; none depends on state left by an earlier attempt.
1. Sync the log directory. Read the manifest (§7) and the mark; scan every unsealed segment to its end.
2. Rule 1: first, with complete flagged batches beyond the mark, write each again in place with the header rule of
   Recovery rule 1, sync those segments and the directory, write the mark again in place as read and sync it, then
   update it past the last kept batch (current boot id, reset stamp unchanged) and sync it; from here "the mark" is
   that update, and a stop before it leaves the same batches for the next start. Then truncate after the mark,
   unless a complete batch or any non-zero byte lies beyond it (a reset point); sync every segment holding a lost range
   found (rule 2) and the directory, with step 3's rule for a failed sync; unless step 6 writes the mark, write the
   mark's slot again in place, as read, and sync the mark file (its update may not be on stable storage, and one whose
   sync failed may no longer be dirty, `format.md` A.10 Update); go to step 4. At a reset point the bytes beyond the
   mark are left in place here: step 5 writes the recovery entry over them, or adopts the entry an earlier start wrote
   there (written again in place first, as in step 3), and only then zeroes the rest, so a start that stops before
   the entry is on disk leaves those bytes for the next start to find. When the entry does not fit after the mark, it
   goes to a new segment, and the bytes, in the mark's segment, are zeroed after it. A start that stopped between the
   two leaves a recovery entry, first in a later segment, whose `after` is below the batches kept before it: those
   batches are dropped, zeroed once the entry is adopted, and the entry stands (the stale mark this shows makes it
   rule 3, with Adoption). The mark's segment is truncated, though not the last in use, when it holds a complete
   batch or, with no valid seal blocks, any non-zero byte beyond the mark and every later segment in use holds only
   its header (a batch beyond the mark rules out
   seal blocks there); and a segment's header is rewritten with F only after the entry is written in it, so no stop
   leaves a later segment in use by its header alone. A start writes the header of the segment it resumes in
   again, in place, before its first sync (a header taken into use by a sync that failed may be in the page cache
   only).
3. With the mark missing, unreadable or stale, or a recovery entry adopted, first write the last complete batch, and
   the flagged batches directly before it, again in place, with the segment's header when the first of them is the
   segment's first (Recovery rule 3). Sync the kept segments and the directory. If that sync fails the engine stays read-only, leaves the old mark and
   the manifest as they are and writes nothing more; what it found is reported in health as found and recorded by the
   next start that can write.
4. Repair the manifest's tail (§7 Manifest): drop a torn last record and truncate the file after the last valid
   record, write the last valid record's bytes again in place, and sync. Then, for every lost range found (rule 2
   regions, recovery-entry gaps) none of whose positions a lost-range record holds, append a lost-range record; for
   one that overlaps a recorded loss and extends it, upward or downward, append a record that extends that loss and
   names its identity (one loss, counted once). A loss's **identity** is the first position of its range when it
   was first reported (a `possible_loss` has its recovery entry's position as identity; identities are unique within
   a kind), so a loss reported before any record holds it can be acknowledged at once, and the record,
   when written, carries that identity. Sync the
   manifest.
5. Rule 3, or rule 1 with a complete batch or any non-zero byte beyond the mark: write or adopt the recovery entry (in the open segment,
   or first in a new segment when it does not fit, its header written after it) and sync; zero the rest of the torn
   tail, or the bytes after the mark in whichever segment holds them, and batches a stopped reset's entry dropped
   (step 2), and sync.
6. Rule 3, or rule 1 with a complete batch or any non-zero byte beyond the mark: write the mark (current boot id, last position F, highest
   series id assigned, the entry's reset stamp) and sync the
   mark file and the directory. An existing mark file is first written again in place, as read, and synced, then updated in place,
   whatever its content (except a slot of unknown version, `format.md` A.10 Update); a new file is created only when none exists. Then record the entry's gap, if any, as in step 4.
7. Resume. Each consumer's deliveries are computed from its cursor file and the log (§4 Resync, §6).

A failed sync in any step leaves the engine read-only (§3 Write or sync failure); the next start begins at step 1.

## 4. Delivery (funnels)
| Rule | |
|---|---|
| Funnel | Declared in configuration: series selection (ids or name patterns), optional operator (§6), condition, consumer. A **name pattern** has exactly the three `/`-separated segments of a name, each a literal or `*`, which matches one whole segment (`plant/*/power` matches `plant/line-1/power`, not `plant/line-1/power-smoothed`); a series matches when any of its names does, aliases included. A pattern with another number of segments, an empty segment, a `*` inside a segment (`plant/line-*/power`) or more than 255 bytes is refused when routes are compiled, in a funnel as in `emits`. A funnel or `emits` a log holds from before this grammar with a pattern it refuses stays in force, read as built: each segment a literal and `*` alone a wildcard, so it selects only names equal to it segment by segment; it is refused when routed or declared again (Q-670 (b)). A name from before the rule of §2.1 is matched the same way, segment by segment: no pattern this grammar accepts selects one with another number of segments, and `*` matches an empty segment of one. |
| Routing | Array indexed by series id → funnels, built at start from the registry (§2.1); a funnel declared in configuration that the registry does not hold, or holds differently, is first written as a routing change, so the routes in force are always those the log names. Changes are log entries, kept in the registry file with their positions, and take effect at their position; catch-up applies the routes in force at each position. Patterns are re-evaluated when a definition entry commits. A consumer declares the series it may emit (`emits`, ids or name patterns, default none), recorded with its routing changes, and emits through its own handle, which refuses and counts a reading on any other series. Compilation, at start and on every routing change, draws an edge from consumer A to consumer B where a series A may emit can be selected by a funnel of B: two patterns that one name could match (segment by segment, equal or either `*`), an id against a pattern through any name of its series, or the same id. A change that closes a cycle, a consumer's own included, is refused, naming the cycle. The test is on the selections, whether or not a matching series exists yet, so a definition entry cannot close a cycle; an alias that would is refused. Readings an application sends through the store's own `emit` are outside the check. |
| Concurrency | Delivery and consumer code never run on the writer thread. A consumer emits readings through its own handle (Routing), with the non-blocking `try_emit` or from its own task. |
| Consumer state | Bounded queue (default 4 096); a durable cursor (last confirmed position) and one latch per condition funnel (§6). Written to a per-consumer file, synced at most every 1 s, at once when a confirmation changes a latch, on clean shutdown and when a `resync` carrying `forget_above` is confirmed; never ahead of the consumer's confirmation, and never naming a position above the last synced mark (invariant 26), which a write syncs first when it must. |
| New consumer | Declares `start: now | position | time` (default now). |
| Order | A consumer sees fact deliveries in position order (invariant 3). A **condition delivery**, from an evaluation on a routed fact or on the timer, is identified by `(funnel, sequence)`; it carries the last committed position at evaluation (on a routed fact, that fact's) as data, not as identity. The sequence is per funnel, kept in memory, and starts at 1 at every engine start; within one run it continues when a funnel is removed and added again under the same id. A consumer that resumes at an engine start receives no condition delivery before the `resync` that ends its resume at that start (§6 Restart and resume; §8 Restore step 2); a funnel added at run time follows §6 Latch. |
| At-least-once | After a restart a consumer resumes after its cursor and may see a delivery again. Consumers de-duplicate fact deliveries by position; commands carry ids. A condition delivery is never delivered twice: one not confirmed before a stop is not sent again, and the `resync` that ends the next resume covers it. A confirmation that changes a latch returns once the cursor file holding the latch is synced, so a confirmed condition delivery is never reported again as a transition; a state acted on but not confirmed before a stop is reported as a transition by that `resync` if it differs from the latch, so actions on conditions are at least once, like facts. Its identity holds only until the consumer's next `resync`, on which the consumer forgets the condition deliveries it has seen; a consumer that derives command ids from it adds something of its own. A `resync` may carry `forget_above` = X: the consumer must drop every position above X from its de-duplication memory, its own last confirmation included, because such a position can now name a different fact (§3 Recovery rule 3). It confirms that `resync` only when the forgetting is durable wherever that memory is kept. Records it keeps outside the engine at those positions, and series ids it first saw there, are copies of facts the store may no longer hold; the engine never asks for them to be deleted and does not protect them. A `resync` without `forget_above` requires no forgetting of positions. |
| Overflow | A full queue switches the consumer to catch-up: it reads the log from after the last position already enqueued for it in the current session (not from its confirmed cursor, so order holds within a session) up to the durable mark, and on through the flagged batches written beyond it (§3 Commit), then re-attaches to live at the next position under the writer's delivery lock (no gap, no repeat). It never blocks the log or other consumers. Time behind is measured. Catch-up applies reset points as a resume does (Resync). While a consumer catches up, its condition funnels are still evaluated, on live facts and on the timer, but send nothing, and the last state emitted to it does not change; a condition delivery that finds the queue full is not queued and switches the consumer to catch-up in the same way. On re-attaching, before any live delivery, each condition funnel whose state differs from the last state emitted to the consumer (on change: by more than the dead-band) sends it as one condition delivery; a `resync` delivered during the catch-up counts as emitting the states it carries (none, unless it ends a resume), so the same transition is not sent twice. Transitions in between are not delivered and are counted per funnel (`transitions_not_delivered`): a condition true only during the catch-up is not seen, as one true only during downtime is not (Timers). Catch-up reads from hot and cold, never from deep: where the next entry it needs is on no local tier, it ends, the consumer receives `resync`, which supersedes the positions passed over, and it re-attaches at the next position. |
| Replay | Fact deliveries produced in catch-up are flagged `replayed` and keep their original times. They are for record-keeping and are never fed to a condition operator a second time (§6). A consumer that acts on devices must not act on old deliveries (default: `received_at` more than 60 s before now; event time does not count). |
| Resync | The `resync` that ends a resume (§6 Restart and resume; after `generation_changed`, §8) carries, per funnel, the current live state and condition state evaluated now; any other `resync` carries no funnel state, except that of a consumer whose cursor file was lost (below). Sent on resume after downtime (§6), on `generation_changed` (§8), when the cursor is older than the oldest local log segment, when catch-up after overflow needs an entry that is on no local tier (Overflow; `forget_above` = the last position enqueued for the consumer, id = the mark's last position), and at every **reset point**. Reset points are: a recovery entry (`forget_above` = its `after`, id = its position), and a lost range not described by a recovery entry (`forget_above` = the position before it, id = its last position, since it may have held a recovery entry). Consecutive reset points with nothing routed to the consumer between them are delivered as one, with the lowest `forget_above` and the highest id. A reset point applies to a consumer whose position (its cursor on resume, the last position enqueued for it in a session or in catch-up) is below the reset point's id; it is delivered in position order, carrying `forget_above`, and nothing further is delivered to that consumer, live or by catch-up, until it has confirmed it and its cursor file, naming the id, is synced. A consumer that never confirms receives nothing more; the wait is measured as time behind. A cursor at or above the id is past the reset point. When a lost range is recorded at run time, consumers past it receive a plain `resync`. A reset point at the last position is the same delivery as the `resync` that ends the resume. A consumer whose cursor is older than the oldest local log segment receives no replay and one `resync` with `forget_above` = its cursor and id = the mark's last position. The **position before its start** is the mark's last position for `start: now`, the position before the first fact at or after T for `start: time`, and P for `start: position` P, after which reset points above P apply as to a cursor at P. A new consumer has nothing to forget: it receives no `resync`, its cursor file is written at once naming the position before its start, and it starts after it. A consumer whose cursor file was lost (missing or unreadable at a start, while routing changes name it; §3 Recovery rule 3) receives, before its first delivery, a `resync` of cause `NoCursor` with `forget_above` = the position before its start, carrying, per funnel, the current live state and condition state evaluated now; with no latch to compare, each condition's state is a transition. P is given as a saved point (§7 Readers, Reset stamp and saved points): kept, it is P; void above X, it is X; unknown, the registration is refused and the consumer starts with `start: now` or `start: time`. With no latch to compare, as after a restore, the `resync` that ends a resume delivers each condition's present state, true or false, as a transition. During a restore a condition or live state may be **unknown** (§2.3, §6 Conditions): `resync` reports it so for that funnel, which is no transition and leaves the latch as it is. It is not flagged `replayed`; acting on it is acting on the present. |
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
  window edge and the duplicate fast path. Over all series the windows are bounded in bytes too
  (`limits.window_memory`, Q-751): beyond it the oldest keys leave memory and are read from the log (§2.4).
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
the node.** Min and max are over readings only, in IEEE total order, −0.0 below +0.0 (Q-181).

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
  Series with a decimal scale sum in exact `i128`. A node of a decimal series records its scale: the highest stored
  scale among the current facts it covers that are not bad. Each value is rescaled exactly to it. A merge takes the
  higher of its children's scales and rescales the other child exactly; the empty node has no scale. Readers rescale
  results to the current scale. A node is thus a function of its facts alone, whatever raise entries are kept, and
  every fact fits `i64` at the node's scale, so a sum of n values stays below n × 2^63. On a series without a decimal
  scale, sums, integrals and increases are exact: each is dyadic (a sum of `f64` values and their products with whole
  nanoseconds), so a node stores it exactly (`format-tree.md` T.3, Q-180). Min and max are taken in IEEE total
  order, −0.0 below +0.0 (Q-181). Results assembled in other orders (windows, coarse queries) therefore equal direct
  computation exactly (invariants 6, 7); a result is converted to its output type once, at the end.
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
| Aggregates | avg (gauge: integral ÷ covered seconds), sum, integral, min, max, count, increase, rate (increase ÷ elapsed seconds between the readings that bound it; the result carries coverage and is flagged when a step it includes spans more than S, in force at the step's earlier reading; empty when no step falls in the window), last, time-in-state. With no reading in the window but a valid held value (not for interval series): `last`, avg, integral and time-in-state use it; `min`, `max`, `count`, `sum` and rate are empty; increase counts only a step credited to the window (Edge stretches). **Interval series:** sum is the amount in the window; rate is sum ÷ covered seconds; count, min, max and last are over the readings whose `t` lies in the window, each with its whole amount, never pro rata: they describe the readings that start in the window, so each reading is counted in exactly one of adjacent windows. Where a window edge lies inside the interval of a current reading that is neither bad quality nor displaced, their result is flagged `apportioned`, as sum is: the readings do not line up with the window; avg, integral, increase and time-in-state are refused. Edge stretches do not apply: a window edge inside an interval credits that interval's amount pro rata, amount × overlap ÷ length (end − `t`, §2.1) computed exactly (§5.6), and the result is flagged `apportioned`; sums of whole intervals stay exact for a decimal scale. An edge lookup reads the one reading whose interval can contain the edge: the reading at the interval start at or before it (fixed length: at most d back; calendar length: the start of the interval containing the edge). On an observed interval series a reading enters live state and windows only once its end ≤ `now`. A displaced reading (§5.4) is ignored and the result flagged `displaced`. |
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
  8-byte boundaries, for a valid record with a higher epoch, under the window rule of the scan bound (`format.md` A,
  common rules), which damage does not reach. An invalid record followed by a valid one was synced
  and is **corrupt**. So is an invalid record after which the search passed part of a window under that rule and
  found no valid record: the bytes it did not look at are not known to be a torn record, so they are kept and the
  record is fetched again or the manifest rebuilt, as for any corrupt record (a corrupt span with no later record,
  Q-650). Any other invalid last record is a **torn tail**: the writer drops it at start and truncates the
  file after the last valid record (§3 Order of a start, step 4). So that dropping is always safe, the last record is neither uploaded nor acted
  on irreversibly (no file it retires, moves or quarantines is removed) until a later record has been synced; the
  writer appends an empty record when it needs one, and at once after a restore record. What a dropped record
  described is found again: an unlisted compaction output is discarded (§9 row 10), an unlisted sealed log segment
  is listed again, a lost range is found again by the scan or the scrub, an acknowledgement is given again. New
  epochs continue above every epoch named by the last valid record and by any lease file, as its epoch or as the
  highest epoch its reader has read (Readers). A corrupt record is
  fetched from deep (by generation and epoch) or from the mirror path, verified, written again in place over the
  corrupt bytes and synced; where that cannot be done, the manifest is rewritten to a new file and renamed while no
  handle holds the old one open (§3 Sync primitive). Every file is self-describing: its header and trailer carry
  everything a publish record says about it (`format.md` A.14; a log segment's plaintext hash is computed from its
  bytes, A.9). **If no copy exists** (the mirror path holds none and deep has answered that it holds none, or
  none is configured), the writer **rebuilds the manifest** from the local files by itself, with the
  procedure of the operator call `rebuild_manifest` (roadmap 6.3), and continues; the files are kept, so no fact is
  lost. Only the corrupt record is lost; every valid record, before and after it, is still readable. The rebuilt
  manifest holds, in order: the rebuild record; every valid record of the old manifest, before and after the corrupt
  one, written again with a new epoch and its old epoch; the records that restate what the corrupt record held, as
  far as steps 1–3 find it; an empty record. Steps 1–3 reconstruct only what no valid record states. So that one
  record's loss leaves nothing unknown, no fact about a file or a loss is held by one record alone once a second
  record names it: every record that names a file repeats its kind, position range and plaintext hash, and every
  record that names a loss repeats its identity (`format.md` Part B). The rebuild:
  1. Lists every valid local file (hot and cold paths; nothing under `quarantine/` is live) and verifies each by its
     own checksums and plaintext hash.
  2. Takes every log segment as live. Of compacted files whose position ranges overlap, an output that names the
     others as its inputs and verifies against them by the §7 procedure is live and its inputs are retired (it holds
     exactly their facts, so either choice holds the same facts); an output that does not verify is discarded, as an
     unlisted output is (§9 row 10). Liveness is decided from the valid records and the files' named inputs alone:
     a file is retired if a valid record retires it or a live file names it as an input. A file that a valid record
     names and no valid record published was published by the corrupt record, with the inputs its header names, or
     its chunk index when it is not local (`format.md` Part B). A live file whose location a valid record gives
     (cold or deep) keeps it, to be verified there again. A live file that no local path holds and whose location
     no valid record gives is **unlocated**: queries that need it return an error, and it is not a lost range until
     cold, the mirror path and deep have each answered that they do not hold it (deep is asked for that object and
     its hash compared); it is then handled as §8 Corrupt block. The bucket is never deleted from, so a file found
     in deep is not live for that reason.
  3. Records again every lost range the files show (positions missing between the batches of a segment, between
     segments, or below the mark, as found by the scan of the sealed and unsealed log segments on hot and cold; a
     position held by a file that a valid record names is not missing) that no valid record holds, with the identity that any valid record names for it, else its first missing position,
     and with the cause that a valid record or a recovery entry gives, else unknown. A loss whose own record was the
     corrupt one is recorded again, unacknowledged; a lost acknowledgement is given again, as a torn one is (§8
     Acknowledging a loss). With one corrupt record a loss keeps its identity: an identity that differs from the
     first missing position arises only from an extension or an acknowledgement, a second record that names it.
     Where two corrupt records leave an identity unknown, the loss is listed under its first missing position,
     unacknowledged, and the rebuild record counts it.
  4. Writes the new manifest as `MANIFEST.new` (`format.md` A.12), opened by a rebuild record that carries, per file
     kind, a `file_id` floor above every id seen (`format.md` A.14), with epochs above every epoch named by a valid
     record and by any lease file (as above), and above the epochs a corrupt span with no later record is taken to
     hold (`format.md` A.14; Q-872 says the one case that bound cannot cover); syncs it; renames it over the old file
     while no handle holds that open (§3 Sync primitive); syncs the directory. A `MANIFEST.new` left by a crash is
     removed at the next start.
  A rebuilt manifest takes no file as verified in another tier until it has been verified there again (§7 inputs
  kept, §8).
  The rebuild is reported: health is degraded (`manifest_rebuilt`) until the rebuild is acknowledged, and rebuilds are
  counted. A **restore record** (§8 Generations) is never rebuilt: its content cannot be derived, and a guess could
  serve positions that were set aside. It is read from the local restore record file or from deep; where neither
  holds it the store stays read-only (`manifest_corrupt`, naming the restore record), and the way out is a new
  restore. A drill does not rebuild: a corrupt record ends the drill, which is run again.
  **Deep unreachable.** While deep cannot be reached, so that nobody can say whether a copy exists, the store is
  read-only with fatal health (`manifest_corrupt`) for `manifest.rebuild_wait` (default 15 minutes; `never` keeps it
  so until deep answers). The wait is counted on the writer's monotonic clock from each start, once that start has
  asked deep and failed; a restart starts it again. Accepted limit: a process that restarts more often than the wait
  stays read-only; the operator call `rebuild_manifest` is the way out. When the wait ends, or deep answers first,
  the store leaves the read-only state by itself, without a restart, and the start resumes from step 4 (§3 Order of a
  start): the one exception to §3 Write or sync failure, as this read-only state comes from no failed write. Steps 1
  to 3 of that start (the scan, any truncation under rule 1 and the sync of the kept segments) run before the wait;
  a failed sync among them is a fault as usual and ends the start. The start pauses before step 4, which repairs
  the manifest, and writes nothing while it waits. After
  the wait the writer rebuilds as above, **provisionally**, and continues with health degraded (`manifest_rebuilt`,
  provisional). While provisional it uploads nothing (deep cannot be reached then anyway), evicts and removes no file
  and acts on no retirement, so nothing irreversible rests on the reconstruction. A loss whose own record was the
  corrupt one and whose file has already left the local tiers (§8 Eviction at 90 days) cannot be found again from the
  local files, so `losses()` lacks it until deep answers. When deep answers with a copy of the corrupt record, the
  copy replaces what was reconstructed, by appended records; when deep answers that it holds none, the rebuild is
  final. A provisional rebuild cannot be acknowledged: `acknowledge_manifest_rebuild` is refused (`api.md` `WriteError::NotFinal`) until the rebuild is
  final, and one acknowledgement is given then. A rebuild record that no acknowledgement record names is taken as
  provisional at every start, so that the holds survive a restart: they apply until the mirror path and deep have
  each answered, in that run, for every corrupt record it stands for (the rebuild record lists each corrupt span,
  `format.md` A.14 field 7), or are not configured; a copy then found replaces the reconstruction as above. This
  includes a rebuild that was final before the restart but not yet acknowledged: if deep cannot be reached at that
  start, the rebuild is provisional again, with the holds and the refused acknowledgement, and health stays degraded
  (`manifest_rebuilt`, provisional) until deep answers. Two corrupt records read as one span
  when nothing valid lies between them; a span whose epochs leave room for more than one record (the next valid
  epoch more than 2 above the last valid one before it), or that has no later record, counts as two for step 3.
- Compaction of compacted files (merging overlapping chunks created by late data) publishes outputs and retires
  inputs in one record. Policy: merge when a series' chunks overlapping any 15-minute span exceed 4.
- A sealed file's **inputs are kept** until the file is verified in another tier. Log segments are never retired by
  compaction (§8).
- **Retired files** are removed locally only after the grace period (1 hour of the writer's monotonic clock since the
  later of the retirement record and the writer's start), once the files that replaced them are verified in another
  tier, and when no live reader lease names an older epoch.
- **Leases:** a lease file holds the epoch, the highest epoch the reader has read, the boot id (from the same source
  as the mark's) and a refresh counter that the reader increments at every renewal. The writer expires a lease when its counter has not changed for
  1 hour of the writer's monotonic clock since the writer first observed that value; at writer start, every lease of
  the current boot counts as first observed then. A lease is void only when both boot ids are non-zero and differ.
  The writer looks at leases at its own turns, at most once a minute, with no timer of its own: a late look only
  makes an expiry later.
- **Readers** read under cuts: a cut is the manifest epoch whose files it reads, with the durable position and reset
  stamp it was taken at. A reader's lease names the lowest epoch of its live cuts and records the highest epoch the reader
  has read, written before any file is read under a higher one; the reader renews it at least every 30 minutes and at
  the check of a held cut; a cut ends `cut_max_age` (`api.md` `ReaderConfig`; at most the grace period of Retired
  files) after the last manifest check that found its epoch current; a lease found gone or changed ends every cut. A reader needs nothing from the writer's memory: it reads via
  the manifest, the documented layout, tree files and log segments up to the durable mark. A reader that reads a recovery entry (§3) voids what it took above the entry's
  `after`; it compares the mark's reset stamp before and after a pass that took something (a batch, a void, a
  region or a moved resume point; a pass that took nothing has nothing to discard, Q-851), and discards what it read
  if the stamp changed meanwhile; a call with a materialised result then reads once more under the new cut, and a second change
  ends it with `CutExpired` (Q-850). A reader reads nothing without a lease, renewed within `lease_refresh`, that names an epoch at or below
  its cut's: if a lease write that a call needs fails, the call returns the error. Positions are read in order, so a reader catching up (a consumer, a live read's tail) that
  cannot read a file returns the error for that file and reads nothing at or past it until it can; a query (roadmap
  9.0f) returns the error only if the file's segment index (`format.md` A.8) lists one of its series or the file has
  entries, or the index cannot be read either, and otherwise passes over it. A manifest record a reader cannot take an epoch from is an
  error of the manifest for the reader. A reader parses only records appended past those it holds (Q-852), so one
  that has parsed the manifest keeps serving after an earlier record is damaged, from what it read of the valid
  records, until the manifest is rebuilt or the reader opens again. Accepted residuals: a reader may read a mark that is not yet synced (from
  the page cache, before a start's sync of it or after that sync failed); the new reset stamp of the start that then
  truncates voids what it took. Corruption events reported by a pass that a start overtook (it truncated and wrote
  past the old mark before the reader read the mark again) may be wrong; the next refresh's check of the last batch
  still finds the rewrite.
- **Derived files** (`format.md` A.18): the log directory (the sealed segments, with their sequence and commit-time
  ranges, checked against the catalogue), the series directory (per series, as of a position: the latest recorded
  `t`, the lateness reference, counts, the live candidate and the next pending fact) and the status file (the
  writer's counters, health and pressure flag). Only the writer writes them, on its maintenance thread; they are
  checksummed, renamed into place and never synced or backed up, and none names a position above the last synced
  mark. A reader treats a missing, invalid or behind one as absent and reads the log instead, up to an allowance of
  sealed bytes; the files speed reads up and never change a result. A series directory, or a root, is a function of
  the kept log while its `through` with the reset stamp it records, judged as a saved point (below), is kept or void
  above no lower position; the writer, which judges at each start, keeps one written under the current stamp or with
  `through` at or below the `after` of the recovery entry its start wrote (Q-776); a position at or below a series
  directory's `through` lost since it was written (a new lost range or one extended under its identity) makes it no
  longer one. A part of the log directory whose
  segments the catalogue no longer describes (a replaced segment, a restore, a manifest rebuild) is not used.
- **Reset stamp and saved points.** The mark carries the current reset stamp: zero until the first reset, then
  the stamp of the last recovery entry a start wrote (§3 Recovery rules 1 and 3) or of the last restore record (§8
  Generations; its sequence is the restored generation's first position, its nonce fresh). A position kept for later
  use (an export continued by position, row 7b; `start: position`, §4 Resync) is kept as a **saved point** (P, S):
  P and the stamp S of the mark it was read under. It is judged against the current mark (generation g', stamp S'):
  - P's generation is g' and S' = S: **kept**; nothing was reset since.
  - Otherwise the **later resets** are the recovery entries and restore records, in log order, after the one that
    carries S (every one of P's generation and after when S is zero, or when no kept entry or record carries S), and
    the lost ranges recorded since: each lost-range record carries the reset stamp in force when it was recorded (the
    mark's or, with the mark lost, that of the last kept recovery entry), and one that carries none counts as
    recorded after every reset the start found. The record of a kept recovery entry's gap (its positions K + 1 to
    F − 1, §3 Recovery rule 3, recorded with that entry's stamp) is part of that entry's reset, not a later lost
    range: the entry's `after` already covers it, and it voids nothing saved under that entry's stamp. P is **void above** the lowest of P, each later recovery entry's `after`, each
    later restore record's restored end in P's generation, and the position before each later lost range of P's
    generation; the holder forgets every position above that and continues from it.
  - If no kept recovery entry or restore record carries S' (the reset the mark names is in a lost range, or was
    lost with a manifest record), the answer is **unknown**: the holder starts again as if it kept no position. An
    adopted recovery entry written before the stamp existed carries none; the mark then takes a fresh stamp that no
    entry carries, so every saved point is unknown until the next reset.
  A reader opens the mark by path for every read of it and
  reads files by position, never mapping them (`format.md` A.10, A.13), so it never reads past a mark the writer no
  longer maintains (invariant 11).

## 8. Tiers, backup and rebuild
| Tier | Location | Holds |
|---|---|---|
| Hot | fast local disk | log segments < 90 days, current detail, revised-key index, version index, chunk indexes, registry, the whole stored tree |
| Cold (optional) | another local path | older revision chunks and sealed log segments; moved current detail |
| Deep | S3-compatible bucket, via a pluggable backup sink | every sealed file, every manifest record, tree checkpoints (with the registry, latest recorded `t` and lateness reference, largest magnitude and highest stored scale of each decimal series, revised-key index and version index), generation claims, last-uploaded markers, and the store descriptor (`format.md` A.12), uploaded once before any other file of the store; keys and object forms in `format.md` A.17. Until that upload the local descriptor is the only copy, an accepted limit; an operator repairs a damaged local copy from the bucket's (roadmap 8.2) |

| Rule | |
|---|---|
| Continuous backup | Each sealed file is queued for upload once; upload never blocks ingest or delivery. After its files, each manifest record is uploaded (the last record only once a later record has been synced, §7 Manifest); a tree checkpoint at least daily, and only after all log up to its position is backed up. Each tree checkpoint carries the registry file, each series' latest recorded `t` and lateness reference, each decimal series' largest magnitude and highest stored scale (§2.1), the revised-key index and the version index, as of its position. Cursor files are not backed up. |
| Encryption | Always: there is no unencrypted mode. On the machine before upload, with a key supplied by the embedding application: XChaCha20-Poly1305 in authenticated 64 KiB chunks under a header carrying the format version, the algorithm, a key check value and the nonce base, so that a range decrypts on its own and a wrong key is told from damage. The nonce base is derived from the key, the object key and the plaintext's hash, so the envelope of a file is the same at every attempt (`format.md` A.17). |
| Hashes | The manifest records SHA-256 of the plaintext at seal (hot and cold verification) and, per upload, SHA-256 of the ciphertext, sent with a single-part upload. The bucket must reject a mismatch and its reported hash must equal the manifest's; only then is the file backed up. Scheduled check: re-read reported hashes; sampled download-decrypt-verify proves key and content. |
| Bucket | The engine never deletes from it. Versioning or object lock required. It must create an object only where its key is absent, and must check the SHA-256 sent with a single-part upload and report it. `attach_backup`, and a restore before it claims, check both with a probe (`format.md` A.17) and refuse a bucket that fails (`BackupRefused`); there is no weaker mode. Versioning and object lock are the operator's to confirm. |
| Eviction at 90 days | Revision chunks (age per series, "never" allowed) and sealed log segments leave hot only after they are verified in cold or deep. A sealed log segment whose index carries `has_labels` (`format.md` A.8) is removed locally, by this or any rule of this section and by the release of a compacted segment (Q-745), only once a label part covering its label changes has been written and read back, and, on a store with a backup, uploaded and verified. Current detail and the stored tree stay, except as the retention class allows. A file that holds a lost range leaves the local tiers (by any rule of this section) only after the manifest record of that loss is uploaded or in the mirror path, so that a manifest rebuild never needs the file to find the loss again (§7 Manifest). |
| Retention class | For a diagnostic series (§2.1), raw detail (current and revision chunks) whose span ends more than the retention period before `now` leaves hot once verified in cold or deep; for revision chunks, the earlier of this and their own age applies. The stored tree stays hot. Nothing is deleted: such detail is fetched on demand like moved current detail, so, as with any moved detail, the duplicate check for it can be unchecked (§2.4) and window edges over it are approximate (§6). |
| Fill pressure | Above 70 % fill, verified sealed log segments leave first, then the oldest current raw detail (verified first, fetched on demand); the tree, chunk indexes, revised-key index and version index never move (§12 D4). |
| Behind consumers | A consumer whose cursor, or whose catch-up position after overflow, is older than the oldest local log segment receives `resync`, not a replay. The engine never downloads from deep for catch-up. Its `resync` carries `forget_above` and an id as §4 Resync gives for the cursor case and the catch-up case; recovery entries in segments that are no longer local are not needed. |
| Corrupt block | File quarantined (a quarantine record names it, and it stays under `quarantine/` until its corruption is seen, Self-repair); the engine fetches the copy from cold, else deep, verifies the plaintext hash and swaps it in by a manifest record. If no other tier holds a verified copy, the file is compacted again from its kept inputs, verified by the §7 procedure, recorded with its new identity and hash, and swapped in by one manifest record that retires the quarantined file. Queries over the range return an error until then. Never repaired in place. Because inputs are kept until a file is verified elsewhere (§7), the only single copies are the open log segment, a sealed log segment not yet uploaded and not yet compacted, and a manifest record not yet uploaded (§7 Manifest: its corruption loses no fact; the manifest is rebuilt from the local files). Corruption of such a log segment loses only the damaged region, from the last valid batch before it to the next valid batch after it or, with none after it, to the position before the next segment's first (in the open segment, to the mark's last position, as in §3 Recovery rule 2), reported when found and recorded as in §3 Recovery rule 2: for the open segment by the next start that can write (§3 Order of a start, step 4), for a sealed segment by the record below. For a sealed segment the valid batches are copied unchanged to a new segment, verified by their commit checksums, and one manifest record adds it with its new identity and hash, retires the quarantined file and records the lost range; the error ends with that record and the new segment is backed up like any sealed file. A sealed segment that no manifest record lists and that fails its checks when a start describes it (`format.md` A.9) is neither a `data_lost` nor acknowledged: it degrades health (`segment_damaged`) while the store runs and again at each start until it is repaired, and it is not listed, so not backed up, until then (Q-221 (c)). One that cannot be opened or read is retried at a growing interval, and once that interval reaches its cap it degrades health (`segment_unreadable`) until a read succeeds, since an unlisted segment is never backed up (Q-640). An optional local mirror path for sealed log segments and the manifest (configuration `mirror_path`, off by default) closes this window for sealed segments and manifest records. A restore reads the files it lists from deep alone: a listed file that the bucket has lost or corrupted is handled as here with deep as the failed tier (Restore, Failures and end). A loss in the bucket found outside a restore is handled by Self-repair. |
| Self-repair | A loss runs through three stages by itself. **1. Copy.** Where another tier holds a verified copy of a damaged file, the copy is tried before anything is recorded: the file is quarantined, fetched, verified and swapped in (Corrupt block); there is no lost range and no reset point. A catch-up that meets the quarantined file waits for the swap, its time behind measured; a query over it returns the Corrupt block error until the swap (Q-245). While deep cannot be reached the store waits, with a backup reason in health; while deep answers but reports the object absent, the store waits for the lost-object rule (Lost in the bucket) before it records anything, so a lagging listing never makes a lost range. A copy is swapped in by a manifest record, so never while the store cannot write; what a scrub found then stays found until a start that can write records it. A quarantined file holds the Corrupt block error, fatal in health, only while a swap can still happen. A loss already recorded (found by a start, or found while the store could not write, before a copy could be swapped in) whose copy is then found is restored in the same way and closed as restored in full: its positions hold the same facts again, consumers are not sent them (the reset point's `resync` already covered them, invariant 2), and derived structures are recomputed over them as for late data. **Lost in the bucket** (Q-85). The scheduled check (Hashes) judges an object as a restore does (Restore, Failures and end; Q-82): lost when the bucket reports it absent on 3 lookups spanning at least 10 minutes while it reports the store descriptor present, corrupted when its reported size or hash differs from its copy's or 3 downloads with the right key fail; any other fault, a transient one included, is retried and never counts. A file so judged is uploaded again from a verified local copy where one exists (a corruption repaired, no loss); otherwise the positions of it that no other file holds are a lost range of cause lost in the bucket, recorded and reported as `data_lost`, and go to stage 2. **2. Re-send.** What no copy holds is recorded as a lost range and offered to the application as a **re-send request**: the loss's identity, its positions, the commit times before and after it, and, where the segment index survived, each series with the span of `t` it held there (otherwise every series). Sources answer through ordinary ingest, where the duplicate rule drops what is still recorded; re-sent readings take new positions (invariant 1). A request is offered to every `repairs()` stream open before the stage ends, a stream opened later included, so that a loss a start records reaches an application that subscribes after opening. The stage ends when the application calls `repair_done(identity)`, or `repair.resend_wait` (default 24 h) after the loss's latest lost-range record (for a loss a restore record carries open, after that restore record), or at once with `repair.resend_wait` 0, by which the application says nothing listens (Q-242). A lost range with no copy starts at stage 2: one of an unsealed log segment (§3 Recovery rule 2, a lowest unsealed segment damaged, zeroed or missing included, Q-321), a recovery entry's gap, and one a restore records (Restore, Failures and end). **3. Closed.** The engine appends a **closing record** (an acknowledgement record with its outcome: restored in full, re-send ended, or by an operator, Acknowledging a loss) and the loss no longer degrades health. It stays in the list of losses for ever, with its state. With `repair.acknowledge = manual`, a loss whose stage 2 has ended waits for `acknowledge_losses`, which closes it as by an operator. A torn closing record is dropped as a torn acknowledgement is, and the loss is closed again with the same outcome (how a start tells "restored in full" again is Q-249). `acknowledge_losses` does not close a loss whose copy is still being brought back (stage 1): it closes as restored in full when the copy is swapped in (Q-248). A closed loss is not opened again by a copy found later (Q-246), in this generation or any later one: an object that answers again after its loss was closed gets no copy-verified record, so no restore brings its facts back (Q-600); its quarantined file then holds no error and no health reason, since no swap can happen, and leaves `quarantine/` when its corruption is seen; one whose range grows is open again, at stage 2. A loss closed as restored in full whose positions are lost again (its file found damaged or its object judged lost once more, with no copy left) is open again under the same identity, at stage 2: the new lost-range record names that identity, a closing record "restored in full" closes only the lost-range records written before it, and the new finding is a new corruption. **Attention.** Health asks for a person (`attention`) for a fault, a failed or stalled backup, each corruption until it is seen, repaired or not, and repeated corruption (`repair.suspect_after`, default 2 corruptions in 30 days, counted by the records that name them, Q-247). Repeated corruption also degrades health (`storage_suspect`) while the count in the window is at or above the threshold, seen or not, so that failing storage stays visible. A **corruption** is each finding of a file damaged on any tier or lost by the bucket outside a restore (each named by its own quarantine record, so a file found damaged or lost again after its repair is a new corruption and alerts again), a region of an unsealed log segment found corrupt or of invalid content by a start (§3 Recovery rule 2, Q-321) or a file a restore finds lost or corrupted (Restore, Failures and end), each named by its loss's first lost-range record, or a corrupt manifest record (§7 Manifest); a truncated tail and a `possible_loss` are not. A corruption keeps its alert through its repair and its loss's closing until a manifest record names it as seen: `acknowledge_corruptions`, `acknowledge_losses` naming the loss it caused, or, for a manifest record, `acknowledge_manifest_rebuild`. A restart, the passing of time, a read of health and the end of the repair never clear it, and an alert for a corruption does not degrade health (Q-240). Corruptions, their alerts and the repeated-corruption count are read from the manifest at every start, from the quarantine records and the lost-range records of the losses `losses()` reports (each loss once, under its identity), never from a running count kept by a writer, a deliverer or a reader: a restart, or a record that extends a loss or finds its range again, reads the same corruption and neither alerts again nor counts twice, while a new finding of a file has its own quarantine record and does both. After a manifest rebuild (§7 Manifest) the corruptions are those the carried records name, the corrupt manifest record itself, named and dated by the rebuild record, each file under `quarantine/` that no carried record names, dated at the rebuild record, and each lost range found again with cause unknown, dated at the rebuild record; the count is then low only by corruptions already repaired and seen whose records lay in the corrupt span, less one (Q-244). A restore record carries the closing outcomes, the corruptions seen and the corruptions not yet seen of the generations it restores from, so no outcome or alert is lost across a restore (Q-241). |
| Acknowledging a loss | `acknowledge_losses` appends one manifest record naming, for each `data_lost` acknowledged, its identity (the first position of its range when it was first reported, §3 Order of a start, step 4; a loss reported as found but not yet recorded can be acknowledged) and its first and last position as reported then; a `possible_loss` holds no degraded state and needs no acknowledgement (§11 Health). It does what stage 3 of Self-repair does, by an operator: the record is a closing record with the outcome "by an operator" (`format.md` A.14), and in the same record names as seen the corruption each loss came from. A closing record that Self-repair writes names one loss, its outcome, and for "restored in full" the number of positions restored. The list of losses is kept for ever; an acknowledgement or a closing record only ends the degraded state. A loss is acknowledged (closed) while a record names its identity with a range that covers its present range, so a range that grows, upward or downward, needs a new acknowledgement. Acknowledging an acknowledged loss, or none, writes nothing, except to name as seen a corruption not yet seen. A torn acknowledgement record (§7 Manifest) is dropped and the loss is degraded again. The call is refused while read-only and in a drill. A restore record carries the lost ranges, acknowledgements and recovery-entry loss reports of the generations it restores from, each with its generation, so a loss whose entry or record the restore sets aside or does not restore stays listed; a lost range the restore itself records is unacknowledged. |
| Upload stalled | Files queue locally, health degraded, nothing evicted. |
| No deletion | Every fact is kept forever in at least the deep tier; the engine has no purge. An exception is a fact held only by a listed file that the bucket has lost or corrupted when a restore reads it, which becomes part of a lost range (Restore, Failures and end), and a fact held only by a file that the bucket has lost or corrupted when no local copy is left, found outside a restore, which becomes part of a lost range of cause lost in the bucket (Self-repair, Q-85). |
| Re-ingest | Exported or foreign data goes through the normal ingest path, in batches. |
| Restore drill | The restore procedure is testable end to end; operators should exercise it regularly. A drill restores read-only and claims and uploads nothing. Its restore record is kept locally only, for the generation a restore would claim (one above the highest claimed), which stays unclaimed; the claims are not read again and the drill is never superseded. A drill of a live store can see a record uploaded between its listing and its tail check (Generations) and is retried, as for a lagging listing; one whose listing lags behind the last-uploaded marker fails after ten tries (Q-614); on a bucket whose listings lag longer than the time between two record uploads it never passes, so there a drill runs from a paused copy: a copy of the store's objects made while the store uploads nothing (Q-612 (d)). |
| Generations | The first store of a bucket writes generation 0, which counts as claimed. Before accepting ingest, a restore claims the generation one above the highest claimed, by a create-if-absent marker in the bucket; if the marker already exists it takes the next. A claim is never withdrawn, also when its restore fails. Manifest records are keyed by generation. Every generation above 0 begins with its **restore record**, or, after a manifest rebuild in that generation, with a rebuild record followed by the carried restore record (§7 Manifest): its generation, its reset stamp (the generation's first position and a fresh random nonce, which the mark then carries, §7 Readers), checkpoint and restored end, the restored ends carried from the restore record it read (each with its generation), the lost ranges, acknowledgements and recovery-entry loss reports of the generations it restores from (each with its generation, Acknowledging a loss), and every file it restores from, set-aside segments included, each with its hash, tier and position range. The records of one generation are therefore complete without those before it, and a restore reads the records of the highest generation below its own that has any in the bucket. The restore record is synced locally, in the manifest and in the local restore record file (`format.md` A.1 kind 12), then uploaded before any other file of its generation; the restore then reads the claims again and, if a generation above its own is claimed, stops before accepting ingest (superseded, fatal health). A writer that finds a higher generation claimed stops uploading and becomes read-only (fatal health, §3); what it uploads after the later restore has read its records stays in the bucket, in no later generation's records, and is lost to them as in a disaster. A restore keeps the store id of the files it restores (`format.md` file header), and backup is refused on a bucket that holds files of another store id. A restore from a bucket with no manifest records is refused. Manifest records are uploaded in epoch order; a record that cannot be read stops the upload of those after it. Each names the record before it in its manifest (`format.md` A.17), so a restore checks the records it reads, and those listed after them, for one missing from its listing: one the bucket reports is a lagging listing and the restore is retried, one it does not report fails the restore (Q-611); it also asks the bucket for the records a few skipped epochs could hide after the last listed, and after the last it reads, and is retried when one is there (Q-612). After each record the uploads put a **last-uploaded marker** naming it, the first record of the latest manifest uploaded and the records a restore of the records uploaded reads (`format.md` A.17); a full restore puts the first after its restore record. A restore reads the latest marker of the generation before it lists its records (one that cannot be read is passed over for the one before it, and the restore reports that it was), and fails, claiming no generation over an earlier state, when the bucket does not hold a record the marker names, when the records it reads end before those the marker names, or when no marker can be read; a record it names that the listing does not show is a lagging listing, tried at most ten times by one restore (Q-614). A generation with records and no marker fails the restore unless the operator accepts restoring it without the check; a store still uploading puts a marker at its next start. Restoring what the bucket holds when a marker names records it does not hold, reported record by record, is an operator's choice, off by default (Q-962). A generation that lists no records and has a marker is not skipped. A rebuilt manifest's records (§7 Manifest) are uploaded in order under their new epochs, once the rebuild is final, so the rebuilt manifest is self-contained; a restore that finds rebuild records in the generation it reads starts at the last of them whose rebuilt manifest is whole in the bucket (its closing empty record uploaded) and reads up to the next rebuild record; the records before it stay in the bucket and are not read. Until a rebuilt manifest is whole, a restore reads the records uploaded before its rebuild record (Q-108). |

**Restore** (onto an empty disk):
1. Claim a generation and read the manifest records it restores from (Generations above). Each restore record, the
   carried ones and the new one, **sets aside** the positions above its restored end in the generations below its own;
   a position set aside once stays so in every later restore. The **restored log** is the positions held by the listed
   files that are not set aside. The **restored end** is its last position before the first gap that is not a recorded
   lost range (§3) and is not followed directly by a recovery entry whose `after` is the position before the gap. Such
   a gap is recorded as a lost range by the start that wrote the entry or the next one that can write (§3 Order of a
   start, steps 4 and 6), before that segment can be sealed; as the sealed-segment record is appended after that
   start (`format.md` A.14), the end follows from the records alone, before any file is downloaded (Q-290). The
   scan for that gap covers only the generation whose records are read, from its sequence 1; if
   that generation holds no position, the restored end is that of the restore record read. Positions of earlier
   generations at or below a recorded restored end were settled by that restore, and a file missing there is a failure
   of this restore (below), not a new restored end. Write and upload the restore record (Generations above) before
   ingest is accepted. Download the latest tree checkpoint at or below the restored end (labelled derived-unverified),
   with its registry file, latest recorded `t` and lateness reference per series, largest magnitude and highest
   stored scale per decimal series, revised-key index and version index, and the sealed log segments after its position, and the chunk index of every compacted file the restore
   record lists, whatever tier the record gives the file, so that chunk indexes are on hot before ingest is accepted
   (§5.6); replay the log segments into the tree, which brings the registry, latest
   recorded `t`, lateness reference, largest magnitude, highest stored scale and version index up to the restored
   end. With no checkpoint, download the whole
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
   Positions of the new generation start at sequence 1. A full restore writes the generation's first mark without a
   boot; only that mark is read as no mark, by the generation's first start, which writes it again with its boot
   (Q-260). A drill appends no recovery entry and writes no mark after
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
A crash before the record starts the restore again with the next generation. The record reaches the manifest whole (written to a
temporary name, synced, renamed), so a directory holding only that temporary file is a restore that did not reach
its record: it is removed and the restore starts again (Q-39 (f)).

**Failures and end.** The restore record is written before any file is downloaded, so a restore that stops before it
leaves nothing local; `restore` refuses a directory that holds files and no restore record. Until step 1 is complete
no ingest is accepted: a failure in it, other than a lost or corrupted file (below), leaves the engine down with fatal
health (`restore_failed`, naming the file) and a restart resumes it. After that, a file that cannot be fetched, or
whose plaintext hash fails, is retried, with degraded health naming it and the reason, while the others continue; the
restore stays in progress. A listed file that the bucket no longer holds, or has corrupted, in step 1 or later, is
treated as §8 Corrupt block with deep as the failed tier: what it held is compacted again from the other listed files,
and the positions that no other listed file holds become a lost range, recorded and reported as `data_lost` (§3
Recovery rule 2); the restore then completes without them. Facts of a log segment found lost in step 1 that other listed files hold
(such as compacted chunks) are fetched in step 1, before ingest is accepted, so that the version index, each
series' latest recorded `t` and each decimal series' largest magnitude stay exact at the restored end (§2.4, step 2). A lost or corrupted tree checkpoint is
replaced by the latest earlier checkpoint the records list, or, with none, by the whole sealed log, as step 1 allows.
A wrong key is told by the key check value of the bucket's store descriptor before any file is judged: the restore
fails, recording nothing. A listed file is lost when the bucket reports it absent on 3 tries spanning at least 10
minutes while it reports the store descriptor present, and corrupted when its reported ciphertext hash differs from
its copy's, or when 3 downloads with the right key fail its ciphertext or plaintext hash or authentication; any other
fault is retried and never counts (Q-82). A definition entry lost this way is lost as §2.1 says (only the kept log applies; its facts are orphaned); how the registry file, trees and indexes are brought back after it is open (Q-33). A file
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
| 7 | Process crash after write, before sync | Same boot: a complete flagged batch (§2.1 `durability`), acknowledged and delivered at its write and still in the page cache, is kept, written again and synced, with no reset point (§3 Recovery rule 1); anything after it, or after the mark with none, is truncated on restart and was never delivered; a complete batch or any non-zero byte truncated this way is followed by a recovery entry and reported as `possible_loss` (§3 Recovery rule 1). With the mark lost, recovery treats it as a different boot and may keep the batch, which was never acknowledged; an invalid tail is truncated and described by a recovery entry, which reports the lost mark and the tail as `possible_loss` and positions named between the last kept position and the entry as `data_lost` | kill loop |
| 7a | Power loss after write, before sync | New boot: torn tail truncated by checksum and reported as `possible_loss` (a flagged batch acknowledged and delivered at its write may be among it, or may have left no byte, so the entry is a `possible_loss` whenever a series is at the `written` level at K; a flagged batch missing before a complete one is a lost range, rule 2), and positions named between the last kept position and the recovery entry as a `data_lost` lost range (cause truncated tail); complete batches synced on recovery before use; a recovery entry at the first new position; every consumer below it receives `resync` with `forget_above` before any later position, on this start and on every later one until its synced cursor has passed the entry | power-loss rig |
| 7b | Export continued by position across a power loss | The export keeps a saved point (§7 Readers); judged against the mark after the restart, nothing above the position it is void above is kept as valid, and an unknown answer starts the export again | power-loss rig |
| 7c | Crash on the same boot after a rule 3 recovery, before a consumer received its `resync` | Rule 1 start; the replay meets the recovery entry before any reused position | kill loop |
| 8 | Crash after sync, before delivery | Delivered on restart from each consumer's cursor | kill loop |
| 9 | Crash during compaction | Output unpublished and discarded; inputs intact | kill loop |
| 10 | Crash around the manifest record | Before: output discarded. After: inputs retired. Never both live | kill loop |
| 11 | Torn or corrupt block in a sealed file | Quarantine; fetch from cold/deep, else compact again from kept inputs; verify, swap by manifest; errors meanwhile. Log segment with no other copy: valid batches copied to a new segment, the damaged region recorded as lost, `data_lost` | fault injection |
| 11a | Corrupt block inside the open segment, or below an old boot's mark after a power loss | Later complete batches kept; range recorded as lost, `data_lost`; positions not reused; a reset point (§4 Resync) for cursors below its end | fault injection |
| 12 | Sync returns an error | Read-only, fatal; a restart on the same boot truncates to the durable mark, after keeping the complete flagged batches beyond it that the page cache still holds (acknowledged, §3 Recovery rule 1; those whose clean pages were evicted are lost unreported, open, Q-254); after a reboot the batch may be kept (never acknowledged), and flagged batches as row 7a | fault injection |
| 12a | Mark deleted or damaged on the same boot after a failed sync | Rule 3; the last complete batch is written again and synced before use; kept only if that sync succeeds, else read-only; reported as `possible_loss` with the mark's state, and positions named between the last kept position and the recovery entry as a `data_lost` lost range (cause truncated tail) | fault injection |
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
| 31 | Value NaN or infinite, a negative counter value, `Absent` of any supplied quality, or a decimal value out of range | Recorded as bad quality without a value, the supplied quality kept for the duplicate rule; counted; out of range also reported in the reading's outcome | unit |
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
2. Every delivered position is durable or belongs to a flagged batch already written (§3 Commit), except where invariant 10 allows a loss (a lost range under §3 Recovery rule 2, §8 Restore or §8 Self-repair, or a tail truncated by rule 3, a flagged batch that left no byte included); every durable position routed to a consumer (an orphaned fact, §2.1, is routed to
   none) is delivered to it, or superseded by a `resync` covering it.
3. For any consumer, fact deliveries are in position order.
4. The current view equals "highest rank per `(series, t)`" (§2.2) over all facts in the files the store holds,
   positions set aside (§8 Restore) and orphaned facts (§2.1) excepted.
5. Every stored node is bitwise equal to its canonical computation over the current view of its span (nodes flagged
   `summary_stale` excepted; lost ranges excepted, open, Q-33).
6. `merge` is associative with the empty node as identity and agrees with direct computation exactly (§5.6).
7. A window result equals direct computation over the current view at evaluation time (exactly, §5.6; results
   flagged `approximate_edges` or `summary_stale` excepted).
8. Rebuilding derived structures from log entries yields exactly the live structures, under the tzdata version the log
   names (lost ranges excepted, open, Q-33).
9. Compaction, tiering and backup preserve the set of positions outside lost ranges, each with identical content;
   restore preserves the positions of the restored log (§8) outside lost ranges it records (§8 Restore, Failures and
   end) and keeps the positions set aside, except those in a set-aside segment recorded lost.
10. No operation removes a durable fact. The only losses short of a disaster are corruption of a log segment, open
    or sealed, with no other copy (§3 Recovery, §8 Corrupt block) or a listed file that the bucket has lost or
    corrupted when a restore reads it (§8 Restore, Failures and end) or, with no local copy left, when a scheduled
    check finds it (§8 Self-repair), each recorded as a lost range and reported as `data_lost`, and a tail truncated by §3 Recovery rule 3 after a reboot or with the mark lost, reported as
    `possible_loss`, with the positions between its last kept position and its recovery entry recorded as a lost range
    (cause truncated tail) and reported as `data_lost`; after a reboot with a readable mark it can hold about one mark-sync interval of acknowledged
    batches of facts (§3 Commit), and with the mark lost it is unbounded. On a store whose file system is unmounted
    within one boot, one batch committed since the last mark sync, whole or damaged, can be truncated by §3 Recovery
    rule 1, reported as `possible_loss` and delivered as a reset point (accepted limit). Flagged batches (§2.1
    `durability` `written`) written since the last log sync that returned before the event (the writer starts that
    sync at the latest `max_unsynced` after the first of them) can also be lost by a power loss, a kernel crash, or a
    failed sync followed by a reboot: in rule 3's tail or with no byte left, reported as `possible_loss` (Q-251), or
    missing before a complete batch, a lost range reported as `data_lost` (Q-257); a crash or restart of the process
    loses none the page cache still holds (§3 Recovery rule 1). A remount within the boot (open, Q-250), or the
    eviction of pages whose write-back failed before a restart on the same boot (open, Q-254), can remove them with
    no report. Corruption of a manifest record with no other copy loses no fact; the manifest is rebuilt
    from the local files (§7 Manifest).
11. No reader observes a position beyond the durable mark as the writer holds it in memory, and no consumer does
    except a position of a flagged batch already written (§3 Commit), which the writer's own queries and live state
    also see (Q-259).
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
    alone (exactly, §5.6; results flagged `approximate_edges` or `summary_stale` excepted); for a calendar-length
    interval series also of the tzdata version in force.
22. For an interval series, the sums over adjacent windows add up to the sum over their union and their counts to its count (exactly, §5.6,
    also where an interval is apportioned; results flagged `approximate_edges` excepted); displaced readings
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
27. The list of losses never loses a loss, across restarts, restores and manifest rebuilds, and each loss keeps one
    identity however its range grows, except a loss whose identity no valid record names after a manifest rebuild
    (two corrupt records), which is listed under its first missing position and counted, and, while a rebuild is
    provisional, a loss whose own record was the corrupt one and whose file has left the local tiers, listed again
    when deep answers (§7 Manifest);
    acknowledging or closing a loss changes only its state and the health state, and a loss closed by self-repair
    stays listed (§8 Self-repair, Acknowledging a loss).

## 11. Interfaces and configuration (outline; signatures follow in the API document)
| Interface | Contents |
|---|---|
| Ingest | `emit(reading, t_source, intent: normal | revise, changed_at)` → recorded(position, the durability level it was acknowledged at, with `rounded` or `out_of_range` where they apply) / duplicate / refused(reason); waits under back-pressure. Exports carry `changed_at` |
| Query | current, as-of P or T, history, tree level for a span, window evaluation; each result carries coverage and flags |
| Consumers | register(start, emits), deliveries, confirm(position), latches; `resync` (with `forget_above` at a reset point, §4 Resync) and `generation_changed` events |
| Health | counters (duplicates, unchecked, refused, regressions, outranked on arrival, evaluations unknown, orphaned facts, back-pressure wait, lag), persisted with the checkpoint on a best-effort basis; each jump of the system clock measured against the monotonic clock, with its size; degraded/fatal states; `data_lost`, one report per lost range caused by corruption (§3 Recovery rule 2, §8 Corrupt block), by the gap between a recovery entry's `after` and its position (cause truncated tail, §3 Recovery rule 3) or by a listed file lost or corrupted in the bucket (§8 Restore, Failures and end), or by a file the bucket has lost or corrupted with no local copy left (cause lost in the bucket, §8 Self-repair) (a record that extends a range is the same loss); `possible_loss`, one report per recovery entry (§3 Recovery rules 1 and 3) that names a non-empty tail, a mark that was not readable or stale, a missing or unreadable cursor file, or a series at the `written` level in the registry at its `after` (§2.1 `durability`; Q-251). Both are read from the manifest and the recovery entries at every start: a restart never clears them and each loss is counted once. A `data_lost` found while the store is open (§3 Recovery rule 2), or by a start that could not write (§3 Order of a start, step 3), is reported from when it is found, marked not recorded, and is the same loss once a start records it (Q-200). Loss reports that cannot be read degrade health, so health is never `ok` while a loss may be hidden (`losses_unknown`, Q-201). Decisions made without local data that could not be read during the run degrade it (`local_data_unreadable`, §2.4) until the next start, and a damaged region no lost range records degrades it until the region is repaired or recorded as lost. A `possible_loss` whose recovery entry is later lost stays listed (the registry file and each tree checkpoint keep every recovery entry, §2.1, §8), and the range that took it is a further `data_lost`. Neither makes the store read-only. A `data_lost` holds health degraded until it is closed (§8 Self-repair: restored in full, re-send ended, or by an operator through Acknowledging a loss), and each report carries its state (found, copying, awaiting re-send, awaiting an operator, closed and how); a `possible_loss` is listed and counted, does not hold health degraded and has no state. Health asks for a person (`attention`) as §8 Self-repair says: each corruption until it is seen, repeated corruption, a fault, a failed or stalled backup; an alert for a corruption does not degrade health, and repeated corruption degrades it (`storage_suspect`) while the count is at or above `repair.suspect_after`. A tail written after an adopted recovery entry with no readable commit block may go unreported (§3 Recovery rule 3, Adoption). A corrupt manifest record with no other copy is rebuilt from the local files and degrades health until the rebuild is acknowledged (`manifest_rebuilt`); while deep cannot be reached to tell whether a copy exists it is fatal (`manifest_corrupt`) for `manifest.rebuild_wait`, after which a provisional rebuild degrades it (`manifest_rebuilt`, provisional, not acknowledged until final; §7 Manifest); a rebuild final but not acknowledged before a restart is provisional again at a start that cannot reach deep, and stays so until deep answers; a corrupt restore record with no copy stays fatal. A seal blocked by another program holding the file (`format.md` A.13) degrades health until it succeeds. A sealed segment that no manifest record lists and that fails its checks degrades health (`segment_damaged`) until it is repaired, and one that still cannot be read once its retries have reached their longest interval degrades it (`segment_unreadable`) until a read succeeds (§8 Corrupt block). Whether the mark is synced at every commit, and why (no boot identity, or configured), is reported in statistics, not as a health reason. So are the last position acknowledged and the last position durable (the durable mark, "durable up to"): between them lie the flagged batches not yet synced (§3 Commit). Each report carries what is known of its range: positions, bytes and the commit times of the batches on either side. Query results carry no flag for a lost range: the engine knows its positions, not the series and times it held. Each restore record's set-aside range, and `restore_failed` (§8 Restore) |

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
| Sync rate and SSD wear | Syncs/s and bytes written on a representative SSD with N unbatched 1 Hz series (N stated in the benchmark), segments created ahead of use and zero-filled, `fdatasync`, plus the timed mark sync | A longer `sync_interval_floor`, with `min_sync_interval` at least as long (§3 Group commit; the sync budget bounds every load, Q-981); a longer `mark_sync_interval` |
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
| Bucket conditional writes and SHA-256 checksums | The probe at `attach_backup` (`format.md` A.17): a second create under a taken key is refused, a mismatched single-part SHA-256 is rejected, and the stored hash is reported; sampled downloads as the scheduled check | The bucket is refused (`BackupRefused`); there is no weaker mode |
| Memory per series | Resident bytes per series at the ceiling | Lower ceiling; smaller recent window |
| Overlapping chunks | Read amplification after a year of late corrections from upstream sources | Tune overlap threshold |
