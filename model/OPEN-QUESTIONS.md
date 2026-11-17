# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-16 | §5.4, §2.3, §4 | Which commits refresh live staleness, when a fact falls due, and across a restart |
| Q-33 | §2.1, §3 Recovery, invariants 5 and 8 | The Derived check: derived files after a lost range |
| Q-44 (b) and (c) | §2.1 timing, §2.2, §2.3 Views, §2.4, §8 Restore step 2, §11 | Withdrawn values on scheduled and forecast series, far-future values on any series |
| Q-52 (b) | §7 Manifest, §3 Recovery rule 3, Order of a start steps 3 and 4 | Whether writing the last manifest record, or the last log batch, again in place makes the next sync write it |
| Q-83 | §5.6, §5.2 holds, §2.1 parameters, invariants 5 and 8 | Node contents after a past-effective change of the staleness limit |
| Q-84 | §2.3 Live state, §6 Conditions, §8 Restore step 3, invariant 16 | A condition or live state that stays unknown |
| Q-85 | §8 Corrupt block, No deletion, Fill pressure, §7 | A loss in the bucket found outside a restore |
| Q-86 (b) | §3 Write or sync failure, Disk space | No space at a log write before any sync, checked on a file system that tracks pages |
| Q-88 | §4 Funnel, Routing, `api.md` Consuming | The grammar of name patterns |
| Q-90 | §8 Restore step 1, §8 Acknowledging a loss, §11 Health | Whether a restore record's set-aside report holds health degraded |
| Q-92 | §2.4 revised-key index, §8 Restore step 2, invariant 8, principle 2 | The "incomplete" mark on a revised-key index entry |
| Q-93 (b) and (c) | §5.7 tzdata version in force, §7 Readers | A release name standing for its rules; a writer that cannot write, and values other than calendar results in a reader of another release |
| Q-94 | §7 compaction order, §8 Fill pressure, Corrupt block, §3 Disk space | Compaction in position order when the next segment is not local or is quarantined |
| Q-96 | §6 Restart and resume, §4 Resync, Behind consumers | A resume replay with a segment missing in the middle, and `resync` per funnel or per consumer |
| Q-110 | `format.md` A.2 Resynchronisation | The residual cost of resynchronising over nested batch images |
| Q-111 | `format.md` common rules (Sizes), A.4 (Sizes) | A full facts block against the batch limit |
| Q-115 (e), (f), (j), (k), (n) | §3 Commit, Segments, Disk space, Back-pressure; `format.md` A.8, A.13 | The commit path: the points still open |
| Q-116 (a), (c), (k), (n) | §3 Recovery, Order of a start; `format.md` A.14; `design.md` Budgets | Recovery at a start: the points still open |
| Q-118 (c), (f), (k) | §7 Readers, Leases, Manifest; §8 Generations | The reader process and leases: the points still open |
| Q-123 (a), (c)–(g) | §2.2, §3 Commit, §11; `api.md` Writing, Series, Health | The interface on the durable log: points left open |
| Q-140 | §5.4, §2.3, §7 Readers; `api.md` Reader processes | Live staleness of an observed series in a reader process |
| Q-141 | §2.2 `received_at`, `format.md` A.7 `commit_time` | A fact's `received_at` after the wall clock steps back |
| Q-142 | §5.5, §2.1 counter rules; `format.md` A.6 field 11 | A reset ceiling off the parts-per-million grid |
| Q-161 | §8 Generations; `api.md` Backup and restore | Telling a restore's own claim marker from another restore's |
| Q-163 | §8 Generations, Restore step 1 | Reading the claims again on a bucket whose listings lag |
| Q-164 | §8 Retention class, Restore; `api.md` Reading, Backup and restore | Interface points deferred to 8.2 that need tiers or restore |
| Q-170 | §1 principles, §2.1, §3 Commit, §6, §11; `api.md` Writing, Series | Durability levels chosen per source: acknowledged once written, before the sync |
| Q-173 | §7 Manifest, `api.md` | The operator call `rebuild_manifest`: finality, a readable manifest, several corrupt records |
| Q-175 | §7 Manifest (rebuild step 2) | An intermediate output no record and no local file describes |
| Q-198 | §8 Deep, Encryption, Generations, Restore; `api.md` Backup and restore | The backup and restore interfaces ahead of their implementations |
| Q-210 | §8 Bucket; `api.md` Backup and restore; `format.md` A.17 Probe | Telling a bucket that reports no checksum from another sink failure |
| Q-211 | §7 Manifest, §8 Generations; `format.md` A.14, A.17 | An uploaded epoch written again after the last two synced records are damaged |
| Q-212 | §8 Continuous backup, Hashes; `format.md` A.17 | A key already taken by an object that is not the file |
| Q-270 | §8 Encryption; `format.md` A.17 Reading | Envelope damage that `EnvelopeError` has no variant for |
| Q-280 | §7 Manifest (rebuild), §8 Continuous backup, Hashes | How a rebuilt manifest queues files for upload again |
| Q-281 | §8 Hashes; `format.md` A.14 | The copy-verified record (`0x27`): layout and fold |
| Q-282 | §8 Continuous backup, Generations; §7 Manifest | The upload queue, the record cursor and which records wait |
| Q-283 | §8 Hashes, Bucket, Generations | The attach probe |
| Q-284 | §8 Hashes, Upload stalled | Upload failures: retries, other content, an object not reported after its `put` |
| Q-285 | §8 Continuous backup; `format.md` A.12 | Uploading files with no local layout yet, and the hash a file is checked by |
| Q-291 | §8 Acknowledging a loss, Generations, Restore step 1 | Recovery-entry loss reports of the generation a restore reads |
| Q-292 | §8 Generations; `format.md` A.2 sizes, A.14 `0x25` | A restore record larger than one block |
| Q-294 | §8 Restore step 1, Generations; model `Model::restore_from` | Where the model's restore differs from the plan |
| Q-301 | §8 Restore (the paragraph after step 4) | The files a resumed restore fetches |
| Q-302 | §8 Restore (Failures and end), §8 Corrupt block | The lost range of a listed file the bucket has lost, and files other than log segments |
| Q-303 | §8 Generations, §7 Readers; `format.md` A.1 kind 12 | The local restore record file |
| Q-304 | §8 Generations, Restore step 1 | A manifest record object the bucket has lost or a listing does not show |
| Q-310 | `design.md` Budgets; §3 Group commit | The delivery budget's p50 at exactly 100 calls a second with the default bound (settled by an engineering fix, pending the next NVMe run) |
| Q-341 | §8 Bucket; `format.md` A.17 Probe | The attach probe at `<sid>/probe`: its wrong-hash put there, and an attach after a full restore |
| Q-342 | §8 Continuous backup, Generations; `api.md` `BackupStats` | `backed_up_through` in a restored generation |
| Q-322 | §3 Durable mark; `format.md` A.10 Update | A torn mark update that makes a damaged older slot valid again |
| Q-760 | §2.2 `changed_at`; `format.md` A.5 | A stored `changed_at` of kind `time` above `i64::MAX` |
| Q-761 | `docs/export.md` Partitioned layout | Long and renamed series names in the partitioned layout |

## Q-16 — Which commits refresh live staleness
**Scenario.** S = 3 minutes. (a) A series has been silent for 10 minutes and is stale; a late fact for last week is
committed. Read literally, "since its last commit of a fact that is not bad quality" makes the series fresh again,
although live state still shows the 10-minute-old reading. (b) A revision for the same `t` as the live-state fact,
committed after S of silence, likewise refreshes it. (c) A future fact is excluded until due (§4), but it is not
said whether staleness counts from its commit or from when it fell due, nor whether "due" follows `now` (which a
forward clock jump moves) or the monotonic clock (§4 Timers). (d) The monotonic clock restarts at boot, so after a
restart there is no last-commit instant to measure from. (e) "Bad quality" may mean the supplied or the engine
quality.
**Model.** Stale-for conditions measure monotonic time since the last arrival of any reading. A resume, which has no
monotonic instant from before it, rebuilds that time on the host clock: the wall clock now less the `received_at` of
the last reading committed, not below zero. A stale-for condition true now is reported true in `resync`
(`q16_stale_for_true_now_reported_true_on_resync`), and a backward correction of the clock neither fires nor
suppresses it (`q16_backward_clock_correction_neither_fires_nor_suppresses_stale_for`); a forward correction while
the engine is down still makes the last reading look older.
**Tried and rejected.** Counting from a staleness reference that restarts only when the live state's `t` rises past
a high-water mark, for facts not flagged `late`, with "due" on the monotonic clock. Review found that this breaks
principle 2 (when a fact fell due depends on a clock the log does not hold, so replay cannot rebuild it), that one
reading far in the future flags every later reading `late` and keeps the series stale, and that a device always
more than the lateness bound behind would never be fresh.
**Needed.** A rule that is a function of the log, ignores late facts and same-`t` revisions, survives a restart and
a clock jump, and says whether a skewed device counts as stale.
**Proposal.** Live staleness follows live state (§2.3). The **refresh instant** of an observed series is that of its
live-state fact's key: the commit of the earliest-recorded fact for that `(series, t)` whose recorded quality (after
any lowering by the engine) is not bad and which was not outranked at commit, or, if it was committed ahead of
`now`, the instant `now` reached it (§4 Timers). The series is stale when more than S has passed since then, or when
it has no live-state fact. A commit that leaves the live-state `t` unchanged (a late fact for an earlier time, a
revision of the same `t`, a bad-quality, outranked or not-yet-due fact) refreshes nothing; the first good reading that
raises the live-state `t` refreshes the series even when flagged `late`, so a device whose clock runs behind is fresh
while it reports and health reports the skew (row 5). For a refresh instant before the engine's present start, the
time passed is taken once, at the first `now` after the clock is reported synchronised, as `now − max(received_at,
t)` of that fact, not below zero; until then a stale-for condition keeps its latched state. Proposed additions: a row
for a restart with a series silent since before it, a row for a live-state fact revised to bad quality, and an
invariant that a commit leaving the live-state `t` unchanged does not change live staleness.
**Objections to the proposal.** (1) A host with no real-time clock restarts unsynchronised: until the time passed is
taken, staleness is undefined for a consumer with no latch (a new consumer, or any consumer after a restore), for the
staleness `resync` carries, and for `reader.live()`. (2) The seed `now − max(received_at, t)` fails when
`received_at` ran ahead of real time (the clock stepped back after synchronisation, or facts were committed before
it), so a series stale before a restart can be fresh after it. (3) A first good reading that raises the key "however
old its `t`" makes a week-old value fresh, against "a reading is valid for at most S" (§5.4), row 3, and `api.md`
`live()` returning the "latest valid value". (4) A parameter entry that changes S with an effective time at or
before the live-state `t` changes staleness without changing `t`, against the proposed invariant, and whether the S
in force at `t` or at `now` applies is not stated. (5) Already so in the current text: a reader process (§7) has no
refresh instant, since the writer's monotonic clock is not in the log.
**Roadmap 5.4.** The durable backend gives live state and staleness as the model backend does (`kvik/src/durable/`
`windows.rs` and `clock.rs`): an observed series is stale when more than S has passed on the monotonic clock since its
last commit, in the writer's process, of a fact that is recorded, not outranked, not bad quality and not `future`
(the instant is taken when the log acknowledges the call, after the covering sync, whether or not the caller waits); with no such commit since the store was
opened, in event time, `now − t` of the live-state fact; a scheduled series in event time, a forecast series never.
So (a) and (b) still refresh it, as in the model, and (d) falls back to event time. A reader process has no refresh
instant at all (objection 5, Q-140). Not settled: the owner's ruling on the proposal and its objections is pending
(`kvik/tests/windows.rs`, `a_reader_process_answers_after_shutdown_and_power_loss`), and 5.4 stays unticked until it
is given.

## Q-33 — The Derived check: derived files after a lost range
**Scenario.** The kept-log principle, orphaned facts and id floors are settled (§2.1; see Settled). A lost range
(§3 Recovery) held a definition, parameter change, alias, routing change, site-zone or tzdata entry, or facts that tree files and
the revised-key and version indexes were built over. The registry file may still hold the lost entry and the derived
files the lost facts, so rebuilding from the kept log no longer equals the live structures (invariants 5 and 8). The
same holds for a lost range that a restore records when the bucket has lost or corrupted a listed file (§8 Restore,
Failures and end). A lost log segment whose facts were already compacted loses only its non-fact entries: a scattered
set of positions, not a range between batches, so the report's bytes and commit times on either side (§11 Health)
are undefined for it, the batch `commit_time` that as-of-time queries over the surviving facts need (§2.3) is lost
with it, and which definitions count as lost is not said (a chunk or the registry file may still hold one). Since
Q-100, each decimal series' largest magnitude and highest stored scale are derived values too, carried by tree
checkpoints: a checkpoint taken before a lost range may carry a magnitude or scale held only by lost facts, so the
scale in force (§2.1, which counts kept facts only) and the range check must be brought back to the kept log with
the other derived values (a carried magnitude only makes the range check stricter; a carried scale would raise the
scale in force without a kept fact or raise entry behind it).
**Model.** Not modelled: a lost range comes only from a damaged batch of facts in the open segment (row 11a), and
`Model::corrupt_open_batch` refuses a batch holding a registry, routing or zone entry
(`tests/edge_cases.rs` `row_11a_fault_injection_limits`).
**Tried and rejected.** Syncing every non-fact entry to the registry file and re-appending lost ones to the log with
their original positions; keeping facts of undefined series apart until a definition names their id (review found
that re-appended entries put lower sequences after higher ones, §2.2, `format.md` A.4, and that a definition taking
effect from its earliest fact contradicts §2.1). A stamp recorded with each lost range and compared with the stamp
each derived file was built under (`loss_stamp`) fails on requirements 1 to 3 below.
**Needed.** How the registry file, trees and indexes are brought back to a function of the kept log after a lost
range (the Derived check). Whatever is designed must hold these:
1. It covers the writer's memory, not only files: a lost range found at run time below the in-memory applied position
   must not be checkpointed under a fresh stamp. Candidate: the writer restarts recovery when it records one.
2. It names a rebuild base that exists when the range lies below the newest uploaded checkpoint, as for a range
   recorded by a restore (§8 Restore, Failures and end).
3. It covers a lost segment whose facts were already compacted: the non-fact positions lost are scattered, the
   report's bytes and commit times are undefined, and series still held by chunks must not be orphaned by dropping
   their definition from the registry file. Candidate: a definition is lost only if no local file holds it.
4. No fact is accepted for a series between a run-time lost-range record and the registry correction.
5. It is recomputed at every start, rule 1 included, with no stored label: the cut-back of derived files must not
   depend on a step a crash can skip (the mark written and the process crashing on the same boot while the registry
   file still holds a definition from a lost position).
Until then invariants 5 and 8 hold "lost ranges excepted" (§10), as the model has it (damage only to batches of
facts).
**Roadmap 5.1, for the registry file** (`format.md` A.16, `kvik/src/durable/`). Every start rebuilds the registry from
the kept log and checks the file against it: the file must hold exactly the kept log's registry entries up to the last
synced mark; otherwise it is written again from the kept log, carrying the writer's id floor, so the id of a lost
definition stays assigned (`kvik/tests/durable.rs`, `the_registry_file_follows_the_kept_log`,
`the_registry_file_carries_the_id_floor_when_the_mark_is_lost`). This meets requirement 5 (recomputed at every start,
no stored label). Requirements 1 and 4 hold for now because no lost range is recorded at run time: only a start finds
one, and the registry is rebuilt after it. Requirements 2 and 3 cannot arise yet: until log segments leave the hot tier
(roadmap 8.1) or are compacted (6.x) and checkpoints exist (7.1), the kept log holds every registry entry, and the file
is checked, never trusted over the log. The rest of this question (requirements 1 to 4, the indexes and trees,
invariants 5 and 8 across a lost range) moved to 6.2 at the 5.1 review (no derived file other than the registry
exists before 6.2). Recovery entries, reset points, rows 7b and 7c and invariants 25–27 are modelled since roadmap 5.1a.

## Q-44 (b) and (c) — Withdrawn and far-future values
**Scenario.** (b) A newer issue that no longer covers a valid time cannot withdraw the older issue's value there, so
the current view can mix issues. (c) A wrong value far in the future on a scheduled or forecast series is never
flagged, since those series are exempt from `future`. Parts (a), a silent feed, and (d), forecast accuracy, are
settled (see Settled).
**Model.** Not modelled.
**Proposal for (b) and (c).** The engine sets no horizon and infers no withdrawal. A source withdraws a value by a
reading for that `t` with a higher `changed_at` (on a series that is not versioned, a later reading) and quality bad,
carrying the value it withdraws, which summaries, windows and live state ignore (§2.5, §2.3). A wrong value far ahead
is corrected by a revision or withdrawn the same way, and per-series statistics show the latest recorded `t`.
**Objections to the proposal.** (b) Under §12 D6 the withdrawing bad reading is ignored and the previous value's hold
continues up to S, so withdrawing the 14:00 value of a half-hourly price credits the 13:30 price across 14:00 with
full coverage; "coverage ends where the remaining values' validity ends" holds only at the tail of an issue. On a
series that is not versioned, a later unchecked replay of the withdrawn value (§12 D5) can revert the withdrawal
where the key's revised-key index entry is incomplete (§2.4, since Q-72), which is the hazard §2.4 gives for
declaring a series versioned. (c) Neither a revision nor a withdrawal lowers the
latest recorded `t`, so a wrong value for 2099 disables the duplicate fast path (§2.4) for good, makes every fact
during §8 Restore step 2 `duplicates_unchecked`, and keeps the latest-`t` statistic wrong. The same holds on an
observed series: since Q-74 the lateness reference leaves out a `future` fact, but the latest recorded `t` still
includes it, so a device glitch to 2099 disables the fast path for good and makes Restore step 2 count every fact of
the series `duplicates_unchecked`.
**Roadmap 5.1.** The durable backend's current view implements the proposal for (b): the fact of highest rank per
`(series, t)`, with no horizon and no inferred withdrawal, so a withdrawal is a bad-quality reading with a higher
`changed_at`. Not settled: the owner's ruling on the proposal and its objections is pending, and 5.1 stays unticked
until it is given.

