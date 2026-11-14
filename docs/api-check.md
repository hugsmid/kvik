# Interface check (roadmap 3.2)

[`api.md`](api.md) checked for implementability against [`spec.md`](spec.md) v0.2. The question asked of each part
was: can the code of the roadmap item that first builds it be written from the documents alone, without guessing
anything a caller depends on? This document lists problems; it proposes no signatures and does not change `api.md`.
Problems already recorded in [`model/OPEN-QUESTIONS.md`](../model/OPEN-QUESTIONS.md) (F-items and Q-items) are
cross-referenced, not restated. Format problems are in [`format-check.md`](format-check.md) (FC-n).

## Severity
- **Blocks:** the document contradicts the specification, or a type or rule needed by a section 4 item is absent.
  To be resolved at gate 3.3, before section 4 starts.
- **Before 4.x:** a choice an implementer would otherwise guess, needed by the named section 4 item; it can be
  decided in that item and recorded in `api.md` in the same pull request.
- **Can wait:** first needed after section 4; to be decided before the named item.

## Summary
| Severity | New (AC) | Existing (cross-referenced) |
|---|---:|---|
| Blocks | 5 | F-2, F-6 (outcome unknown after a fault) |
| Before 4.x | 9 | F-6 (reader), F-11, Q-34 (c), Q-46, Q-48 |
| Can wait | 11 | F-3, F-8, F-10, F-12, F-13, F-14, F-16, F-17, Q-44 (d), Q-47, Q-49 (b), Q-61, Q-63, Q-78, Q-80, Q-81 |

The counts above are of AC items; the cross-referenced items are not counted in them. Checked with no finding: the
single-writer and read-only handle split; and the `BootId` seam.

## Existing items and when they are needed
| Item | Needed by | Severity | Note |
|---|---|---|---|
| F-2 | 4.4 | Blocks | Back-pressure listed as a refusal; part of AC-4 and AC-10 |
| F-6 | 4.4, 4.6 | Blocks (outcome unknown, AC-4, AC-10); before 4.6 (reader lease and boot identity, AC-11) | |
| F-11 | 4.1, 4.5 | Before 4.1 | `mark_sync_interval` key (AC-7); `possible_loss` reason (AC-13) |
| Q-34 (c), Q-48 | 4.5 | Before 4.5 | Whether loss reports are latched; `store.lost_ranges()` (AC-13) |
| Q-46 | 4.5 | Before 4.6 | `reader.epoch()` and a truncated-tail check (AC-11) |
| Q-47 | 4.5 | Can wait (5.3) | `Resync.forget_from` (AC-22); settled by 4.5, built in 5.3 |
| F-10, F-12, F-13, F-14, F-17 | 5.1 | Can wait | `SeriesSpec`, `Reading` and refusal additions (AC-15, AC-16) |
| F-3, F-16, Q-81 | 5.4, 7.3 | Can wait | Window and calendar results (AC-17, AC-18) |
| Q-61, Q-63, Q-78 | 5.2 | Can wait | Funnel identity, latches, emitted series (AC-20) |
| Q-49 (b) | 5.3 | Can wait | "Unknown" condition state in `Resync` (AC-22) |
| Q-80 | 5.1 | Can wait | An interval zone unknown to tzdata: an error variant (AC-4) |
| F-8 | 8.2, 8.3 | Can wait | Backup sink, restore, drill, re-ingest (AC-23, AC-24) |
| Q-44 (d) | 5.1 | Can wait | "Not in v1" may gain a view by issue time |

## Principles, environment and handles