## Q-52 (b) — Whether writing the last manifest record, or the last log batch, again in place makes the next sync write it
**Scenario.** Parts (a) and (c) are settled at gate 3.3 (see Settled). §7 Manifest writes the last valid record's
identical bytes again in place at every start, so that a record kept from the page cache after a failed sync is on
disk before a successor is appended. Since Q-87, §3 Recovery rule 3 does the same for the last complete log batch
(and its segment's header) whenever the mark is missing, unreadable or stale, and `format.md` A.4 seals again from
step 1 an `.open` file whose trailer is found, writing the segment index, trailer and end marker again. Whether the next sync then writes those
bytes, on each platform, was not measured by the roadmap 2.2 spike.
**Model.** Not modelled; manifest records are not corrupted and no batch is written again.
**Needed.** A check, with the page-tracking fault file system and on each platform (on Linux, with a device that
fails writes on demand), that each rewrite reaches the disk. If it does not, the proposed fallbacks are: for the
manifest, rewrite it to a new file and rename it; for the log, copy the kept batches to a new segment and rename it;
for a seal, the same copy before the rename to `.kvl`.
To be settled on each platform. Roadmap 4.8 did not settle it: the check needs a device that fails writes on demand
(on Linux, for example a device-mapper target that fails or drops writes) or power cuts, which its timing runs do not
give (`docs/measurements/4.8-commit-latency.md`). A hosted Linux runner with root could build such a device-mapper
target; gate 4.9 will try that, with the owner's run on the target hardware.
**Gate 4.9** keeps (b) open until a trial passes. The `fault-trials` workflow (`.github/workflows/fault-trials.yml`,
run by hand and weekly) with `scripts/trial-rewrite.sh` and the example `log/examples/fault_trial.rs` checks it on
ext4 and XFS on a loop device: a device-mapper `error` target is swapped in over exactly the file's extents, so a
sync fails while the file system stays writable; a control arm syncs again without writing, a treatment arm writes
the same bytes again and syncs, and the device is read directly in both. The engine is then checked the same way
for the last batch, the mark slot and the manifest record: the writer stops at the fault, the start writes the bytes
again, and every acknowledged position reads back after a remount (mark synced at every commit, a new boot id per
remount). If the treatment arm passes on both file systems, (b) is settled for Linux as written; if it fails on
either, the fallbacks above are adopted.
**Settled on Linux at gate 4.9** by the third run of that workflow (`docs/measurements/4.9-fault-trials.md`): on ext4
and on XFS every arm passed. The control arm showed the behaviour the rewrite rules exist for (Q-87, Q-117): after a
failed write-back a sync alone left the device without the new bytes, as the page was clean; the treatment arm,
writing the same bytes again, made the next sync write them. The engine's start wrote the last batch, the mark slot
and the manifest record again, the device then held each, and every acknowledged position (and the recorded lost
range) read back after a remount. The rewrite rules stand as written; the fallbacks are not needed on Linux. Still
open: owner decision pending on the deferral wording for macOS and Windows, where no device fails writes on demand
without kernel code.

## Q-83 — Node contents after a past-effective change of the staleness limit
**Scenario.** A gauge with an expected interval of 30 s (S = 90 s) stores the 15-minute level and calendar nodes; it
reports every 30 s but falls silent for 5 minutes every hour. A parameter entry committed today sets S to 10 minutes,
effective a week ago (§2.1: a parameter applies to readings with `t` at or after its effective time). The hold of
each reading uses the S in force at its `t` (§5.2): the hold of the reading before each outage grows from 90 s to the
whole gap, so the coverage and integral of every stored node of that week holding an outage change. Since Q-60 no
node is created or deleted by a parameter entry, but §5.6 recomputes only the minutes a batch's facts touch, and a
parameter entry touches no fact: it is not said how the nodes from the effective time are folded again, whether all
at once at commit (a week of nodes, or years of them, some over detail that is not local) or later, and what nodes
and results show meanwhile. Until they are, invariant 5 fails for nodes still folded under the old S unless those
nodes are flagged, and invariant 8 (a rebuild gives the live nodes) fails as well. The same holds for any other
parameter a node depends on: an expected interval that sets S by default, `wrap_at` and `reset_ceiling` on a counter.
**Model.** Every parameter is fixed for the life of a series (no parameter entries), so the case does not arise (see
"Effective-dated parameters" in the table below).
**Needed.** How stored nodes from a past effective time are brought to the parameters now in force, and what holds
until they are.
**Proposal (smallest rule).** Treat the entry as late data (§5.6) for every minute from its effective time on: those
nodes are recomputed from their facts under the parameters now in force, ancestors from their children, and a node
whose facts are not local is flagged `summary_stale` and recomputed when they are fetched (row 29).
**Objection.** As for late data, only results are flagged while the recompute is pending, not nodes, so invariant 5
fails for nodes not yet recomputed; and counting the recompute complete when nodes over detail that is not local are
only flagged conflicts with invariant 8. A rule must say which nodes carry the flag, and when invariant 8 is judged.

## Q-84 — A condition or live state that stays unknown
**Scenario.** Since Q-49 (b), during a restore a condition whose evaluation has no local detail, and live state of a
series whose latest recorded `t` no local fact or stored node holds, are unknown (§2.3, §6 Conditions). A state
series' last fact is in a revision chunk or log segment that step 3 does not bring back (it stays in deep), or in a
file recorded lost (§8 Restore, Failures and end). Its state stays unknown until `restoring` ends; then §2.3's scope
ends and the live state silently becomes none, and the condition false, with no delivery, so invariant 16's
exception is never discharged.
**Model.** Not modelled: nothing is unknown in the model (see the table below).
**Needed.** What ends an unknown state whose fact never becomes local: a fetch on demand for it, a delivery when
`restoring` ends, or a state that stays unknown (and what `resync` then carries).

## Q-85 — A loss in the bucket found outside a restore
**Scenario.** Fill pressure moved a sealed log segment off hot and cold once it was verified in deep (§8 Fill
pressure, Eviction at 90 days), so deep holds its only copy. The scheduled hash check, or an on-demand fetch, then
finds the object missing or corrupted in the bucket. §8 Corrupt block fetches another copy and otherwise compacts
again from kept inputs, which are gone once the file is verified elsewhere (§7); the restore exception of Q-39 (e)
does not apply, and No deletion says nothing of it.
**Model.** Not modelled; the bucket never loses or corrupts a file.
**Needed.** Whether such a loss is handled as Corrupt block with deep as the failed tier (compacted again from other
files that hold its positions, the rest a lost range reported as `data_lost`), and how it is reported.

## Q-86 (b) — No space at a log write before any sync
Found by spike 2.2 (`docs/measurements/2.2-commit-latency.md`). To be settled by roadmap item 4.8 (moved from 4.5); moved again, to gate 4.9, by 4.8 (see the end of the question).
**Scenario.** (a) Free space is above the critical mark at the last check; another process then fills the file
system, and creating the next preallocated segment (§3 Segments) fails with `ENOSPC`, leaving a partial `.open`
file. Every acknowledged batch is durable and nothing has been written to the new file. (b) On a copy-on-write file
system, a write of a batch into preallocated space returns `ENOSPC` before any sync; no acknowledged byte and no
dirty page is lost. Both are write failures on the log (§3 Write or sync failure: read-only, fatal, recovery by
restart), and both are the disk being full (§3 Disk space: ingest waits).
**Spike.** Follows Write or sync failure (`spikes/commit/tests/faults.rs`
`no_space_when_preallocating_the_next_segment_is_fatal_although_nothing_is_lost`,
`no_space_on_a_log_write_is_fatal_and_nothing_is_acknowledged`).
**Needed.** Which rule applies to each case, and what recovery does with a partial `.open` segment that holds no
batch (`format.md` A.4 does not say).
**Proposal.** (a) is Disk space: the partial file is deleted, health is critical, ingest waits, and the creation is
retried when a free-space check passes. (b) stays fatal, as now: on a copy-on-write file system the same shortage can
as well surface at the sync, where the batch's fate is unknown, and the critical mark makes either rare. Recovery
deletes an `.open` segment that holds no complete batch and is shorter than the preallocation or has an invalid
header.
**Settled at gate 3.3.** (a) as proposed: `spec.md` §3 Segments, `format.md` A.4 Creation and the states recovery
may find. (b) stays a write failure (`spec.md` §3 Segments); what remains is to confirm with the page-tracking fault
file system of roadmap 4.5 that no acknowledged byte or dirty page is lost. **Roadmap 4.5** does not confirm it: its
in-memory file system fails a write for lack of space only when a file grows, so a batch written into a preallocated
segment never meets the case; it is confirmed by trials on a copy-on-write file system at roadmap 4.8. **Roadmap 4.8**
ran none: filling a copy-on-write file system needs a small volume of its own, which its timing harness does not
create, and filling the development machine's own disk is not an option. A hosted Linux runner with root could fill
Btrfs on a loop device; gate 4.9 will try that, with the owner's hardware run
(`docs/measurements/4.8-commit-latency.md`).
**Gate 4.9** keeps (b) open until a trial passes. The Btrfs job of the `fault-trials` workflow
(`scripts/trial-no-space.sh`) fills a Btrfs loop device while the writer commits; it records which call failed
(write or sync) and the error kind, which must be storage-full, checks that nothing after the fault is
acknowledged, and that every acknowledged position reads back after a remount with the disk still full and again
after space is freed, when a new batch must commit. A second arm repeats it on a log directory with copy-on-write off
(`chattr +C`). If it passes, (b) stays a write failure as settled at gate 3.3.
**Settled on Linux at gate 4.9** by the first run of that job (`docs/measurements/4.9-fault-trials.md`): with
copy-on-write on, lack of space surfaced at the batch write (storage-full, before any sync), the writer stopped and
acknowledged nothing after it, and every acknowledged position read back after a remount with the disk still full
(the start stays read-only) and after space was freed, when a new batch committed. (b) stays a write failure, as
settled at gate 3.3; the text does not change. With copy-on-write off for the log directory no write failed: the
zero-filled segment filled, and the next creation then waited for space (Q-86 (a)). Still open: owner decision
pending on whether `format.md` A.4 advises copy-on-write off for the log directory (the second arm shows that it
removes the failure), and on the wording for macOS and Windows, where the check is not made.

## Q-88 — The grammar of name patterns
**Scenario.** §4 Funnel selects series by "ids or name patterns", and since Q-78 a consumer's `emits` does too;
compilation refuses a change that closes a cycle where "two patterns that one name could match" (§4 Routing), which
must be decidable from the two patterns alone. No document defines a pattern: `api.md` shows only `plant/*/power`.
Whether `*` matches one whole segment of `<instance>/<device>/<metric>`, any run of characters within a segment
(`plant/line-*/power`), or runs across `/` decides both which series a funnel receives and which selections overlap.
Q-78's scenario takes `plant/*/power` to match `plant/line-1/power-smoothed`, which it does under none of these
readings. Overlap is a per-segment comparison for whole-segment wildcards and needs an intersection test for
wildcards within a segment.
**Model.** A selection is a list of ids or every series; there are no patterns. The `kvik` crate's in-memory backend
pins a provisional choice for now: each `/`-separated segment is matched on its own, `*` matching any run of
characters within it (so `plant/line-*/power` works); a live series matches if any of its names does; patterns are
re-evaluated when a series is defined or aliased, so a retired series leaves the selection at the next
re-evaluation; a condition funnel's pattern is resolved once, when routed, and must match exactly one series. It has
no overlap test, as it compiles no `emits`.
**Implemented (roadmap 5.2).** The durable backend keeps a pattern as declared in the routing change (`format.md`
A.6 field 55) and matches it as the in-memory backend does: a fact funnel's pattern is resolved again whenever a
definition or alias is applied, a condition funnel's once, when routed, its ids carried in the entry (field 54). The
cycle check of `emits` takes two patterns to overlap when they have as many segments and, in each, some string that
both match (`*` within a segment; `kvik/src/durable/routing.rs`, `patterns_overlap`); an id overlaps a pattern when
any name of its series matches it. Accepted as provisional at the review of roadmap 5.2 (`api.md` Consuming states
the grammar as provisional); to be settled by roadmap 5.6, so that the grammar is ruled with the gate's other
interface points once both backends have used it.
**Needed.** The pattern grammar and its overlap test.
**Proposal.** A pattern has the three segments of a name, each a literal or `*`, which matches any one whole segment;
two patterns overlap when, in each segment, they are equal or either is `*`; an id overlaps a pattern when any name
of its series matches it.

## Q-90 — Whether a restore record's set-aside report holds health degraded
**Scenario.** Q-38 left clearing the set-aside report to Q-34 (c). Q-34 (c) settled only the loss reports:
`data_lost` holds health degraded until acknowledged and `possible_loss` does not (§11 Health, §8 Acknowledging a
loss). Each restore record's set-aside range (§8 Restore step 1) is derived from the record at every start, so a
restart does not clear it, and `api.md` lists it among the health reasons. After the operator has exported the
set-aside facts and re-ingested what is wanted, nothing ends that report, so a store restored once would report
degraded health for ever; and it is not said whether the report degrades health at all.
**Model.** Not reported (see the table below).
**Needed.** Whether the set-aside report holds health degraded, and if so what ends it (for example
`acknowledge_losses` naming the range's first position).

## Q-92 — The "incomplete" mark on a revised-key index entry
**Scenario.** Since Q-72 an entry of the revised-key index is incomplete when it was started by an unchecked fact or
rebuilt during a restore while an earlier fact of its key was not local (§2.4). Whether an entry is incomplete
depends on which tier held a fact when another arrived, which the log does not record, so a rebuild from the log
(invariant 8, principle 2) cannot give the same mark; it is the same kind of exception as "completed when its chunk
is back" (§8 Restore step 2), but now outside a restore too. Nothing is said to complete an entry outside a restore,
or for a chunk that stays in deep after one: §2.4 says "until the detail is fetched and completes it", but a fetch on
demand for a read is not said to update the index, and a key whose earlier fact is never read stays incomplete for
good, so a replay of its earlier value can revert a correction indefinitely (row 2a).
**Model.** A full in-memory history of every key stands in for the index, so no entry is incomplete outside a
restore (see "Revised-key index outside a restore" in the table below).
**Needed.** Whether the mark is derived state excepted from invariant 8 (and stated as such), and what completes an
entry outside a restore and for detail left in deep.

## Q-93 (b) and (c) — A release name standing for its rules; what else a reader of another release computes
**Scenario.** Part (a) is settled at gate 3.3 by the owner (see Settled): readers never refuse a tzdata release, a
reader computes calendar results with its own release from the UTC data, using a stored calendar node only where its
recorded release is the reader's, and the writer adopts a new host release without a restart. What remains:
(b) The version is a release name: a patched or partial build under the same name, or a vendor's build with other
rules, is trusted as the release the log names, so the derived values of two hosts with the same name can differ.
(c) The ruling covers readers and calendar results. Not said: which rules a writer that cannot write (read-only after
a failed sync, §3) uses for its reads, and how a reader of another release gives the other values that depend on
tzdata (interval starts and displaced readings of calendar-length series, §2.1, §5.4). Nor whether a reader whose
release does not know the site zone or an interval zone answers `QueryError::UnknownZone` (`api.md` Errors) or
computes as the version in force would, given that reads are not to fail because a release changed.
**Model.** The model holds several releases and refuses nothing (see "Adopting a tzdata release" in the table below).
**Needed.** (b) Whether the version must identify the rules (a fingerprint of the zones used) rather than the
release name. (c) Whether a read-only writer reads as a reader does, and whether interval starts in a reader follow
the version in force (named by the log) or the reader's own release.
**Roadmap 5.4 (c).** The durable backend takes local days for calendar results from the zone rules the reference
model embeds (`kvik_model::calendar::Tzdb`), in the writer's process and in a reader process alike, not from the
release `Env::tz` reports: the `TzRelease` seam gives offsets and transitions, and the day spans of §5.7 (a midnight
that does not exist or occurs twice, zone changes) would need a second implementation over it. So `Env::tz` names
the release in the log, but the rules used are the embedded ones whatever it reports; with the default tzdata seam
the two are the same. Needed with (c): whether a reader's "own release" is the seam's (then calendar spans move onto
it) or the build's embedded rules (`kvik/src/durable/windows.rs`).

## Q-94 — Compaction in position order when the next segment is not local or is quarantined
**Scenario.** Since Q-71 compaction takes sealed log segments in position order, and a segment at or below the last
position held by a live compacted file counts as compacted (§7). While compaction is paused at the low mark (§3 Disk
space), fill pressure (§8) moves verified sealed segments to cold or deep, uncompacted ones included; and a segment
can be quarantined (§8 Corrupt block). When compaction resumes, the next segment in position order is not local, or
is quarantined. It is not said whether compaction stalls until the segment is local, fetches it from deep, or skips
it; skipping and compacting a later segment would put the skipped positions below the mark, so they would count as
compacted although no chunk holds them.
**Model.** Compaction takes every sealed segment not yet compacted, wherever it is held: the model reads entries from
any tier.
**Needed.** What compaction does when the next segment in position order is not local or is quarantined.

## Q-96 — A resume replay with a segment missing in the middle, and `resync` per funnel or per consumer
**Scenario.** (a) A consumer resumes after its cursor (§6 Restart and resume) and the log segments after it are local
except one in the middle, moved to deep by fill pressure (§8). §4 Resync and §8 Behind consumers cover a cursor older
than the oldest local log segment; a gap in the middle is not covered: the replay cannot deliver the missing
positions without a download, which §8 forbids for catch-up. (b) §6 Restart and resume has "the funnel then sends
`resync`", one per funnel, while §4 Resync describes one `resync` delivery carrying the state of every funnel of the
consumer; a consumer with several funnels would see one `resync` or several, and Q-61's "forgets the condition
deliveries it has seen" on `resync` depends on which.
**Model.** A consumer whose cursor is too far behind the local log segments receives `resync` alone; a gap in the
middle is not examined. One `resync` delivery carries the state of every funnel in force (`engine::Resync`).
**Needed.** What a resume replay does at a non-local segment in the middle, and whether `resync` is one delivery per
consumer or per funnel.

## Q-110 — The residual cost of resynchronising over nested batch images
Found at the review of roadmap 4.2 (crate `kvik-log`). To be settled at gate 4.9.
**Scenario.** Resynchronisation (`format.md` A.2) judges candidates on block headers and reads in full only a
candidate whose framing and `batch_crc32c` hold. Such candidates can be as dense as one per 80 bytes: a 16-byte
block header whose image nests inside the previous one, each closed by its own 64-byte commit block, all with valid
`batch_crc32c`. Each is checksummed in full, so within one 8 MiB window the bytes hashed grow with the square of the
number of images (about 40 n² bytes for n images; `log/src/scan.rs` `nested_batch_images_hash_each_image_once`).
Building them needs the store id, the file id and the offsets, so only a process that can write the file can.
**Implemented.** Each passing candidate is checksummed once; the cost is pinned by the test above.
**Needed.** Whether the cost is accepted (write access is needed to cause it), or bounded, for example by a
prefix-CRC table over the window so that each candidate's body checksum costs O(1) after one linear pass.
**Gate 4.9** recommends accepting the cost now and stating the trust boundary (`design.md` Budgets: the restart
budget holds for files the engine wrote, damaged or not, and a file crafted by a process with write access to the
store is outside it; `SECURITY.md`: write access to the store directory is full trust), with the prefix-CRC table
recorded as hardening before the format is declared stable (roadmap 10.3). Owner decision pending: the trust-boundary
wording, which narrows a budget's scope.

## Q-111 — A full facts block against the batch limit
Found at the review of roadmap 4.3 (crate `kvik-log`). To be settled at gate 4.9.
**Scenario.** `format.md` A.4 closes a facts block at a plain-encoded size of 8 MiB and lets one call be up to 8 MiB
plain-encoded, "so every batch fits an empty segment"; the common rules limit a batch to 8 MiB of data blocks plus its
commit block. A call that fills one facts block to 8 MiB of plain-encoded rows becomes a block whose body adds the
24-byte prefix and the column directory, and whose header adds 16 bytes: its data blocks exceed 8 MiB, so the batch
is over the limit although the call is within its own.
**Implemented.** A call is measured by its data blocks encoded alone, not by its plain size: an incompressible 8 MiB
call is refused, a compressible larger one accepted.
**Needed.** One of: measure the batch limit in plain-encoded size, as the call and block limits are; or allow a batch
8 MiB plus the per-block overhead (prefix, directory, header and padding of each data block). Related: A.6 lets
consecutive entries share one entries block up to a body of 16 MiB (Q-106 (e)), a split that the 8 MiB batch limit
means is never reached; whichever option is chosen, the entries rule should name the limit that actually applies.
**Gate 4.9** recommends keeping the one hard limit and stating the call limit in the same unit, as implemented: one
call is at most `min(65,536, limits.ingest_queue)` readings and 8 MiB of data blocks when encoded alone, so every
batch fits an empty segment (`EmitError::TooLarge` otherwise, its `bytes` the encoded size); A.6 then says that in a
log segment the 8 MiB batch limit applies before the 16 MiB body limit of A.2. Owner decision pending: the wording in
`api.md` Writing, whose unit changes while the figure does not.

## Q-115 (e), (f), (j), (k), (n) — The commit path: points `spec.md` §3 and `format.md` A.8 and A.13 leave open
Found while building the log writer (roadmap 4.4, crate `kvik-log`: `store`, `writer`, `fs`); (n) recorded at gate
4.9. Parts (a)–(d), (g)–(i), (l) and (m) are settled at gate 4.9 (see Settled). Tests in `log/tests/writer.rs` pin
what is implemented.
**Scenario.** (e) *The low mark.* "5 % of the filesystem" needs the file system's size, which the `Fs` seam (`api.md`,
accepted at gate 3.3) does not give; and on a platform with no free-space source (Windows, A.13) neither mark can be
judged. (f) *The segment index's reach.* A.8's `max_t` for an interval series needs the series' interval length,
which the writer knows only for definitions it committed itself; the registry (roadmap 5.1) holds the rest. (j)
*Windows name durability.* A.13 assigned to roadmap 4.4 the check that a name made durable by flushing the file
survives a power loss on Windows; it needs a Windows machine and power-loss trials. (k) *The interface.* `api.md`
names the waits `QueueFull`, `DiskFull` and `UnsyncedClock` and lists `IngestStats.waited` and commit latency; the
log writer reports `QueueFull`, `DiskCritical` and `NoSegment` (a roll waiting for the segment created ahead) and
measures neither. (n) *Bounds on ingest and on blocked seals.* The ingest queue bounds entries, not bytes: 65,536
one-entry calls of up to 8 MiB each are accepted (facts are small, so this needs entry calls of large definitions).
Blocked seals have no bound: each holds a file handle, is read again every 10 seconds, and adds a segment to every
later start's scan.
**Implemented.** (e) The low mark uses the byte floor alone (`disk_low_bytes`); an unknown free space applies neither
mark, so ingest never waits for the disk there. (f) Since roadmap 5.1 a start takes the interval of every series the
registry file defines (`format.md` A.16), and then of every definition in the unsealed segments; definitions this
writer commits add theirs. The registry file is written at every start and at clean shutdown, so a series lacks its
reach only when the file is missing or invalid, until the next start writes it (`log/src/writer/recovery.rs`). (j) Not done: a cloud runner can
time `FlushFileBuffers` but cannot show whether a device cache is flushed or a name survives a power loss
(`docs/measurements/4.8-commit-latency.md`). Nothing in the writer depends on it. (k) Done by roadmap 5.1: the
interface reports the log's `QueueFull` and `NoSegment` as `Wait::QueueFull` (a roll waiting for its segment holds the
queue as a full queue does) and `DiskCritical` as `Wait::DiskFull`, measures `IngestStats.waited` as the time a call
spends waiting for room in the queue and commit latency from queueing to acknowledgement (percentiles over the last
1,024 calls), and takes batches and syncs from the log (`kvik/src/durable/mod.rs`). (n) Neither is bounded.
**Gate 4.9.** (e) Recommended: the seam returns size and free space (`fn space(&self, path: &Path) ->
io::Result<Space>` with `Space { free, total }` replacing `free_space`; low mark = `max(total × 5 %,
disk_low_bytes)`; Windows stays unsupported, and neither mark applies where the platform gives no free space), in
its own pull request before 5.1 wires the interface; the low mark only pauses compaction, which does not exist
before roadmap 6. Owner decision pending: the seam change. (f) Deferred to 5.1: the registry gives the writer every
series' interval at open; until then `max_t` may be low for series defined before the start, so the index must not
be used for pruning before 5.1. (j) Proposed: A.13 says Windows name durability and the device cache are not
verified, the flushed handle is the rule the engine follows, no durability guarantee is given there, and a trial on
Windows hardware with power cuts is left to roadmap 10.1 and gates no item. Owner decision pending: that wording.
(k) Deferred to 5.1, as the question says. (n) Recommended: a byte bound on the queue (64 MiB of data blocks, a limit
in `api.md`) and an ingest wait at 8 blocked seals (`Wait::SealBlocked`), or at least a health figure for blocked
seals. Owner decision pending: both, as each adds a limit or a wait.
**Needed.** The owner's decisions on (e), (j) and (n), at gate 4.9; (f) and (k) with roadmap 5.1.

## Q-116 (a), (c), (k), (n) — Recovery at a start: points `spec.md` §3 and `format.md` A.14 leave open
Found while building recovery (roadmap 4.5, crate `kvik-log`: `writer::recovery`, `manifest`). Parts (d)–(j), (l),
(o) and (p) are settled at gate 4.9, and (b) and (m) at the review of roadmap 5.2 (see Settled). Tests in
`log/tests/recovery.rs` pin what is implemented.
**Scenario.** (a) *A corrupt manifest record.* §7 fetches it from deep or the mirror path, or rebuilds the manifest
(roadmap 6.3); neither exists yet. (c) *Loss history.* `api.md` reads loss reports "from the manifest and the recovery entries at every start"; reading every
earlier recovery entry means reading every segment that holds one. (k) *No space for the recovery entry's segment.*
A creation that fails for lack of space waits (§3 Segments); at a start that needs a new segment for its entry,
nothing can be written until it exists. (n) *Restart time.* A start reads every unsealed segment whole,
the last sealed one for its trailer, and the sealed ones above the registry file's `through` for their routing
changes (Q-130), against the restart budget of `design.md`.
**Implemented.** (a) The start refuses to open (`StoreError::ManifestCorrupt`). (c) A start reports what it found and
what the manifest holds. Since roadmap 5.1 the interface reads every recovery entry of the kept log at every start and
every reader refresh, as it reads the whole kept log to rebuild the registry anyway: `losses()` lists the manifest's
lost-range records and, for each recovery entry that reports a torn tail or names a consumer whose cursor file was
missing or unreadable, a `PossibleLoss` (`kvik/src/durable/index.rs`). The cost is that of reading the log, which
roadmap 6.x bounds. (k) The start stays read-only; the next start retries. (n) Measured by roadmap 4.8 on macOS:
130–241 ms to open a store of 7–10 segments (446–639 MiB) again, after a clean shutdown with a warm page cache, against the 1 s
budget, and 129–138 ms for two segments holding almost nothing, so the time is mostly reading the unsealed 64 MiB
segments whole. The cloud runners of `bench.yml` (warm cache, not durability-representative) took 0.24–0.68 s for 3–5
segments. On the Linux NVMe target (three runs, warm cache, clean shutdown) a store of 7–10 segments (446–639 MiB)
opened again in 392–742 ms on the same boot and 393–739 ms on another. A cold cache and a crash are still to measure
(`docs/measurements/4.8-commit-latency.md`).
**Gate 4.9.** (a) Deferred to 6.3: the refusal stays until deep fetch or a rebuild exists. (c) Deferred to 5.1: the
interface reads it. (k) Recommended as implemented: `spec.md` §3 step 5 to say that if the
segment cannot be created for lack of space the start stays read-only and reports it, and the next start retries, so
a full disk after a power loss needs space freed and a restart. Owner decision pending: that availability wording.
(n) Open: the owner's run on the Linux NVMe target measured a warm cache only (0.39–0.74 s, inside the budget with
0.26 s to spare at worst); a cold start is still to measure. If a cold start there is over 1 s, a start scans segments
awaiting a seal only up to their last complete batch; independently, a start should fold each batch's content into
the segment's summary while scanning rather than hold it all at once.
**Roadmap 6.3a.** (a) The crate `kvik-files` reads a manifest past its corrupt records (`image::read`), writes a
verified copy back in place or by a rewritten manifest (`rebuild::repair_with_copy`), decides between copy, rebuild
and wait from the answers of the mirror path and deep (`rebuild::CorruptStart`), and rebuilds the manifest from the
local files, final or provisional (`rebuild::rebuild_manifest`); the log reads cause 5 and passes the publish and
sealed-segment records over. The start still refuses (`StoreError::ManifestCorrupt`) until roadmap 6.3b calls these
from step 4 and supplies the local files, the scan's losses and the sink. Points for 6.3b found at the review of
6.3a: the caller of `rebuild::repair_with_copy` must pass a copy of the first record of the corrupt span (a shorter
copy is written over the span's start, the rest left corrupt); `appender::Irreversible` is `Copy` and bound to no
appender or run, so it must be bound to both before anything consumes it; and while a rebuild stays
unacknowledged, each start that is given deep's copy appends it again (the catalogue takes it as a no-op, but the
manifest grows by one record per such start). Open points found: Q-171 to Q-175; Q-171 and Q-174 settled at the review of 6.3a, Q-172 withdrawn (spec §7 step 2 settles it).
**Needed.** (a) with roadmap 6.3 (6.3b), (c) with 5.1; (k) the owner's decision and (n) a cold start on the Linux
target, at gate 4.9.

## Q-118 (c), (f), (k) — The reader process and leases: points `spec.md` §7 and §8 leave open
Found while building the reader process (roadmap 4.6, crate `kvik-log`: `reader`, `lease`). Parts (a), (b), (d),
(e), (g)–(j) and (l) are settled (see Settled). Tests in `log/tests/reader.rs` pin every part.
**Scenario.** (c) *A new generation.* A restore (roadmap 8.3) sets positions aside "also in reader processes"; a
reader that sees the mark's generation change has no restore record to read yet. (f) *Which epoch a refresh takes.*
"Readers take an epoch ... and refresh both": whether a refresh moves to the manifest's current epoch, and whether a
read that only renews keeps the old one. (k) *What removal must honour.* Retired files (§7 Manifest) and tier moves
(§8) do not exist yet.
**Implemented.** (c) Everything taken is void (`VoidCause::Generation`) and the log is read again from its start.
(f) A refresh takes the current epoch; `keep_alive` keeps the epoch unless the lease may have expired. With no files
to retire this is safe; once tree and compacted files exist, whether a refresh may move the epoch under an open
iteration is for roadmap 6.3. (k) `LeaseTable::oldest_live_epoch` is the input for roadmap 6.3 and 8.1; today the
writer deletes only segments that hold no batch.
Not a question: the reset nonce comes from std's OS-seeded hash keys for now; it must come from `Env::entropy`
(`api.md` Environment seams) when the interface wires the log in.
**Needed.** (c) with roadmap 8.3; (f) and (k) with 6.3 (deferred there at gate 4.9).

## Q-123 (a), (c)–(g) — The interface on the durable log: points `spec.md` §2.2, §3 and `api.md` leave open
Found while putting the `kvik` interface on the log (roadmap 5.1, `kvik/src/durable/`). Each part names the reading
implemented for now; `kvik/tests/durable.rs` and `kvik/tests/api.rs` pin it.
**Scenario.** (a) *Stamping before the commit time.* `future` and the second clause of `late` compare `t` with
`received_at`, "host clock at commit" (§2.2), but a call's outcomes, flags included, are decided before it is queued,
and the log takes the batch's receive time when it writes the batch (Q-115 (i)), a few milliseconds later or, under
back-pressure, much later: a reading at the 60 s edge can be flagged against one clock reading and stored with
another. (b) *A call with nothing to record* (settled, see below). A call whose readings are all refused or
duplicates writes no batch; its duplicates may be duplicates of facts queued by an earlier call that is not yet
acknowledged, and if that call's sync then fails, the store turns read-only having answered `Duplicate` for a
reading that is not stored. (c) *The
log's timers and the clock seam.* `api.md` makes every clock a seam; with a manual clock that stands still, the
log's `min_sync_interval` never passes and a second batch is never synced. (d) *Acknowledging a loss.*
`acknowledge_losses` appends an acknowledgement record to the manifest (A.14), which the log's writer does not write
at run time yet. (e) *Counts that are not in the log.* Per-series duplicates, refusals and `outranked_on_arrival`
are not recorded anywhere, and a value recorded without one does not say whether it was a supplied `Absent` or a NaN
(`absent_supplied` against D1). (f) *Definitions and ordering.* A definition resolves after its mark sync (§3 Commit);
a second definition of the same name, or a reading of the new series, that is decided before the first is
acknowledged would rest on an entry that may still be lost. (g) *The id floor with both carriers unreadable.* A registry file that
cannot be read is passed over (`format.md` A.16: the log alone suffices). A definition lost from the log (synced bytes
damaged) is then remembered by the mark's `max_series_id` and by the registry file; if the mark is also unreadable,
neither carries its id. What still protects it: the id floor of every recovery entry in the kept log (field 46, the
floor at that start), every series id in a kept fact (the series' orphaned facts), and a definition still readable in
the torn tail. A gap remains for a definition written after the last recovery entry, with no kept fact of its series:
its id can be assigned again. This is the residual §2.1 already accepts for a disk that loses synced data ("an id
defined only in the lost bytes can be assigned again"), here reached by damage to three files.
**Implemented.** (a) The wall clock read when the call is decided stamps the flags; the stored `received_at` is the
log's. (b) Settled at the review of roadmap 5.1, as the existing promise that an acknowledgement means durable
requires: a duplicate of a reading queued by an earlier call that is not yet acknowledged resolves only when that
call does, with its result (the call's other outcomes are final at once); an outcome that rests only on facts already
acknowledged resolves at once. A call with readings to record resolves with its own batch, which is acknowledged after
every earlier batch, so its duplicates need nothing more (`api.md` Writing; `kvik/tests/durable.rs`,
`a_duplicate_of_a_queued_reading_waits_for_the_original`). (c) The log's timers run on the process's monotonic
clock; `received_at` comes from `Env::clock`. (d) Acknowledgements are held in memory until the store closes. (e)
Recorded, late and future facts are counted from the log at every open; the others count since the store was opened.
(f) Calls that append an entry other than facts hold the decision lock until their mark sync returns, blocking the
calling thread; readings are decided against acknowledged definitions only. An `emit` that waits for the clock to be
synchronised looks again every millisecond with `std::thread::sleep` on the polling thread, as there is no event to
wait for. (g) Accepted for now as the §2.1 residual; nothing further is done.
Roadmap 6.3a builds the run-time manifest appender (`kvik-files`, `appender::Appender`) that (d) needs; the
interface's use of it is roadmap 6.3b.
**Needed.** A rule for each other part: (a) whether a batch's receive time may be taken before it is queued, or flags
decided at write time on the writer thread; (c)
whether the seam gains a monotonic clock for the engine's timers or tests take `min_sync_interval` 0; (d) the run-time
manifest append (with roadmap 6.3 at the latest); (e) whether these counts must survive a restart, which would need
them recorded; (f) whether a definition, and an emit waiting for the clock, may wait without blocking the caller's thread; (g)
whether the residual is accepted. To be settled at gate 5.6.

## Q-140 — Live staleness of an observed series in a reader process
**Scenario.** S = 3 s. The writer commits a reading for `t` = −2 s at 0 s; at 2 s a reader in another process asks
`live()`. The writer's own reader answers fresh (2 s since the commit, §5.4 monotonic rule); the reader process
cannot, since the commit instant on the writer's monotonic clock is in no file (Q-16 objection 5). The same holds for
the writer's own process after a restart, before its next refreshing commit (Q-16 (d)).
**Implemented (roadmap 5.4).** A reader process, and a writer before its first refreshing commit since it was opened,
measure live staleness in event time, `now − t` of the live-state fact against S: stale at 2 s in the scenario
(4 s since `t`). This is the model backend's fallback after a restart. A reader process keeps its own window clock
`now` from its open (max(wall clock, previous value), advanced by the monotonic clock), so it can also differ from
the writer's after the wall clock stepped back before the reader opened. Every other result (windows, buckets,
calendar results) is a function of the facts and `now`, and agrees (`kvik/tests/windows.rs`).
**Needed.** Whether `live().stale` in a reader process may differ from the writer's, or the refresh instant must be
made a function of the log (with Q-16), or the reader must say it cannot tell (an `Unknown` staleness, an `api.md`
change).

## Q-141 — A fact's `received_at` after the wall clock steps back
**Scenario.** A batch commits at wall-clock 12:00:00; the clock is corrected back by 1 s; the next batch is formed at
11:59:59.5. §2.2 defines `received_at` as the host clock at commit (11:59:59.5) and `commit_time` as
`max(received_at, previous commit_time)` (12:00:00). The `commit_time` row of `format.md` A.7 (commit block body) says
"`received_at` and the commit time are the wall clock when the batch is formed, raised to the previous batch's commit
time", so the log keeps one value, 12:00:00, as both. The model backend shows 11:59:59.5 in the fact's `received_at`;
the durable backend shows 12:00:00. Flags are stamped from the wall clock when the call is decided (Q-123 (a)), so
`future` and `late` agree; only `received_at` differs, and with it anything a client derives from it.
**Implemented.** As the log has it: `received_at` is the batch's commit time. The differential tests clear
`received_at` when the wall clock stepped back (`kvik/tests/windows.rs`).
**Needed.** Which of the two texts holds: a separate `received_at` in the batch (a `format.md` change), or §2.2
reworded to the raised value.

## Q-142 — A reset ceiling off the parts-per-million grid
**Scenario.** A counter is defined with `reset_ceiling` 0.3333335; readings 10,000,000 then 3,333,337. §5.5 with
that ceiling: 3,333,337 is not below 0.3333335 × 10,000,000 = 3,333,335, so the decrease is a regression and
contributes 0. The log keeps the ceiling in whole parts per million (`format.md` A.6 field 11), 333,334 after
rounding, so a store read back from the log would take 3,333,337 < 3,333,340 as a reset contributing 3,333,337: the
increase differs with the arithmetic used.
**Implemented (roadmap 5.4).** Both backends refuse at definition a ceiling between 0 and 1 that does not survive
the round trip through parts per million (`DefineError::Invalid` naming `counter`); every ceiling of at most six
decimal places is kept exactly (`kvik/tests/api.rs`, `a_reset_ceiling_off_the_parts_per_million_grid_is_refused`).
**Needed.** Whether the refusal stands in `spec.md` §5.5 and `api.md` Series, or the field holds the ratio exactly
(the `f64` bits or a rational, a `format.md` change).

## Q-161 — Telling a restore's own claim marker from another restore's
Recorded at roadmap 8.2. To be settled before roadmap 8.3 (8.3a). Roadmap 8.1a laid out the marker (`format.md` A.17)
with the claiming restore's reset nonce, as suggested below; the procedure is still open.
**Scenario.** Restores R1 and R2 of the same bucket both read generation 2 as the highest claimed and try to claim
3 at once. R1's create-if-absent upload of the marker is stored but its reply is lost; R2's is refused. R2 moves on
to 4. Had R2's upload landed first instead, R1's retry would meet a marker for 3 already there. The sink accepts a
repeated `put` when the object has the same size and SHA-256 (so that a retry after a lost reply succeeds); if every
marker has the same content (empty, say), R1 takes R2's marker for its own and both restores own generation 3.
**Model.** Not modelled; a claim is a create-if-absent step that cannot be confused.
**Needed.** Marker content unique to the restore attempt. Suggested: the marker holds the restore's reset stamp
nonce (fresh random, §8 Generations), so a `put` that succeeds proves the marker is this restore's. Other
content is `Exists` only when no attempt can have reached the bucket, and `HashMismatch` after an attempt that may have landed (`api.md` Backup and restore);
either way the restore treats the claim as possibly lost and takes the next generation, except that a `HashMismatch`
with no marker reported by a following `stat` retries the same generation (Q-305).

## Q-163 — Reading the claims again on a bucket whose listings lag
Recorded at roadmap 8.2. To be settled with roadmap 8.3.
**Scenario.** §8 Generations: after uploading its restore record, a restore "reads the claims again" and stops if a
higher generation is claimed. Restore R1 claims 3; R2 claims 4 a moment later. R1 lists the claims, but the
bucket's listing does not yet show R2's marker (some S3-compatible stores list with a delay; the fake injects it,
`a_lagging_listing_does_not_decide_whether_a_key_is_taken`). R1 accepts ingest in generation 3 while R2 does in 4.
**Model.** Not modelled.
**Needed.** A way of reading claims that does not depend on listings. Suggested: read the claims by `stat` of
the markers above one's own generation, in ascending order, until one is absent; a `stat` of a key is consistent
after its write on buckets with read-after-write consistency, which the bucket requirements would then name.

## Q-164 — Interface points deferred to 8.2 that need tiers or restore
Recorded at roadmap 8.2, which settled the sink only. To be settled by roadmap 8.1 and 8.3.
**Scenario.** Three interface points were deferred at gate 3.3 to roadmap 8.2: (a) whether `Reader::facts` fetches
detail that is not local itself (`api-check.md` AC-12), which would put network calls in a `Reader`,
whose calls are blocking file I/O and which owns no thread; (b) what happens to files sealed while no sink is
attached (AC-25); (c) the restore life cycle (AC-24). None can be decided without tiering (8.1) or restore (8.3).
**Model.** Not modelled.
**Needed.** (a) and (b) settled with 8.1, (c) with 8.3. Suggested for (a): `facts` does not fetch; detail not local
is reported as such and fetched by an asynchronous `Store` call on the maintenance thread.

## Q-170 — Durability levels chosen per source

Decided by the owner: a source may choose between the default level, where a reading is acknowledged only after the
sync that covers it, and a faster level, where a reading is acknowledged once it is written (in the operating
system's cache, before the sync). The faster level survives a crash or restart of the process but not a power cut or
a kernel crash; the background sync bounds what such an event can lose. The level is chosen per source (per series
definition), with the default level unless the definition says otherwise.

Scenario: a price feed defined at the faster level emits a reading, gets its acknowledgement within microseconds, and
a consumer acts on it; a power cut 200 ms later removes it. A meter series in the same store, at the default level,
is acknowledged only after its sync and never loses an acknowledged reading.

To settle (design at roadmap 5.5a): the definition parameter and its default; the bound on unsynced data (for example
`max_unsynced`, default 1 s, never beyond the timed sync); what an acknowledgement and `Outcome` say about the level;
a "durable up to" watermark in health and statistics; how a reading at the faster level that a power cut removes is
reported (a reset point and `possible_loss`, as rule 1's reported truncation does), and what consumers that acted on
it receive; batches mixing levels (a batch is acknowledged per reading at its own level; a definition or route
change is always synced); principle 1 and invariant wording ("acknowledged at its level"); whether a per-call
override (wait for the sync on a faster-level series) is offered; and, since rule 1 truncates everything beyond the
mark on the same boot (Q-125), rule 1, principle 3 and invariant 2 rewritten for the faster level so that a process
crash keeps checksum-valid complete batches beyond the mark.

## Q-173 — The operator call `rebuild_manifest`: finality, a readable manifest, several corrupt records
Found while building the manifest rebuild (roadmap 6.3a, `kvik-files` `rebuild::rebuild_manifest`).
**Scenario.** (a) §7 names `rebuild_manifest` as the way out for a process that restarts more often than the wait, but
not whether the rebuild it makes is final (the operator vouching that no copy exists) or provisional (deep has still
not answered). (b) Whether the call may rebuild a manifest with no corrupt record, and what the rebuild record's
fields 1 and 2 then hold. (c) With two corrupt records, fields 1 and 2 name one corrupt record.
**Implemented.** (a) The caller passes the finality; the crate decides nothing. (b) A readable manifest is rebuilt like
any other; fields 1 and 2 are 0. (c) Fields 1 and 2 name the first corrupt record.
**Needed.** (a) and (b) with the call's signature in `api.md` (roadmap 6.3b); proposed: provisional unless deep has
answered, and allowed on a readable manifest. (c) a sentence in A.14 if the first is not what is meant.

## Q-175 — An intermediate output no record and no local file describes
Found at the review of roadmap 6.3a (`kvik-files` `rebuild::plan`).
**Scenario.** Merge A (compacted 1 and 2 into 3) and merge B (3 and 4 into 5) are both corrupt records; a later valid
record retires 5, naming 3 among its inputs. 3 was retired by B and removed locally after its grace period, and no
valid record describes it, so nothing says what 3's own inputs were: 1 and 2, live by the valid records that
published them, cannot be retired. If they are still on disk they are listed beside 5's replacement, which holds
their facts (invariant 14 as the records show it is not broken, as no live file names them; positions are held
twice). Not reachable with one corrupt record.
**Implemented.** Each input no valid record names is followed through its description (a valid record or a local
file) to a fixpoint; one described nowhere is reported (`RebuildPlan::undescribed`) and its inputs stay as the valid
records leave them. An output that names such an input is never discarded for it: the input is not at hand, so the
output is live from its named inputs, even where 1 and 2 are gone too and the output is the only compacted file
holding their facts. `files/tests/rebuild.rs`, `two_corrupt_merges_retire_every_input_down_the_chain`;
`files/tests/rebuild_scenarios.rs`, `q175_an_output_whose_unnamed_input_is_gone_stays_live_and_is_never_discarded`.
**Needed.** A rule: proposed, a compacted file whose position range lies within a live output that names, through
any chain of described files, an undescribed input is retired; or the chunk index of 3 (kind 14, roadmap 6.2), which
carries its inputs, is kept until every file naming 3 is retired. With roadmap 6.3b.

## Q-180 — Stored nodes keep sums, integrals and increases exact, not in `f64`

**Scenario.** A gauge read every 5 s stores 15-minute nodes. A window over a day takes 96 stored nodes in the middle
and computes its edges from facts (§6 Evaluation). With `f64` node fields, the window's sum depends on how the parts
are bracketed: the same day computed from facts alone, or from 1-minute nodes, differs in the last bits, and the
reference model, which is exact, differs again. §5.6 asked for bitwise reproducible stored nodes and compared other
orders within a relative 1e-9; `format-check.md` FC-34 assumed `f64` fields for a series without a decimal scale.
**Settled at the review of roadmap 7.1a:** exact. Every field a fold or merge gives is a sum of `f64` values or their
products with whole nanoseconds, so a dyadic number: ± an odd mantissa × 2^e with e ≥ −1074 and a magnitude below
2^1100 stores it exactly (`format-tree.md` T.3). Every aggregate is then identical to the model's whatever the
bracketing, and invariants 6, 7, 21 and 22 hold exactly (`spec.md` §5.6, §6, §10). `kvik-tree` folds with a dyadic
accumulator (an integer mantissa and a binary exponent, so no gcd runs); a rational appears only after the division of
a window's pro rata interval credit, and window nodes are never stored. Measured in release
(`cargo run --release -p kvik-tree --example fold_cost`, one machine): a gauge read every 5 s folds in 2.0 µs per
reading with rationals and 0.05 µs with the dyadic accumulator; a counter in 0.40 µs and 0.03 µs. Node sizes are the
same either way: 86 B for a 1-minute gauge node of 12 readings, 88 B for its 15-minute node of 180 readings, and 346 B
for a 1-minute node holding one subnormal reading.

## Q-181 — Which zero is the minimum of +0.0 and −0.0

**Scenario.** A gauge reads 0.0 and then −0.0 in one minute. IEEE `minNum` does not specify which zero it returns, nor
does Rust's `f64::min`, so two builds, or two orders of merging, can store different bytes for the same readings,
and rebuild-equals-live (invariant 8) compares bytes. Results do not differ: both zeros give the result 0.
**Settled at the review of roadmap 7.1a:** min and max are taken in IEEE total order, −0.0 below +0.0 (`spec.md`
§5.2, §5.6; `format-tree.md` T.5), so stored bytes depend neither on the order of folding nor on the platform. The
model's `f64::min` may pick the other zero, which compares equal; the tree's tests compare min and max by bit pattern
(`tree/tests/properties.rs`).

## Q-182 — A state name declared twice

**Scenario.** A state series declares the names `["on", "on"]` and reads index 0, then index 1. The durable backend
folds states by index, so it counts a transition and splits the time in state between two keys; the model folds by
name, counting no transition and one key. The two backends' `transitions` and time-in-state then differ for the same
facts.
**Settled at the review of roadmap 7.1a:** the names of a state series are distinct. A definition declaring a name
twice is refused by both backends (`kvik/tests/api.rs` `a_state_name_declared_twice_is_refused`), and a definition
entry carrying one is invalid content in the log (`format.md` A.6 field 13; `log/tests/batches.rs`), so index and name
count the same transitions (`spec.md` §2.1 `value type`).

## Q-190 — The Parquet export schema as a user contract

**Scenario.** A user loads monthly exports into an analytics tool by column name. If a later release renames
`supplied_quality` or turns `generation` signed, their queries break, although no fact changed.
**Draft (roadmap 6.4a).** `docs/export.md` drafts the columns, their Parquet logical types and the file metadata; every
file carries `kvik.export_version` = `0`, and the crate `kvik-export` writes and reads it back (round-trip tests in
`export/tests/`).
**Settled by the owner, 2026-11-16,** as recommended: the schema as drafted is a user contract, and every file
carries `kvik.export_version` = `1`. Adding a column or a metadata key is not a breaking change; renaming, retyping or
removing one raises the version (`docs/export.md` Compatibility; `export/tests/rows.rs`
`file_metadata_names_the_series_view_and_range`).

## Q-191 — The default view of an export, and Parquet for orphaned and set-aside facts

**Scenario.** A versioned price series has three revisions of the 14:00 value. An export in the current view holds
one row for 14:00; one in the history view holds all three, which re-ingest into an empty store (§8 Re-ingest)
needs to reproduce the revisions and the duplicate rule's state.
**Draft (roadmap 6.4a).** `ExportOptions::view` defaults to `Current`, and `History`, `AsOf(P)` and `AsOfTime(T)` are
options. Orphaned and set-aside facts are returned as facts by their own calls (`api.md`), not written to Parquet.
**Settled by the owner, 2026-11-16,** as recommended. (a) The default view is `Current`; `docs/export.md` Options
names `History` as the view to export for re-ingest. (b) Orphaned and set-aside facts get the same schema, one file
per call with rows of several series, added with roadmap 8.3 (settle-by row).

## Q-192 — Decimal values in the export, and the scale in force on `SeriesInfo`

**Scenario.** A series is defined at scale 2 and raised to scale 4 (§2.1 `decimal scale`); facts are stored at
both. One Parquet column has one `DECIMAL(p, s)`.
**Draft (roadmap 6.4a).** `value` is `DECIMAL(19, s)`, `s` the series' scale, each value rescaled exactly (19 digits
hold any `i64` units), and `stored_scale` keeps each fact's stored scale, so the units and stored scale come back
unchanged; a fact stored above `s` is an error, never rounded. `SeriesInfo` exposes only the definition's scale, so the
exporter uses it; decimal series are not stored by either backend yet, and the tests use synthetic rows
(`export/tests/rows.rs` `decimals_round_trip_with_their_stored_scale`,
`a_decimal_stored_above_the_export_scale_is_an_error_not_a_rounding`).
**Settled by the owner, 2026-11-16,** as recommended. (a) `DECIMAL(19, scale in force)` with `stored_scale`. (b) The
scale in force reaches `SeriesInfo` or the export call with decimal series on the durable backend (roadmap 6.4b,
settle-by row); until then the export refuses a fact stored above the definition's scale (`ScaleAboveExport`).

## Q-193 — Fact flags as boolean columns or a bitfield

**Scenario.** A user wants every late reading of last week: `WHERE late` against `WHERE flags & 2 <> 0`.
**Draft (roadmap 6.4a).** Seven `BOOLEAN` columns, named as in §2.2 and §2.5.
**Settled by the owner, 2026-11-16,** as recommended: seven `BOOLEAN` columns (`export/tests/rows.rs`
`each_flag_lands_in_the_column_of_its_name`).

## Q-194 — Quality and named states in the export

**Scenario.** A dashboard filters `quality = 'good'`; a re-ingest needs a named state's index, which a reordered
list of names would change.
**Draft (roadmap 6.4a).** `quality` and `supplied_quality` are strings (`good`, `estimated`, `suspect`, `bad`),
dictionary-encoded; a named state is its index (`INT(16, false)`) in `value` with its declared name in `state_name`, and
the declared names in the metadata.
**Settled by the owner, 2026-11-16,** as recommended: quality as dictionary-encoded strings (the outcome pinned by
`export/tests/rows.rs` `quality_columns_are_dictionary_encoded`; the writer's explicit request matches its default
today); a named state as its index in `value` with `state_name` beside it.

## Q-195 — A `changed_at` of kind `time` beyond the timestamp range; forecast labels

**Scenario.** An adapter sends `changed_at` = 2^63 on a series of `changed_at kind` `time`. §2.2 makes `changed_at` a
`u64` and the store records it; a Parquet timestamp is a signed 64-bit integer and cannot hold it.
**Draft (roadmap 6.4a).** The export fails with `ChangedAtOutOfRange` (`export/tests/rows.rs`
`changed_at_must_match_the_series_versioning_and_fit_a_timestamp`). A forecast series is labelled only by `kvik.timing`
= `forecast`: `t` is the valid time and `changed_at` the issue time (F-13).
**Settled by the owner, 2026-11-16,** as recommended. (a) A `changed_at` above `i64::MAX` on a series of `changed_at
kind` `time` is refused at ingest and counted, as every other time in `format.md` is an `i64`:
`Refusal::ChangedAtOutOfRange` on both backends, checked after `ChangedAtMissing`, `ChangedAtUnexpected` and
`ReviseOnVersioned` (`spec.md` §2.2, `api.md` Errors; `model/tests/rules.rs`
`s2_2_a_time_changed_at_above_i64_max_is_refused`, `kvik/tests/api.rs`
`a_time_changed_at_above_i64_max_is_refused_and_a_version_is_not`). `i64::MAX` itself is accepted; a `changed_at` of
kind `version` stays any `u64`. The export keeps `ChangedAtOutOfRange` for rows that reach it otherwise (`export_rows`,
or a log that holds one, Q-760). (b) Forecast exports keep the column names `t` and `changed_at`; the label
`kvik.timing` = `forecast` says which is the valid time and which the issue time.

## Q-196 — Export file layout, compression and store-level metadata

**Scenario.** A user exports a hundred series: a hundred files, each with a typed `value` column, or one file whose
value column cannot have one type. Later they want to know which store and which durable point an export came from.
**Draft (roadmap 6.4a).** One file per series per call; row groups of at most 131,072 rows; zstd level 1. The metadata
names the series, view and range, not the store: its id, tzdata version, durable position and reset stamp are not
reachable through the public `Reader` without a side effect (`refresh`).
**Settled by the owner, 2026-11-16,** as recommended, and beyond the draft. (a) One file per series per period. (b) Row
groups of at most 131,072 rows; zstd level 1. Beyond the draft: store and site identity in every file's metadata now,
and a partitioned path layout `site=<site>/series=<series>/month=<YYYY-MM>/` for consolidating many sites from a
bucket. Applied as follows (`docs/export.md` Options, Partitioned layout, Metadata; `export/tests/layout.rs`), the
choices below being the implementation's, not the ruling's:
- `kvik.store_id` (32 lower-case hex digits, as `format.md` A.17 writes a store id) is in every file `export_series`
  and `export_partitioned` write: they take it from `Reader::store_id`, an additive read-only item of the interface
  that returns the id read at open (`api.md` Reader processes; `kvik/tests/api.rs`
  `the_store_id_is_the_same_through_every_reader_and_after_a_reopen`), and refuse a different one in the options.
  `kvik.site` is in every partitioned file, which needs a site; `export_series` writes it when given. `export_rows`,
  which has no reader, writes either only when given.
- The rest of the store-level metadata (tzdata version, durable position, reset stamp) is not reachable through the
  public `Reader` without a side effect (`refresh`), so it is not yet in the files; it comes with the export call in
  the store's interface, and the saved point of a continued export (row 7b) after 5.3 (c, settle-by row 8.3).
- A site label is 1 to 64 bytes of `a`–`z`, `0`–`9`, `-` and `_`, starting with a letter: one folder on every file
  system and bucket, case-folding or not, never `.` or `..`, and never read as a number by a Hive-style reader
  (pyarrow and polars read `site=0042` as 42).
- `export_partitioned` writes one file per series per UTC month with a fact, at
  `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<YYYY-MM>/<view>.parquet` (the amendment below). Each
  segment is escaped on its own, byte by byte: `a`–`z`, `0`–`9`, `-` and `_` kept, every other byte `%XX` in
  upper-case hex; reversible, one escaped form per segment, and injective under case folding (property tests
  `any_name_round_trips_through_its_segment`, `each_name_segment_round_trips_through_its_own_path_segment`,
  `a_segment_built_from_valid_tokens_is_the_escape_of_its_name`, `a_segment_with_one_invalid_token_does_not_unescape`).
  An all-digit segment stays all digits, so `docs/export.md` tells readers to declare the partition columns as
  strings. The month is the UTC month of `t` for every series, interval series included (an interval goes under
  the month of its start): it needs no tzdata, so it is the same in every release and site zone and months line up
  across sites (`months_cut_at_utc_midnight_on_the_first`, `a_month_holds_its_timestamps_and_starts_where_the_last_ends`
  against a days-from-civil oracle). The file name is the view (`current`, `history`, `as_of-<g>-<s>`,
  `as_of_time-<ns>`), so exports in different views do not replace one another; a month the range cuts is named
  `<view>.partial-<from_ns>_<to_ns>.parquet`, so it never replaces a whole month's file. The files of one call are
  not one cut of the store, and a caller publishes them after the call returns (`docs/export.md` Partitioned
  layout). Long and renamed names: Q-761.

**Amended by the owner, 2026-11-17.** With three-segment names (Q-670), the path splits the series name into its
segments, `site=<site>/area=<segment 1>/device=<segment 2>/metric=<segment 3>/month=<YYYY-MM>/`, so cloud engines can
prune by area, device or metric; file contents and export version 1 unchanged. Applied as follows, the choices being
the implementation's:
- The name is split at `/` (`name_segments`), and each segment escaped as above; a segment holds no `/`, so no
  escaped segment holds `%2F`. Until Q-670 is enforced at definition and alias, `export_partitioned` refuses a series
  whose name has fewer or more than three segments, or an empty one, with `UnsupportedSeries` before it reads a fact
  or calls `open`, rather than filing it elsewhere (`a_name_without_exactly_three_non_empty_segments_has_no_partition_folder`,
  `a_name_of_other_than_three_segments_or_with_an_empty_one_is_refused`,
  `a_partitioned_export_refuses_a_name_without_three_non_empty_segments_and_opens_nothing_on_both_backends`).
- `area`, `device` and `metric` join `site`, `series` and `month` as names no file column may take (`docs/export.md`
  Compatibility); `series` stays reserved for readers of the first form.
- The reading examples declare all five partition columns as strings; DuckDB, pyarrow and polars were checked on
  exported files, pruned by device and by metric across devices.

## Q-197 — The interval end in an export, and calendar-length interval series

**Scenario.** A half-hourly import series is exported for a billing tool that wants each interval's end, not its
length.
**Draft (roadmap 6.4a).** An interval series has an `interval_end` column, `t + d` (F-14, F-17), and
`kvik.interval_length_ns`. Calendar-length interval series are refused by the export, as neither backend stores them.
**Settled by the owner, 2026-11-16,** as recommended: keep `interval_end`. For calendar lengths each end is taken from
the interval zone under the tzdata version in force at export, recorded in the metadata; built with calendar-length
interval series (roadmap 7.3, settle-by row).

## Q-198 — The backup and restore interfaces ahead of their implementations

Recorded at the interfaces commit of roadmap 8.1 and 8.3, which defines the types, traits and fakes the parallel
items build against (`kvik-backup`, `files/src/{tier,restore}.rs`, `log/src/setaside.rs`,
`log/src/writer/options.rs`). Decided: (a) to (d); (e) and (f) settled at roadmap 8.1a.
**Scenario.** (a) A `put` of a sealed segment is stored but its reply is lost; the store restarts and seals the
same file again. With a random nonce the bytes differ, the sink refuses the repeat (`HashMismatch`), and the
segment can never be uploaded. (b) An operator points two stores at one bucket under different prefixes; a restore
must find the store without being told the prefix. (c) A restore is killed after its record is synced but before
every file is installed; `Store::open` cannot finish it without the sink. (d) The upload driver runs on its own
thread, so the sink it holds must outlive the call that attached it; and a partial restore needs a scope, which a
`Copy` mode cannot hold. (e) Two parallel items must agree on every object key before either is merged. (f) Two
restores race for one generation (Q-161).
**Decided.** (a) The envelope is deterministic: its nonce is derived from the key, the object key and the
plaintext hash (`Envelope::seal`), so a repeated `put` sends identical bytes. (b) There is no key prefix in the
engine: keys start at the store id, a restore finds the store id by listing the bucket, and two store ids fail it
(`RestoreError::SeveralStores`). (c) An unfinished restore is resumed by calling `Store::restore` again (Q-164 (c)).
(d) `attach_backup` and `restore` take `S: BackupSink + 'static`; `RestoreMode` is not `Copy`, defaults to `Full`
and gains `Partial(RestoreScope)`, `NotSupportedYet` until roadmap 8.3f; `RestoreScope` holds only the time range
until a series selection exists.
**Provisional at the interfaces commit.** (e) The key layout of Q-160's suggestion without the prefix, in `backup/src/keys.rs`, the only
place a key is built or parsed; kinds other than log segments under `files/<kind:2 hex>/`. (f) The claim marker is
12 bytes, the generation (`u32`) then the restore's reset-stamp nonce (`u64`), little-endian (Q-161's suggestion).
Both were to be fixed by `format.md` A.17 before anything is uploaded.
**Settled at roadmap 8.1a.** (e) and (f) as above: `format.md` A.17 takes this contract's keys (other kinds under
`files/<kind:2 hex>/`, the probe at `<sid>/probe`) and its 12-byte marker, which repeats neither the store id (in the
key, bound by the envelope) nor a version (in the envelope header). This contract was ruled authoritative over the
earlier A.17 draft (`data/` and `tree/` prefixes, a 40-byte marker with magic, version and store id); nothing
had been uploaded. A.17 was aligned to the contract's types in the same commit: a key that starts with no store id
is ignored, since `Refused::OtherStore` and `RestoreError::SeveralStores` carry store ids; an uploaded record is the
canonical encoding (`Record::encode`, as the manifest port hands out decoded records); the attach probe checks the
key against `<sid>/STORE` (`Refused::WrongKey`). Two mismatches remained, Q-213 and Q-214; roadmap 8.3b settled both.
**Model.** Not modelled.

## Q-210 — Telling a bucket that reports no checksum from another sink failure
Recorded at roadmap 8.1a. To be settled by roadmap 8.1c.
**Scenario.** The owner ruled that a bucket reporting no checksums is refused (`BackupRefusal::NoChecksums`). The
probe's `stat` meets such a bucket, and `kvik-sink` fails it with `SinkError::Failed { reason }`, the same variant as
refused credentials or a missing bucket. The driver cannot tell the refusal from a fault it should report as
`BackupError::Sink`.
**Model.** Not modelled.
**Needed.** A distinct report. Suggested: an additive variant `SinkError::ChecksumMissing { key }` in
`kvik-sink-api` (the enum is non-exhaustive), returned by `kvik-sink` where it now returns `Failed` for a missing
SHA-256, and mapped to `NoChecksums` by the probe. Until then the probe reports `Failed` as `BackupError::Sink`.

## Q-211 — An uploaded epoch written again after the last two synced records are damaged
Recorded at roadmap 8.1a. To be settled by roadmap 6.3d.
**Scenario.** Records 1 to 20 are synced; record 19 is uploaded, since 20 was synced after it (§7 Manifest). The
device then damages records 19 and 20 together. No valid record follows them, so a start reads them as a torn tail
(§7 Manifest), truncates after 18 and gives the next record epoch 19 again, with other content. Its upload under
`manifest/<generation>/<epoch>` (`format.md` A.17) meets the uploaded record 19: the key is taken, and the bucket
holds two different records 19 for one generation, one of them in no local manifest.
**Model.** Not modelled.
**Needed.** A rule that a start does not reuse an uploaded epoch. Suggested: a start that drops a tail, with deep
configured, asks deep (`stat`) for the record keys above the last valid epoch in its generation; one that is present
was uploaded, so its successor had been synced and the tail is corrupt, not torn: it is fetched and written back as
§7 repairs a corrupt record, and new epochs continue above the highest present.

## Q-212 — A key already taken by an object that is not the file
Recorded at roadmap 8.1a. To be settled by roadmap 8.1c.
**Scenario.** The envelope is deterministic (`format.md` A.17), so a repeated upload of the same file sends the same
bytes and the sink accepts it. A `put` that fails with `Exists` or `HashMismatch` therefore means the key holds
something else: another writer under `<sid>/`, damage in the bucket before the copy was verified, or Q-211. The key
can never be used for the file (no overwrite, no delete), and the restore derives keys from the file's name.
**Model.** Not modelled.
**Needed.** What the driver does. Suggested: no alternate key in format v1; the file stays local, is never evicted,
and health asks for attention (`BackupStalled` naming the key); the queue continues with other files, but no
manifest record that names the file is uploaded while it is not backed up.

## Q-270 — Envelope damage that `EnvelopeError` has no variant for

Recorded by roadmap 8.1b, which implements the envelope (`backup/src/envelope.rs`, `ChunkedXChaCha`). Decided
there, provisionally.
**Scenario.** (a) One bit of an object's header flips in the bucket: the magic or `header_crc32c` no longer
matches. A.17 calls this a damaged object, never `WrongKey` or a version refusal, but `EnvelopeError` (fixed by
the interfaces commit, Q-198) has no damage variant other than `Authentication { chunk }`. (b) The same with valid
CRC but non-zero reserved bytes. (c) An object longer than its header's length gives (an extension). (d) A ranged
read for plaintext past the length, or an inverted range.
**Decided (provisional).** (a) and (b) are `Authentication { chunk: 0 }`: every chunk binds the header as
associated data, so chunk 0 could not authenticate anyway; a restore treats it as damage (retry, then `Lost`,
Q-82). The order of A.17 Reading holds: magic and checksum, then version and algorithm (`Unsupported`), then
reserved bytes, then the key check (`WrongKey`). (c) is `Authentication { chunk: n }`, `n` the chunk count, as for
a chunk that cannot exist; a shorter object is `Truncated`. (d) is `Truncated`, as the test envelope does. A
header whose length needs more than 2^32 chunks is `TooLarge`, before anything is allocated. An empty range opens
to nothing without reading any chunk, and `span` of it is empty. Pinned by `backup/tests/envelope.rs`
(`a_damaged_header_is_damage_never_a_wrong_key_or_a_version_refusal`, `truncation_and_extension_are_refused_by_size_before_any_chunk`,
`a_range_past_the_plaintext_or_inverted_is_refused`). A `Damaged` variant would be clearer; adding one changes the
seam, so it is the lead's call.
**Model.** Not modelled.

## Q-280 — How a rebuilt manifest queues files for upload again

Recorded and decided by roadmap 8.1c.
**Scenario.** Segments 1 to 3 are uploaded and each has a copy-verified record (`0x27`). A record is then found
corrupt and the manifest is rebuilt: the rebuild record comes first and every valid record follows it, carried
with field 100 (`format.md` A.14), the three `0x27` records included. §7 says a rebuilt manifest takes no file as
verified in another tier until it is verified there again. Folded as written, the carried `0x27` records would mark
the files verified again after the rebuild record cleared the marks; ignored, they would leave no copy, and every
file would be uploaded again.
**Decided.** A `0x27` carried into a rebuilt manifest (it holds field 100) sets the file's deep copy (`FileEntry::deep`)
and no verified mark; a `0x27` written since then sets both. The upload queue is every live file not verified in deep
(not `deep == None`). For a queued file with a copy, the driver asks the bucket for the object's reported size and
hash; if they equal the copy, it appends a fresh `0x27` with no upload, and otherwise uploads the file as for one with
no copy (the envelope is deterministic, so the same key takes the same bytes; other content under the key fails,
Q-284). A copy of a corrupt record appended later (`rebuild::resolve`) carries field 100 too, so it is checked the
same way. Reason: the copy is a fact about the bucket that the rebuild does not change, so a restore reading the
rebuilt records still finds it, while eviction and removal wait for the fresh check §7 asks for. The restore plan uses a file's recorded copy (`deep`); "verified in deep" governs the upload queue only (owner's
ruling, see Q-295). Pinned by
`files/src/catalogue.rs` `q280_a_carried_copy_verified_record_sets_the_copy_but_no_verified_mark` and
`backup/tests/upload.rs` `q280_a_rebuilt_manifest_verifies_each_copy_again_without_a_new_upload`,
`q280_a_rebuilt_copy_the_bucket_no_longer_reports_is_uploaded_again`.
**Model.** Not modelled.

## Q-281 — The copy-verified record (`0x27`): layout and fold

Recorded by roadmap 8.1c; field 1 settled by the owner's ruling.
**Scenario.** A.14 reserved `0x27` for a tier move; the backup and restore interfaces (Q-198) use it for the
copy-verified record, which a restore reads to know which files have a copy in deep. §7 Manifest and `format.md`
Part B say every record that names a file repeats its kind, position range and plaintext hash; once eviction lands,
a corrupt publishing record would otherwise leave a file in deep known by reference only.
**Decided.** `0x27` copy verified: 1 the file as a whole file description (A.14), so `CopyVerified::file` is a
`FileDescription` (owner's ruling, before anything is uploaded); 2 the tier `u8` (1 hot, 2 cold, 3 deep); 3 the
object's size `u64`; 4 its SHA-256 as the bucket reported it (32 bytes); each exactly once, other fields below
`0x8000` refused as unsupported. Written as a reserved type, so older readers carry it as a field list; decoded and
encoded strictly. The catalogue refuses one that describes a known file otherwise (`CatalogueError::Conflict`), sets
the deep copy and the verified mark (Q-280 for a carried one), and for a file it does not know changes nothing.
Pinned by `files/tests/records.rs` `a_copy_verified_record_round_trips_through_its_block`,
`the_checks_of_a_copy_verified_record`, `a_block_of_type_0x27_is_decoded_and_encoded_strictly` and
`files/src/catalogue.rs` `q281_a_copy_verified_record_describing_a_file_otherwise_is_refused`.
**Model.** Not modelled.

## Q-282 — The upload queue, the record cursor and which records wait

Recorded and decided by roadmap 8.1c.
**Scenario.** (a) The store restarts after uploading records up to epoch 40; nothing local says so, and a queue file
would be a new durable structure. (b) A compacted file is published at epoch 5 and retired by a merge at epoch 9
before it was uploaded; if record 5 waits for its file, it waits for ever. (c) Settling the last record appends an
empty record, which is then the last; settling that one too never ends. (d) Files sealed while no sink is attached
(Q-164 (b)). (e) Record 5 of 8 is damaged on disk while the store runs, after records 1 to 4 went up; epochs are
not dense, so the driver cannot see the gap itself, and a port that passes over the damaged record lets 6 to 8 go up
with 5 missing from the bucket for good.
**Decided.** (a) No queue file: the file queue is recomputed at each step from the manifest's snapshot (live files not
verified in deep, ascending position), and the record cursor is the highest epoch a listing of the generation's
records shows, read at the driver's first step. A listing that lags only makes the driver `put` records it had
uploaded, with identical bytes, which the sink accepts. (b) A record waits only for the files it publishes or lists
that are still live; a file retired before its upload is not uploaded (the files that replace it hold its facts).
So the files records wait for are exactly the queue. (c) The last record is uploaded only after `settle`; an empty
last record is never settled, so a drained queue appends at most one empty record and the driver is then `Idle`,
also after a restart, since the rule reads only the manifest. (d) They are in the snapshot, so they are queued at the
first step after attach, oldest first (input to Q-164 (b), which roadmap 8.1d settles). Only records at or below the
snapshot's last epoch are uploaded, so a record whose sync failed never goes up. (e) By the owner's authorisation
to change the frozen port: `ManifestPort::records_after` returns an error for a record after `after` that cannot be
read, never skipping it, and `DirectPort` does so; the driver then fails and uploads no later record (§8
Generations). This is the contract the store's own port (roadmap 6.3c) keeps. Pinned through `DirectPort` by
`backup/tests/upload.rs` `a_damaged_record_stops_the_upload_of_those_after_it_through_the_direct_port`.
**Model.** Not modelled.