### AC-1 — The runtime and threading model are not defined
**Where:** Principles, Opening · **Severity:** blocks (4.1) · **Relates to:** §4 Concurrency ("the writer thread"),
§3 Commit, design budget "reading accepted → delivered ≤ 2 ms"
**Problem.** "The engine runs its tasks on a runtime the caller provides", but neither `StoreConfig` nor `Env` takes
one, and no trait describes it. The specification speaks of a writer thread. Syncs block for milliseconds; on an
asynchronous runtime they need a blocking pool or a dedicated thread, which is a thread the caller did not create.
Periodic work has no stated owner: the mark sync (1 s), cursor writes (1 s), tree checkpoints (60 s), funnel timers
(1 s), lease refreshes (30 min) and backup uploads.
**Scenario.** An embedding application on a single-threaded runtime calls `emit`; the sync blocks its only thread,
and every consumer stalls for the length of the sync.
**Needed:** which runtime the engine targets (and whether its crates appear in the public interface), whether a
dedicated writer thread is allowed, and how the caller provides and stops each periodic task.

### AC-2 — The environment seams cannot be implemented as sketched
**Where:** Environment seams · **Severity:** blocks (4.1) · **Relates to:** §4 Timers (monotonic clock), §2.5 clock
synchronisation, §2.1 and §5.7 (tzdata version in force), FC-21, FC-28
**Problem.**
- `Clock::monotonic` returns the standard library's `Instant`, which can only be derived from the real clock: a test
  clock must offset a real base, cannot go before it, and cannot write the value to disk or compare it across
  processes. It is not said whether that suffices for deterministic runs against the model; `Timestamp` is not
  defined (AC-3).
- `Fs` is a comment. It lacks a lock (FC-28), truncation or set-length, reads at an offset, directory listing, removal
  (retired files, §7) and opening a file read-only from another process. `mmap` needs unsafe code (FC-21), and writes
  through a mapping are not calls, so the seam cannot inject a crash between them or drop them at a simulated power
  loss.
- `Clock::synchronised` has no stated source on any platform.
- Nothing injects the tzdata version in force or a tzdata change (rows 21, 45, 46), or the randomness for a new
  `store_id`.
**Scenario.** The kill-loop harness of 4.5 must lose the mark's unsynced updates at a simulated power loss; with the
mark written through a mapping, the seam never sees those writes.
**Needed:** complete trait definitions, a monotonic time type tests can control, and seams for tzdata and
randomness.

### AC-3 — Core value types are undefined
**Where:** throughout · **Severity:** blocks (4.1, 4.3) · **Relates to:** §2.1, §2.2, §6 Windows (offsets of either
sign), F-13
**Problem.** `Timestamp`, `Position`, `SeriesId`, `Value`, `Quality`, `LocalTime`, the date type of `calendar`, and the
zone type (a `String` in `IntervalLength::Calendar`) are not defined. `Duration` is used for window offsets, which
§6 allows to be negative; the standard `Duration` is unsigned. `Value` must cover `f64`, a decimal (given as a scaled
`i64`, or as `f64` with a rounding rule, or refused when not representable), a state index, a bool, text and "no
value" (§2.5).
**Scenario.** A window from ten minutes ago to five minutes ahead, `[now − 10 min, now + 5 min)`, cannot be written:
its lower offset is negative and `Duration` is unsigned.
**Needed:** the definitions, and whether a time library's types appear in the public interface (a semver
commitment).

### AC-4 — The error and refusal types are not listed
**Where:** Principles ("errors are typed"), Writing · **Severity:** blocks (4.1) · **Relates to:** F-2, F-6, F-10,
F-13, F-14, Q-80, row 30, row 32
**Problem.** Only `Busy`, `OffPhase` and `DetailNotLocal` are named. The specification requires at least: refusals
for an unknown or retired series, a reading off its phase, `changed_at` missing or unexpected, `revise` on a versioned
series, a value of the wrong type, a state index out of range, text over 255 bytes or not UTF-8, and a new series over
the ceiling (row 30); faults that make the store read-only, with their cause, and the unknown outcome of a batch whose
sync failed (F-6); open errors for a held lock, a store id mismatch, a newer major format (row 32) and a configured
site zone that differs from the store's (Q-75's model); query errors for detail not local, an aggregate refused on an
interval series (F-14), a funnel refused at compilation (F-13, Q-78) and an interval zone unknown to tzdata (Q-80's
proposal).
**Scenario.** An adapter submits a 300-byte text reading. With no variant for it, the engine reuses another refusal,
and the per-series counter (principle 5) cannot say why.
**Needed:** the error and refusal enumerations, and which are per reading and which per call.