## Q-283 — The attach probe

Recorded and decided by roadmap 8.1c, applying the owner's ruling on Q-162 (a bucket without conditional writes is
refused).
**Scenario.** §8 refuses a bucket that holds another store's files and requires a bucket that rejects a hash mismatch
and honours create-if-absent; the engine never deletes, so the probe's object stays, and a second attach meets it.
**Decided.** In this order, reads first: (1) the bucket is listed around this store's keys and any key under another
store id refuses it (`OtherStore`; a lagging listing can miss one, so this is best effort); (2) the bucket's `STORE`,
if any, must open under the key (`WrongKey`; an unreadable header is a sink failure). Then, on this store's probe
key: (3) a `put` with a wrong hash must be refused (`NoChecksums` otherwise); (4) the probe content, put with its own
hash, must be reported back with that size and hash (`NoChecksums` otherwise); (5) a `put` of other content must be
refused (`NoConditionalWrites` otherwise). At a later attach `Exists` and `HashMismatch` both count as refusals in
(3) and (5). A bucket that checks the signed payload hash refuses (3) whatever it does with the checksum header, so
it passes; that is the check the hash rule needs. A sink failure is `Refused::Sink`.
**Model.** Not modelled.

## Q-284 — Upload failures: retries, other content, an object not reported after its `put`

Recorded and decided by roadmap 8.1c; part (c) settled by the owner's rule on Q-82 (an object is lost after 3 failed
lookups over at least 10 minutes while other calls answer), which the owner confirmed applies to the read-back after
an upload.
**Scenario.** (a) Transient faults, a busy sink, and a `put` refused with `HashMismatch` (damaged in transit, or after a
lost reply). (b) Another object already under a file's key. (c) The bucket accepts a `put` and then reports no
object for the key.
**Decided.** (a) `Retry`, with delays doubling from `retry_first` to `retry_max`, reset by the next progress;
`failing_since` is the first failure of the run, which the caller turns into `BackupStalled`. (b) `Failed` (the
bucket is never overwritten; how the engine recovers is Q-85's). Other content reported after a `put` likewise fails,
and so does a `STORE` object other than the sealed local descriptor, found before the first upload.
(c) The driver asks again, by `stat` alone, without a new `put`; a lookup counts only when another call to the bucket
answers in the same step. After 3 counted lookups over at least 10 minutes the object is taken as lost and uploaded
again. Upload never records a loss: the local file is the source. The S3 sink already reads an object back before
its `put` returns, so (c) arises only when an object vanishes between two lookups.
**Model.** Not modelled.

## Q-285 — Uploading files with no local layout yet, and the hash a file is checked by

Recorded by roadmap 8.1c. Open.
**Scenario.** `format.md` A.12 lays out local paths only for log segments; compacted files, tree checkpoints and the
other kinds under `data/` and `tree/` have none yet. A compacted file published by roadmap 6.2 or 6.3 is live and
queued.
**Decided for now.** The driver reads only log segments; a queued file of another kind fails the upload by name
(`UploadError::Io`, unsupported), so the gap is visible in health, never skipped silently. A file is checked before
upload by the SHA-256 of the whole local file against its description's hash (the hash a sealed segment records,
roadmap 6.3b); a mismatch is `UploadError::LocalHash`.
**Needed.** The local path of each kind, from the item that lays it out.
**Model.** Not modelled.

## Q-291 — Recovery-entry loss reports of the generation a restore reads
Recorded by roadmap 8.3a. To be settled by roadmap 8.3d.
**Scenario.** A restore record carries the recovery-entry loss reports of the generations it restores from, so a loss
whose entry the restore sets aside stays listed (§8 Acknowledging a loss). A `possible_loss` (§3 Recovery rule 1) is
held only by its recovery entry, inside a log segment, and the record is written before any file is downloaded
(§8 Restore, Failures and end). A store restores generation 0 to `0:20`; the recovery entry at `0:25`, which reports
a possible loss, is set aside. The plan, made from the records, carries only the reports of the record it read
(`RestoreRecord::entry_losses`), so generation 1's record lists none, and with set-aside positions excluded nothing
reports that loss.
**Model.** Not modelled; the model writes no recovery entries.
**Needed.** Where the source generation's reports come from. Suggested: read from the listed segments once step 1
has installed them (set-aside ones included; they stay local) and reported at every start, as the set-aside range
is; the record keeps carrying the earlier generations' reports.

## Q-292 — A restore record larger than one block
Recorded by roadmap 8.3a, which laid out the restore record. To be settled by roadmap 8.3, before a store can list that many files.
**Scenario.** A restore record lists every file it restores from (§8 Generations), and log segments are never
retired by compaction (§7), so the list only grows: each listed log segment takes 128 bytes (field header, tier,
copy size and hash, file description), so the 16 MiB block body holds about 130,000. A store that seals a segment
every hour reaches that in about fifteen years; compacted files and their inputs make it sooner. The record can
then not be written and the restore cannot proceed.
**Model.** Not modelled; the model's restore record holds no file list.
**Needed.** A form for a longer list: several `0x25` records of one restore, each naming the same stamp, read as
one; or a list of log segments by ranges of ids. Until then such a record fails to encode (`EncodeError::BodyTooLong`).

## Q-294 — Where the model's restore differs from the plan
Recorded by roadmap 8.3a, whose plan is tested against the model (`kvik/tests/restore_plan_model.rs`). To be settled
by roadmap 8.3b, with the restore executor.
**Scenario.** (a) A store defines a series (`0:1`) and ingests `0:2` to `0:6`; the batch at `0:3` is corrupted
below the mark and the next start records it as a lost range; the segment is sealed and uploaded. The model's
restored end is `0:2`: it ends at any missing position. §8 Restore step 1 and the plan give `0:6`, since a recorded
lost range does not end the restored log (checked with `Model::corrupt_open_batch`, `restart`, `upload` and
`restore_from`). (b) A store seals and lists segments but none is uploaded yet;
its records are in the bucket. The model refuses the restore (`Error::EmptyBucket`); §8 Generations refuses only a
bucket with no manifest records, and the plan restores an empty log, restored end `0:0`, everything later set aside.
**Model.** As described; the differential test leaves both cases out by construction.
**Needed.** (a) The model's restored end to skip recorded lost ranges, as the plan does. (b) Whether a restore whose
records list no copied file is refused (the model's rule, stated in the text) or restores an empty generation (the
text as written). Suggested for (b): refuse, with a named error, since nothing would be restored.
## Q-301 — The files a resumed restore fetches
Recorded by roadmap 8.3b.
**Scenario.** A plan returns the restore record and the files to fetch; only the record is durable. A restore killed
after its record resumes and must fetch "the files its restore record lists that steps 1 and 3 bring back and are not
yet local", so that list must follow from the record alone.
**Provisional.** The executor refuses a plan whose files to fetch are not exactly the files its record lists
(`PlanError::Invalid`), and fetches, fresh or resumed, every listed file that is not installed and has no lost
range recorded in the new manifest, ascending by first position. This is exact for a log-only restore, in which every
listed file is a sealed log segment.
**Needed.** With tree checkpoints, chunk indexes and tiers, a rule from the record (its tiers, the checkpoint) to the
files steps 1 and 3 bring back, used by both the plan and a resume.

## Q-302 — The lost range of a listed file the bucket has lost, and files other than log segments
Recorded by roadmap 8.3b.
**Scenario.** §8 Restore, Failures and end: what a lost listed file held is compacted again from the other listed
files, and the positions no other listed file holds become a lost range. A lost-range record also carries a byte
offset and length, a boot id, an id floor and the commit times around the loss, which a restore record does not
give.
**Provisional.** The executor installs only log segments holding positions (a plan listing anything else is
refused: compacted files have no local layout yet, `format.md` A.12 `data/`). A lost segment's lost range is its whole
position range: identity and first its first position, last its last, its `file_id`, offset 0 and length 0, a zero
boot id, id floor 0, no commit times, cause 3 lost in the bucket. It is appended to the new manifest, in a drill too.
**Needed.** The fields for a loss found by a restore, and the split into several ranges once compacted files can hold
part of a lost segment's positions.

## Q-303 — The local restore record file
Recorded by roadmap 8.3b.
**Scenario.** §8 Generations syncs the restore record "in the manifest and in the local restore record file
(`format.md` A.1 kind 12)", and §7 Readers reads restored ends from that file or from deep. Kind 12 is reserved with
no layout, and the committed interface (`RestoreDriver::prepare`) names `MANIFEST` only.
**Provisional.** The executor syncs the record in `MANIFEST` only; a reader process can read it there through
`kvik-files`.
**Needed.** A layout for kind 12 and who writes it, or the spec text without it.

## Q-304 — A manifest record object the bucket has lost or a listing does not show
Recorded by roadmap 8.3b. To be settled by roadmap 8.3.
**Scenario.** A restore lists the records of the generation it reads and fetches each. A record object can be
missing from that listing for two reasons: the bucket has lost it, or the listing lags behind the write, which is
ordinary (`api.md` Backup and restore, Listings: absence from a listing proves nothing). Epochs may jump after a
start (`format.md` A.14), so the gap is not visible. The plan is then made from fewer records: a file those records
listed is missing from the restore, or the restored end is earlier, with nothing reported. A record that is listed
but damaged fails the restore by name (`RestoreError::Envelope` or `Record`), as does a missing or damaged `STORE`.
**Decided at the review of roadmap 8.3b, for a generation that lists no record at all.** A generation's first
record is epoch 1 (above generation 0, its restore record), so the restore reads `record(g, 1)` by `stat`, generation 0
included; if it is present the listing is incomplete and the restore fails with
`RestoreError::ListingIncomplete { generation }`, which `Store::restore` reports as `BackupError::Sink(Transient)`, to
be retried, rather than falling back to an older generation and dropping everything in g. If it is absent, the
generation counts as having no records (a claim whose restore stopped before its record). `backup/src/restore/driver.rs` `source_records`,
`backup/tests/restore_guards.rs`.
**Model.** Not modelled.
**Needed.** A way to see a record missing from a listing that shows others: for example each uploaded record naming
the previous record's epoch (`format.md` A.17), so that a restore checks the chain and the tail, and what a broken
chain does (refuse, or restore to the record before the gap with a reported loss). This is its own item, settled by
roadmap 8.3.

## Q-310 — The delivery budget's p50 at exactly 100 calls a second with the default bound
Found by the roadmap 4.8 run on the Linux NVMe target (`docs/measurements/4.8-commit-latency.md`, Linux NVMe target).
**Scenario.** 100 series at 1 Hz, staggered evenly, so a call of one reading every 10 ms; `WriterConfig::default()`
(`min_sync_interval` 10 ms, mark timed). `design.md` Budgets sets p50 ≤ 5 ms "while syncs are not back to back,
roughly up to 100 synced batches a second". Three runs: p50 6.95, 6.75 and 7.02 ms; p90 11.54, 11.40 and 11.50; p99
12.33, 12.33 and 12.24 (inside 20 ms); 99 syncs a second, 1.0 call per batch. The same load with the bound at 0:
p50 2.36, 0.68 and 2.24 ms. Quiet and 10 × 1 Hz loads pass at the default (p50 2.30–2.52 ms).
**Cause.** The bound counted from the previous sync's start (spec §3 Group commit, `LogWriter::sync_ready_at`).
With calls exactly one interval apart, each late timed wake of the writer (about 0.1 ms here) delays every later
sync, so the wait climbs to the whole interval before two calls share a batch, and latency spreads evenly from the
sync (2.2 ms here) to the sync plus 10 ms: p50 is the sync plus about 5 ms. Only a wake with no lateness avoids it.
`design.md`'s statement that the default adds its wait "only when syncs are back to back" does not hold at the rate
equal to one call per interval. The cause rests on the code and the fit of that spread (predicted p50 / p90 / p99
7.2 / 11.2 / 12.2 ms), not on a trace. The miss is not the bound's average wait on random arrivals: the harness
spaces calls evenly, and random arrivals would give about 73 syncs a second with 1.4 calls per batch (measured 99
and 1.0), p99 about 11 ms at 10 × 1 Hz (measured 2.71–2.79) and p50 about 7 ms on macOS (measured 4.14). So it is
not inherent in the interval and this drive's sync time; option (b) below would remove it. macOS shows a tail of
the same size (p90 12.0 ms) without a budget.
**Options.** (a) Word the p50 budget as applying below 100 synced batches a second at the default, putting this
load with the higher rates. (b) Stop the lateness compounding: after a wait on the bound, count the next interval
from when the sync was due, not when it started; two syncs could then start up to the lateness less than the
interval apart, so spec §3 Group commit's "at least `min_sync_interval` apart" would need rewording, with tests. (c)
A lower default interval moves the edge to a higher rate, at the cost of the host-write cap roadmap 2.2 measured.
**Settled** by an engineering fix, pending the next run on the Linux NVMe target. The writer thread confirms the
cause: it sleeps until `sync_ready_at` with a timed wait and stamps the sync's start when it wakes, so any lateness
moved the next deadline. Two more ways to start late give the same lasting delay at one call per interval, with no
spare slot to recover in: a turn entered late with the bound already passed (a call arriving during the timed mark
sync), and the sync that takes a segment into use at a timed seal, which took a slot of the bound at whatever phase
it fell. A rule correcting only syncs that waited on the bound misses the first; capping the correction at half an
interval leaves a stall over half an interval, and a seal late in the interval, as a lasting delay.
**Rule.** Two clocks. A batch sync is due when the previous one's due time let it start, but at most one interval
before it starts; the next may start once `min_sync_interval` has passed since that due time and half of it since
the previous log sync started, a timed seal's included. The seal's sync spaces the next batch but takes no slot.
So late wakes and stalls are caught up at half-interval spacing and delay later syncs only while the load stays at
the bound (below), under saturation batch syncs keep to a grid one interval apart, any n + 1 consecutive batch syncs
span at least n − 1 intervals, no batch sync starts less than half the interval after any log sync, and a call
during a sync still waits for the next batch. Costs: after a
quiet period a burst's first batch follows the lone first reading after half the interval rather than a whole one;
a seal adds one sync per seal interval beyond the bound and delays at most one call, by under half an interval;
spec §3 Sealing's "counts as the previous log sync for the next batch" now holds for the spacing only.
**At the bound (owner's ruling: accepted, no rule change).** The catch-up is the one interval by which a due time
may trail its start; a late start stays stored in the due time. One extra batch (a stray call between two periodic
ones) spends that interval, and at exactly one call per interval no slot is ever missed to regain it, so every later
call waits for the stored lateness, under one interval. Scenario: one call every 10 ms, call 10 starting 3 ms late,
one extra call 6 ms after call 30: calls 11 to 29 wait 20 µs, every call after the extra one waits about 3.1 ms.
Any rule that bounds the long-run rate at 1/interval has no slack when the load runs at exactly that rate, so an
extra call can delay later calls until the load drops below the bound; at a 10.1 ms period the same history drains
within 20 calls. Pinned by
`at_the_bound_a_stray_call_after_a_late_start_delays_later_calls_until_the_load_drops`.
**Rejected.** (a) would have kept a known miss at the named load; (c) only moves the edge and spends host writes.
Anchoring to the oldest call's arrival avoids the burst cost but needs a monotonic stamp on every call and a change
to the call source, for one batch after a quiet period.
**Tests.** `log/tests/cadence.rs`: late wakes, stalls of 3, 8 and 30 ms, a seal early and late in the interval
after a late start, saturation with late wakes and stalls, a burst after a quiet period, a stray call at and just
below the bound. macOS before and after:
`docs/measurements/4.8-commit-latency.md`, the dated note under Results: macOS.
## Q-341 — The attach probe at `<sid>/probe`: its wrong-hash put there, and an attach after a full restore
Recorded when roadmap 8.3b was stacked on 8.1c. To be settled by roadmap 8.1d.
**Scenario.** (a) `format.md` A.17 Probe has `attach_backup` put the checksum probe, with a wrong SHA-256, to a fresh
`<sid>/probe/<nonce>`; the attach probe (`backup/src/upload/probe.rs`, Q-283) puts it to `<sid>/probe`, the
conditional-write probe's key, and never builds `Keys::checksum_probe`. That is the folding Q-213 warned of: a bucket
that does not check stores the wrong-hash object at `<sid>/probe` and is refused; at the next attach the key is
present, the wrong-hash `put` fails with `Exists`, which cannot be told from a checked hash, and only the later
checks of the reported hash stand between the refused bucket and its acceptance. The restore's probe
(`backup/src/restore/probe.rs`) follows A.17. (b) A full restore's probe leaves at `<sid>/probe` an envelope of 16
random bytes, for ever. The restored store then attaches to the bucket it was restored from: the attach probe puts
its own fixed probe to that key with the right hash, the sink refuses it with `Exists`, and the attach fails with
`Refused::Sink(Exists)`. This is the normal path of every restored store. The other order passes: a restore's probe
accepts the object an earlier attach left.
**Model.** Not modelled.
**Needed.** One probe for both callers: the attach probe moved to A.17's form (the shared code in
`restore/probe.rs`: an existing `<sid>/probe` must fail a second put and be reported unchanged, then the checksum
probe under a fresh `<sid>/probe/<nonce>`), with tests of a second attach to a bucket refused once for
`NoChecksums` and of an attach after a full restore; or A.17 amended to the attach probe's form with the reason.
Neither path is reachable before 8.1d: `Store::attach_backup` and `Store::restore` return `NotSupportedYet`.

## Q-342 — `backed_up_through` in a restored generation
Recorded when roadmap 8.3b was stacked on 8.1c. To be settled by roadmap 8.1d.
**Scenario.** A full restore installs segments 1 to 3 of generation 0 and writes its restore record, whose fold
gives each file a deep copy and the verified mark (8.3a), so the upload queue skips them. The upload cursor counts a
file as copied only from a fresh copy-verified record (`backup/src/upload/driver.rs` `fresh_copy`, `copied_at`;
Q-280), and none is written for a file the queue skips. After the restore record and two later records of
generation 1 are uploaded, the driver reports `backed_up_through: None` with nothing queued, and the figure stays
there until a rebuild carries the restore record and the files are verified again. It under-reports, so nothing is
evicted early.
**Model.** Not modelled.
**Needed.** Suggested: the cursor takes the files a fresh (uncarried) restore record lists as copied at its epoch,
with a test of the figure after a restore; with `BackupStats` (8.1d), which reports it.
## Q-320 — Which bytes beyond a same-boot mark make a reset point

**Scenario.** Three inputs the kill loop found, rare enough that a default run fails only now and then, as each run
takes a fresh seed (`log/tests/recovery.rs`, one test each). (a) The writer takes the segment created ahead into use
and stops before its sync; a remount within the boot keeps part of the first batch but not the header rewrite, so the
segment is not in use (`base_sequence` 0, no complete batch) and holds non-zero bytes after its header
(`kill_loop_a_torn_batch_in_a_segment_never_taken_into_use_is_a_reset_point`). The start deleted it with no recovery
entry and no report. (b) Seal blocks that hold lie after the last batch of the segment the mark still names, the next
segment in use by its header alone (`kill_loop_seal_blocks_beyond_a_same_boot_mark_are_not_a_reset`). (c) A rule 1
start writes its entry in a new segment and stops before zeroing the batch beyond the mark; the next start finds the
mark stale, adopts the entry and drops that batch
(`kill_loop_an_adopted_entry_in_a_later_segment_drops_the_batch_before_it`).
**Settled by the owner's ruling Q-125, which covers a store with no readable mark too:** any non-zero byte beyond the
mark is reported before it is deleted. (a) The bytes after the header of every unused segment beyond the mark's
segment, wherever segments in use lie, or of every unused segment when no mark is readable, are part of a recovery
entry's tail: under rule 1 a reset point with a `possible_loss`, under rule 3 counted in the new entry's tail. With
no readable mark such a store is never a first start; rule 3 writes the entry. The segment is deleted only after an
entry that reports it is synced: the one written, or one adopted. With the mark lost, or its current slot damaged so
that an older slot is read, the writer may have written the segment after an entry the mark read does not cover, so
an entry followed by such a segment in a later file is never adopted, on the rule 1 or the rule 3 path, and a new
entry reports the bytes. A start cut short and then killed or remounted leaves the bytes, or a durable entry naming
them, for the next start
(`a_torn_batch_in_a_segment_never_taken_into_use_is_reported_before_it_is_deleted`,
`a_torn_batch_in_a_segment_never_taken_into_use_is_in_a_reboots_entry`,
`a_torn_batch_in_a_segment_never_taken_into_use_is_reported_with_the_mark_lost`,
`an_entry_with_the_mark_lost_is_not_adopted_over_bytes_in_an_unused_segment_after_it`,
`an_unused_segments_readable_commit_blocks_count_in_the_entry`,
`an_entry_is_not_adopted_over_bytes_in_a_later_unused_segment_with_the_marks_current_slot_damaged`). The same bytes
can be reported twice, each time by an entry that is its own reset point and `possible_loss`: when a start's entry
lies in a file below the segment and the start stops before its mark, the next start cannot tell that entry from one
the writer wrote before the segment, and writes another; and when a start removes the segment and writes its mark,
and a remount undoes the removal before the mark's directory sync, the next start finds the bytes beyond the new mark
and resets again. A second report costs a second `resync` and no position; nothing adds up the bytes of several
reports. A damaged header counts the same way when bytes
follow it; with zeros after it nothing was written and the file is only deleted
(`a_damaged_header_beyond_the_mark_is_a_reset_point_only_with_bytes_after_it`). (b) Valid seal blocks are not lost
data and are sealed again, as step 2 says. (c) The one dropped batch is the adopted entry's reported tail: the kill
loop checks that at most one batch is dropped, that the entry's tail is at least that batch's bytes, and that it
counts a readable commit block. A damaged header and damaged batches in the mark's segment are rule 2 corruption: the
positions up to the mark's are a recorded `data_lost`, with no reset point, before the file is deleted
(`a_damaged_header_and_batch_in_the_marks_segment_are_a_recorded_loss_at_every_cut`); below the mark's segment, see
Q-321. The kill loop also
remounts after a start cut short, with the mark synced at every commit. `spec.md` §3 Recovery rules 1 and 3,
Adoption and the first start, and `format.md` A.4, say so. The reference model has no headers or seal blocks; its
torn tail is a reset under rule 1 already, and its first start now also requires an empty invalid tail, a state its
interface cannot reach (a definition is committed before any reading), so no test pins it.

## Q-321 — The lowest unsealed segment damaged, zeroed or missing, with no sealed segment below it