### AC-5 — Handles, lifetimes, shutdown and drop
**Where:** Principles, Opening · **Severity:** before 4.1 · **Relates to:** §3 Single writer, Write or sync failure
**Problem.** Not stated: whether `Store` is `Send`, `Sync` or cloneable (many adapter tasks share one writer); whether
a `Consumer` or a `Reader` borrows the store or owns a shared handle; whether `shutdown` consumes the store while
consumers, readers or emits are outstanding; what `shutdown` does on a read-only store, where the final sync is
forbidden (§3); what dropping a store without `shutdown` does (no asynchronous drop exists), and whether it releases
the lock; whether a second `Store::open` in the same process is refused.
**Scenario.** An adapter task still holds the store when the application calls `shutdown`; its next `emit` has no
stated result. If the application drops the store instead, it is not said whether the mark is synced or the lock
released.
**Needed:** ownership and life-cycle rules for each handle.

### AC-6 — Create versus open, and the site zone at open
**Where:** Opening · **Severity:** before 4.1 · **Relates to:** §5.7, Q-75 (model), F-16, FC-3, §8 Restore
**Problem.** Whether `Store::open` creates a store in an empty directory, or a separate call does, is not said.
`StoreConfig.time_zone` is required, but §5.7 makes the site zone a log entry with an effective time: it is not said
whether the configured zone is the store's zone at creation only, is compared with the zone in force (the model
refuses a mismatch), or records a change (F-16 needs a call). Nor which call resumes a directory part-way through a
restore (§8: a restart resumes it).
**Scenario.** An operator restarts a store with `time_zone` set to another zone. The model refuses to open; an
implementation following `api.md` alone might record a site-zone change effective now.
**Needed:** the creation path and the meaning of `time_zone` at each open.

### AC-7 — Configuration keys are not enumerated
**Where:** Opening (`LogConfig`, `Limits`, `TierConfig`) · **Severity:** before 4.1 (log keys), can wait (others) ·
**Relates to:** §11 ("every default is a configuration key"), F-11, F-12
**Problem.** No field is listed. The specification's defaults include segment size, seal interval, linger, mark sync
interval (missing, F-11), ingest queue, consumer queue, series ceiling, disk marks, fill threshold, eviction age,
grace period, lease expiry and refresh, checkpoint interval, recent window, cursor sync interval, funnel timer
cadence, replay age, the `future` threshold, the default lateness bound, `reset_ceiling` and retention period, the
merge threshold and `mirror_path`. It is not said which may change between opens (segment size) and which are fixed
for a store.
**Scenario.** Item 4.1 defines the configuration types. Without the list, `mark_sync_interval` and linger get names
and units chosen ad hoc that every later item must keep.
**Needed:** a table from each default to its key and type.

## Writing

### AC-8 — `Ack` cannot report the outcome of each reading
**Where:** Writing · **Severity:** before 4.4 · **Relates to:** §11 Ingest (recorded(position) / duplicate /
refused(reason), per reading), F-10
**Problem.** `Ack { first, last, duplicates_dropped, refused }` gives a position range and counts. Duplicates take no
position and refusals none, so when they interleave with recorded readings a caller cannot tell which index got which
position. A call whose readings are all duplicates or refused has no `first` or `last`. Outranked (F-10) and unchecked
readings are not reported per reading.
**Scenario.** An adapter submits five readings; the second is a duplicate and the fourth is refused. `first..last`
spans three positions and nothing maps them to indices 0, 2 and 4.
**Needed:** a per-reading outcome, or a stated reason why the specification's per-reading outcome is not offered.

### AC-9 — What `try_emit` returns, and when outcomes are known
**Where:** Writing · **Severity:** before 4.4 · **Relates to:** §4 Concurrency, F-2, §2.1 retirement
**Problem.** `try_emit` "never waits", but readings are durable, de-duplicated and possibly refused only at commit (a
retirement can commit while a reading waits in the queue, and the model then refuses it). It returns no `Ack` and no
handle to await one.
**Scenario.** A consumer emits derived readings with `try_emit` from its delivery loop, as §4 requires; one is
refused at commit because its series was retired meanwhile, and the consumer cannot learn it except from statistics.
**Needed:** what `try_emit` returns, and how its caller learns each reading's outcome.

### AC-10 — Back-pressure, cancellation and batch boundaries
**Where:** Writing · **Severity:** blocks (4.4) · **Relates to:** §3 Back-pressure, Disk space (critical mark), §2.5
(host-stamped readings wait), F-2, F-6
**Problem.**
- `emit` waits under back-pressure, but the caller cannot tell why it waits: a full queue, the critical disk mark
  (which may last indefinitely) or an unsynchronised clock holding host-stamped readings (§2.5). No timeout is offered.
- It is not said what happens when the `emit` future is dropped after its readings were queued: are they still
  committed, and if so how does the caller learn it?
- It is not said whether one call is one batch. A call larger than the ingest queue (65,536 readings) cannot be
  queued at once: it either waits forever or is split across batches, and then "a batch is all-or-nothing for
  durability" no longer describes the call.
- `try_emit` returns `Busy` under back-pressure, but whether it also does so for host-stamped readings before clock
  synchronisation is not said.
**Scenario.** An adapter wraps `emit` in a timeout and drops the future after 5 s; its readings commit a moment later
and are acknowledged to no one. The adapter replays them; the duplicate rule absorbs the repeat only if the check can
run (§2.4), and on a series that is not versioned an unchecked replay can revert a correction.
**Needed:** back-pressure signalling, cancellation semantics and the relation between a call and a batch.

## Reading and reader processes

### AC-11 — The reader process interface
**Where:** Opening (`Reader::open`), Reading · **Severity:** before 4.6 · **Relates to:** §7 Readers and Leases, F-6,
Q-46, FC-25, FC-29
**Problem.** `Reader::open(path)` takes no `Env` (no boot identity for the lease, F-6, no file system seam for tests),
and there is no lease refresh call (F-6) or task to run it under "no hidden threads". Not stated: how a reader learns
of new data (reads the mark on every call?) and of new registry entries; how it keeps an epoch for a long read against
the 30-minute refresh; how it serves `live` and windows, which need state the writer holds in memory, when §7 says a
reader needs nothing from the writer's memory (replaying the log since the last tree checkpoint at open?); whether an
in-process reader is obtained from the store; and Q-46's truncated-tail check.
**Scenario.** A dashboard process opens a `Reader` and calls `live(id)` an hour after the writer's last tree
checkpoint. An exact answer needs an hour of log replayed; nothing says whether the reader does that or returns an
older value.
**Needed:** the reader's life cycle, its lease handling and what each read call costs in a reader process.

### AC-12 — Read calls are synchronous but may fetch, and some results are unbounded
**Where:** Reading · **Severity:** before 4.6 (`facts`), can wait (others) · **Relates to:** §8 Restore step 2 and
Retention class (fetch on demand), `BackupSink` (asynchronous)
**Problem.** `facts`, `window`, `buckets`, `calendar` and `live` are synchronous, but `facts` "fetches on demand" from
cold or deep through an asynchronous `BackupSink`, and every read does blocking file I/O. `facts` returns a value over
any range, which can hold billions of rows.
**Scenario.** A chart server calls `reader.facts(id, a_year, View::History)` from an asynchronous handler; the call
blocks the runtime on a network fetch and then tries to hold a year of per-second rows in memory.
**Needed:** which calls are asynchronous, and a bounded or streaming result for `facts`.