**Scenario.** Segment 1 holds acknowledged batches and awaits its seal; no segment is sealed yet, and the mark names a
later segment. Media damage hits segment 1's header and every batch, or zeroes the file, or the file is gone. A start
found segment 1 not in use (or not at all) and began the chain at the next segment's own base, so no gap was seen:
the positions vanished with no `data_lost`, no manifest record and no recovery entry, on the same boot or after a
reboot.
**Settled after the review of Q-320:** with no sealed segment, the chain of generation 0 starts at position 1, so the
positions below the first segment kept are a gap, reported as `data_lost` and recorded as a lost range (`spec.md` §3
Recovery rule 2; `format.md` A.4). This covers a damaged, a zeroed and a missing file alike, and a start cut at any
operation, since the gap is read from the segments that remain
(`the_lowest_unsealed_segment_damaged_zeroed_or_missing_is_a_recorded_loss`). When that segment holds the mark, the
mark's position already gave the range (`the_marks_only_segment_damaged_zeroed_or_missing_is_a_recorded_loss`). A
restored generation, whose first position is not 1, is left to roadmap 8.3. The reference model injects no damage to
headers or whole segments.
**Open part (settle by 8.1).** Position 1 as the start holds only while sealed segments stay in the log directory. If
every sealed segment left it, the start would report a false `data_lost` from position 1 up to the first segment
kept. Nothing evicts sealed segments today, so this is not live; eviction must give the log start another anchor
(for example the last evicted segment's last position, recorded before it leaves) before the last sealed segment
leaves the log directory.

## Q-322 — A torn mark update that makes a damaged older slot valid again

**Scenario.** Found by the kill loop's sweep (`kill_loop_keeps_every_acknowledged_entry`, "a complete batch above
position 3"; about once in 9,000 runs of 5,000 cases). Slot 1 of the mark holds counter 4 of boot 2; both slots lose
their first 8 bytes (the magic). After a reboot the start finds no valid slot and writes counter 1 into slot 0. The
writer's next update goes to slot 1, and a remount within the boot keeps only its first 8 bytes, which equal the old
magic: slot 1 is valid again, with counter 4, and wins. The next start reads a mark of the previous boot, takes
rule 3, and keeps the unacknowledged batch beyond the real mark with no reset owed for it.
**Now.** Not changed (it needs a change to how a start numbers the slots when none is valid). No acknowledged entry
is lost in the case found. Not yet checked: whether a slot of the same boot brought back this way can make rule 1
truncate after an older position.
**Needed.** Proposed: with no valid slot, a start takes its counter above every counter field it can read in either
slot, valid or not, or writes both slots.

## Q-760 — A stored `changed_at` of kind `time` above `i64::MAX`

**Scenario.** Since Q-195 (a) ingest refuses a `changed_at` above `i64::MAX` on a series of `changed_at kind` `time`.
A log written before that rule, or by other software, holds a fact row of such a series with `changed_at` = 2^63
(`format.md` A.5 column 8). Recovery and the readers accept it as stored; the export then fails on that series with
`ChangedAtOutOfRange`.
**Now.** A reader accepts any `u64` in column 8 (`format.md` A.5); only ingest checks the range. The facts block
alone does not know the series' versioning, so a check would sit with the registry, as the Q-182 check of a
definition's state names does in its own entry.
**Needed.** Whether such a row is invalid content in the log (recovery treats the batch as damaged), is kept and
reported, or stays accepted as now. Recommendation: accept it as now, because no store has recorded one under the
rule and refusing it at recovery would turn a valid older log into a loss.

## Q-761 — Long and renamed series names in the partitioned layout

**Scenario.** (a) A series name may be up to 255 bytes (`format.md` A.6 definition field 2), so its three segments
(Q-670) hold at most 253 bytes together and one at most 251. In the partitioned layout (Q-196 as amended) each is
escaped to up to three times its length, so a folder such as `device=<segment>` can reach 760 bytes and exceed the
255-byte limit most local file systems put on one file name. The longest path is 960 bytes (a 64-byte site, 759
bytes of escaped segments and the longest partial `as_of` file name, 95 bytes), so a bucket key (1,024 bytes) holds
it only while the caller's own prefix is at most 64 bytes. (b) A series renamed in
March is filed under its new name from March on, and so is January when it is exported again, so one series' months
sit in two folders.
**Now.** The layout is as settled: the folders are the escaped segments of the display name at export time; `kvik.series_id` in every
file ties a renamed series' folders together; a path the caller cannot create fails in `open` (`ExportError::Open`).
**Needed.** (a) Whether a long name keeps its full segment (as now), or a shorter form takes its place, such as a
truncated segment with a hash, which is not reversible. (b) Whether the path should carry the series id as well
or instead of the name, which a rename does not change but a reader cannot filter by name.
Recommendation: (a) as now, as a bucket is the target; (b) as now, with the id in the metadata.

## Open at v0.2

The specification was tagged v0.2 with these questions still open, by the owner's decision. Each is to be settled
by the roadmap item named here, before that item is done. Until then the model's choice, where a test pins it, is the
one named in the question. Settling Q-33 or Q-88 may add fields to `docs/format.md` or calls to `docs/api.md`
after gate 3.3; such a change is made by the item that settles the question, in its own pull request, and noted in
the question. Q-34 (c), Q-47, Q-48 and Q-79, and parts of Q-33 and Q-52, were settled before gate 3.3, with their
changes to both documents (see Settled). Q-97 to Q-99, and parts of Q-52, Q-86 and Q-93, were settled at gate 3.3;
Q-87 was reopened there. Q-87 and Q-100 to Q-107 were settled after gate 3.3 by the owner's rulings; Q-108 to Q-111 were recorded then. Q-117 was settled by roadmap 4.5 as an extension of the Q-87 ruling. Q-118 and Q-119 were recorded by roadmap 4.6; Q-119, Q-46 and parts (e) and (l) of Q-118 were settled after it by
the owner's rulings. Q-121 and Q-122 were recorded by roadmap 4.7, which settled Q-122. Gate 4.9 settled Q-109 and parts
of Q-115, Q-116
and Q-118, recorded Q-115 (n), and added the parts of Q-115, Q-116 and Q-118 still open to this table. The first run of
the `fault-trials` workflow settled Q-86 (b) on Linux, and its third run Q-52 (b) on Linux (ext4 and XFS). Roadmap 5.1 settled Q-121 and recorded Q-123; roadmap 5.1a recorded Q-124 and Q-125, both settled after its review. Roadmap 5.2 recorded Q-130 to Q-132, and its review settled them with Q-91, Q-95 and Q-116 (b) and (m). Roadmap 5.4 recorded Q-140 to Q-142. Roadmap 6.1 recorded Q-150, settled at its review. Roadmap 8.2 recorded Q-160 to Q-164; the interfaces commit of roadmap 8.1 and 8.3 recorded Q-198. Roadmap 6.4a recorded Q-190 to Q-197, the provisional Parquet export schema (`docs/export.md`). Roadmap 6.3a recorded Q-171 to Q-175, settled Q-171 and Q-174 at its review, withdrew Q-172 and moved the parts of 6.3 it does not build to 6.3b. Roadmap 7.1a recorded Q-180 to Q-182, settled at its review. Roadmap 8.1a settled Q-160 and Q-162, settled Q-198 (e) and (f), and recorded Q-210 to Q-214. Roadmap 8.1b recorded Q-270 and Q-271. The Linux NVMe run of roadmap 4.8 recorded Q-310. Roadmap 8.1c recorded Q-280 to Q-285. Roadmap 8.3a recorded Q-290 to Q-295, settled Q-108, Q-290 and Q-293, and Q-295 by the owner's ruling at its review, and left the claim points of Q-39 (f), Q-161 and Q-163 to the restore executor. Roadmap 8.3b recorded Q-300 to Q-305 and settled Q-39 (f), Q-82, Q-213, Q-214 and Q-305. Stacking 8.3b on 8.3a settled Q-300, settled Q-271 as superseded, and recorded and settled Q-340, both by the owner's rulings, and recorded Q-341 and Q-342. The owner's rulings of 2026-11-16 settled Q-190 to Q-197, leaving Q-191 (b) to 8.3, Q-192 (b) to 6.4b, Q-196 (c) to 8.3 and the calendar part of Q-197 to 7.3; applying them recorded Q-760 and Q-761.

| Question | Settle by roadmap item | Note |
|---|---|---|
| Q-16 | 5.4 | Live state and staleness are built there; built by roadmap 5.4 as the model backend has them (see the question), not settled: needs the owner's ruling on the proposal and its objections |
| Q-33 | 6.2 | Moved from 4.5, which has no derived file to check, to 5.1, which did the registry file's check and requirement 5, then to 6.2 at the review of 5.1 (indexes at 6.2, trees at 7.1): the rest of the Derived check (requirements 1–4) and invariants 5 and 8 across a lost range. Recovery entries, reset points, rows 7b and 7c and invariants 25–27 are modelled since roadmap 5.1a |
| Q-44 (b) and (c) | 5.1 | Series registry and timing; not settled by roadmap 5.1, which needs the owner's ruling on the proposal and its objections (see the question) |
| Q-52 (b) | 4.9 | Settled on Linux (ext4 and XFS) by the rewrite job of the `fault-trials` workflow (`docs/measurements/4.9-fault-trials.md`); open only for the owner's decision on the macOS and Windows wording |
| Q-83 | 7.2 | Late and corrected data; recorded after the tag, from the Q-60 ruling |
| Q-84 | 8.3 | Restore, with 5.3 (resync) |
| Q-85 | 8.1 | Tiers and corrupt blocks |
| Q-86 (b) | 4.9 | Settled on Linux by the Btrfs job of the `fault-trials` workflow (`docs/measurements/4.9-fault-trials.md`); open only for the owner's decisions on the `chattr +C` sentence and the macOS and Windows wording |
| Q-88 | 5.6 | Routing table; recorded after the tag, from the Q-78 ruling; implemented by roadmap 5.2 and accepted there as provisional; moved to the gate so that the grammar is ruled once both backends have used it |
| Q-90 | 8.3 | Restore |
| Q-92 | 6.2 | Revised-key index; recorded after the tag, from the review of the Q-72 ruling |
| Q-93 (b) and (c) | 7.3 | Calendar; (a) settled at gate 3.3 by the owner |
| Q-94 | 6.3b | Compaction order; recorded after the tag, from the review of the Q-71 ruling; with 6.3b, which publishes compaction outputs (6.3a lays out the records only) |
| Q-96 | 5.3 | Catch-up, resync and replay; recorded after the tag |
| Q-110 | 4.9 | Gate; recorded at the review of roadmap 4.2; owner decision pending on the trust-boundary wording |
| Q-111 | 4.9 | Gate; recorded at the review of roadmap 4.3; owner decision pending on the call-limit wording in `api.md` |
| Q-115 (e), (j), (n) | 4.9 | Gate; owner decisions pending: the `space()` seam, the Windows wording, and bounds on the ingest queue's bytes and on blocked seals |
| Q-115 (f), (k) | 5.1 | Series registry (the segment index's reach) and the interface (waits and statistics); deferred by gate 4.9; both implemented by roadmap 5.1 (see the question) |
| Q-116 (a) | 6.3b | Deep fetch or manifest rebuild for a corrupt record; deferred by gate 4.9; roadmap 6.3a builds both in `kvik-files` (see the question), the start's call into them is 6.3b |
| Q-116 (c) | 5.1 | The interface reads loss history; deferred by gate 4.9; implemented by roadmap 5.1 (see the question) |
| Q-116 (k), (n) | 4.9 | Gate; (k) owner decision pending on the availability wording; (n) the Linux NVMe run measured a warm cache (0.39–0.74 s); a cold start is still to measure |
| Q-118 (c) | 8.3 | Restore: a reader that sees a new generation; deferred by gate 4.9 |
| Q-118 (f), (k) | 6.3b | Compaction and retirement: which epoch a refresh takes, and what removal honours; deferred by gate 4.9; with 6.3b, which retires files (6.3a gives `Catalogue::at_epoch` and the settled and provisional holds) |
| Q-123 (a), (c)–(g) | 5.6 | Gate; recorded by roadmap 5.1; (b) settled at its review |
| Q-140 | 5.6 | Gate, with the ruling on Q-16; recorded by roadmap 5.4 |
| Q-141 | 5.6 | Gate; recorded by roadmap 5.4 |
| Q-142 | 5.6 | Gate; recorded by roadmap 5.4 |
| Q-161 | 8.3 | Restore's own claim marker; recorded by roadmap 8.2; the marker's layout, with the nonce, is in `format.md` A.17 (roadmap 8.1a) |
| Q-163 | 8.3 | Reading the claims on a lagging bucket; recorded by roadmap 8.2 |
| Q-164 | 8.1, 8.3 | Interface points deferred to 8.2: (a) and (b) with 8.1, (c) with 8.3; recorded by roadmap 8.2 |
| Q-170 | 5.5a | Durability levels per source; decided by the owner, design and spec text at 5.5a |
| Q-173 | 6.3b | The operator call `rebuild_manifest`; recorded by roadmap 6.3a; its signature and finality come with the call in `api.md`, which 6.3b adds |
| Q-175 | 6.3b | An intermediate output no record and no local file describes; recorded at the review of roadmap 6.3a; needs two corrupt merges, so it arises only once 6.3b publishes compaction outputs |
| Q-191 (b) | 8.3 | Parquet form of orphaned and set-aside facts; settled by the owner, 2026-11-16 (the export schema, one file per call); built with the export call at 8.3 |
| Q-192 (b) | 6.4b | The scale in force on `SeriesInfo` or the export call; settled by the owner, 2026-11-16; comes with decimal series on the durable backend, `ScaleAboveExport` until then |
| Q-196 (c) | 8.3 | Store-level metadata beyond the store id (tzdata version, durable position, reset stamp) with the export call in the store's interface; the saved point of a continued export after 5.3; settled by the owner, 2026-11-16; the store id is in every file already, from `Reader::store_id` |
| Q-197 (calendar lengths) | 7.3 | Interval ends of calendar-length series from the interval zone, tzdata version in the metadata; settled by the owner, 2026-11-16; built with calendar-length interval series |
| Q-198 | 8.1b, 8.3 | Backup and restore interfaces; recorded at the interfaces commit; (a) to (d) decided and built by 8.1b to 8.3d; (e) and (f) settled by `format.md` A.17 at 8.1a; the remaining mismatches, Q-213 and Q-214, settled by 8.3b |
| Q-210 | 8.1c | A sink's report of a missing checksum; recorded by roadmap 8.1a |
| Q-211 | 6.3d | An uploaded epoch written again after a damaged tail; recorded by roadmap 8.1a |
| Q-212 | 8.1c | A key taken by other content; recorded by roadmap 8.1a |
| Q-270 | 8.1 | Envelope damage mapped onto the existing error variants; recorded by roadmap 8.1b; a `Damaged` variant is the lead's call |
| Q-280 | 8.1 | Rebuilt manifest and the upload queue; decided by roadmap 8.1c, accepted by the owner |
| Q-281 | 8.1 | Copy-verified record layout; decided by roadmap 8.1c, field 1 a whole file description by the owner's ruling |
| Q-282 | 8.1 | Upload queue and record cursor; decided by roadmap 8.1c, accepted by the owner |
| Q-283 | 8.1 | Attach probe; decided by roadmap 8.1c under the owner's Q-162 ruling, accepted by the owner |
| Q-284 | 8.1 | Upload failures; decided by roadmap 8.1c, (c) settled by the owner's Q-82 rule |
| Q-285 | 6.2 | Local layout of kinds other than log segments; the upload driver fails on them until then |
| Q-291 | 8.3d | Recovery-entry loss reports at a restore; recorded by roadmap 8.3a |
| Q-292 | 8.3 | Restore record size; recorded by roadmap 8.3a |
| Q-294 | 8.3b | Model restore against the plan; recorded by roadmap 8.3a |
| Q-301 | 8.3 | The files a resumed restore fetches; recorded by roadmap 8.3b; exact for a log-only restore |
| Q-302 | 8.3 | A restore's lost range and files other than log segments; recorded by roadmap 8.3b |
| Q-303 | 8.3 | The local restore record file (kind 12); recorded by roadmap 8.3b |
| Q-304 | 8.3 | A manifest record lost or not listed; recorded by roadmap 8.3b; an empty listing decided at its review; the chain of epochs and tail check is its own item, with `format.md` A.17 (8.1b) |
| Q-310 | 4.9 | Gate; recorded by the Linux NVMe run of roadmap 4.8; settled by the engineering fix to the sync schedule, to be confirmed by the next NVMe run before gate 4.9 |
| Q-341 | 8.1d | The attach probe at `<sid>/probe`, also after a full restore; recorded when 8.3b was stacked on 8.1c |
| Q-342 | 8.1d | `backed_up_through` in a restored generation; recorded when 8.3b was stacked on 8.1c |
| Q-321 | 8.1 | Tiers and eviction: the log start needs another anchor than position 1 before the last sealed segment leaves the log directory, or a start reports a false `data_lost` from 1; settled otherwise after the review of Q-320 |
| Q-322 | 5.6 | Gate; recorded from the kill loop's sweep at the review of Q-320; the kill loop fails on it rarely until settled |
| Q-760 | 5.6 | Gate; recorded when the owner's ruling on Q-195 (a) was applied |
| Q-761 | 6.4b | Partitioned export layout; recorded when the owner's ruling on Q-196 was applied |

Not a question: the figures behind §3 Sync primitive. The 2.2 spike measured sync costs on macOS only; roadmap 4.8
measured the engine's commit path on macOS and added a harness and a workflow that run it on Linux and Windows cloud
runners. The cloud-runner figures are recorded there and are not durability-representative.
The figures on the Linux NVMe target are the owner's run, recorded there too
(`docs/measurements/4.8-commit-latency.md`); the budget point they raise is Q-310.

## For the format and interface documents
The specification wins where the documents differ. These follow from it, for the checks of `docs/format.md` and
`docs/api.md`. Gate 3.3 resolved or deferred each of them; `docs/format-check.md` and `docs/api-check.md` say where.
- **F-1.** `format.md` A.5: the fact row has one `quality` column, and a `no_value` row is forced to quality bad.
  Duplicate identity (§2.4) compares the adapter-supplied quality, which §2.2 keeps with the fact. Without it, the
  duplicate check and the revised-key index cannot be rebuilt from the log (invariants 8, 15).
- **F-2.** `api.md`: back-pressure is listed as a refusal; §3 says ingest waits. `try_emit` returning `Busy` fits.
- **F-3.** `api.md`: windowed results need a place for the rate flag (§6).
- **F-4.** `format.md` A.5 and the engine-assigned quality: the engine only lowers a quality (§2.2). The quality
  column must hold the adapter-supplied quality, or §2.4 identity is lost for rows without a value and `future` rows;
  the engine quality is then derived from it: bad for a row without a value, else the worse of the supplied quality
  and suspect for a `future` fact. The forced "a `no_value` row has quality bad" applies to the engine quality only.
- **F-5.** `format.md` recovery (§3): scan an unsealed segment to its end, not only to the first invalid block, which
  needs a way to find the next batch after a corrupt region (a batch sync marker, or scanning at 8-byte steps); a
  sealed segment with a lost range counts its surviving entries, and "consecutive" positions allow a recorded lost
  range; the manifest needs a "lost range" record (bytes and positions). A missing, unreadable or half-updated mark
  is a different boot, and recovery after a reboot writes the mark; A.10's "always truncates" on a platform without
  a boot identifier yields to that rule when the mark is missing. Part B, where the lost-range record would go, is
  not yet normative. A.11 needs golden files with corruption inside a segment. A.2 ("the first failure ends the
  readable part") and A.10 (sealed segments read whole) must allow kept batches after a lost range, and A.4's "a
  batch's first sequence follows the previous batch's last" needs a lost-range exception. A lease's boot id has no
  value on a platform without a boot identifier. The lease file needs a format and a
  file kind in A.1: epoch, boot id and refresh counter (§7).
- **F-6.** `api.md`: a fault during `emit` leaves the outcome unknown (§3 Write or sync failure); health reports the
  size of system clock jumps (§6) and `data_lost` per lost range (§3); readers need an explicit lease refresh, with
  its counter, and `Reader::open` a source of boot identity (§7); `WindowResult.value` must allow empty (rate,
  `min`, `max`, §6).
- **F-7.** `format.md`: a segment's first sequence is at least 1, and a last sequence of 0 means empty (§2.2). Part B
  needs a "restore" manifest record (generation, checkpoint, restored end, the restored ends carried from the record
  it read, each with its generation, the lost ranges, acknowledgements and recovery-entry loss reports of the
  generations it restores from (since Q-34 (c)), and every file it restores from, set-aside segments included, each with hash,
  tier and position range), uploaded before any other file of its generation, and manifest records keyed by
  generation; the deep tier holds generation claim markers; a tree checkpoint bundles the registry file, each series'
  latest recorded `t` and lateness reference (F-17), the revised-key index and the version index (§8 Continuous
  backup, Generations).
- **F-8.** `api.md`: the backup sink needs a create-if-absent write for generation claims, and a list that sees a
  completed put; restore needs a read-only drill mode and refuses a directory that holds files and no restore record;
  a consumer can register with its last confirmed position after a restore, so the application must persist its
  confirmed positions; health names a writer superseded by a higher generation, and a restore that finds a higher
  claim after uploading its record; re-ingest needs an entry point; a store's identity across drills (a restore keeps
  the store id, §8 Generations).
- **F-9.** `format.md`: a fact row in a versioned series carries `changed_at` (a u64 column with an id, present per
  series), also in compacted chunks; A's rule that times are i64 needs an exception for a `changed_at` of kind `time`;
  a series definition carries the `versioned` flag and `changed_at kind`; the revised-key index carries `changed_at`;
  A.1 needs file kinds for the version index and the revised-key index; exports carry `changed_at` (§2.1–§2.4, §7).
- **F-10.** `api.md`: `changed_at` belongs on `Reading`; `SeriesSpec` gains `versioned` and `changed_at kind`, and a
  change of either is an error like a change of unit; refusals gain `changed_at` missing, `changed_at` unexpected and
  `revise` on a versioned series; fact deliveries and history results carry `changed_at`; deliveries gain the flag
  `outranked`; `Ack` and health gain an outranked count (§2.2, §2.4, §4, §11).
- **F-11.** `format.md` A.10: the mark file is synced on a timer (`mark_sync_interval`), at once after a batch holding
  an entry that is not a fact, at clean shutdown and by recovery; recovery reads the highest position named by cursor
  files, manifest records and derived files before it writes the recovery entry. `api.md`: the `mark_sync_interval`
  configuration key and `possible_loss` among the losses with its counts; acknowledgement of a batch of facts does not
  wait for the mark sync, while a batch holding any other entry does, and the writer forces that mark sync at once
  (§3 Commit); test seams can inject a failed mark sync (§3). A.6, A.10, A.11 and Part B now carry the
  recovery entry, the mark's highest series id, and the lost-range record's identity and cause; since Q-34 (c)
  `possible_loss` never holds health degraded.
- **F-12.** `format.md`: the parameter-change entry carries the retention class and period, taking effect at its position,
  not by event time. `api.md`: `SeriesSpec` gains the retention class and period, which do not count towards "same
  specification" and may change without a new series; a call to change them; the default period as a configuration key;
  statistics for bytes moved and fetches (§2.1, §8).
- **F-13.** `format.md`: a series definition carries its timing (observed, scheduled or forecast). `api.md`: `SeriesSpec`
  gains timing, fixed for life; window bounds may be relative to `now` with either sign, and calendar windows relative
  to the current one; a fact delivery from a scheduled or forecast series is a notice, not the value in force; live
  state of a forecast series is none; a stale-for condition on a forecast series, or any condition over one without a
  windowed operator, is a compile error; timing joins the list of fields whose change is an error; a forecast series
  that is not versioned, or whose `changed_at kind` is not `time`, is refused; `Window::rolling` needs offsets from
  `now`; exports label issue time and valid time. `format.md`'s series definition entry needs the timing field (and the
  versioned flag and `changed_at kind`, F-9), outside the fields a parameter change may carry (§2.1, §4, §6).
- **F-14.** `format.md`: a series definition carries the kind `interval` with its length and phase. `api.md`: `SeriesSpec`
  gains them; a reading off its phase is refused; avg, integral, increase and time-in-state on an interval series are
  refused, also when a name pattern later matches an interval series, and so is a calendar integral; window results
  and buckets gain the flag `apportioned`; refusals gain an off-phase reason, counted per series; `live()` carries the
  interval end; exports carry d or the interval end. `format.md`: the definition entry gains interval length and phase
  (i64 nanoseconds), which a parameter change may not carry; record on change is refused for interval series; A.8's
  `min_t` and `max_t` must allow for a reading's reach of d, or of one calendar interval (F-17) (§2.1, §6).
- **F-15.** `format.md`: a log segment may follow, or be a copy that omits, a lost range (§8 Corrupt block); the
  lost-range record carries the commit times of the batches on either side (§11 Health).
- **F-16.** `format.md` A.6 has the entry types for the site zone (zone name, effective time) and the tzdata version
  (§5.7, Q-76); the calendar file needs a format recording its tzdata version and, per stored node, the zone of each
  day it covers. `api.md`: `calendar` and `calendar_in` take a day or a month, calendar windows (since local
  midnight, this month, a relative day or month, §6) need a `Window` constructor, and the site zone needs a call that
  records an entry. The engine compares zones by the tzdata link table (`backward`), not by the model's fingerprint
  of a zone's rules; a calendar file records zones by canonical name, and a release that splits a zone (2022g made
  America/Ciudad_Juarez from part of America/Ojinaga) or merges one into a link (2022b made Europe/Amsterdam a link
  to Europe/Brussels) changes which stored nodes match a named-zone query, so the rebuild after a tzdata change
  re-checks the zones of stored nodes as well as their spans.
- **F-17.** From Q-45 and Q-74; `api.md` has the minimum (interval kind and length, `interval`, `displaced`,
  `interval_displaced`, `OffPhase`, `SeriesInfo`). `format.md` A.6, when the interval fields of F-14 are given field
  ids: interval length as a `u8` tag (0 fixed, 1 `day`, 2 `month`), with `i64` d and `i64` phase for tag 0; interval
  zone as a UTF-8 string, as given at definition; day start as a local clock time in `i64` nanoseconds (05:00 is 5 h
  whatever the offset does before it; 0 ≤ day start < 24 h), tag 1 only. None of them may appear in a parameter-change
  entry. No change to fact columns or flags: a reading carries no length, and `displaced` is derived, not a fact flag.
  A.8's `min_t` and `max_t` must not depend on tzdata: a sealed segment bounds a calendar-length reading's reach by a
  fixed upper bound on its length, 50 h for `day` and 33 days for `month` (a date-line change can repeat a date:
  America/Sitka, 1867, has a local day of about 48 h). The tree checkpoint (layout not yet in `format.md`, F-7)
  carries per series the lateness reference (`i64`, absent when none) beside the latest recorded `t`. `api.md` still
  needs: a change of interval length, zone or day start is an error like a change of unit; a day start on a fixed or
  `month` length, or a zone the tzdata version in force does not know, is refused at definition; exports and `live()`
  carry the interval end.