### AC-13 — Health and statistics types
**Where:** Health and statistics · **Severity:** before 4.5 (loss reports), can wait (others) · **Relates to:** §11
Health, F-6, F-8, F-11, Q-34 (c), Q-48, Q-80
**Problem.** `Health` and `Stats` are named with examples only. Missing: the reasons, each with its data, at least
`data_lost` per lost range (positions, bytes, commit times either side), `possible_loss` (bytes, readable commit
blocks, F-11), clock jumps with their size (F-6), `restore_failed`, each set-aside range, a superseded writer and a
superseded restore (F-8), upload stalled, the disk marks and an unknown interval zone (Q-80); whether a reason is a
degradation or fatal; whether loss reports are latched (Q-34 (c)); `store.lost_ranges()` (Q-48); per-series and
per-consumer statistic fields; whether a reader process can see health.
**Scenario.** After a reboot, recovery truncates a torn tail. The application polls `health()` and needs the byte
count and readable commit blocks of `possible_loss`, which have no field.
**Needed:** the types; loss reports first, since 4.5 produces them.

### AC-14 — Views and ranges
**Where:** Reading (`View`) · **Severity:** before 4.6 · **Relates to:** §2.3 Views, §11 Query ("as-of P or T"), F-10
**Problem.** `View` has `Current`, `AsOf(position)` and `History`, but not as-of time T (§2.3), which §11 lists. The
range type of `facts` and `buckets` (half-open? inclusive?) is not given. History results need `changed_at` (F-10).
**Scenario.** An auditor asks what a series looked like at 09:00 yesterday. §2.3 defines as-of time T, but `View`
offers only a position.
**Needed:** the complete `View` and the range convention.

## Series

### AC-15 — Defining, finding and changing series
**Where:** Series · **Severity:** can wait (5.1) · **Relates to:** §2.1, F-10, F-12, F-13, F-14, F-17, row 30
**Problem.** "Defining a series that already exists with the same specification returns its id; a different kind,
value type, scale or unit is an error." It is not said what a difference in any other field (expected interval, S,
lateness bound, `wrap_at`) does: an error, silently ignored, or an implicit parameter change. `series(id)` is "also by
name" without a call, and without saying what a name of a retired series or an alias returns. There is no call to list
series or match a name pattern (needed by `Select::Names`). `SeriesInfo` has two fields named; `ParameterChange` none.
**Scenario.** An adapter restarts with a new expected interval in its `SeriesSpec`. The call may return the old id
silently (the new interval never applies), fail, or append a parameter change with an unstated effective time.
**Needed:** the rules and the missing calls and types.

### AC-16 — Interval series calls
**Where:** Series, Reading (`interval`) · **Severity:** can wait (5.4) · **Relates to:** §2.1, §5.4 displaced
readings, F-14, F-17, Q-80
**Problem.** `reader.interval(id, t)` does not say what it returns for a `t` that is not an interval start, for a
displaced reading, for a `t` with no reading, or for an interval zone unknown to the tzdata version in force (Q-80).
`IntervalLength::Fixed { d, phase }` admits `phase ≥ d` and `d = 0`; the refusal is not named. `live()` should carry
the interval end (F-14, F-17).
**Scenario.** A chart calls `interval(id, t)` for the `t` of a reading displaced by a tzdata change; that `t` is
neither an interval start under the version in force nor without a reading.
**Needed:** results and errors for each case.

## Windows and calendar results

### AC-17 — Window result and aggregate types
**Where:** Reading (`WindowResult`, `Aggregate`, buckets) · **Severity:** can wait (5.4) · **Relates to:** §5.6, §5.7
(exact sums for a decimal scale), §6, F-3, F-6, F-14
**Problem.** `WindowResult.value` must allow empty (F-6) and needs a place for the rate flag (F-3). Its type is not
given: sums of whole intervals and decimal series are exact (`i128` at a scale), others `f64`; time-in-state is a map,
not a number. The flag set (`restoring`, `approximate_edges`, `apportioned`, `displaced`, `summary_stale`, the rate
flag) has no type; coverage needs seconds and a fraction. The bucket type of `buckets` (count, min, max, coverage, and
`apportioned`, F-14) is not defined.
**Scenario.** A decimal energy series sums whole days exactly in `i128`. Returned as `f64`, a year's sum loses the
exactness §5.7 promises.
**Needed:** the result types.

### AC-18 — `calendar` and `calendar_in`
**Where:** Reading · **Severity:** can wait (7.3) · **Relates to:** §5.7, §6 Windows, F-16, Q-75, Q-81
**Problem.** Both take a `date`, so a month cannot be asked for (F-16). The zone argument's type and its error when the
zone is unknown are not given. Calendar windows (since local midnight, this month, a relative day or month) have no
`Window` constructor (F-16). Which days `calendar` returns around a site-zone change is Q-75.
**Scenario.** A report asks for March in a named zone. `calendar_in` takes a date, so it cannot ask for a month.
**Needed:** the signatures and errors.

## Consumers

### AC-19 — Consumer identity and life cycle
**Where:** Consuming · **Severity:** can wait (5.2) · **Relates to:** §4 Consumer state, New consumer, F-8
**Problem.** Not stated: the rules for a consumer's name; what a second handle for the same name does; how a consumer
is removed (its cursor file deleted); the variants of `Start` and whether `start` is ignored when a cursor exists;
registering with `start: position` after a restore (F-8); when `next()` returns `None` (shutdown, read-only); whether
`confirm` must be in order, whether confirming a position confirms everything below it, and what confirming a timer
delivery does to the cursor, which is a position.
**Scenario.** A supervisor restarts a crashed consumer while the old handle is still being dropped. Two handles for
one name confirm different positions into one cursor file.
**Needed:** the life cycle and the confirmation rules.

### AC-20 — Funnels and routes
**Where:** Consuming (`route`) · **Severity:** can wait (5.2) · **Relates to:** §4 Funnel and Routing, Q-61, Q-63,
Q-78, F-13, F-14, FC-16
**Problem.**
- A funnel has no identity in the interface, but latches, timer deliveries `(funnel, sequence)` and removal need one
  (Q-61, Q-63), and there is no call to remove a route, though "routes may be added and removed".
- Routes are log entries. If the application calls `route` at every start, each start appends the same routes again,
  unless `route` is idempotent by identity; and §4 says funnels are "declared in configuration", which `api.md` does
  not provide.
- The serialised form of a funnel (format entry type 5) is deferred to this document and is not here (FC-16).
- Compilation errors (a cycle, Q-78; a forecast stale-for, F-13; an interval avg, F-14) have no type (AC-4), and the
  series a consumer emits are not declared (Q-78).
- "A consumer that emits readings uses `try_emit` or its own task, never the delivery callback": the interface has
  no callback (`next().await` is a pull), so the rule does not say what is forbidden.
**Scenario.** An application calls `route` at every start. After a hundred restarts the log holds a hundred routing
entries for one funnel, and nothing says whether they are one funnel or a hundred.
**Needed:** funnel identity, removal, persistence across restarts and the serialised form.