- **F-18.** From Q-60 to Q-81. `format.md`: the tzdata entry (A.6, type 7) and the first site-zone entry are in a
  store's first batch (§5.7; since gate 3.3 the store descriptor, `format.md` A.12, holds them), and `api.md` needs how
  a store is created with them; the routing-change entry (A.6,
  type 5) needs its fields, the consumer's `emits` selection among them; the registry file (Part B) holds routing
  changes, site-zone and tzdata entries with their positions, so that every tree checkpoint carries them (§2.1, §4
  Routing, F-7); a revised-key index entry needs one bit, incomplete (§2.4; the index's file kind, F-9); a compacted
  segment's index can be fetched without its chunk data (Part B, §8 Restore step 1). The cursor file is unchanged: a
  condition delivery's sequence is not persisted (§4 Order). `api.md`: the pattern grammar (Q-88); the `Select` value
  for `emits` that selects nothing.

## Settled
| ID | Question | Settled in |
|---|---|---|
| Q-01 | Corruption before backup | §7 (inputs kept until verified), §8 Corrupt block, invariant 10 |
| Q-02 | Catch-up position on overflow | §4 Overflow |
| Q-03 | Low mark and tiering | §3 Disk space |
| Q-04 | Day nodes and edge stretches | §5.7 |
| Q-05 | Rate across a gap | §6 Aggregates |
| Q-06 | Days outside 92–100 quarter-hours | §5.7 |
| Q-07 | Forward clock jumps and staleness | §5.4 |
| Q-08 | Age of a delivery | §4 Replay |
| Q-09 | Quality and future stamping | §2.2, §2.4 |
| Q-10 | Coverage for text series | §5.2 |
| Q-11 | Counters and grace across reboots | §7, §11 |
| Q-12 | Invariant 9 and disaster loss | invariant 9 |
| Q-13 | Negative counter values | §2.5, §5.5 |
| Q-14 | State transition at a window's leading edge | §6 Edge stretches, invariant 17 |
| Q-15 | Rolling-window coverage after a forward clock jump | §6 Windows (event time against `now`), §11 Health (clock jumps) |
| Q-17 | Identity of a reading recorded without a value | §2.4 |
| Q-18 | Rate flagged in windows without a gap | §6 Aggregates (replaces the Q-05 flag condition) |
| Q-19 | A corrupt compacted file whose inputs are still local | §8 Corrupt block, row 11 |
| Q-20 | A corrupt block inside the open segment after a reboot | §3 Recovery, §8 Corrupt block, §11 Health, rows 11 and 11a, invariants 1, 9 and 10 (open part: Q-33, the Derived check) |
| Q-21 | Which clock measures lease expiry | §7 Leases (refresh counter and boot id; grace period on the writer's monotonic clock), row 26 |
| Q-22 | Negative counter values if D1 changes | §2.5 |
| Q-23 | Stored calendar nodes for days off 15-minute boundaries | §5.7 |
| Q-24 | A failed sync followed by a power loss | §3 Write or sync failure, row 12 |
| Q-25 | When recovery writes the mark; a missing or invalid mark | §3 Recovery |
| Q-26 | The first sequence of a generation, and "no position yet" | §2.2, §8 Restore step 1 |
| Q-27 | Where consumer cursors come from after a restore | §8 Continuous backup, §8 Restore step 2, §4 Resync |
| Q-28 | Two restores from one bucket; a restore from an empty bucket | §8 Generations, §8 Restore drill, row 28b |
| Q-29 | The restored end, and what a continuing cursor receives | §8 Restore steps 1–2, §8 Continuous backup, invariants 4 and 9, row 28 (Q-39 (f) settled since) |
| Q-30 | `duplicates_unchecked` during a restore compared a time with a position | §8 Restore step 2; versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 1, 2c and 2d), so that a replay never reverts a correction |
| Q-31 | Accepting ingest before the registry is back | §8 Continuous backup, §8 Restore step 1 |
| Q-32 | A crash or reboot during a restore | §8 Restore, row 28a (Q-39 (f) settled since) |
| Q-33 | Registry entries after a lost range, in part: the kept log, orphaned facts and id floors | §2.1 (only the kept log applies; a lost registry entry is issued again through the normal calls, a definition issued again being a new series; orphaned facts kept, outside views, trees, indexes, routing and delivery, counted and exported; id floors; the residual on a disk that loses synced data), §2.1 `id`, §3 Commit (a batch holding an entry that is not a fact is acknowledged only after a mark sync), §3 Durable mark (highest series id assigned), §11 Health, invariants 2 and 4, `format.md` A.6 and A.10, `api.md` `export_orphaned` and `orphaned_facts`, as the owner decided (open part: Q-33, the Derived check, with invariants 5 and 8 across a lost range) |
| Q-34 | What `data_lost` and `possible_loss` mean beyond the report, parts (a) and (b) | §8 Corrupt block (only the damaged region is lost; the valid batches of a sealed segment are copied to a new segment by one manifest record, which ends the error), §11 Health (each report carries what is known of its range; no flag on query results), row 11 (Q-34 (c) since settled; open part: Q-52, rebuilding a corrupt manifest record never uploaded) |
| Q-34 (c) | Whether loss reports survive a restart, and whether they are latched | §11 Health (one report per loss, read from the manifest and the recovery entries at every start, never cleared, each counted once; `data_lost` holds health degraded until acknowledged, while `possible_loss` is listed and counted and does not), §8 Acknowledging a loss (an acknowledgement record; the list of losses kept for ever; a range that grows needs a new acknowledgement; carried by a restore record), §3 Order of a start step 4 (a loss keeps one identity, the first position of its range when first reported, extended upward or downward, not repeated; recovery-entry loss reports carried by a restore record), invariant 27, `format.md` Part B, `api.md` `losses` and `acknowledge_losses`, as the owner decided (open part: Q-90, the set-aside report) |
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (the levels fixed by the definition since Q-60) |
| Q-37 | Which manifest records a restore reads, and how generations begin | §8 Generations (generation 0 counts as claimed; each generation above 0 begins with a self-contained restore record listing the files it restores from and carrying the earlier restored ends, uploaded before any other file of its generation, after which the claims are read again; a restore reads the highest generation below its own that has records, and keeps the store id), §8 Restore step 1 (a position set aside once stays so) and the paragraph after step 4, invariant 9, row 28b |
| Q-38 | Segments beyond the restored end | §8 Restore step 1 (positions set aside, a term distinct from Corrupt block's quarantine: excluded by position everywhere, also in reader processes, with no query error; a set-aside definition keeps its id reserved; the set-aside range reported in health from the record at every start; re-ingested only by the operator, through export and Re-ingest), §2.1 `id`, §11 Health, invariants 4 and 9, row 28, `api.md` Backup and restore (open part: Q-90, whether the report holds health degraded) |
| Q-39 | Failures during a restore, and when it ends, parts (a)–(d) | §8 Restore, Failures and end (any failure before step 1 is complete is fatal and resumed at restart, and later failures are retried with degraded health, except a listed file the bucket has lost or corrupted, part (e); the record precedes every download; `restoring` is derived and ends when every file step 3 brings back is installed, or since (e) replaced or recorded lost), §8 Restore steps 1–4, §8 Continuous backup and Deep (each tree checkpoint carries the revised-key index), §2.4, §11 Health, §13, `api.md` (part (f) settled since) |
| Q-39 (e) | What ends a restore when the bucket has lost or corrupted a listed object | §8 Restore, Failures and end (a listed file that the bucket no longer holds, or has corrupted, in step 1 or later, is treated as Corrupt block with deep as the failed tier: compacted again from the other listed files, the positions no other listed file holds becoming a lost range reported as `data_lost`, and the restore completes; facts of a lost log segment held by other listed files fetched in step 1; a lost tree checkpoint replaced by an earlier one or the whole sealed log; `restoring` ends when every file step 3 brings back is installed, replaced or recorded lost), §8 Corrupt block and No deletion (the exception for a restore), invariants 2, 9 and 10, §11 Health and the set-aside report (open parts: Q-85, a loss in the bucket found outside a restore; Q-33, the Derived check after a definition entry lost this way; Q-82, how a loss in the bucket is told from a transient fault, a damaged download or a wrong key; a content conflict between two listed files still waits for an operator) |
| Q-40 | Ranking facts of versioned series during a restore | §2.4 Version index, §8 Continuous backup and Deep, §8 Restore steps 1–2 (each tree checkpoint carries the version index, and step 1's replay brings it to the restored end before ingest opens), §13 |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (b) and (c)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (calendar lengths: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (a `resync` carrying `forget_above` = X: the consumer drops every position above X from its de-duplication memory and confirms only when that is durable), §4 Resync (reset points: a recovery entry and a lost range not described by one, each applying to a consumer whose position is below its id), §3 Recovery rule 3 (a recovery entry at F, positions above K assigned again), row 7a; no margin of skipped positions (as refined by Q-47 and Q-48; open part: Q-46, positions kept by readers) |
| Q-44 | Gaps left by scheduled and forecast series, part (a): a silent feed | §2.1 `timing` (a silent forecast source is reported by stale-for on the observed series its adapter emits on each successful fetch), row 37b (open parts: Q-44 (b) and (c)) |
| Q-44 (d) | Forecast accuracy | Not served by the engine in v1: history keeps every issue with its `changed_at`, so a reader computes "known as of issue X" from `View::History` and compares it with the outcome series; `api.md` Not in v1 (a view as of an issue time, forecast-accuracy operators) (open parts: Q-44 (b) and (c)) |
| Q-45 | Intervals of varying length | §2.1 `interval length`, `interval zone` and `day start` (a calendar length, `day` with a day start or `month`, in a zone fixed for the series, its interval starts taken under the tzdata version in force), §2.2, §5.1, §5.4 (no stored level; displaced readings), §5.7, §6 Aggregates, invariants 21–23, rows 20a, 40 and 43–46, `api.md` Series and Not in v1 (irregular billing periods) (the tzdata version in force, a day start that occurs twice, an interval zone unknown or become a link, and count, min, max and last when an edge cuts an interval were settled later by Q-76, Q-77, Q-80 and Q-81) |
| Q-47 | How a consumer learns its recovered cursor, and what forgetting means | §4 At-least-once (`resync` carries `forget_above`; the consumer confirms it only when the forgetting is durable; records kept outside the engine are the consumer's duty, never deleted or protected by the engine, as the owner decided), §4 Resync (reset points; consecutive ones delivered as one; a consumer with no cursor file; what `start: position` forgets), §4 Consumer state, §11 Consumers (the `cursor` event removed), `api.md` Consuming |
| Q-48 | The resync owed after a truncated tail is not durable | §3 Recovery rule 3 (a recovery entry at F in the log, adopted on a repeat; nothing owed as state), §3 Order of a start, §3 Durable mark (no file written naming a position above the last synced mark), §4 Resync and Overflow (reset points on resume, in a session and in catch-up), §6 Restart and resume, §8 Behind consumers, §2.2, §8 Restore step 1 (a gap followed by its recovery entry is not a gap), rows 7a and 7c, invariants 1, 16, 25 and 26, `format.md` A.1, A.4, A.6, A.7, A.10 and A.11 |
| Q-49 | What a series with no stored level serves during a restore, part (a): queries | §8 Restore step 2 and §6 Old spans (window edges move to where detail is local, the error `detail_not_local` only when no span is left, never an empty result; reads of facts fetch on demand; no priority download), row 28 |
| Q-49 (b) | Conditions and live state with no local detail during a restore | §6 Conditions (the state is unknown, not false: the evaluation is skipped and counted, `evaluations_unknown`, the condition keeps its latched state, and the first evaluation with detail is delivered as a transition if it differs from the latched state, or there is none), §2.3 Live state (unknown, not none, for a series whose latest recorded `t` no local fact or stored node holds), §4 Resync and §6 Restart and resume (`resync` reports "unknown" for that funnel, no transition), §8 Restore step 2, invariant 16, §11 Health, `api.md` Consuming, Reading and statistics (`ConditionState::Unknown`, `Live::Unknown`) (open part: Q-84, a state that stays unknown) |
| Q-50 | Window edges over moved detail of a series with no stored 1-minute level | §6 Old spans (an edge whose detail is not local moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, `detail_not_local`; no fetch on the evaluation path), invariants 7, 17 and 22, `api.md` Reading |
| Q-52 | Manifest records as single copies, in part | §7 Manifest (a record appended only after the previous record's sync; the last valid record written again in place at every start; a torn tail, dropped, told from a corrupt record by a valid successor; the last record neither uploaded nor acted on irreversibly until a later record is synced; a corrupt record fetched from deep or the mirror path, else read-only with `manifest_corrupt` and no fact lost), §8 Corrupt block (a manifest record not yet uploaded is a single copy; `mirror_path` covers the manifest), invariant 10, `format.md` Part B (open part: Q-52, rebuilding a corrupt record never uploaded, and checking the in-place rewrite on each platform); the read-only stop for a record with no copy is superseded by Q-52 (a), the automatic rebuild) |
| Q-52 (a), (c) | Rebuilding a corrupt manifest record never uploaded; a mark without a checksum | §7 Manifest (every file self-describing; with no copy in the mirror path or deep, the writer rebuilds the manifest from the local files by the procedure of `rebuild_manifest`, continues, and reports `manifest_rebuilt` until acknowledged; what a rebuild cannot recover from local files: Q-103, settled), §11 Health, `format.md` A.10 (two checksummed slots), A.14 (rebuild record; open part: Q-52 (b)), `api.md` Health |
| Q-60 | Calendar nodes when the expected interval changes | §5.1 density rule (the levels a series stores are fixed by the expected interval its definition gives; a later change moves the staleness limit and keep-alive, never the levels), §5.1 table and §5.7 (calendar nodes for a series that stores the 15-minute level), §5.6 Canonical order (1-minute nodes computed for the merge where they are not stored), row 20a (open part: Q-83, node contents after a past-effective change of the staleness limit) |
| Q-61 | Identity of a condition delivery, and where its sequence is kept | §4 Order (every condition delivery, on a fact or on the timer, is `(funnel, sequence)`; the sequence is in memory, from 1 at every engine start, and a consumer resuming at a start receives none before that start's `resync`), §4 At-least-once (never delivered twice; the identity holds until the consumer's next `resync`), `api.md` Consuming; no field in `format.md` |
| Q-62 | Condition transitions while a consumer catches up after overflow | §4 Overflow (funnels evaluated but silent; the present state sent on re-attaching where it differs; transitions in between counted, `transitions_not_delivered`; catch-up reads hot and cold only and ends with `resync` where its next entry is on no local tier), §4 Resync, §8 Behind consumers, `api.md` Health and statistics |
| Q-63 | Condition state now, for conditions with memory | §6 Restart and resume (state now: a threshold between its levels keeps the latched state, false with no latch; on change compared with the latch by the dead-band), §6 Latch (a latch belongs to one incarnation of a funnel; a funnel added at run time has none and sends no `resync`), invariant 16 |
| Q-64 | Routing changes before a restore's checkpoint | §2.1 (routing changes, site-zone and tzdata entries are kept with their positions in the registry file, which every tree checkpoint carries), §4 Routing (built at start from the registry, so a route is in force from step 1 of a restore and after its log segment has left hot), `format.md` Part B Registry (open part: Q-91, how a funnel declared in configuration is matched with the registry, and what removing or changing one does) |
| Q-70 | Chunk indexes after a restore | §8 Restore steps 1 and 3 (step 1 downloads the chunk index of every compacted file the restore record lists, before ingest is accepted), §5.6, `format.md` Part B Compacted segment; the loss of such an index in the bucket follows §8 Restore, Failures and end (Q-39 (e)) |
| Q-71 | A log segment and the chunks compacted from it hold the same positions in one epoch | §7 (sealed segments compacted in position order, each once: a segment at or below the last position of a live compacted file is not compacted again, except to replace a quarantined file), §7 Manifest, invariant 14 (live compacted files) |
| Q-72 | Whether the revised-key index is consulted when the current fact is off the machine | §2.4 (a match with the key's entry in the revised-key index is a duplicate wherever the facts are; unchecked only for a key with a single fact off the machine and for an incomplete entry), §5.4 Displaced readings, §8 Retention class and Restore step 2, §12 D5, rows 2b, 2c and 40 |
| Q-73 | Who leaves out the unchanged readings of a record-on-change series | §2.6 (the adapter leaves out unchanged readings between keep-alive facts; the engine records every reading it is given), as §2.4 identity including `t`, principle 5 and §11 already required |
| Q-74 | One reading far ahead makes later on-time readings `late` | §2.2 `late` and the lateness reference (facts flagged neither `future` nor `unsynced_clock`; a reading up to 60 s ahead still raises it, an accepted limit; the latest recorded `t`, over all facts, stays for the §2.4 fast path, §8 Restore step 2 and statistics), §2.4 Fast path, §4 Future facts, §8 Deep, Continuous backup and Restore step 1 (each tree checkpoint carries both), invariant 24, rows 4 and 4a, `api.md` `SeriesInfo` (open part: Q-44 (c), far-future facts and the latest recorded `t`) |
| Q-75 | Local days around a site-zone change to another offset | §5.7 (the local date is the date in the zone in force at each instant; the day of D starts at the first instant whose local date is D or later, so days and months tile; "today" is the latest date whose day has started; a change rebuilds from the start, under the old zone, of the local day containing the effective time), row 21, `api.md` Reading |
| Q-76 | Whether a tzdata change is a log entry | §5.7 (the version in force is named by the latest tzdata entry, the first in a store's first batch; the engine reads one release at open, and a writer whose release differs writes a tzdata entry before ingest, newer or older; readers and drills of another release are refused with `tzdata_mismatch`), §2.1, §2.2, invariant 8, row 21, `format.md` A.4 and A.6, `api.md` Opening, Environment seams and Health (the refusal of readers and drills, and adoption only at open, superseded by Q-93 (a); the first batch's entries replaced at gate 3.3 by the store descriptor, `format.md` A.12) |
| Q-77 | A day whose midnight occurs twice | §5.7 (the day starts at the earlier instant), §2.1 `interval length` (a day start that occurs twice likewise; a reading at the later instant is refused off phase) |
| Q-78 | How routing compilation knows the series a consumer emits | §4 Routing (a consumer declares `emits`, recorded with its routing changes, and emits through its own handle; the cycle test compares selections, at start, on every routing change and on an alias), §4 Concurrency, row 33, `api.md` Consuming, `format.md` A.6 (open part: Q-88, the pattern grammar) |
| Q-79 | A lost range found by a recovery whose sync fails | §3 Order of a start, steps 3 and 4 (the kept segments are synced before anything is written; a failed sync writes nothing, and the range is reported as found and recorded once by the next start that can write), §3 Recovery rule 2 |
| Q-80 | An interval zone that the tzdata version in force does not know, or that has become a link | §2.1 `interval length` (the name is resolved by the version in force each time; a link is followed to its target; an unknown name has no interval starts, so recorded readings are displaced, new ones refused off phase and health degraded until a version that knows it is in force), row 45, `api.md` Reading and Health |
| Q-81 | Count, min, max and last over readings whose `t` lies in the window, on a calendar-length series in another zone | §6 Aggregates (the readings whose `t` lies in the window, each with its whole amount; flagged `apportioned` where an edge lies inside an interval), invariant 22 (counts over adjacent windows add up), row 41 |
| Q-86 (a) | No space when a log segment is created | §3 Segments (the partial file deleted, health critical, ingest waits, creation retried), `format.md` A.4 (open part: Q-86 (b)) |
| Q-52 (b), on Linux | Whether writing the last manifest record, the last log batch or a seal again in place makes the next sync write it | §7 Manifest, §3 Recovery rule 3 and Order of a start, `format.md` A.4 and A.10 unchanged; checked on ext4 and XFS at gate 4.9 with a device that fails one file's sectors (`docs/measurements/4.9-fault-trials.md`; open part: the owner's decision on the macOS and Windows wording) |
| Q-86 (b), on Linux | No space at a log write before any sync, on a copy-on-write file system | §3 Segments and `format.md` A.4 Creation unchanged (a write failure on the log), checked on Btrfs at gate 4.9 (`docs/measurements/4.9-fault-trials.md`; open part: the owner's decisions on the `chattr +C` sentence and the macOS and Windows wording) |
| Q-87 | A mark lost on the same boot after a failed sync | §3 Recovery rule 3, Last batch written again (with the mark missing, unreadable or stale, the start writes the last complete batch, and its segment's header when it is the segment's first, again in place before the sync of step 3; kept only if that sync succeeds, else read-only; rests on the Q-52 (b) check, moved from roadmap 4.8 to gate 4.9), Order of a start step 3, row 12a, `format.md` A.10 (No usable mark) and A.4 (a valid trailer found in an `.open` file is sealed again from step 1, the same hole after a failed seal sync), `api.md` Environment seams (the test file system reports per page whether a sync wrote it). Engine fault tests (roadmap 4.5, page-tracking file system): after a failed sync and a lost mark every position delivered after the restart is on disk at a power loss; the rewrite's sync fails; the same with an adopted recovery entry whose sync failed; a kill-loop property for invariant 2 over failed syncs, mark damage, restarts and power losses; the seal state found with a trailer after a failed `sync_all` |
| Q-93 (a) | A store nobody can open after the host's tzdata changes | §5.7 (readers and drills never refuse; calendar results computed with the reader's own release; a stored calendar node used only where its recorded release is the reader's; the writer checks the host release about once a minute and at each local midnight and adopts a new one without a restart, by a tzdata entry and a rebuild of the affected days), `api.md` Opening and Environment seams (open part: Q-93 (b), (c)) |
| Q-97 | A memory-mapped mark in a project that forbids unsafe code | §3 Durable mark, Sync primitive, §7 Readers (the mark updated by positional writes and synced like any file; no file is mapped), `format.md` A.10, A.13, `api.md` Environment seams |
| Q-98 | Same boot identity after the page cache was lost | §3 Recovery rule 1 (a batch written only after the previous batch's sync and mark update; two or more complete batches beyond the mark on the same boot mean the mark is stale and rule 3 applies; accepted limit: one batch committed since the last mark sync can be lost unreported), invariant 10, `format.md` A.10, `api.md` `log.mark_sync` |
| Q-99 | An exclusive `flock` on the store directory on Windows | §3 Single writer (an operating-system lock on the file `LOCK`, released when the process ends; network file systems not supported), `format.md` A.12, `api.md` Handles |
| Q-100 | Where the largest stored magnitude is kept, whether the policy may change, stored nodes across a scale raise, and a lost raise | §2.1 `decimal scale` ((a) the largest magnitude a per-series function of the log, carried by tree checkpoints with the highest stored scale and brought up to date by replay; units limited to ±(2^63 − 1), `i64::MIN` out of range; (d) the scale in force is the highest of the defined scale, the kept raise entries and the highest stored scale among kept facts, and a lost raise is not written again), `excess precision` and the paragraph under the table ((b) the policy and its maximum changed by a parameter change effective at its position), §5.6 ((c) a node records the highest stored scale among the current facts it covers that are not bad; a merge takes the higher), §8 Deep, Continuous backup and Restore step 1 (checkpoint contents), `format.md` A.5 (`i64::MIN`), A.6 (fields 23–26 in a parameter change) and Part B (the scale of a chunk summary and a tree node), `api.md` Series (`set_excess_precision`). Model property tests (the tracked magnitude equals the maximum over the log after any history, restart and restore, a raise refused exactly when a fact would not fit; each value handled by the policy in force at its position; rebuild equals live and merges in any grouping bitwise equal across raises; after any lost range every kept fact rescales exactly to the scale in force) wait for decimal values in the model (before roadmap 5.1; (c) before 7.1); engine: a node spanning a raise, the batch holding a raise corrupted |
| Q-101 | A replay across a scale raise, rounded or not | §2.4 (decimal values compare by exact numeric value; a fact flagged `rounded` also matches a value that rounds to it at its stored scale; the check runs on the value as submitted, before the excess-precision policy; a correction inside a rounding is dropped as a duplicate unless sent with `revise` or a higher `changed_at`; the revised-key index holds each fact's stored scale and `rounded` flag), `api.md` Core types. Model property test (replaying any recorded history after any raises and policy changes records nothing, writes no raise entry, counts no refusal and changes no current fact) and unit tests (reading, correction, raise, replay; an unflagged 0.1234 then 0.12345 is recorded) wait for decimal values in the model (before roadmap 5.1) |
| Q-102 | A finite value too large for `i64` at the series' scale | §2.5 (out of range, `i64::MIN` units included: recorded as quality bad with no value whatever the policy, counted `out_of_range`, and reported in the reading's outcome while the call succeeds; `auto-raise` never raises for it), §2.1 `excess precision`, §2.4 (compares as "no value"), row 31, §11 Ingest, `api.md` Core types, Writing (`Outcome::Recorded.out_of_range`) and `series_stats` (`out_of_range`). Model property test (values across the `i64` boundary at every scale 0 to 18: none panics, each recorded exactly or out of range) and a unit test (`auto-raise` does not move the scale for 1e300) wait for decimal values in the model (before roadmap 5.1) |
| Q-103 | What an automatic manifest rebuild cannot recover from local files | §7 Manifest (the rebuilt manifest carries every valid record with its old epoch and reconstructs only the corrupt record's effect; no fact about a file or a loss held by one record alone; (a) liveness from valid records and named inputs, a file found in deep never live for that reason, an unlocated file not a lost range until cold, the mirror path and deep have answered; (b) a restore record never rebuilt, read from the local restore record file or deep, else read-only, and a drill never rebuilds; (c) identities kept from valid records, cause unknown where none is given, and a double fault listed under its first missing position and counted; (d) a provisional rebuild after `manifest.rebuild_wait`, uploads, evictions and removals held, deep's copy winning when it answers (the wait and acknowledgement: Q-107, settled)), §8 Eviction at 90 days (a file holding a lost range leaves the local tiers only after that loss's record is uploaded or mirrored), §8 Generations ((f) uploads in epoch order, a rebuilt manifest self-contained, uploaded once final; the local restore record file), §11 Health, invariants 10 and 27 (the counted double-fault exception), `format.md` A.12, A.14 (cause 5 unknown, rebuild record fields 5 ((g) the `file_id` floor) and 6, field 100 old epoch; (e) no fact held only by a quarantine or tier-move record) and Part B (chunk index and local restore record), `api.md` `LossCause::Unknown`, `Reason::ManifestRebuilt.provisional`, `manifest.rebuild_wait`. Engine fault tests with roadmap 6.3, 8.2 and 8.3: corrupt in turn a tier-move record of an evicted file, a publish-and-retire record whose inputs are in deep, a lost-range record, its extension and its acknowledgement, the restore record (with and without its local copy); a sink that does not answer (ingest resumes after the wait, nothing uploaded or removed, the manifest equal to a direct repair once it answers); a restore from records, a rebuild record and carried records; ids issued after a rebuild above the floor; a property that no evictions and one corrupt record shorten `losses()` |
| Q-104 | A supplied `Absent` value with a quality other than bad | §2.5 (recorded as NaN is: quality bad, no value, flagged and counted as `absent_supplied`, the supplied quality kept for the duplicate rule; never live state, so on a state or text series the previous value stays live, and "unknown" is a declared state), §2.4, row 31, `api.md` Core types and `series_stats` (`absent_supplied`). Model: `Model::commit`; `tests/rules.rs` `s2_5_absent_recorded_bad_whatever_quality_supplied`, `s2_4_absent_compared_by_supplied_quality`, `s2_5_absent_changes_no_summary_or_live_state`. Engine: round trip of `no_value` rows at each supplied quality with roadmap 4.3 |
| Q-105 | Reading a segment: zeros below the mark, unknown skippable blocks in a batch, refusals while resynchronising, the first batch against `base_sequence`, a header that disagrees with its name | `format.md` A.2 ((a) sixteen zero bytes at a block boundary are end of data only when the limit is the end of the file, an `.open` segment scanned without a mark, and every byte to it is zero; below the mark's offset or before the seal blocks zeros start an invalid region, also when they run to that limit), A.7 ((b) `block_count`, `batch_bytes` and `batch_crc32c` cover every block before the commit block; a skipped block takes no sequence and carries no `received_at`; a batch of skipped blocks alone is not complete; every block before the seal blocks belongs to a batch; (d) the first batch starts at or above `base_sequence`, higher only after an invalid region or with a recovery entry, checked by recovery at roadmap 4.5), common rules ((c) a version refusal only from a valid position or in an otherwise complete batch at a resynchronisation candidate; any other block image is not a block), A.12 ((e) a header disagreeing with its name is refused, never scanned or deleted). As implemented in `kvik-log`; `log/tests/segment.rs` pins each part |
| Q-106 | Decoding batch bodies: canonical form against the checks, the text heap, run-length choice, entry field presence, entries block splitting, reserved ids, the parameter change, the recovery entry alone | `format.md` common rules ((a) a reader applies only the listed checks; a canonical check also requires the canonical bytes), A.5 ((b) canonical heap in row order, none shared, a reader accepting any offsets inside the heap, shared bytes included; (c) unpadded payload lengths, plain wins a tie), A.6 ((d) presence checked as invalid content, a named-state series needs a name, kind and value type not compared; (e) entries share a block up to 16 MiB; (f) entry type 5 refused until roadmap 5.2, field 41 skipped (superseded by Q-46: field 41 is the reset stamp); (g) fields 25 and 26 allowed in a parameter change since Q-100, fields without a range take any value, a change with only field 1 valid; (h) a recovery entry not alone in its batch is invalid content), A.11 (nothing allocated from a declared count or length before it is checked; output bounded by the row and entry limits). As implemented in `kvik-log`, with (g) changed for Q-100; `log/tests/batches.rs` pins each part |
| Q-107 | The wait before a provisional manifest rebuild, leaving the read-only state, and acknowledging a provisional rebuild | §7 Manifest, Deep unreachable (owner: the wait is counted on the monotonic clock from each start, once deep has been asked and failed, and a restart starts it again; accepted limit: a process restarting more often than the wait stays read-only, `rebuild_manifest` being the way out; the store leaves read-only by itself when the wait ends or deep answers, the start resuming from step 4, the one exception to §3 Write or sync failure; a provisional rebuild cannot be acknowledged until it is final), §3 Write or sync failure, §11 Health, `api.md` Health. Engine fault tests with roadmap 6.3: the wait across restarts; leaving read-only when deep answers during the wait; the acknowledgement refused while provisional |
| Q-109 | A forged seal in an `.open` file | `format.md` A.4, the states recovery may find (settled at gate 4.9 by a structural rule, with no checksum pass at open and no change to A.9: an `.open` file counts as seal-written only if its seal blocks start exactly at the end of its last complete batch, the trailer's `batch_count` and `last_sequence` equal the scan's, and it is not the last segment in use; otherwise the trailer and end marker are bytes of what precedes them, and recovery reads the file again with them ignored, so a torn tail's commit blocks and definitions count; `log/tests/recovery.rs` `a_forged_seal_after_a_torn_batch_in_the_last_segment_hides_nothing`, `a_forged_seal_in_a_segment_that_is_not_the_last_in_use_is_ignored`, `a_forged_seal_in_the_last_segment_is_a_torn_tail`) |
| Q-117 | A recovery entry adopted after its own sync failed | §3 Recovery rule 3, Last batch written again, and Order of a start step 3 (an adopted entry is written again in place before the sync of step 3, whatever the mark; extends the Q-87 ruling). Scenario: a readable mark of the old boot; the start's sync of its recovery entry fails, so the entry is in the page cache only; the next start on the same boot adopts it, and its sync writes nothing; the mark then names F, a batch is acknowledged at F + 1, and a power loss removes the entry, which the following start reports as `data_lost` with nothing damaged (`log/tests/recovery.rs`, `an_adopted_entry_whose_own_sync_failed_is_written_again`) |
| Q-115 (a)–(d), (g)–(i), (l), (m) | The commit path, in part: closing a batch, the seal clock, the first mark, retrying a creation, the first manifest record, failures creating ahead, the receive time, memory at seal, a blocked seal | Settled at gate 4.9: `format.md` A.4 Sizes ((a) a batch takes whole calls in order while the sum of their data blocks, each encoded alone, stays within `max_batch_bytes`, and at least one; calls that do not encode together are committed one by one); `spec.md` §3 Sealing ((b) the 5 minutes run from when the segment is taken into use; the sync that takes the next segment into use at a timed seal carries no batch, is not bound by `min_sync_interval`, and counts as the previous log sync for the next batch); `format.md` A.10 First mark ((c) slot 0, counter 1, all position fields 0, the current boot id); `spec.md` §3 Segments ((d) retried when free space is at least `disk_critical` + `segment_size`, checked every second, and every 30 seconds where free space is unknown, `log/tests/writer.rs` `with_free_space_unknown_a_failed_creation_is_retried_after_30_seconds`; (h) any failure of a creation other than lack of space at the create or a write, a failed sync included, is a write failure on the log); `format.md` A.14 ((g) the first record takes epoch 1); A.7 ((i) `received_at` and the commit time are the wall clock when the batch is formed, raised to the previous batch's commit time); `design.md` Memory ((l) sealing uses memory independent of `segment_size`, the checksum streamed in 1 MiB reads, `log/src/writer/maint.rs` `sealing_reads_at_most_1_mib_at_a_time`); `format.md` A.13 ((m) a sharing error — on Windows the system errors 5, 32, 33 and 1224, on any platform 'resource busy' — at the rename or length change, and lack of space at the rename, are retried every 10 seconds with health degraded; any other failure of either is a write failure on the log; a rename that fails with the `.open` name gone and the `.kvl` name present is a failed sync, never retried; `log/tests/writer.rs` `a_seal_blocked_by_a_sharing_error_is_retried`, `no_space_at_a_seal_rename_is_retried`, `a_failed_rename_or_length_change_of_a_seal_is_a_write_failure`, `a_rename_whose_flush_fails_is_a_failed_sync_not_retried`, and on Windows `a_seal_blocked_by_another_handle_finishes_after_its_release`) (open part: Q-115 (e), (f), (j), (k), (n)) |
| Q-116 (d)–(j), (l), (o), (p) | Recovery at a start, in part: a first start cut short, rule 1 and later segments, the tail's extent, epochs above leases, gaps between sealed segments, an empty segment in use, a batch beyond the mark in a sealed file, unused segments with bytes, a mark from the page cache, a header put in use by a failed sync | Settled at gate 4.9: `spec.md` §3 Recovery ((d) a mark with no valid slot beside a log with no segment in use and a manifest naming no position is a first start, and an empty `log/` stops being a safe test at roadmap 8.1; (f) the tail reported is that of the last segment in use; (j) a sealed segment holding positions above the mark makes the mark stale); `format.md` A.4 ((e) under rule 1 the headers of later segments stay and the writer resumes in the last segment in use; (h) a start checks the gap rule for unsealed segments and against the last sealed one, and the scrub checks gaps between sealed segments; (i) a segment in use by its header alone, not the last in use, is left as it is and never sealed, at the cost of being read whole at every start; how the state arises is a follow-up for 5.1; (l) an unused segment is kept as created ahead only if every byte after its header is zero); (g) by roadmap 4.6 (`format.md` A.14: a start's new epochs are above every epoch a lease file names); (o) by Q-122, its residual (a reader may read a mark not yet synced; the reset stamp voids what it took) accepted in `spec.md` §7 Readers; (p) `spec.md` §3 Order of a start step 2 and `format.md` A.4 In use (a start writes the header of the segment it resumes in again, in place, before its first sync; `log/tests/recovery.rs` `a_header_put_in_use_by_a_failed_sync_is_written_again_by_a_same_boot_start`) (open part: Q-116 (a)–(c), (k), (m), (n)) |
| Q-118 (a), (b), (d), (e), (g)–(j), (l) | The reader process and leases, in part: no mark, retry pace, a refused file, truncation without a reset point, a failed lease write, when the writer looks at leases, a corrupt manifest record, the lease header, a start during a refresh | `format.md` A.10 Read ((a) a missing mark file is `MarkUnreadable`; (b) the 8 retries are back to back); `spec.md` §7 Readers ((d) nothing at or past a refused file is read; (g) no read without a renewed lease; (i) a manifest record a reader cannot take an epoch from is an error, `ReaderError::Manifest`); `spec.md` §7 Leases ((h) the writer looks at leases at its own turns, at most once a minute, and a lease is void only when both boot ids are non-zero and differ); `format.md` A.15 ((j) the lease header has `file_id` 0 and the generation at creation): settled at gate 4.9 as implemented. (e) and (l) by the owner's rulings after roadmap 4.6: rule 1 writes a recovery entry with a new reset stamp whenever it truncates a complete batch, amended by Q-125 to a complete batch or any non-zero byte beyond the mark (`spec.md` §3 Recovery rule 1; `log/tests/recovery.rs` `a_kill_on_the_same_boot_truncates_the_batch_beyond_the_mark`), and a reader compares the stamps of its two reads of the mark and discards the pass when they differ (`log/tests/reader.rs` `a_mark_with_a_new_reset_stamp_during_a_refresh_discards_the_pass`); the residual of (l), corruption events of a pass that a start overtook may be wrong, is accepted and stated in `spec.md` §7 Readers (open part: Q-118 (c), (f), (k)) |
| Q-119 | `batch_crc32c` did not depend on the blocks it was said to cover | `format.md` A.7 and A.2 (owner's ruling after roadmap 4.6: the sum covers bytes 0..12 of each block header, without `header_crc32c`, so every body is reached through its `body_crc32c`; the candidate check stays O(blocks)). Scenario: each 16-byte header ends with the CRC32C of its first 12 bytes, and a CRC fed a message and then that message's own CRC returns to a fixed state, so every set of valid headers gave the same sum for one store, file, offset and block count; a batch assigned again at the same offset after a remount had a byte-identical commit block (`log/tests/segment.rs`, `batch_crc_tells_apart_blocks_that_differ_only_in_a_header`; golden files regenerated) |
| Q-46 | Checking a kept position against a later truncation or restore | §7 Readers, Reset stamp and saved points; §3 Recovery rules 1 and 3; §4 Resync (`start: position`); §8 Generations; row 7b; `format.md` A.6 field 41, A.10 bytes 84..100, A.14 `0x25`; `api.md` `ReaderState.stamp`, `SavedPoint`, `Start::Position`, `check` (owner's ruling after roadmap 4.6: a reset stamp, a reset sequence and a random nonce, in the mark, every recovery entry and every restore record; a saved point is (position, stamp); kept when the generation and stamp are the same, else void above the lowest of the later entries' `after`, restored ends and the positions before later lost ranges, and unknown when the reset the mark names cannot be found). This answers the review's three objections: the lower bound combines entries, restored ends and lost ranges; a restore takes a fresh nonce, so a reset stamp is never reused; a lost entry gives unknown. The writer writes the stamp and the reader reports it (`log/tests/recovery.rs`, `log/tests/reader.rs`); the check itself, `start: position` and row 7b's export are built with roadmap 5.3 |
| Q-122 | A torn mark update could leave a mark older than the last one synced | `format.md` A.10 Update and Read, `spec.md` §3 Order of a start step 2 (settled by roadmap 4.7: every update between two mark syncs goes to the slot that does not hold the last synced mark, and a start writes the mark file again in place, as read, and syncs it before its own mark write, or, keeping the mark it read, writes that slot again and syncs it before the first update). Scenario: updates alternated between the slots whether or not the last was synced; with a timed mark sync, A held counter c, synced, and B c − 1; c + 1 went to B and c + 2 to A, neither synced, and a power loss that tore the write of c + 2 left A invalid and B at c − 1, older than the mark synced last. Rule 3 kept every complete batch, but the start took its id floor from that mark and the unsealed segments only, so an id defined after c − 1 in a batch then in a sealed segment was above the floor; the low floor was durable, in the recovery entry and in the new mark's highest series id, so ids would have been assigned again once roadmap 5.1 assigns above floors (`log/tests/recovery.rs`, `a_torn_mark_update_never_falls_back_past_the_last_synced_mark`) |
| Q-150 | Canonical bytes of the zstd-compressed XOR fallback | `format-chunk.md` C.8 and C.10 (settled at the review of roadmap 6.1 by option (2): encodings 4 and 5 are one canonical class, compared by the XOR stream they hold; the class is the canonical choice when the XOR payload is strictly shorter than every other candidate, never by a compressed length, and an encoding 5 payload is canonical only when shorter than its stream; cost over 2,000 rows, in bytes of XOR / best other / compressed XOR / in the class: a °F→°C walk changing every row 9,767 / 12,141 / 3,591 / yes, a two-level flicker 6,284 / 9,045 / 1,057 / yes, a cycle of five ninths 13,255 / 13,146 / 191 / no, so written in delta; rejected: the class winning within 1/16 of the best other candidate, which recovers such cycles but makes blocks up to about 1.5 % larger on common data where zstd finds nothing (a sine: 16,243 against 16,000), and common data matters more than that shape; the payload is exactly one frame, no dictionary, no checksum, no skippable frame; no level or library is pinned; `chunk/src/block/tests.rs` `xor_class_membership_ignores_compressed_length`, `xor_class_is_canonical_in_either_encoding_and_any_frame`); roadmap 6.5 measures a pure-Rust codec |
| Q-121 | Series id 0 against an id floor of 0 | §2.1 `id`, `format.md` A.6 field 46 and A.10 `max_series_id` (settled by roadmap 5.1, which assigns ids: ids start at 1 and 0 names no series, so a highest id assigned or an id floor of 0 means none; the model's registry assigns from 1). Scenario: the model assigned ids from 0, so the mark's `max_series_id` and a recovery entry's id floor were 0 both in a store that had assigned no id and in one whose only series was id 0; read literally, "above every id floor" then forbade id 0 after a power loss in an empty store, and no reader of the floor could tell id 0 from none. Decoders do not refuse id 0, so files written before stay readable (`kvik/tests/durable.rs`, `ids_start_at_one_and_never_reuse_a_floor`) |
| Q-124 | Loss reports of recovery entries, and the order of lost ranges against saved points | §3 Recovery rule 3, Order of a start step 4, §7 Readers, §11 Health, invariant 10, rows 7, 7a and 12a, `format.md` A.14 field 13 (settled after roadmap 5.1a: (a) by the owner's ruling, (b)–(d) at its review). (a) The gap of a recovery entry (positions between K and F) is a `data_lost` with cause truncated tail, which holds health degraded until acknowledged; a `possible_loss` is one per recovery entry that names a non-empty tail, a mark not readable or stale, or a missing or unreadable cursor file. (b) A `possible_loss` has its entry's position, F, as identity; identities are unique within a kind. (c) A lost-range record carries the reset stamp in force when it is written; with none known it counts as later than every reset the start found. (d) A loss reported as found by a start that could not write is not lost when a later start finds the damaged batch as a torn tail: this adds a behaviour, that start keeps reporting it as a `data_lost` under the same identity, the entry's gap (positions named above K), now recorded. Scenario: §11 listed the gap as a `possible_loss` while the engine reported it as a `data_lost`; the engine named a `possible_loss` by K + 1, which two entries can share (an entry at K + 1 lost with the mark, the next written at K + 2 with `after` = K); a lost range found later below an entry could not be told from one recorded before it, so a saved point could not be checked against it; and a found loss could vanish from the list, against invariant 27 (`tests/rules.rs` `s3_lost_mark_and_tail_leave_a_gap_described_by_the_entry`, `s7_saved_point_and_a_lost_range_recorded_with_the_mark_lost`, `s7_saved_points_across_a_gap_and_its_lost_entry`, `s11_a_found_loss_stays_reported_after_the_mark_is_lost`; `log/tests/model_histories.rs`) |
| Q-125 | Bytes damaged beyond a same-boot mark | §3 Recovery rule 1, Order of a start steps 5 and 6, row 7, invariant 10 (settled after roadmap 5.1a by the owner's ruling: a complete batch or any non-zero byte beyond the mark is a reset point, with a recovery entry, a new reset stamp and a `possible_loss`; damaged batches beyond the mark go into that entry's tail, so one entry at K + 1 describes them). This amends the Q-118 (e) ruling: "whenever it truncates a complete batch" becomes "whenever it truncates a complete batch or any non-zero byte beyond the mark"; a write cut short on the same boot now also costs a recovery entry and a report. Scenario: with a timed mark sync, a batch was acknowledged and delivered, the file system was remounted within the boot, and that batch, now beyond the mark, was then damaged; it looked like the garbage of a write cut short, so rule 1 truncated it with no recovery entry, no loss and no reset point, and its positions were given again (`tests/rules.rs` `s3_damaged_bytes_beyond_a_same_boot_mark_are_a_reset_point`, `s3_damaged_and_complete_batches_beyond_a_same_boot_mark_are_one_reset`; `log/tests/recovery.rs` `damaged_bytes_beyond_the_mark_after_a_remount_are_a_reset_point`, which fails on the earlier rule). See Q-170: its faster durability level (acknowledged once written) promises that a reading survives a crash of the process, but rule 1 truncates everything beyond the mark on the same boot; for that level rule 1, principle 3 and invariant 2 must be rewritten to keep checksum-valid complete batches beyond the mark (roadmap 5.5a) |
| Q-171 | A provisional manifest rebuild across a restart | `spec.md` §7 Deep unreachable, `format.md` A.14 (decided at the review of roadmap 6.3a, the restart point decided by the owner: at open, a rebuild record that no acknowledgement record names is provisional, so the holds survive a restart, until the mirror path and deep answer in that run for every corrupt span it stands for, or are not configured; a provisional rebuild's acknowledgement is refused, also at append; after a restart with deep unreachable, a rebuild that was final but not yet acknowledged is provisional again, and health stays degraded until deep answers). The format change is rebuild record field 7 in `format.md` A.14, an optional repeated field; a rebuild record without it still reads. The residual found at that review, that after a restart only the first corrupt record (fields 1 and 2) was known, so a later span whose loss only deep's copy holds could be dropped once the first answered, is closed by rebuild record field 7 (`format.md` A.14), which lists every corrupt span with the epochs around it and is read at every start (a rebuild record without it falls back to fields 1 and 2, bounded above by the first record it carried past them). `q171_every_corrupt_record_is_asked_for_again_after_a_restart`, `q171_after_a_restart_a_corrupt_record_whose_epoch_jumped_is_still_taken_from_deep`, `files/tests/rebuild_copies.rs`, `q171_a_rebuild_no_acknowledgement_names_stays_provisional_across_a_restart`, `q171_a_final_rebuild_not_yet_acknowledged_holds_again_after_a_restart`, `q171_a_final_unacknowledged_rebuild_restarted_with_deep_unreachable_is_provisional_until_deep_answers`, `q171_a_rebuild_of_two_corrupt_records_is_final_only_once_both_have_their_answers` |
| Q-174 | Adjacent corrupt records read as one | `spec.md` §7 Deep unreachable, `format.md` A.14 field 6 (decided at the review of roadmap 6.3a: a corrupt span whose next valid epoch is more than 2 above the last valid one before it counts as two corrupt records for the field-6 count, accepting a count that may include a loss whose identity is right after a legitimate epoch jump) |
| Q-91 | Funnel identity across configuration, run-time routes and restarts | `api.md` Consuming (a funnel equal to one in force is that funnel), §4 Funnel, Routing, §6 Latch, row 16 (owner's rulings at the review of roadmap 5.2 and its audit: a funnel is identified by its consumer's name and its funnel id, both in the routing change; a new funnel's id is one above the highest any routing change of the consumer named, removed funnels included, so an id is never given again; the routing table is rebuilt from the log at every start; a latch belongs to the incarnation that wrote it, the position of the routing change that added the funnel, and is dropped with the funnel. (c) `Consumer::route` with a funnel equal to one in force adds nothing: it returns that funnel's id, writes no routing change and keeps the latch, so an application may route its funnels at every start, as `api.md`'s example does; on both backends. (a) and (b) do not arise while the interface declares no funnels in configuration; the item that adds them records them again). Scenario: an application routes `above 10` at every start; the value rises to 20 while it is down; after the restart its route returns the same id, the funnel is not doubled, and the resync that ends the resume reports the condition true as a transition, once; at the next start, routing again, it is no transition (`kvik/tests/consumers.rs` `routing_again_at_every_start_keeps_the_funnel_and_delivers_a_change_once`) |
| Q-95 | A consumer with no cursor file at a same-boot restart, and one that registers again during a run | §4 Resync, New consumer, Consumer state, §6 Latch, Restart and resume (owner's rulings at the review of roadmap 5.2 and its audit: (a) a consumer whose cursor file is missing or unreadable at a start while routing changes name it, on the same boot or after a reboot, whether or not a recovery entry names it, has lost its cursor: it starts with no latches and receives the `NoCursor` resync of Q-131; a consumer that no routing change or cursor file names is new. (b) A consumer taken again during a run, its process having restarted and not the engine, resumes from its confirmed cursor with its confirmed latches, by a replay and a `resync`, as after downtime; what it was handed and had not confirmed is dropped; its condition delivery sequences continue within the run and start at 1 at the next engine start (§4 Order)). Scenario: (a) a cursor file is damaged while the engine is down after a clean shutdown, so no recovery entry names it; the consumer's next handle first receives the `NoCursor` resync, each condition a transition (`kvik/tests/consumers.rs` `a_lost_cursor_file_gives_a_resync_with_funnel_states_by_start_kind`); (b) the consumer process restarts: it is replayed what it had not confirmed, then a resync (`kvik/tests/api.rs` `a_consumer_taken_again_replays_then_resyncs`, both backends) |
| Q-116 (b), (m) | Recovery at a start, in part: F's sources, and the consumers in the recovery entry | §3 Recovery rule 3, `format.md` A.6 field 47, A.15, A.16 (owner's ruling at the review of roadmap 5.2, as implemented: (b) F is above every position named by the manifest's lost-range records, the registry file's `through` and every local cursor file (its cursor, its latches' positions and its last confirmed reset point), each kind of file contributing through the list of position sources a start reads (`log/src/writer/sources.rs`), to which roadmap 6.2 and 7.1 add derived files, each with a test; (m) field 47 names every consumer whose cursor file is unreadable, and every consumer named by a routing change in the registry file, an unsealed segment or a segment sealed above the registry file's `through` (Q-130) whose cursor file is missing; `losses()` reports such an entry as a `PossibleLoss` naming them (`spec.md` §11 Health)). Scenario: after a power loss the kept log ends at position 4 while a cursor file names position 12; the recovery entry is written at 13, so position 12 is never assigned again, and names the consumer whose cursor file is unreadable and the one, known from its routing change, that never wrote one (`log/tests/recovery.rs` `cursor_files_raise_f_and_missing_or_unreadable_ones_are_named`) |
| Q-130 | A consumer's missing cursor file that a start cannot tell | §3 Recovery rule 3, §4 Consumer state, §2.1, `format.md` A.6 field 47, A.16 (owner's ruling at the audit of roadmap 5.2: the registry file is written after each routing change, before that change's position can be sealed). Implemented: the routing call commits the change, and once its batch is acknowledged (with the mark synced, so the file names no position above it, invariant 26) writes the registry file before it returns, under the lock that orders routing changes; a write that fails turns the store read-only. A seal may still complete between the acknowledgement and the file, so a start also reads the routing changes of the segments sealed above the registry file's `through`, newest first, stopping at the first segment that begins at or below it. The registry file is written at a start, a clean shutdown and each routing change, so after a crash the start reads every segment sealed since the last of those, which can be the whole previous run, and with no registry file every sealed segment; that restart cost is tracked by Q-116 (n). Scenario: a consumer's only routing change lies in a segment sealed since the last registry file was written, the process stops, and the consumer's cursor file is lost; the start names the consumer in the recovery entry (`log/tests/recovery.rs` `a_routing_change_sealed_above_the_registry_file_names_its_consumer`); in the interface, the file holds a routing change as soon as `route` returns, and after a power loss with the cursor file unreadable the consumer is reported and sent the `NoCursor` resync (`kvik/tests/consumers.rs` `the_registry_file_holds_each_routing_change_when_the_call_returns`) |
| Q-131 | The resync of a consumer with no cursor file | §4 Resync, New consumer, `api.md` Consuming (owner's ruling at the audit of roadmap 5.2): the position before a start is the position before the first fact at or after T for `start: time`, P for `start: position` P (a saved point: kept, P; void above X, X; unknown, the registration is refused with `ConsumerError::UnknownPoint`), and the mark's last position otherwise. A new consumer receives no resync: its cursor file is written at once naming that position and it starts after it, reset points above it applying as to a cursor there. A consumer whose cursor file is lost (Q-95 (a)) receives, before its first delivery, a resync of cause `NoCursor` with `forget_above` = that position, carrying each funnel's current live and condition state, every condition a transition since its latches are lost; §4's "any other `resync` carries no funnel state" names this exception, and `api.md` lists `NoCursor` among the causes. Scenario: facts at 0, 1, 2 and 3 s; a consumer started at 2 s starts after the fact at 1 s, one started now after the last position, one at a kept point at it; a point above what a power loss kept is void above the recovery entry's `after`, and that entry is then a reset point for it; with their cursor files damaged, three consumers latched true each receive the `NoCursor` resync with `forget_above` by their start kind and the condition true as a transition (`kvik/tests/consumers.rs` `a_new_consumer_starts_after_the_position_before_its_start_with_no_resync`, `a_lost_cursor_file_gives_a_resync_with_funnel_states_by_start_kind`; `kvik/tests/api.rs`: a new consumer receives nothing on either backend) |
| Q-132 | A confirmed latch lost with the last second of a cursor file | §4 Consumer state, At-least-once, §6 Latch, invariant 26 (owner's ruling at the review of roadmap 5.2 and its audit: a confirmation that changes a latch writes the cursor file at once and returns once it is synced, so a confirmed condition delivery is never reported again as a transition; a state acted on but not confirmed before a stop is reported again by the resync that ends the next resume if it differs from the latch, so actions on conditions are at least once, as `spec.md` §4 At-least-once now says; the immediate write never names a position above the last synced mark: it asks for a mark sync first and is skipped, the store read-only, if the mark cannot be synced past it). Scenario: a consumer confirms a trigger to true and the process stops a moment later, the mark synced only on request; the cursor file already holds the latch, so the resume reports the condition true as no transition; every cursor file synced names no position above the mark synced at that moment (`kvik/tests/consumers.rs` `a_cursor_file_never_names_a_position_above_the_synced_mark`, `routes_cursors_and_latches_survive_a_restart`; `kvik/tests/consumer_properties.rs` `deliveries_after_crashes_follow_the_rules`: after crashes part-way through a confirmation, a condition reported as no transition is always in a state handed out before a confirmation) |
| Q-160 | Objects in the deep tier: keys, claim marker and envelope | `format.md` A.17, `spec.md` §8 Deep and Encryption, `api.md` Backup and restore (settled by roadmap 8.1a; owner: backups always encrypted, no unencrypted mode; XChaCha20-Poly1305 from an audited crate; 64 KiB authenticated chunks under a header with format version, algorithm id, key check value and nonce base). Keys start at the store id with no engine prefix (Q-198), keep a log segment's local name with other kinds under `files/<kind>/`, and list a generation's records in epoch order; a restore finds the store id by listing. Every object, `STORE`, records, claim markers and probes included, is an envelope (amended by Q-340: claim markers and checksum probes are not). An uploaded record is its A.2 block alone; the store id is bound by key derivation and associated data. Subkeys and the key check value come from HMAC-SHA-256 of the key and store id; chunk nonces are the nonce base, the chunk index and a final flag, with the header and object key as associated data, so ranges decrypt alone and truncation, reordering and moved objects fail. **The nonce base is derived** (SIV-style, from the key, the object key and the plaintext's SHA-256) rather than random, as the owner's ruling worded it. Scenario: the sink accepts a repeated `put` only with identical bytes, so with a random base a driver that restarts after its upload landed would send other bytes and lose the key for good, unless it kept a local record of each attempt, a new mechanism. Reason: the derived base makes a file's envelope the same at every attempt; it reveals only that two objects under one object key and key are identical, and keys never hold other content. To be confirmed by the owner as a change of wording. Golden files and the code are roadmap 8.1b |
| Q-162 | Buckets without conditional writes or checksum checks | `spec.md` §8 Bucket and §13, `format.md` A.17 Probe, `api.md` Backup and restore (owner's ruling at roadmap 8.1a: such a bucket is refused, `BackupRefused`; no weaker mode). The engine probes at `attach_backup` and before a restore claims: a second create under a taken key must be refused (scenario (a)), a put with a wrong SHA-256 must be rejected and leave nothing (scenario (b), a bucket that trusts the sent hash), and `stat` must report a hash. Scenario (a) is added to §13, whose metadata fallback is dropped. The probe object stays (no deletion), one per store on a bucket that passes. A checksum check that fails only sometimes is still caught by the sampled download alone. Telling a missing checksum from other sink failures: Q-210 |
| Q-180 | Stored nodes keep sums, integrals and increases exact | `spec.md` §5.6, §6, invariants 6, 7, 21, 22; `format-tree.md` T.3 (settled at the review of roadmap 7.1a: exact, stored as dyadic numbers, folded with a dyadic accumulator; measurements in the question) |
| Q-181 | Which zero is the minimum of +0.0 and −0.0 | `spec.md` §5.2, §5.6; `format-tree.md` T.5 (settled at the review of roadmap 7.1a: IEEE total order, −0.0 below +0.0) |
| Q-182 | A state name declared twice | `spec.md` §2.1 `value type`; `format.md` A.6 field 13 (settled at the review of roadmap 7.1a: names are distinct, a definition repeating one is refused) |
| Q-190 | The Parquet export schema as a user contract | `docs/export.md` Columns, Metadata and Compatibility (settled by the owner, 2026-11-16: the schema as drafted, `kvik.export_version` = `1`; adding a column or metadata key is not breaking, renaming, retyping or removing one raises the version) |
| Q-191 | The default view of an export, and Parquet for orphaned and set-aside facts | `docs/export.md` Scope and Options (settled by the owner, 2026-11-16: (a) `Current` by default, `History` named for re-ingest; (b) the same schema, one file per call, built at 8.3) |
| Q-192 | Decimal values in the export, and the scale in force on `SeriesInfo` | `docs/export.md` Columns (settled by the owner, 2026-11-16: (a) `DECIMAL(19, scale in force)` with `stored_scale`; (b) the scale in force with decimal series on the durable backend, 6.4b; `ScaleAboveExport` until then) |
| Q-193 | Fact flags as boolean columns or a bitfield | `docs/export.md` Columns (settled by the owner, 2026-11-16: boolean columns) |
| Q-194 | Quality and named states in the export | `docs/export.md` Columns and File layout (settled by the owner, 2026-11-16: dictionary-encoded quality strings; a named state as index plus `state_name`) |
| Q-195 | A `changed_at` of kind `time` beyond the timestamp range; forecast labels | `spec.md` §2.2, `api.md` Errors and Writing, `format.md` A.5, `docs/export.md` (settled by the owner, 2026-11-16: (a) refused at ingest with `Refusal::ChangedAtOutOfRange` on both backends, `i64::MAX` accepted; (b) forecast exports keep their column names, labelled by `kvik.timing`; the stored case is Q-760) |
| Q-196 | Export file layout, compression and store-level metadata | `docs/export.md` File layout, Partitioned layout, Options and Metadata; `api.md` Reader processes (settled by the owner, 2026-11-16: one file per series per period, row groups of at most 131,072 rows, zstd level 1; site and store identity in the metadata, the store id from `Reader::store_id`; `site=<site>/series=<escaped name>/month=<UTC YYYY-MM>/<view>.parquet`, amended 2026-11-17 to `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<UTC YYYY-MM>/<view>.parquet`, each segment escaped on its own and a name without three non-empty segments refused (Q-670); a clipped month as `<view>.partial-<from_ns>_<to_ns>.parquet`; the rest of the store-level metadata at 8.3, Q-196 (c); names: Q-761) |
| Q-197 | The interval end in an export, and calendar-length interval series | `docs/export.md` Columns (settled by the owner, 2026-11-16: keep `interval_end`; calendar-length ends from the interval zone under the tzdata version in force at export, recorded in the metadata, built at 7.3) |
| Q-108 | A restore from a rebuilt manifest only partly uploaded | §8 Generations (settled by roadmap 8.3a, as the question suggested: a rebuilt manifest counts for a restore only once it is whole in the bucket, its closing empty record, the one without field 100, uploaded; the restore reads from the last such rebuild record up to the next rebuild record, and with none whole it reads the records before the first rebuild record that opens a rebuilt manifest, which the old manifest uploaded before it; records upload in epoch order, so a whole rebuilt manifest is self-contained). Scenario: a rebuild record and some carried records are in the bucket, the rest not; read from the rebuild record, a restore misses files named only by records not yet carried, and their positions become a gap or a set-aside range although the bucket holds the files and the records naming them. A rule keyed on the rebuild record alone reads that truncated list. What a whole rebuilt manifest restores before its files are verified again is Q-295. The selection is `kvik_backup::plan::records_to_read`, which the restore driver applies before the fold (`backup/tests/restore_plan.rs` `q108_a_rebuilt_manifest_only_partly_uploaded_is_not_read`, `q108_records_after_a_whole_rebuilt_manifest_are_read_with_it_and_a_later_partial_one_is_not`) |
| Q-290 | The restored end and recovery entries inside segments | §8 Restore step 1 (settled by roadmap 8.3a, the general case of the rule). The plan scans the copied log segments' ranges in the source generation; a gap that recorded lost ranges cover does not end the restored log, nor one followed directly by a recovery entry whose `after` the caller names (`PlanInput::recovery_after`); a gap that holds a position of a listed segment with no copy is never bridged, since the entry is then not the next restored position. The restore driver names none, since the record is written before any file is downloaded, and needs none: a recovery entry's gap is recorded as a lost range (cause 2) by the start that wrote the entry (step 6) or by the next start that can write (step 4), and the segment holding the entry is sealed, and listed by a sealed-segment record, only by a run that could write after that, so the listing has the higher epoch; records upload in epoch order, so a restore that lists the segment has read the loss. Gaps inside a listed segment's range are recorded losses for the same reason (rule 2 regions at step 4), so a segment's range is taken as held. The loss side is guaranteed by the log's start, which goes read-only rather than seal without the record. The listing side rests on the sealed-segment record being appended after the start that recorded the gaps, which is roadmap 6.3b's and not yet built (`format.md` A.14): 6.3b must state it as an invariant with a test (no sealed-segment record has an epoch below a lost-range record of the start that preceded its seal), and roadmap 8.4a needs a kill-loop case with a truncated tail, a seal and a restore. Should it fail, the plan ends the log at the gap and sets later data aside: conservative. The hook stays for a caller that has read the segments. Scenario: a power loss truncates a tail; the start writes a recovery entry at F above K + 1 and the lost range K + 1 to F − 1; a restore from the records sees a gap between the segment ending at K and the one starting at F. Tests: `backup/tests/restore_plan.rs` `a_gap_a_recorded_loss_covers_does_not_end_the_restored_log_a_partly_covered_one_does`, `a_gap_followed_by_a_recovery_entry_naming_the_position_before_it_does_not_end_the_restored_log`, `a_recovery_entry_in_a_segment_with_no_copy_bridges_nothing`, `the_restored_end_is_the_one_the_spec_walks_to` |
| Q-293 | What a restore plan lists, places and fetches | §8 Restore steps 1 and 3, Generations; `format.md` A.14 `0x25` (decided by roadmap 8.3a). Every live file with a recorded copy in deep is listed, set-aside files included, and no other file; the copy counts whether or not a fresh check has marked it verified since a rebuild (Q-295). A listed file's tier is the one the source records give it, which is where the restore places it (step 3). The files fetched are every listed log segment (with no tree checkpoint, step 1 downloads the whole sealed log) and every other listed file placed on hot or cold, in ascending position. The restored log is scanned over log segments only: a compacted file holds only some positions of its range, and log segments are never retired by compaction (§7). When the source generation's sequence 1 is not held, the restored end is that of the restore record read, or `0:0` in generation 0. Positions of an earlier generation at or below a carried end that no copied segment or recorded loss holds fail the plan: `NotCopied` when a listed segment without a copy holds them, else `MissingBelowEnd`. Scenario: a store compacted some segments into a current file in deep and lost no segment; the plan fetches the segments, lists the compacted file and leaves it in deep. Tests: `backup/tests/restore_plan.rs` `a_listed_segment_with_no_copy_ends_the_restored_log_and_is_not_named`, `a_compacted_file_on_hot_is_listed_and_fetched_but_never_fills_a_gap`, `a_compacted_file_left_in_deep_is_listed_but_not_fetched`, `a_file_missing_at_or_below_a_carried_end_fails_the_plan`, `the_plan_names_every_copied_file_and_never_one_without_a_copy`, `a_recorded_copy_is_restored_from_after_a_rebuild_clears_its_verified_mark` |
| Q-295 | A restore from a rebuilt manifest whose files are not yet verified again | §8 Generations, §7 Manifest (the owner's ruling at the review of roadmap 8.3a): a restore plan uses a file's recorded copy in deep (`FileEntry::deep`), not the verified mark. A copy recorded but gone from the bucket since is found when it is fetched, by the hash checks and the lost-object rule (Q-82), and reported as `data_lost`: loud. Leaving it out of a claimed generation would be silent and permanent, since later restores read that generation's records only. "Verified in deep" (Q-280) keeps its meaning for the upload queue only: what roadmap 8.1c must check again or upload again after a rebuild. A carried restore record, like a carried copy-verified record, gives its files' copies and no verified mark. Scenario: segments 1 to 3 (`0:1` to `0:30`) are uploaded and verified; the manifest is rebuilt and its records upload whole; the rebuild clears every mark and the carried records set none; a disaster then strikes before the files are checked again. Read as "verified only", a full restore claims generation 1 with restored end `0:0` and lists no file, and the segments are in no record a later restore reads; with the copy, it restores to `0:30`, as from the old records a moment earlier, so the plan is monotone in the records uploaded across a rebuild. Tests: `backup/tests/restore_plan.rs` `a_recorded_copy_is_restored_from_after_a_rebuild_clears_its_verified_mark`, `q108_a_rebuilt_manifest_only_partly_uploaded_is_not_read`, `the_plan_is_monotone_in_the_records_uploaded`; `kvik/tests/restore_plan_model.rs` (rebuilt manifests); `files/src/catalogue.rs` `a_carried_restore_record_gives_copies_but_no_verified_mark` |
| Q-39 (f) | A crash while the restore record is written | §8 Restore, Failures and end (settled by roadmap 8.3b: the restore record reaches `MANIFEST` whole: the manifest's header and the record as its first record are written as `MANIFEST.new`, synced and renamed, and the directory synced; a directory holding only `MANIFEST.new` is a restore that did not reach its record, which is removed, and the restore starts again with the next generation, its claim kept. A torn record therefore never reaches `MANIFEST`. `backup/tests/restore.rs`, `a_restore_killed_at_every_file_operation_completes_after_a_restart`, `a_leftover_manifest_before_the_record_is_removed_and_the_next_generation_claimed`) |
| Q-82 | How a restore tells that the bucket has lost or corrupted a listed file | §8 Restore, Failures and end (settled by roadmap 8.3b with the owner's thresholds: a wrong key is told by the key check value in `STORE`'s envelope header before any file is judged, `RestoreError::WrongKey`, and records nothing; a listed file is lost when `stat` reports it absent on 3 tries spanning at least 10 minutes while `STORE` is reported present in the same step; it is corrupt at once when the reported size or ciphertext SHA-256 differs from its copy, and after 3 downloads with the right key whose bytes are not the copy, whose chunks fail authentication or whose plaintext hash is not its description's (a sink's `HashMismatch` on a download counts as one); any other fault, a `Transient` download included, is retried with backoff and never counts. The thresholds are `RestoreConfig::lost_tries` and `lost_over`; tries are counted per run, so a restart starts them again. A lost or corrupt file becomes a lost range, cause 3 (Q-302), carrying the restore record's reset stamp, the one in force when it is recorded (§7 Readers, `format.md` A.14 field 13). `backup/src/restore/judge.rs`, `backup/tests/restore.rs`, `backup/tests/restore_properties.rs`) |
| Q-213 | The checksum probe's key, which the key contract cannot build | `backup/src/keys.rs` (settled by roadmap 8.3b at its review, by Q-213's suggestion: the checksum probe keeps its own key `<sid>/probe/<nonce:32 hex>`, built by `Keys::checksum_probe` and parsed as `ObjectName::ChecksumProbe`, so a refused bucket's wrong-hash probe never lands on `<sid>/probe` and a later probe cannot mistake it for an earlier one) |
| Q-214 | A restore that meets a bucket it must refuse | `api.md` Backup and restore, Restore (settled by roadmap 8.3b at its review, by Q-214's suggestion: `RestoreDriver::prepare` probes the bucket in a full restore after the key check and `STORE` and before reading the claims, with the probe `format.md` A.17 defines for `attach_backup` (`<sid>/probe` for conditional writes, then a wrong-hash `put` to a fresh `<sid>/probe/<nonce>` that must fail and leave nothing); a failure is the additive `RestoreError::Refused(Refused)` (`NoConditionalWrites`, `NoChecksums`), which `Store::restore` maps to `BackupError::Refused`; a sink fault during the probe stays `RestoreError::Sink`; a drill and a resumed restore do not probe; the probe's bytes come from the restorer's nonce source. `RestoreError::SeveralStores` maps to the additive `BackupError::SeveralStores { store_ids }`. A `stat` that reports no SHA-256 reaches the probe as the sink's `Failed` (the S3 sink refuses such a reply), so it fails the restore as `Sink`, not as `Refused(NoChecksums)`. `backup/src/restore/probe.rs`, `backup/tests/restore_guards.rs`) |
| Q-305 | A claim refused with `HashMismatch` while nothing is stored | §8 Generations; `api.md` Backup and restore, No overwrite (settled by roadmap 8.3b at its review. Scenario: a restore's claim `put` for generation g fails with `HashMismatch` and stores nothing; taking g + 1 leaves g unclaimed, and a later drill, which runs one above the highest claimed generation read by `stat` in ascending order (Q-163), stops at the gap, plans from an older generation and passes while ignoring the live one. Ruling: after a `HashMismatch` the restore reads the marker by `stat`; present, it takes the next generation (Q-161: the marker may be another restore's); absent, it tries g again, at most 3 tries of g, then fails with the sink's `HashMismatch` without taking another generation, since a sink that rejects every upload must not walk the generations. `backup/src/restore/driver.rs` `claim`, `backup/tests/restore_guards.rs`) |
| Q-300 | The restore record's bytes ahead of its layout | `format.md` A.14 `0x25` (settled when roadmap 8.3b was stacked on 8.3a, as the question asked: the executor writes and reads the restore record with `RestoreRecord::to_body` and `from_body`, and the provisional codec is gone; `backup/src/restore/codec.rs` keeps only the uploaded form of a record. Scenario: the provisional codec wrote a carried loss as 100 fixed bytes, which 8.3a's strict check of every `0x25` in `Record::encode` refuses, and could not carry a lost range's reset stamp (field 13). Nothing was written with it. `backup/src/restore/codec.rs` `a_restore_record_round_trips_through_an_uploaded_block`; the executor tests in `backup/tests/restore.rs`) |
| Q-340 | Objects in the bucket that are not envelopes | `format.md` A.17 (owner's ruling when roadmap 8.3b was stacked on 8.1a's A.17: claim markers and checksum probes are unsealed objects of fixed form whose meaning lies in their key, and the only exceptions to "every object is an envelope"; this amends the Q-160 wording. The probe's content and the change of `restore/probe.rs` from a sealed random probe to it were decided at the stacking, under the ruling. A claim marker is the 12 bytes of A.17, read only by whether its key exists (Q-163); a checksum probe is the 19 ASCII bytes `kvik checksum probe` under a fresh `<sid>/probe/<nonce>`, stored only by a bucket that is refused. Scenario: A.17 said every object is an envelope while the restore executor put claim markers unsealed and sealed a random checksum probe; a sealed probe binds nothing a refused bucket's stray object needs, and a marker's store id and generation are already in its key. Neither holds any of the store's data. `backup/src/restore/probe.rs`; `backup/tests/restore_guards.rs` `a_bucket_without_checksum_checks_is_refused_before_the_claim_and_a_drill_does_not_probe`) |
| Q-271 | Where `format.md` A.17 and the backup interfaces disagree | `format.md` A.17 Keys, Claim marker, Probe (settled as superseded by the owner's ruling: the A.17 merged at roadmap 8.1a already takes the interfaces' forms, (a) other kinds under `files/<kind:2 hex>/` and (b) the 12-byte claim marker (Q-198 (e) and (f)), so `Keys` and `ClaimMarker` stay as they are; part (c), the checksum probe's key, is Q-213, settled by roadmap 8.3b. Scenario: 8.1b was built against an earlier A.17 draft with `data/` and `tree/` prefixes, a 40-byte marker and a checksum probe key `Keys` could not build; the merge of 8.1a and 8.1b ruled the interfaces authoritative before anything was uploaded. `backup/tests/keys.rs`) |
| Q-320 | Which bytes beyond a same-boot mark make a reset point | `spec.md` §3 Recovery rules 1 and 3, Adoption, first start; `format.md` A.4 (settled by the owner's ruling Q-125, with or without a readable mark: the bytes after the header of an unused segment beyond the mark's segment are reported in a recovery entry's tail before the file is deleted; valid seal blocks are not) |
| Q-321 | The lowest unsealed segment damaged, zeroed or missing, with no sealed segment below it | `spec.md` §3 Recovery rule 2; `format.md` A.4 (settled after the review of Q-320: with no sealed segment the chain of generation 0 starts at position 1, so the gap below the first segment kept is a recorded `data_lost`) |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), calendar results (§5.7, §6 Windows: a day or month result is a
window evaluation over its span, edge stretches included, in the site zone or a named IANA zone; stored day and month
nodes, kept only for spans on 15-minute boundaries of a series with a stored 15-minute level, are inputs only where
every day they cover was taken in the zone of the query, compared after resolving links, their span is the day's and
their tzdata version is the one in force, and never past the credit limit; days with no quarter-hours, days whose midnight
does not exist and historical offsets off 15-minute boundaries; the site zone as an effective-dated log entry, whose
change rebuilds calendar nodes (for days around a change to another offset, see the table), and tzdata changes,
recorded as a log entry, which rebuild only the affected days (for when a release is adopted, see the table); a
midnight in a gap that straddles it and a midnight that occurs twice (the earlier instant); calendar windows since local midnight, this month and relative days and
months; rows 20, 20a, 21 and 21a, invariants 5, 7, 8, 17 and 22 over calendar days; `tests/rules.rs` `s5_7_…` and
`s6_calendar_…`), the duplicate rule with `revise` (§2.4: identity by value and
adapter-supplied quality; a fact whose key's current fact is on no local tier, hot or cold, is recorded and counted
`duplicates_unchecked`, outside a restore whatever the revised-key index holds (see the table); invariant 15, rows 2,
2a and 2b), versioned series (§2.1–§2.4: `changed_at` required on
every reading of a versioned series and refused on any other, `revise` refused there, rank `(changed_at, position)`
in every view, identity including `changed_at`, the version index and the revised-key index, carried by each tree
checkpoint, so that during a restore ranks are exact and a replay of an earlier value of a key revised by the
checkpoint is dropped before the current chunks are back; §4: an outranked fact delivered flagged `outranked`, live and in catch-up (where catch-up judges it from the local
history and, while a restore brings detail back, for a fact after the checkpoint from the checkpoint's version index;
a replayed fact at or before the checkpoint whose outranking fact is not yet local can miss the flag), counted, and
never fed to a condition or refreshing stale-for; §5.6: it changes no node; invariants 4, 15 and 19, rows 2c, 2d and
37), compaction and files (§7: per-series current and revision chunks, ranked within the compaction's inputs, and a
chunk of the registry and routing entries; one manifest record that adds, retires and moves files atomically;
compaction of compacted files publishing its outputs and retiring its inputs in one record; retired files removed
locally only after the grace period on the writer's monotonic clock since the later of the record and the writer's
start, once their replacements are verified in another tier and no live lease names an older epoch, and, for
invariant 12, once the retired file itself has a verified copy in another tier; reader leases with boot id and
refresh counter, expired on the writer's monotonic clock, first observed at the writer's start, void on another boot;
a restore leaves out files retired by a record whose added files are in the bucket; invariant 14 among compacted files, a sealed log segment compacted once, rows 10
and 26; `tests/rules.rs` `s7_…`), tiers and eviction (§3 Disk space: tiering and eviction continue at every mark;
§8: sealed log segments and revision chunks leave hot at 90 days, revision chunks by an age per series that may be
never; current detail stays except under the retention class, a registry entry in force from its position that
moves a diagnostic series' current and revision chunks whose span ends more than its period before `now`, and moves
nothing back; fill pressure moves verified sealed log segments first, oldest first, then the current chunks of the
oldest data, never the registry's chunk; every move needs a verified copy in cold or deep and is recorded in one
manifest record; invariant 12; `tests/rules.rs` `s3_low_mark_…` and `s8_eviction_…`, `s8_retention_…`,
`s8_fill_…`), the late and future
flags by timing and per-series lateness bound (§2.2), `t_source` and the clock (§2.2, §2.5: `host_time`; before
synchronisation, `unsynced_clock`, no `future` or `late`, and a host-stamped emit waits as back-pressure, counted;
row 22a), invalid values (§2.5: NaN, ±∞ and a negative counter value recorded as bad quality without a value,
flagged `no_value` and counted), interval series (§2.1, §2.2, §5.2, §5.4, §6: length and phase fixed by the definition, a reading off its phase
refused and counted, `t + d` standing for `t` in `future`, in the second clause of `late`, in live state and in
windows of an observed series, the node form only for levels whose edges no interval crosses, windows from stored
nodes in the middle and apportioned edges flagged `apportioned`, sum and rate over covered seconds, avg, integral,
increase and time-in-state refused; rows 20a, 40–42, invariants 5, 18 and 22), series timing (§2.1, §2.2, §2.3, §4, §5.4, §6: no `future`, and no `late` by its first clause, on a scheduled or
forecast series; no live state for a forecast series; the credit limit `now` for an observed series and the window
end for a scheduled or forecast one, a window wholly after `now` on an observed series being empty; scheduled live
staleness, on `now`, and stale-for conditions over a scheduled series that follow it on the timer; a stale-for condition over a forecast series, or any condition over one without a windowed operator,
refused when routed; rows 36, 37b, 38 and 39, invariants 18, 20 and 21), the density rule (§5.1, §5.7: the 1-minute level stored only for an expected interval of 5 s or shorter, the
15-minute level and calendar nodes only for 60 s or shorter, other levels answered from facts, also in a tree
checkpoint and its verification; row 20a for a series without a stored 15-minute level), counters and rate (§5.5, §6: resets, wraps and regressions, each regression
contributing 0 and counted; rate as the increase over the elapsed time between the readings that bound its steps,
empty with no step, flagged when a step spans more than S; row 18a, `tests/rules.rs` `s6_rate_…`), retirement at `t_r` (§2.1, also for a reading that waited in the queue while the
retirement committed), the durable mark and recovery by boot id (§3),
delivery (§4, §6: routing changes as log entries taking effect at their position, in live delivery and in catch-up
(for a route before a restore's checkpoint, see the table); condition funnels evaluated on routed facts and on the
timer, never on replayed facts, their deliveries carrying the last committed position and a per-funnel sequence (for
their identity, see the table); condition state now from the latch, a threshold between its levels keeping the latched
state, and a funnel added at run time with no latch; one latch per condition funnel, moved when the consumer confirms;
on every resume, after a restart, a recovery on a new boot or a consumer restart, a replay up to the last committed
position followed by `resync` with the live state and the condition state now, a transition where it differs from
the latch or there is no latch, or `resync` alone when the cursor is too far behind the local log segments; and
catch-up after overflow from the last position enqueued in the session, condition funnels silent meanwhile and
the present state sent on re-attaching where it differs (for reads from deep, see the table); rows 14–16 and 34, invariants 2, 3, 11, 13
and 16; `tests/rules.rs` `s4_…` and `s6_…`), loss of a corrupt log segment with no other copy (§8; the whole range, recorded as a lost range, see the table), lost ranges in the open
segment found by recovery (§3 Recovery rule 2, row 11a: later batches kept, the range recorded in the manifest and
reported as `data_lost`, its positions not reused; see the table), and generations, tree checkpoints
and restore (§2.2, §8 Continuous backup, Generations, Restore drill, Restore steps 1–4 and the paragraph after them,
rows 28, 28a and 28b; for 28a, a crash before the record, during step 1 or between later downloads, a download
being atomic): a tree checkpoint carries the stored nodes, the registry and each series' latest recorded `t`
as of its position, and is uploaded only once log segments in the bucket hold every position up to it; a restore
claims the generation one above the highest claimed or present in the bucket by a create-if-absent marker, taking the
next if the marker exists, and starts it at sequence 1; a bucket with nothing backed up is refused; the restored end
is the last position before the first gap, and later positions of older generations are set aside, outside views,
trees, the registry and delivery in the writer; the restore writes a local record (see the table) and downloads
the latest checkpoint at or below the restored end and the log segments after it (with no checkpoint, the whole log)
before it accepts ingest; window queries are then flagged `restoring`; until the current chunks (and with them the
revised-key index) are back, a fact at or before the latest known `t` of its series that matches no local fact is
recorded and counted `duplicates_unchecked`; a consumer whose kept cursor is at or below the restored end catches up
to it, flagged `replayed`, if log segments after it are local, and otherwise only receives `resync`; every consumer
then receives `generation_changed` and `resync`; current chunks, then revision chunks, then log segments come back
one per step, a position in two files must have identical content, and as each span's detail is back its nodes are
recomputed from the facts as of the checkpoint's position and compared with the checkpoint's, a difference is
counted and the recomputed nodes win, and the `derived-unverified` label goes when every span the checkpoint covers
is verified; a restart after the record resumes the restore in its generation (but see the table for a failed
retry), finishing an interrupted step 1 before it accepts ingest, and one before it starts again with the next
generation; a drill is read-only, claims nothing and raises no health condition of its own; a writer that
finds a higher generation claimed stops uploading and stays read-only across restarts, with fatal health
(`tests/rules.rs` `s8_…`, `tests/invariants.rs`
`inv01_only_restore_starts_generation` and `inv09_stepwise_restore_from_checkpoint`). It does not yet implement:

| Rule | Specification | Model now | Test (`tests/model.rs` unless stated) |
|---|---|---|---|
| Commit time and as-of time | §2.2, §2.3 | As-of by `received_at` | — |
| Effective-dated parameters | §2.1, §5.1 | Every parameter (staleness limit, expected interval, counter rules, lateness bound) is fixed for the life of a series, so neither a later change of the expected interval, which leaves the stored levels as they are (§5.1), nor a past-effective change of a parameter a node depends on (Q-83) arises | `calendar_depends_on_zone_utc_levels_do_not` |
| Zones compared by canonical name | §5.7: "after resolving IANA links" | The bundled database has no link table, so a zone's identity is a fingerprint of its bundled rules (initial offset and abbreviation, every transition to 2200), which a link shares; two zones with identical rules would count as one. Europe/Amsterdam is a link to Europe/Brussels there | `tests/rules.rs` `s5_7_named_zone_compared_after_resolving_links` |
| tzdata version | §5.7: recorded per calendar file | The bundled database is labelled `bundled`; a later release is the bundled data with one zone's rules replaced (`Tzdb::with_rules`) | `tests/edge_cases.rs` `row_21_tzdata_change_rebuilds_the_affected_days` |
| Calendar files | §5.1: per-series files on hot, loaded at start | Built from the trees at every start with the tzdata version the log names, taken from the releases this process has seen (the host's database when it has not seen that version), and refreshed per changed minute at commit | `tests/invariants.rs` `inv08_calendar_rebuild_equals_live` |
| Adopting a tzdata release | §5.7: the writer reads the host's release at open, about once a minute and at each local midnight, and whenever it is not the version in force writes a tzdata entry naming it and rebuilds the affected days, without a restart; readers and drills never refuse, and compute calendar results with their own release, using a stored node only where its recorded release is theirs (Q-93 (a)) | The model holds several releases: installing one (`Model::install_tzdb`) writes nothing, results then follow the new rules before any entry names them, a restart keeps the version the log names, and `Model::rebuild_calendars` writes the tzdata entry and rebuilds when called; nothing is refused | `tests/open_questions.rs` `q76_tzdata_change_is_a_log_entry_written_by_the_rebuild` (its restart step), `tests/edge_cases.rs` `row_21_tzdata_change_rebuilds_the_affected_days` (results between the install and the rebuild) |
| Decimal series | §2.1 `decimal scale` (default 4, raised by an entry, never lowered; range check against the largest magnitude, a function of the log; the scale in force after a lost raise, Q-100), `excess precision` (`round` flagged `rounded`, `refuse`, `auto-raise`; `f64` input converted to its shortest decimal first; the policy changed at a position, Q-100), §2.4 (exact numeric comparison and rounded facts, Q-101), §2.5 (out of range, Q-102), §5.6 (node scale, Q-100), row 24 | Values are `f64` only (`ValueType::Number`): there is no scale, raise, policy, `rounded` flag or out-of-range value. The model property tests of Q-100 to Q-102 wait for decimal values in the model (before roadmap 5.1; Q-100 (c) before 7.1) | `tests/edge_cases.rs` `row_24_unit_or_kind_change_is_new_series` |
| Local days around a site-zone change | §5.7: the day of date D starts at the first instant whose local date, in the zone in force at that instant, is D or later, so days and months tile; "today" is the latest date whose day has started | `calendar::Zoning::Site` takes a day in the latest entry's zone under which it starts at or after that entry's effective time, else in the store's zone, so a change to another offset leaves hours in no day or two days overlapping; a month node is stored only where its days are contiguous | `tests/open_questions.rs` `q75_site_zone_change_to_another_offset_leaves_a_gap_or_an_overlap` |
| New-consumer start; `emits` and cycles; name patterns | §4 New consumer, Routing (a consumer declares `emits` and emits through its own handle; a change that closes a cycle is refused), row 33; the pattern grammar is open (Q-88) | Consumers are declared when the model is created and start before the first position; a consumer declares no `emits` and emits through the store like any adapter, and no cycle is rejected; a selection is a list of ids or every series | `tests/edge_cases.rs` `row_33_emit_from_a_delivery_does_not_block_and_a_cycle_is_not_rejected` |
| Routes before a restore's checkpoint | §2.1, §4 Routing: routing changes are kept with their positions in the registry file, which every tree checkpoint carries, so a route is in force from step 1 of a restore, and after its log segment has left hot | Routes are taken from the entries that are local; a route added before the checkpoint is not in force until the file holding its entry is downloaded | `tests/open_questions.rs` `q64_route_before_checkpoint_not_in_force_until_its_entry_is_back` |
| Identity of a condition delivery | §4 Order, At-least-once: `(funnel, sequence)`, the sequence in memory, from 1 at every engine start, valid until the consumer's next `resync`; nothing persisted | `(generation, funnel, sequence)`, the sequence kept on disk when the delivery is issued, never issued twice for a funnel within a generation; the `kvik` in-memory backend numbers each funnel's deliveries from 1 at every start, as the specification does and the durable backend compares against | `tests/open_questions.rs` `q61_condition_delivery_sequence_survives_a_crash`, `q61_condition_delivery_identity_includes_the_generation`; `tests/rules.rs` `s4_condition_deliveries_unique_by_funnel_and_sequence` (unique across restarts), `s4_timer_deliveries_carry_last_position_and_funnel_sequence` (the generation in the id) |
| Catch-up after overflow from deep | §4 Overflow, §8 Behind consumers: catch-up reads hot and cold only; where its next entry is on no local tier it ends with `resync`; transitions not delivered meanwhile are counted (`transitions_not_delivered`) | Catch-up after overflow reads an entry wherever it is held; the transitions in between are not counted | — |
| Funnels with an operator and no condition; conditions over a selection of several series; held-for over a windowed operator | §4 Funnel, §6 | Refused when routed (§6 Multi-series leaves composition to the consumer); held-for is modelled on the live-state value only, so that its resume can be rebuilt exactly from the facts | — |
| Timer cadence | §6 Cadence: every 1 s, configurable per funnel | Condition funnels are evaluated on the timer when `Model::tick` is called | — |
| Cursor file synced at most every 1 s | §4 Consumer state (and at once when a confirmation changes a latch, Q-132) | The cursor and the latches are durable at once on confirmation; the mark file is synced first when the cursor would name a position above the last synced mark (§3 Durable mark) | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled; replayed deliveries keep their original times | `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Back-pressure wait | §3, §11: the wait is measured | Waits are counted, not timed (the model is synchronous) | `tests/rules.rs` `s3_full_ingest_queue_waits`, `tests/edge_cases.rs` `row_13_…` |
| Group commit and `min_sync_interval` | §3 Group commit: batch syncs are due at least the interval apart and start at least half of it after any log sync (Q-310); a batch takes in what arrives meanwhile | Not modelled: each commit is one batch and one sync, with no timing (the model is synchronous) | — |
| Live staleness for stale-for conditions | §2.5, §5.4: bad-quality facts do not refresh it (see Q-16) | Any routed reading refreshes it; a resume takes the time since the last reading's `received_at` | `tests/open_questions.rs` `q16_stale_for_true_now_reported_true_on_resync` |
| Clock jumps in health | §6 Windows, §11 | Not modelled | — |
| Unknown condition and live state during a restore | §6 Conditions, §2.3 Live state, §4 Resync, §8 Restore step 2, invariant 16 (Q-49 (b)): an evaluation with no local detail is skipped and counted (`evaluations_unknown`), the condition keeps its latched state, `resync` reports "unknown", and the first evaluation with detail is a transition if it differs from the latch or there is none; live state of a series whose latest recorded `t` no local fact or stored node holds is unknown, not none | Not modelled: a series that stores no level answers windows and live state during a restore from its local facts, without a flag; nothing is reported unknown or counted | — |
| Condition state on resume | §6 Restart and resume, invariant 16 | The engine resumes conditions with `Runner::resume`, from the value now, the history rebuilt from the tree (held-for, stale-for) and the latch; a standalone `Runner::rebuild` scans a history of values and counts a true condition as fired | `restart_rebuilds_threshold_without_firing_again`, `tests/open_questions.rs` `q63_…` |
| Cursor of a consumer after a restore | §4 Consumer state, invariant 13: a cursor is never ahead of the consumer's confirmation | A consumer without a kept cursor, or whose catch-up segments are not local, gets the restored end written as its cursor, which it never confirmed, standing in for "treated as beyond the restored end" (§8 Restore step 2) | `tests/rules.rs` `s8_cursor_catches_up_only_if_its_segments_are_local` |
| Tree files and their checkpoint | §5.1 | Tree rebuilt from facts at restart (tree checkpoints are taken only for backup, by an explicit call) | — |
| Calendar results while restoring | §8 Restore step 2, §6 Old spans | As for windows (see "Window edges over detail that is not local"): a calendar span with an edge inside a minute, such as a day in Africa/Monrovia before 1972, is refused with `Restoring` | — |
| Compaction of compacted files | §7 Policy: merge when a series' chunks overlapping any 15-minute span exceed 4 | Counts and merges the live current chunks of a series on the fast disk only; revision chunks are never merged | `tests/rules.rs` `s7_merge_publishes_outputs_and_retires_inputs_in_one_record` |
| Revised-key index outside a restore | §2.4, rows 2b and 2c: a match with the key's entry in the revised-key index is a duplicate wherever the facts are; only a key with a single fact off the machine, or an incomplete entry, is recorded unchecked | Outside a restore, a fact whose key's current fact is on no local tier is recorded unchecked whatever the index holds; during a restore it is dropped | `tests/open_questions.rs` `q72_replay_off_the_machine_recorded_unchecked_although_the_index_holds_it`; `tests/edge_cases.rs` `row_02c_replay_never_reverts_a_versioned_correction` (its off-tier case) |
| Chunk indexes after a restore | §8 Restore step 1: the chunk index of every listed compacted file is downloaded before ingest is accepted | A restore brings every file back, so no chunk index is missing | — |
| Revised-key index, recent window | §2.4, §5.1, §8 Restore step 2: "A key that gains a second fact meanwhile, in the replay or by ingest, has its index entry completed when its chunk is back" | A full in-memory history of every key stands in for the index and the recent window; while restoring, a key revised only after the checkpoint has its entry completed when every current chunk is back, not when its own chunk is | `tests/rules.rs` `s8_index_entry_completed_when_the_current_chunks_are_back` |
| Age of a compacted file | §8 Eviction at 90 days: no rule for the age of a merged or compacted file | A compacted file takes the seal time of its oldest input, so a revision made today, merged with a 60-day-old chunk, leaves hot after 30 days | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Clock of the eviction age | §8 Eviction at 90 days: no clock named | The engine clock `now`, as for the retention class, from the seal of a log segment, or of a compacted file's oldest input, so a merge does not reset it | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Verification of compaction outputs | §7 Procedure: fact count, per-series checksum of facts, each chunk's summary equals the summary recomputed from its input facts | The outputs hold exactly the input entries; chunks carry no summary | — |
| A restore without the log segments after its checkpoint | §8 Restore step 1 | Where no backed-up log segment holds a position after the checkpoint, step 1 takes the first file in download order that holds it | — |
| Host-stamped readings before clock synchronisation | §2.5: they wait in the ingest queue (back-pressure) | The emit returns `Wait::UnsyncedClock` and the reading takes no queue capacity; the adapter emits it again later, and the wait is counted per attempt, not measured | `tests/edge_cases.rs` `row_22a_…` |
| Interval series | §2.1 (an `i64` value with a decimal scale), §6 (an apportioned amount computed in f64) | Values are `f64`, and sums, including apportioned amounts, are exact rationals, so invariant 22 holds exactly, also over the days of a month in a +05:45 zone | `tests/invariants.rs` `inv22_…` |
| Calendar-length interval series | §2.1 `interval zone` (also unknown or become a link, Q-80) and `day start`, §5.4 displaced readings, §5.7, §6 (count, min, max and last, Q-81), rows 43–46, invariant 23 (Q-45) | Interval series have a fixed length and phase only; calendar lengths, interval zones, day starts and displaced readings are not modelled yet | — |
| Lateness reference | §2.2, §4 Future facts, §8 tree checkpoints, row 4a, invariant 24 (Q-74) | The first clause of `late` compares with the latest recorded `t`, `future` facts included, and a tree checkpoint carries only the latest recorded `t`; not modelled yet | `tests/open_questions.rs` `q74_…` (expects the model's result) |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Compacting a corrupt file again from its kept inputs | §8 Corrupt block, row 11 | Quarantine needs a verified copy in another tier | — |
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery rule 2 and Order of a start, §4 Resync, §8 Corrupt block (reported when found; for the open segment, recorded by the next start that can write), §11, row 11a, invariants 1, 2, 9 and 10 | Damage is injected while the engine is down, only to a batch of facts or a recovery entry alone in its batch below the durable mark (Q-33), found and reported by the next recovery, and recorded by the next recovery that can write (Q-79); a block found corrupt at run time is not modelled. The range recorded is the positions the batch held, which is the range the rule names when its neighbours are valid batches. A damaged batch that a remount or a lost mark leaves beyond the mark, at the end of the open segment, is a torn tail: beyond a same-boot mark it is part of a rule 1 reset (Q-125). A lost range recorded at run time (a scrub of a sealed segment) sends no plain `resync` to consumers past it | `tests/edge_cases.rs` `row_11a_…`, `tests/invariants.rs` `inv01_gap_free_except_lost_ranges` |
| A corrupt sealed log segment with no other copy loses only its damaged region | §8 Corrupt block, row 11: valid batches copied to a new segment by one manifest record, which ends the error | The whole segment's range is lost, recorded as a lost range and reported as `data_lost`, so its positions are not issued again; the quarantine stays, with fatal health | `tests/rules.rs` `s8_corrupt_log_segment_with_no_other_copy_loses_range` |
| Recovery: the default mark sync, torn bytes, Adoption and the last batch written again | §3 Durable mark, Recovery rules 1 and 3, Order of a start, `api.md` `log.mark_sync` (default `Interval(1 s)`) | The model's default is the mark synced at every commit, so existing histories lose no acknowledged batch; the timed sync is `MarkSync::Timed`, fired by `Model::sync_mark_file`. A tail is a count of batches and whether one was torn, not bytes. A start is never cut short, so no recovery entry is adopted, and no batch or header is written again. A reset stamp's nonce comes from a counter. A start whose own sync fails delivers nothing, timer-driven condition deliveries included, and compaction and tree checkpoints are refused while read-only | `tests/edge_cases.rs` `row_07_…`, `row_07a_…`, `row_12a_…` |
| Orphaned facts and id floors | §2.1, §3 Commit, invariants 2 and 4 (Q-33) | A batch holding anything but facts cannot be damaged or lost from a tail (the mark file is synced at once after it, and the fault injections refuse it), so no fact is orphaned; recovery entries carry the id floor, which no definition can exceed | `tests/edge_cases.rs` `row_11a_fault_injection_limits` |
| Torn and corrupt manifest records | §7 Manifest, §8 Corrupt block, invariant 10 (Q-52) | Manifest records are written atomically and never corrupted; there is no empty record and no rewrite of the last record at start | — |
| Positions set aside beyond the restored end | §8 Restore step 1: excluded by position also in reader processes; a set-aside definition keeps its id reserved; the set-aside range reported in health from the restore record at every start (see Q-90); exported for re-ingest (`api.md` `export_set_aside`) | Downloaded in step 1, kept and excluded by position from views, trees, the registry and delivery in the writer; not reported; their series ids are not reserved; reader epochs (§7) still read them; no export | `tests/rules.rs` `s8_restored_end_before_first_gap`, `tests/open_questions.rs` `q38_…` |
| Restore record and the files a restore reads | §8 Generations, §8 Restore step 1: the restore record lists every file it restores from, set-aside segments included, each with its position range, and carries the earlier restored ends; it is uploaded before any other file of its generation, and the claims are then read again; a restore reads only the records of the highest generation below its own that has any, and a position set aside once stays so | The record is local and holds generation, checkpoint and restored end; a restore reads every file in the bucket whatever its generation, except files retired by a record whose added files are all in the bucket, and does not read the claims again. A second restore after a restore with a gap therefore finds the old gap again and sets aside the generation that restore started; under the specification its restored end is `1:1` and generation 1 is kept | `tests/open_questions.rs` `q37_second_restore_finds_the_old_gap_again` (expects the model's result) |
| A failed retry of a restore | §8 Restore, the paragraph after step 4: a restart after the record resumes the restore in its generation | In `restart_restore`, a retry after a crash before the record that writes its record and then fails in step 1 is discarded and stays pending, so the next restart claims another generation | `tests/open_questions.rs` `q39_failed_restart_of_a_restore_is_reported` |
| Failures after step 1 | §8 Restore, Failures and end: a file that cannot be fetched, or whose plaintext hash fails, is retried with degraded health naming it; a content conflict is named in health with both files | Downloads do not fail; a conflicting file is not installed and the step returns `Conflict`, with no health reason | `tests/open_questions.rs` `q39_conflicting_file_leaves_restore_in_progress` |
| A listed object lost or corrupted in the bucket | §8 Restore, Failures and end, §8 Corrupt block and No deletion, invariants 9 and 10 (Q-39 (e)): treated as Corrupt block with deep as the failed tier, in step 1 or later; compacted again from the other listed files, the positions no other listed file holds recorded as a lost range and reported as `data_lost`; the restore completes | Not modelled: the bucket never loses or corrupts a file, so no restore records a lost range or ends that way | — |
| End of `restoring` | §8 Restore step 3 and Failures and end: only files the restore record places on hot (and cold where configured) are brought back, and `restoring` ends when they are installed, replaced or recorded lost (Q-39 (e)); the label goes when the spans whose detail comes back are verified | Every file the restore restores from comes back, and `restoring` ends when all are local; the label goes when every span the checkpoint covers is verified | `tests/open_questions.rs` `q39_restoring_ends_when_every_file_is_local` |
| Query flags and fact reads | §11 Query; §8 Restore step 2 and Failures and end: every query result carries `restoring` while restoring, and fact reads fetch detail not yet local on demand | Only window results carry flags (`restoring`, `summary_stale`); while restoring, `current`, `facts` and `latest` return the local facts, unflagged, without fetching | — |
| Window edges over detail that is not local | §6 Old spans, §8 Restore step 2: an edge moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, the error `detail_not_local`; no bound is refused for its resolution | No window is flagged `approximate_edges` and no edge moves; while restoring, a window bound inside a minute is refused with `Restoring`. A series that stores no level, or only the 15-minute level, answers the unstored levels from its local facts: a minute the replay touches inside a quarter-hour the checkpoint holds is flagged `summary_stale`, but a window over part of a quarter-hour whose detail is not yet local is computed from the local facts without a flag (see Q-49 (b)) | `tests/open_questions.rs` `q39_no_sub_minute_query_or_checkpoint_while_restoring` |
| Replaying the log after a checkpoint | §8 Restore step 1: "replay them into the tree" | A minute for which the checkpoint holds a node and the replay (or new ingest) brings a fact, while the minute's earlier detail is not local, is recomputed from the local facts and flagged `summary_stale` until the detail is back (row 29); an engine that folds facts in `t` order into the checkpoint's node would hold it exact | `tests/rules.rs` `s8_replayed_minute_without_its_detail_is_stale` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap: lost-range records stay in the local manifest and the bucket does not carry them, so a restore sets aside the positions after a lost range | — |