### AC-21 — Delivery types
**Where:** Consuming (`DeliveryKind`) · **Severity:** can wait (5.2, 5.3) · **Relates to:** §4, §8 Restore step 2,
§11 Consumers, F-10, Q-61
**Problem.** `DeliveryKind` has `Fact`, `Trigger` and `Resync`; §8 and §11 also require `generation_changed`. A fact
delivery needs `changed_at`, quality, its fact flags (`late`, `future`), `received_at` and the delivery flags
`replayed` and `outranked` (F-10). The identity of a trigger fired by a routed fact is Q-61. The type of `d.id` is not
given.
**Scenario.** A restore completes and every consumer must receive `generation_changed` (§8 Restore step 2), which
`DeliveryKind` cannot represent.
**Needed:** the delivery types.

### AC-22 — What `Resync` carries
**Where:** Consuming · **Severity:** can wait (5.3; Q-47 settled by 4.5) · **Relates to:** §4 Resync, §3 Recovery rule
3, Q-47, Q-48, Q-49 (b), §4 Future facts
**Problem.** "Live and condition state now" has no shape. Per funnel it needs the live state of each selected series
and the condition state; a funnel over a pattern matching many series makes one `Resync` unbounded (100,000 series
at the ceiling), and whether it is one delivery or many is not said. A funnel over a forecast series carries condition
state only (§4). Q-47 proposes `forget_from`; Q-49 (b) an "unknown" condition state. Whether the consumer can tell why
it receives a `Resync` (recovery, lost range, generation change, cursor behind the local log) is not said.
**Scenario.** A funnel selects `plant/*/power`, matching 20,000 series. After a recovery its `Resync` must carry the
live state of each, in one delivery or many; neither the consumer queue (4,096) nor the delivery size bounds it.
**Needed:** the `Resync` type and its size bound.

## Backup, restore and export

### AC-23 — The backup sink trait
**Where:** Backup and restore · **Severity:** can wait (8.2) · **Relates to:** §8 Hashes, Generations, §13 Deep object
count, F-8, FC-33
**Problem.** Whether a trait with `async fn` methods can be used as a trait object (`dyn BackupSink`) depends on the
language version and must be checked on the pinned toolchain; the trait states no `Send` bounds or error type.
Create-if-absent for generation claims is missing (F-8). `list` returns every key under a prefix at once, while §13
expects many objects per year. `get` with a range on an encrypted object needs a seekable envelope (FC-33). `Stored`
and the key type of `Encryption::Key` are not defined, nor key rotation.
**Scenario.** After a few years the bucket holds millions of objects. `list` returns every key under a prefix in one
`Vec`, and a restore holds them all in memory.
**Needed:** the trait's final shape.

### AC-24 — Restore, rebuild and export calls
**Where:** Backup and restore · **Severity:** can wait (8.3) · **Relates to:** §8 Restore, Re-ingest, §11 Ingest
(exports carry `changed_at`), F-8
**Problem.** `Store::restore` returns a `Position`, but after step 1 the store serves queries and accepts ingest, so
the caller needs a store; it takes no `StoreConfig` (zone, limits, tiers). Not stated: which call resumes a restore
after a restart; the drill mode (F-8); the variants of `RestoreMode`; whether `export_set_aside` is synchronous and
what its range is (positions); a general export (Parquet, §7), which the specification refers to and the interface
omits; the re-ingest entry point (F-8); whether `rebuild` runs while the store serves.
**Scenario.** An operator restores onto an empty disk. `Store::restore` returns the restored end; the application
must then open a directory part-way through a restore, which AC-6 leaves undefined.
**Needed:** the restore life cycle and the export calls.

### AC-25 — `attach_backup` and the mirror path
**Where:** Backup and restore · **Severity:** can wait (8.1) · **Relates to:** §8 Continuous backup, Corrupt block
(`mirror_path`), Upload stalled
**Problem.** Backup is attached by a call after open, so files sealed between open and attach, or in a run that never
attaches, have no stated fate (queued for later? health degraded as for a stalled upload?). `mirror_path` has no key
(AC-7).
**Scenario.** An application opens the store and attaches the backup sink an hour later; segments sealed in that
hour have no stated upload.
**Needed:** what happens to sealed files while no sink is attached.
