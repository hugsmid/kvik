# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-16 | §5.4, §2.3, §4 | Which commits refresh live staleness, when a fact falls due, and across a restart |
| Q-33 | §3 Recovery, §2.1, invariants 1 and 8 | Registry entries and derived files after a lost range |
| Q-34 (c) | §11 Health, §3 Recovery | Whether loss reports survive a restart, and whether they are latched until acknowledged |
| Q-39 (f) | §8 Restore (Failures and end, the paragraph after step 4) | A crash while the restore record is written |
| Q-44 (b) and (c) | §2.1 timing, §2.2, §2.3 Views, §2.4, §8 Restore step 2, §11 | Withdrawn values on scheduled and forecast series, far-future values on any series |
| Q-46 | §3 Recovery rule 3, §7 Readers | Positions kept by a reader process, after a truncated tail |
| Q-47 | §4 At-least-once, Resync, §11 Consumers | How a consumer learns its recovered cursor, and what forgetting means |
| Q-48 | §3 Recovery rules 1 and 3, §4 Resync, §6 Restart and resume | The resync owed after a truncated tail is not durable |
| Q-52 | §8 Corrupt block, invariant 10, §3 Recovery | Manifest records as single copies |
| Q-79 | §3 Recovery rules 2 and 4, §11 Health, row 11a | A lost range found by a recovery whose sync fails |
| Q-82 | §8 Restore (Failures and end), §8 Hashes | How a restore tells that the bucket has lost or corrupted a listed file |
| Q-83 | §5.6, §5.2 holds, §2.1 parameters, invariants 5 and 8 | Node contents after a past-effective change of the staleness limit |
| Q-84 | §2.3 Live state, §6 Conditions, §8 Restore step 3, invariant 16 | A condition or live state that stays unknown |
| Q-85 | §8 Corrupt block, No deletion, Fill pressure, §7 | A loss in the bucket found outside a restore |
| Q-88 | §4 Funnel, Routing, `api.md` Consuming | The grammar of name patterns |
| Q-91 | §4 Funnel, Routing, §6 Latch, `api.md` Consuming, row 16 | Funnel identity across configuration, run-time routes and restarts |
| Q-92 | §2.4 revised-key index, §8 Restore step 2, invariant 8, principle 2 | The "incomplete" mark on a revised-key index entry |
| Q-93 | §5.7 tzdata version in force, §7 Readers, §8 Restore drill | A store nobody can open after the host's tzdata changes, and a release name standing for its rules (for the owner) |
| Q-94 | §7 compaction order, §8 Fill pressure, Corrupt block, §3 Disk space | Compaction in position order when the next segment is not local or is quarantined |
| Q-95 | §4 Consumer state, New consumer, §6 Latch, Restart and resume | A consumer with no cursor file at a same-boot restart, and one that registers again during a run |
| Q-96 | §6 Restart and resume, §4 Resync, Behind consumers | A resume replay with a segment missing in the middle, and `resync` per funnel or per consumer |

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

## Q-33 — Registry entries and derived files after a lost range
**Scenario.** A lost range (§3 Recovery) holds the definition of a series, a parameter change, an alias, a routing
change, or a site-zone or tzdata entry (registry entries since Q-64 and Q-76, §2.1); later complete batches, which
are kept, hold facts of that series. The registry file is compacted from the
log and may not hold the lost entry. Tree files and the revised-key index were built over facts that are now lost,
so rebuilding from the log no longer equals the live structures (invariant 8). The same holds for a lost range that a
restore records when the bucket has lost or corrupted a listed file (§8 Restore, Failures and end). A lost log segment
whose facts were already compacted loses only its non-fact entries: a scattered set of positions, not a range between
batches, so the report's bytes and commit times on either side (§11 Health) are undefined for it, and the batch
`commit_time` that as-of-time queries over the surviving facts need (§2.3) is lost with it.
**Model.** Not modelled: a lost range comes only from a damaged batch of facts in the open segment (row 11a), and
`Model::corrupt_open_batch` refuses a batch holding a registry, routing or zone entry
(`tests/edge_cases.rs` `row_11a_fault_injection_limits`).
**Tried and rejected.** Syncing every non-fact entry to the registry file and re-appending lost ones to the log with
their original positions; keeping facts of undefined series apart until a definition names their id. Review found
that re-appended entries put lower sequences after higher ones (§2.2, `format.md` A.4), that a definition taking
effect from its earliest fact contradicts §2.1, that new ids "above every id seen" can reuse a lost id, and that the
site-zone entry was not covered.
**Needed.** What happens to kept facts whose registry entries are lost, how derived files are re-verified, and how
invariants 1 and 8 read with lost ranges.
**Proposal.** After a lost range or a truncated tail the kept log is the whole truth: entries in the lost positions
(definitions, parameter and site-zone changes, tzdata entries, aliases, retirements, routing changes) no longer apply and are not
appended again. A kept fact whose series has no definition in the kept log is an orphaned fact: kept in the log and
in compaction, outside views, trees and delivery, counted per id and available for re-ingest. The lost-range record
carries an id floor (the highest id in the registry file, the checkpoints, the kept log and every earlier floor,
plus the number of positions in the range or, for a truncated tail, the tail's bytes divided by the smallest
definition entry) and the registry entries and revised keys that derived files hold from the lost positions. It is
synced before any derived file is changed; then everything derived from positions at or above the start of the loss
is removed, or rebuilt from the newest checkpoint below it and the kept log after it. New ids are assigned above
every floor, and invariant 8 reads "from the kept log entries, with the id floors of the manifest's lost-range
records". Lost entries are issued again through the normal calls; a definition issued again is a new series. For
`format.md`: entries of the registry file, revised-key index and version index carry the position they came from.
**Objections.** (1) Nothing orders the cut-back of derived files before the mark is written, or makes a restart redo
it: the mark is written and the process crashes on the same boot while the registry file still holds a definition
from a lost position; rule 1 applies and the name maps to the lost definition. The derived-unverified label and the
duplicate mode of §8 Restore step 2 are not persisted either. The same holds for a lost range found at run time
under §8 Corrupt block. (2) Orphaned facts break invariant 2: a rule 2 lost range loses the definition at position
d, and a consumer's cursor c lies between d and an orphaned fact f routed to it by explicit id. c is not below the
lost range, so no `resync` is sent, and f is excluded from delivery. (3) For the owner: the truncated-tail bound
assumes that acknowledged bytes reached the disk in some form. On a disk that returns from sync without writing, an
id defined in the lost tail can be assigned again; either §2.1's "never reused" states that residual, or a batch
holding a registry entry is acknowledged only after a mark sync.

## Q-34 (c) — Whether loss reports survive a restart, and whether they are latched
**Scenario.** It is not said whether `data_lost` and `possible_loss` clear, and how: a restart on the same boot
right after a `possible_loss` may no longer report it, and nothing says whether health stays degraded. Parts (a) and
(b) are settled (§8 Corrupt block, §11 Health); part (d) is Q-52. The same question applies to the set-aside range
each restore record reports (§8 Restore step 1), which is derived from the record at every start.
**Model.** `data_lost` is read from the manifest's lost-range records, so it is reported after every start, never
cleared, and holds health degraded without making the store read-only (`tests/edge_cases.rs`
`row_11a_corrupt_block_inside_the_open_segment`); a truncated tail is not reported.
**Proposal.** Both reports are read from manifest records at every start (for `possible_loss`, the truncated-tail
record proposed in Q-48), so a restart does not clear them, the list is never shortened and the counts only rise;
neither makes the store read-only. For the owner: whether they also hold health degraded until an operator
acknowledges each loss. That needs a new manifest record type and a call such as `acknowledge_loss`, and changes
operability rather than guarantees; since a power cut under load usually leaves a torn batch, most power cuts would
need an acknowledgement.
**Objection.** A reboot before any new batch commits recomputes the same K and F (the earlier record names positions
up to F − 1), so a second record with the same lost range is written and `possible_loss` is counted twice for one
loss.

## Q-39 (f) — A crash while the restore record is written
**Scenario.** A restore claims generation g and crashes while writing its restore record locally, leaving a torn
record file. §8 Restore (Failures and end) says `restore` refuses a directory that holds files and no restore record,
and the paragraph after step 4 says a crash before the record starts the restore again with the next generation. It
is not said whether a torn record counts as no record (the directory then holds a file, so `restore` refuses it, and
a restart has no record to resume from) or as a record (which cannot be read).
**Model.** Not modelled; writing the record is atomic.
**Needed.** How a torn local restore record is recognised, and whether the restore then starts again with the next
generation or the directory is refused.

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

## Q-46 — Positions kept by a reader process, after a truncated tail
**Scenario.** A reader process (§7) reads position p from the log up to the mark and keeps it, for example in an
export it continues by position. Power is lost before the mark sync, the tail holding p is truncated and p is
assigned to a new fact (§3 Recovery rule 3). Consumers receive `resync` and forget unconfirmed positions (§4); a
reader receives nothing, and may skip the new fact or keep the lost one.
**Model.** Not modelled; the mark is always exact.
**Needed.** Whether readers can see that a tail was truncated (for example a recovery count in the mark or the
manifest that they compare), or whether the residual risk is accepted and stated.
**Proposal.** A reader that keeps a position for later use keeps with it the epoch it took before reading that
position. Before continuing it reads the manifest: if a truncated-tail record (Q-48) has a later epoch and its K is
below the kept position, everything the reader took above K is void, and it continues after K. `api.md`:
`reader.epoch()` and `reader.truncated_since(epoch)`. Proposed addition: a row for an export continued by position
after a power loss.
**Objection.** §7 Manifest says each record has an epoch, but not that every record, including one that publishes no
file, takes a strictly higher durable epoch. A reader can take epoch e from a record not yet synced; a power loss
drops that record, the truncated-tail record written at recovery reuses e, and the reader's check finds no later
record.

## Q-47 — How a consumer learns its recovered cursor, and what forgetting means
**Scenario.** A consumer confirms position p; its cursor file, synced at most every second (§4 Consumer state), still
names an earlier position when power is lost. The tail holding p is truncated and p is assigned to a new fact
(§3 Recovery rule 3). On `resync` the consumer must forget the positions it has seen above its recovered cursor
(§4 At-least-once), p included, but nothing tells it which cursor was recovered; the `cursor` event in §11 is not
defined. Separately, "forget" is undefined for a consumer that keeps state outside the engine keyed by position:
positions in the truncated tail may be the only copy of acknowledged facts that are now lost, and with an upsert
keyed by position the new fact at p overwrites the lost one (see Q-34).
**Model.** Not modelled; the mark is always exact.
**Earlier proposals.** `resync` after a truncated tail carries the consumer's recovered cursor. Alternatively, the engine
delivers one value to every consumer: the first new position after recovery, since only positions at or above it
(above the last kept batch, the synced cursors and the manifest records) can be reused; this is tighter than each
consumer's recovered cursor and also serves readers (Q-46). What a consumer with outside state does with the lost
facts is not proposed.
**Proposal.** `resync` carries one value, `forget_from` = K + 1 of the lowest truncated-tail record (Q-48) whose F
is above the consumer's cursor, or nothing. The consumer drops every position at or above it from its de-duplication
memory, its own last confirmation included. Records it keeps outside the engine at those positions, and series ids
first seen there, are copies of facts the store may no longer hold; the engine never asks for them to be deleted,
and a consumer that needs them sets them aside before it accepts later deliveries. For the owner: whether the engine
should protect such stores, which needs a position identity that changes across a truncation (for example an epoch
inside the position), a new mechanism and a change to §2.2. The undefined `cursor` event is removed from §11.
`api.md`: `Resync` carries `forget_from: Option<Position>`.
**Objections.** (1) Recomputing `forget_from` in every session after every restart repeats it. Recovery gives K =
100 and F = 105; consumer C replays to 100, receives `resync` with `forget_from` 101, receives live 105–200 and
confirms 200, and its cursor file still names 100 when the process crashes on the same boot. On restart C receives
`forget_from` 101 again and must drop the valid 105–200 from de-duplication, so it processes their replay twice;
this repeats on every restart for a consumer whose routed series are rare. Only the first `resync` after a recovery
should carry it, which needs a durable "forgot up to record R" in the cursor file. (2) A consumer with no cursor
takes the lowest record of the current generation; since every reboot writes a record, a consumer whose cursor file
was lost forgets almost the whole generation, which also conflicts with `start: position`. It should use the record
that lists its cursor file as missing.

## Q-48 — The resync owed after a truncated tail is not durable
**Scenario.** Recovery under §3 Recovery rule 3 resumes, and a new fact commits at a reused position p. The process
crashes on the same boot before consumer C has received its `resync`. On restart rule 1 applies, so no `resync` for
a truncated tail is sent, and §6 Restart and resume delivers the facts after C's cursor, p included, before its
`resync`; C may drop the new fact at p as a repeat.
**Model.** Not modelled; the mark is always exact.
**Earlier proposals.** Persist a "resync owed" flag, or a reuse floor, with the mark until each consumer has confirmed its
`resync`.
**Proposal.** Every §3 Recovery rule 3 recovery writes one manifest record, the truncated-tail record, and syncs it
before the tail is truncated, before any derived file is changed and before the mark is written. It holds the boot
id, the last position of the last kept batch (K), the first new position (F), the tail's length and readable commit
blocks, any cursor file found missing or unreadable, and the id floor and derived-file contents of Q-33. The
positions between K and F, if any, are its lost range, and it names no position at or above F. Only a repeat on the
same boot with the same K and F writes no second record. Nothing is owed as state: whether a consumer needs a
`resync` is recomputed at every start from its cursor file and these records. A consumer whose cursor, or `start`
position, is below the F of such a record receives, for the lowest one, the facts after its cursor up to K flagged
`replayed`, then `resync` carrying `forget_from` = K + 1 (Q-47), then the facts from F flagged `replayed`; no
position at or above F reaches it before that `resync`. Proposed additions: an invariant to that effect and a row
for a crash on the same boot after a rule 3 recovery, before a consumer received its `resync`. For `format.md`: the
record's fields above, and every lost-range record carrying its cause, boot id, byte and position range and id
floor; `api.md`: `store.lost_ranges()`.
**Objections.** (1) F ignores positions named by derived files (the registry file, the revised-key and version
indexes, a tree's "complete up to"): an acknowledged definition at K + 1 lies in the torn tail and the registry file
holding it was synced, but no cursor names it, so F = K + 1, the lost range is empty and K + 1 is reused although a
durable file named it. (2) Replay up to K, `resync`, then replay from F conflicts with §8 Behind consumers
(`resync`, not replay; never download from deep): records pile up with every reboot, so the lowest record above a
long-offline cursor can lie in evicted segments. (3) The record holds F yet names no position at or above F: if its
F field counts towards the highest sequence among local manifest records, a repeated recovery computes F + 1 and the
no-second-record rule breaks. (4) Objection 1 of Q-47 applies here too.
Reviewers judged that this group (Q-33, Q-34 (c), Q-46–Q-48) needs one design round as a whole, best done with the
log implementation (roadmap 4.5) where recovery is built concretely.

## Q-52 — Manifest records as single copies
**Scenario.** A manifest record not yet uploaded exists only on the hot tier. §8 Corrupt block names the open log
segment and a sealed log segment not yet uploaded and not yet compacted as the only single copies, and invariant 10
lists only their corruption as a loss; neither names manifest records. More would rest on them under the proposals
of Q-33, Q-34 (c) and Q-48. With the current format, corruption of a record cannot be told from a torn manifest
tail, and a record cannot be rebuilt.
**Model.** Not modelled; manifest records are not corrupted.
**Proposed.** Name a manifest record not yet uploaded as a single copy in §8 Corrupt block. A candidate rule for its
corruption: fetch the record from deep when it has been uploaded, otherwise read-only with fatal health.
**Needed.** A way to tell a torn manifest tail from a corrupt record, and what a corrupt record that was never
uploaded does to the facts and positions it describes.

## Q-79 — A lost range found by a recovery whose sync fails
**Scenario.** After a power loss, recovery finds a corrupt batch below the old boot's mark (rule 2: record a lost
range in the manifest, report `data_lost`), then the sync of rule 4 fails, so the engine stays read-only and leaves
the old mark. The rules apply in order and §8 records a loss "when found", so `data_lost` is reported at once; but
rule 2 does not say whether the lost-range record is still written to the manifest (a write on an engine that has
just failed a sync, against §3 Write or sync failure), or waits for a recovery that can write.
**Settled by the text.** `data_lost` is reported in health as found, while read-only.
**Model.** It reports `data_lost` as found, writes no record while read-only (a read-only engine writes nothing, not
even a seal), and the next recovery that can write records the range once; new positions continue above it
(`tests/edge_cases.rs` `row_11a_failed_recovery_sync_reports_the_range_as_found`). A scrub that finds a corrupt
sealed segment with no other copy while the engine is read-only or stopped does the same (`tests/rules.rs`
`s8_corrupt_log_segment_found_while_read_only_or_stopped_is_recorded_later`).
**Needed.** Whether a recovery whose sync fails writes the lost-range record.
**Proposal.** §3 Recovery rule 4: "If that sync fails, the engine stays read-only, leaves the old mark and writes
nothing; lost ranges it found are reported in health as found, and recorded by the next recovery."

## Q-82 — How a restore tells that the bucket has lost or corrupted a listed file
**Scenario.** Since Q-39 (e), a listed file that the bucket no longer holds, or has corrupted, is handled as §8
Corrupt block with deep as the failed tier, and the positions no other listed file holds become a lost range, while a
file that cannot be fetched, or whose plaintext hash fails, is retried. A download whose plaintext hash fails can
come from an object corrupted in the bucket, a download damaged in transit, or a wrong encryption key (which fails
every file); a bucket can also report an object missing for a while. Treating the first failure as a loss records a
permanent lost range, and with a wrong key would record every position lost; retrying for ever leaves the restore
with no exit, which Q-39 (e) settled against.
**Model.** Not modelled; downloads do not fail.
**Needed.** What shows that the bucket has lost or corrupted a file (for example the bucket's reported ciphertext
hash against the manifest's, §8 Hashes, or a number of failed downloads), and how a wrong key is told apart and
refused rather than recorded as loss.

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

## Q-88 — The grammar of name patterns
**Scenario.** §4 Funnel selects series by "ids or name patterns", and since Q-78 a consumer's `emits` does too;
compilation refuses a change that closes a cycle where "two patterns that one name could match" (§4 Routing), which
must be decidable from the two patterns alone. No document defines a pattern: `api.md` shows only `plant/*/power`.
Whether `*` matches one whole segment of `<instance>/<device>/<metric>`, any run of characters within a segment
(`plant/line-*/power`), or runs across `/` decides both which series a funnel receives and which selections overlap.
Q-78's scenario takes `plant/*/power` to match `plant/line-1/power-smoothed`, which it does under none of these
readings. Overlap is a per-segment comparison for whole-segment wildcards and needs an intersection test for
wildcards within a segment.
**Model.** A selection is a list of ids or every series; there are no patterns.
**Needed.** The pattern grammar and its overlap test.
**Proposal.** A pattern has the three segments of a name, each a literal or `*`, which matches any one whole segment;
two patterns overlap when, in each segment, they are equal or either is `*`; an id overlaps a pattern when any name
of its series matches it.

## Q-91 — Funnel identity across configuration, run-time routes and restarts
**Scenario.** Since Q-64 the routes in force are those the registry holds, and a funnel declared in configuration
that the registry does not hold, or holds differently, is first written as a routing change at start (§4 Routing).
(a) The registry does not record whether a funnel came from configuration or was added at run time, so it is not
said what removing a funnel from configuration does: nothing (the log still names it), or a removing routing change,
which would also remove every run-time route at the next start. (b) A funnel whose condition is changed in
configuration is written as a routing change at start: it is not said whether it is the same funnel, keeping its
latch (§6 Latch: "a latch belongs to one incarnation of a funnel"), or a new incarnation, which has none and sends no
`resync` (Q-63). (c) `api.md` gives a `Funnel` no id: an application that calls `consumer.route()` at every start,
as the example does, would either add a second funnel or, matched as "held differently", replace the funnel and
drop its latch, so a condition that became true while the consumer was down is no longer delivered as a transition
on resume (row 16).
**Model.** Funnels are added with an id chosen by the test (`Model::add_route`); there is no configuration, and a
removed funnel's latch is dropped.
**Needed.** What identifies a funnel across starts; whether configuration can remove or change one; and whether a
changed funnel keeps its latch.

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

## Q-93 — A store nobody can open after the host's tzdata changes, and a release name standing for its rules
**Scenario.** (a) Since Q-76 only a writer that can write adopts the host's release, by writing a tzdata entry at
open; a reader process or a restore drill whose release is not the version in force is refused with
`tzdata_mismatch`, and so is a writer that cannot write. After a host update, a store whose writer is read-only
(after a failed sync, §3) or stopped cannot be opened by any process, not even to read or export, until the old
release is installed again: an operator sees a store that will not open because of an unrelated system update.
(b) The version is a release name: a patched or partial build under the same name, or a vendor's build with other
rules, is trusted as the release the log names, so the derived values of two hosts with the same name can differ.
**Model.** The model holds several releases and refuses nothing (see "Adopting a tzdata release" in the table below).
**Needed (for the owner).** Whether a reader or a writer that cannot write may open with another release (and with
which rules derived values are then reported), and whether the version must identify the rules (a fingerprint of the
zones used) rather than the release name.

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

## Q-95 — A consumer with no cursor file at a same-boot restart, and one that registers again during a run
**Scenario.** (a) The engine restarts on the same boot and a consumer's cursor file is missing (it was never written:
cursor files are synced at most every 1 s, §4 Consumer state). §3 Recovery rule 3 treats a missing cursor file as no
cursor after a reboot, but on the same boot it is not said whether the consumer is new (`start`, §4 New consumer) or
resumes, what first condition state it receives, and whether its funnels' latches, kept in the same file, start
empty. (b) A consumer registers again during a run under a name that has a cursor (its process restarted, the engine
did not): it is not said whether it resumes from its cursor with a replay and `resync`, as after downtime (§6 Restart
and resume), whether its latches are kept, and whether its condition delivery sequences continue (Q-61: "starts at 1
at every engine start").
**Model.** The cursor and the latches are durable at once on confirmation (see "Cursor file synced at most every 1 s"
in the table below); a consumer restart resumes from the cursor with a replay and `resync`.
**Needed.** The first state, the latches and the delivery identity in both cases.

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

## Open at v0.2

The specification was tagged v0.2 with these questions still open, by the owner's decision. Each is to be settled
by the roadmap item named here, before that item is done. Until then the model's choice, where a test pins it, is the
one named in the question. Settling Q-33, Q-34 (c), Q-46 to Q-48 or Q-88 may add fields to `docs/format.md` or
calls to `docs/api.md` after gate 3.3; such a change is made by the item that settles the question, in its own pull
request, and noted in the question.

| Question | Settle by roadmap item | Note |
|---|---|---|
| Q-16 | 5.4 | Live state and staleness are built there |
| Q-33 | 4.5 | Recovery with lost ranges |
| Q-34 (c) | 4.5 | Recovery and health reports |
| Q-39 (f) | 8.3 | Restore |
| Q-44 (b) and (c) | 5.1 | Series registry and timing |
| Q-46 | 4.5 | One design round with Q-33, Q-34 (c), Q-47 and Q-48 |
| Q-47 | 4.5 | Recovery, with 5.3 (resync) |
| Q-48 | 4.5 | Recovery |
| Q-52 | 3.1 | Telling a torn manifest tail from a corrupt record needs a format change |
| Q-79 | 4.5 | Recovery |
| Q-82 | 8.3 | Restore |
| Q-83 | 7.2 | Late and corrected data; recorded after the tag, from the Q-60 ruling |
| Q-84 | 8.3 | Restore, with 5.3 (resync) |
| Q-85 | 8.1 | Tiers and corrupt blocks |
| Q-88 | 5.2 | Routing table; recorded after the tag, from the Q-78 ruling |
| Q-91 | 5.2 | Routing table and durable cursors; recorded after the tag, from the review of the Q-61, Q-63 and Q-64 rulings |
| Q-92 | 6.2 | Revised-key index; recorded after the tag, from the review of the Q-72 ruling |
| Q-93 | 3.1 | For the owner, with the tzdata entry kind; recorded after the tag, from the review of the Q-76 ruling |
| Q-94 | 6.3 | Compaction order; recorded after the tag, from the review of the Q-71 ruling |
| Q-95 | 5.2 | Durable cursors; recorded after the tag |
| Q-96 | 5.3 | Catch-up, resync and replay; recorded after the tag |

## For the format and interface documents
The specification wins where the documents differ. These follow from it, for the checks of `docs/format.md` and
`docs/api.md`:
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
  it read, each with its generation, and every file it restores from, set-aside segments included, each with hash,
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
- **F-11.** `format.md` A.10: the mark file is synced on a timer (`mark_sync_interval`) and at clean shutdown, not never;
  recovery reads the highest position named by cursor files and manifest records before assigning positions. `api.md`:
  the `mark_sync_interval` configuration key and a `possible_loss` health reason with its counts; acknowledgement does
  not wait for the mark sync; test seams can inject a failed mark sync (§3). The manifest's lost-range record needs a
  cause, telling corruption (`data_lost`) from a truncated tail (`possible_loss`); A.10's recovery bullets and A.11's
  tests need the stale-mark case. Whether `possible_loss` is latched until acknowledged is part of Q-34 (c).
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
  store's first batch (§5.7), and `api.md` needs how a store is created with them; the routing-change entry (A.6,
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
| Q-20 | A corrupt block inside the open segment after a reboot | §3 Recovery, §8 Corrupt block, §11 Health, rows 11 and 11a, invariants 1, 9 and 10 (open parts: Q-33, Q-34) |
| Q-21 | Which clock measures lease expiry | §7 Leases (refresh counter and boot id; grace period on the writer's monotonic clock), row 26 |
| Q-22 | Negative counter values if D1 changes | §2.5 |
| Q-23 | Stored calendar nodes for days off 15-minute boundaries | §5.7 |
| Q-24 | A failed sync followed by a power loss | §3 Write or sync failure, row 12 |
| Q-25 | When recovery writes the mark; a missing or invalid mark | §3 Recovery |
| Q-26 | The first sequence of a generation, and "no position yet" | §2.2, §8 Restore step 1 |
| Q-27 | Where consumer cursors come from after a restore | §8 Continuous backup, §8 Restore step 2, §4 Resync |
| Q-28 | Two restores from one bucket; a restore from an empty bucket | §8 Generations, §8 Restore drill, row 28b |
| Q-29 | The restored end, and what a continuing cursor receives | §8 Restore steps 1–2, §8 Continuous backup, invariants 4 and 9, row 28 (open part: Q-39 (f)) |
| Q-30 | `duplicates_unchecked` during a restore compared a time with a position | §8 Restore step 2; versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 1, 2c and 2d), so that a replay never reverts a correction |
| Q-31 | Accepting ingest before the registry is back | §8 Continuous backup, §8 Restore step 1 |
| Q-32 | A crash or reboot during a restore | §8 Restore, row 28a (open part: Q-39 (f)) |
| Q-34 | What `data_lost` and `possible_loss` mean beyond the report, parts (a) and (b) | §8 Corrupt block (only the damaged region is lost; the valid batches of a sealed segment are copied to a new segment by one manifest record, which ends the error), §11 Health (each report carries what is known of its range; no flag on query results), row 11 (open parts: Q-34 (c), Q-52) |
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (the levels fixed by the definition since Q-60) |
| Q-37 | Which manifest records a restore reads, and how generations begin | §8 Generations (generation 0 counts as claimed; each generation above 0 begins with a self-contained restore record listing the files it restores from and carrying the earlier restored ends, uploaded before any other file of its generation, after which the claims are read again; a restore reads the highest generation below its own that has records, and keeps the store id), §8 Restore step 1 (a position set aside once stays so) and the paragraph after step 4, invariant 9, row 28b |
| Q-38 | Segments beyond the restored end | §8 Restore step 1 (positions set aside, a term distinct from Corrupt block's quarantine: excluded by position everywhere, also in reader processes, with no query error; a set-aside definition keeps its id reserved; the set-aside range reported in health from the record at every start; re-ingested only by the operator, through export and Re-ingest), §2.1 `id`, §11 Health, invariants 4 and 9, row 28, `api.md` Backup and restore (open part: Q-34 (c), for clearing the report) |
| Q-39 | Failures during a restore, and when it ends, parts (a)–(d) | §8 Restore, Failures and end (any failure before step 1 is complete is fatal and resumed at restart, and later failures are retried with degraded health, except a listed file the bucket has lost or corrupted, part (e); the record precedes every download; `restoring` is derived and ends when every file step 3 brings back is installed, or since (e) replaced or recorded lost), §8 Restore steps 1–4, §8 Continuous backup and Deep (each tree checkpoint carries the revised-key index), §2.4, §11 Health, §13, `api.md` (open part: Q-39 (f)) |
| Q-39 (e) | What ends a restore when the bucket has lost or corrupted a listed object | §8 Restore, Failures and end (a listed file that the bucket no longer holds, or has corrupted, in step 1 or later, is treated as Corrupt block with deep as the failed tier: compacted again from the other listed files, the positions no other listed file holds becoming a lost range reported as `data_lost`, and the restore completes; facts of a lost log segment held by other listed files fetched in step 1; a lost tree checkpoint replaced by an earlier one or the whole sealed log; `restoring` ends when every file step 3 brings back is installed, replaced or recorded lost), §8 Corrupt block and No deletion (the exception for a restore), invariants 2, 9 and 10, §11 Health and the set-aside report (open parts: Q-85, a loss in the bucket found outside a restore; Q-33, a definition entry lost this way; Q-82, how a loss in the bucket is told from a transient fault, a damaged download or a wrong key; a content conflict between two listed files still waits for an operator) |
| Q-40 | Ranking facts of versioned series during a restore | §2.4 Version index, §8 Continuous backup and Deep, §8 Restore steps 1–2 (each tree checkpoint carries the version index, and step 1's replay brings it to the restored end before ingest opens), §13 |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (b) and (c)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (calendar lengths: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (on `resync` consumers forget positions seen above their recovered cursor), §4 Resync (every consumer, after every §3 Recovery rule 3 recovery), §3 Recovery rule 3, row 7a; no margin of skipped positions (open parts: Q-46 to Q-48) |
| Q-44 | Gaps left by scheduled and forecast series, part (a): a silent feed | §2.1 `timing` (a silent forecast source is reported by stale-for on the observed series its adapter emits on each successful fetch), row 37b (open parts: Q-44 (b) and (c)) |
| Q-44 (d) | Forecast accuracy | Not served by the engine in v1: history keeps every issue with its `changed_at`, so a reader computes "known as of issue X" from `View::History` and compares it with the outcome series; `api.md` Not in v1 (a view as of an issue time, forecast-accuracy operators) (open parts: Q-44 (b) and (c)) |
| Q-45 | Intervals of varying length | §2.1 `interval length`, `interval zone` and `day start` (a calendar length, `day` with a day start or `month`, in a zone fixed for the series, its interval starts taken under the tzdata version in force), §2.2, §5.1, §5.4 (no stored level; displaced readings), §5.7, §6 Aggregates, invariants 21–23, rows 20a, 40 and 43–46, `api.md` Series and Not in v1 (irregular billing periods) (the tzdata version in force, a day start that occurs twice, an interval zone unknown or become a link, and count, min, max and last when an edge cuts an interval were settled later by Q-76, Q-77, Q-80 and Q-81) |
| Q-49 | What a series with no stored level serves during a restore, part (a): queries | §8 Restore step 2 and §6 Old spans (window edges move to where detail is local, the error `detail_not_local` only when no span is left, never an empty result; reads of facts fetch on demand; no priority download), row 28 |
| Q-49 (b) | Conditions and live state with no local detail during a restore | §6 Conditions (the state is unknown, not false: the evaluation is skipped and counted, `evaluations_unknown`, the condition keeps its latched state, and the first evaluation with detail is delivered as a transition if it differs from the latched state, or there is none), §2.3 Live state (unknown, not none, for a series whose latest recorded `t` no local fact or stored node holds), §4 Resync and §6 Restart and resume (`resync` reports "unknown" for that funnel, no transition), §8 Restore step 2, invariant 16, §11 Health, `api.md` Consuming, Reading and statistics (`ConditionState::Unknown`, `Live::Unknown`) (open part: Q-84, a state that stays unknown) |
| Q-50 | Window edges over moved detail of a series with no stored 1-minute level | §6 Old spans (an edge whose detail is not local moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, `detail_not_local`; no fetch on the evaluation path), invariants 7, 17 and 22, `api.md` Reading |
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
| Q-76 | Whether a tzdata change is a log entry | §5.7 (the version in force is named by the latest tzdata entry, the first in a store's first batch; the engine reads one release at open, and a writer whose release differs writes a tzdata entry before ingest, newer or older; readers and drills of another release are refused with `tzdata_mismatch`), §2.1, §2.2, invariant 8, row 21, `format.md` A.4 and A.6, `api.md` Opening, Environment seams and Health |
| Q-77 | A day whose midnight occurs twice | §5.7 (the day starts at the earlier instant), §2.1 `interval length` (a day start that occurs twice likewise; a reading at the later instant is refused off phase) |
| Q-78 | How routing compilation knows the series a consumer emits | §4 Routing (a consumer declares `emits`, recorded with its routing changes, and emits through its own handle; the cycle test compares selections, at start, on every routing change and on an alias), §4 Concurrency, row 33, `api.md` Consuming, `format.md` A.6 (open part: Q-88, the pattern grammar) |
| Q-80 | An interval zone that the tzdata version in force does not know, or that has become a link | §2.1 `interval length` (the name is resolved by the version in force each time; a link is followed to its target; an unknown name has no interval starts, so recorded readings are displaced, new ones refused off phase and health degraded until a version that knows it is in force), row 45, `api.md` Reading and Health |
| Q-81 | Count, min, max and last over readings whose `t` lies in the window, on a calendar-length series in another zone | §6 Aggregates (the readings whose `t` lies in the window, each with its whole amount; flagged `apportioned` where an edge lies inside an interval), invariant 22 (counts over adjacent windows add up), row 41 |

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
| Adopting a tzdata release | §5.7: the engine reads one release at open and uses no other while it runs; a writer whose release is not the version in force writes a tzdata entry naming it before it accepts ingest, so a restart is the adoption; a reader or drill of another release is refused with `tzdata_mismatch` | The model holds several releases: installing one (`Model::install_tzdb`) writes nothing, results then follow the new rules before any entry names them, a restart keeps the version the log names, and `Model::rebuild_calendars` writes the tzdata entry and rebuilds when called; nothing is refused | `tests/open_questions.rs` `q76_tzdata_change_is_a_log_entry_written_by_the_rebuild` (its restart step), `tests/edge_cases.rs` `row_21_tzdata_change_rebuilds_the_affected_days` (results between the install and the rebuild) |
| Local days around a site-zone change | §5.7: the day of date D starts at the first instant whose local date, in the zone in force at that instant, is D or later, so days and months tile; "today" is the latest date whose day has started | `calendar::Zoning::Site` takes a day in the latest entry's zone under which it starts at or after that entry's effective time, else in the store's zone, so a change to another offset leaves hours in no day or two days overlapping; a month node is stored only where its days are contiguous | `tests/open_questions.rs` `q75_site_zone_change_to_another_offset_leaves_a_gap_or_an_overlap` |
| Entries at store creation | §5.7: a store's first batch holds its first site-zone entry and a tzdata entry; there is no store header | A store header beside the log, written at creation and carried by the bucket, holds the site zone and tzdata version before any entry, so that positions start at `0:1`; a start or restore configured with a zone other than the store's zone in force (its site-zone entry with the latest effective time, else the header's) is refused (`OpenRefused`, `SiteZoneMismatch`), a restore before it claims a generation | `tests/rules.rs` `s5_7_store_keeps_its_site_zone_and_refuses_another_at_open` |
| New-consumer start; `emits` and cycles; name patterns | §4 New consumer, Routing (a consumer declares `emits` and emits through its own handle; a change that closes a cycle is refused), row 33; the pattern grammar is open (Q-88) | Consumers are declared when the model is created and start before the first position; a consumer declares no `emits` and emits through the store like any adapter, and no cycle is rejected; a selection is a list of ids or every series | `tests/edge_cases.rs` `row_33_emit_from_a_delivery_does_not_block_and_a_cycle_is_not_rejected` |
| Routes before a restore's checkpoint | §2.1, §4 Routing: routing changes are kept with their positions in the registry file, which every tree checkpoint carries, so a route is in force from step 1 of a restore, and after its log segment has left hot | Routes are taken from the entries that are local; a route added before the checkpoint is not in force until the file holding its entry is downloaded | `tests/open_questions.rs` `q64_route_before_checkpoint_not_in_force_until_its_entry_is_back` |
| Identity of a condition delivery | §4 Order, At-least-once: `(funnel, sequence)`, the sequence in memory, from 1 at every engine start, valid until the consumer's next `resync`; nothing persisted | `(generation, funnel, sequence)`, the sequence kept on disk when the delivery is issued, never issued twice for a funnel within a generation | `tests/open_questions.rs` `q61_condition_delivery_sequence_survives_a_crash`, `q61_condition_delivery_identity_includes_the_generation`; `tests/rules.rs` `s4_condition_deliveries_unique_by_funnel_and_sequence` (unique across restarts), `s4_timer_deliveries_carry_last_position_and_funnel_sequence` (the generation in the id) |
| Catch-up after overflow from deep | §4 Overflow, §8 Behind consumers: catch-up reads hot and cold only; where its next entry is on no local tier it ends with `resync`; transitions not delivered meanwhile are counted (`transitions_not_delivered`) | Catch-up after overflow reads an entry wherever it is held; the transitions in between are not counted | — |
| Funnels with an operator and no condition; conditions over a selection of several series; held-for over a windowed operator | §4 Funnel, §6 | Refused when routed (§6 Multi-series leaves composition to the consumer); held-for is modelled on the live-state value only, so that its resume can be rebuilt exactly from the facts | — |
| Timer cadence | §6 Cadence: every 1 s, configurable per funnel | Condition funnels are evaluated on the timer when `Model::tick` is called | — |
| Cursor file synced at most every 1 s | §4 Consumer state | The cursor and the latches are durable at once on confirmation | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled; replayed deliveries keep their original times | `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Back-pressure wait | §3, §11: the wait is measured | Waits are counted, not timed (the model is synchronous) | `tests/rules.rs` `s3_full_ingest_queue_waits`, `tests/edge_cases.rs` `row_13_…` |
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
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery rule 2, §4 Resync, §8 Corrupt block ("recorded when found"), §11, row 11a, invariants 1, 2, 9 and 10 | Damage is injected while the engine is down, only to a batch of facts below the durable mark (Q-33), found and reported by the next recovery, and recorded by the next recovery that can write (Q-79); a block found corrupt at run time is not modelled. The range recorded is the positions the batch held, which is the range the rule names when its neighbours are valid batches; the `resync` for a cursor below or inside it is the one every resume sends | `tests/edge_cases.rs` `row_11a_…`, `tests/invariants.rs` `inv01_gap_free_except_lost_ranges` |
| A corrupt sealed log segment with no other copy loses only its damaged region | §8 Corrupt block, row 11: valid batches copied to a new segment by one manifest record, which ends the error | The whole segment's range is lost, recorded as a lost range and reported as `data_lost`, so its positions are not issued again; the quarantine stays, with fatal health | `tests/rules.rs` `s8_corrupt_log_segment_with_no_other_copy_loses_range` |
| Timed mark sync; `possible_loss`; no reuse of positions after a truncation; forgetting positions above the recovered cursor | §3 Durable mark, §3 Recovery rule 3, §4 At-least-once, Resync, row 7a | The mark is always exact, so no delivered position is reused and no cursor is at or above a truncated tail; a truncation after a reboot is not reported. The `resync` owed to every consumer after a recovery under rule 3 is the one every resume sends, after the replay up to the last kept batch | `tests/rules.rs` `s4_resync_after_rule_3_recovery_before_later_positions` |
| Positions set aside beyond the restored end | §8 Restore step 1: excluded by position also in reader processes; a set-aside definition keeps its id reserved; the set-aside range reported in health from the restore record at every start; exported for re-ingest (`api.md` `export_set_aside`) | Downloaded in step 1, kept and excluded by position from views, trees, the registry and delivery in the writer; not reported; their series ids are not reserved; reader epochs (§7) still read them; no export | `tests/rules.rs` `s8_restored_end_before_first_gap`, `tests/open_questions.rs` `q38_…` |
| Restore record and the files a restore reads | §8 Generations, §8 Restore step 1: the restore record lists every file it restores from, set-aside segments included, each with its position range, and carries the earlier restored ends; it is uploaded before any other file of its generation, and the claims are then read again; a restore reads only the records of the highest generation below its own that has any, and a position set aside once stays so | The record is local and holds generation, checkpoint and restored end; a restore reads every file in the bucket whatever its generation, except files retired by a record whose added files are all in the bucket, and does not read the claims again. A second restore after a restore with a gap therefore finds the old gap again and sets aside the generation that restore started; under the specification its restored end is `1:1` and generation 1 is kept | `tests/open_questions.rs` `q37_second_restore_finds_the_old_gap_again` (expects the model's result) |
| A failed retry of a restore | §8 Restore, the paragraph after step 4: a restart after the record resumes the restore in its generation | In `restart_restore`, a retry after a crash before the record that writes its record and then fails in step 1 is discarded and stays pending, so the next restart claims another generation | `tests/open_questions.rs` `q39_failed_restart_of_a_restore_is_reported` |
| Failures after step 1 | §8 Restore, Failures and end: a file that cannot be fetched, or whose plaintext hash fails, is retried with degraded health naming it; a content conflict is named in health with both files | Downloads do not fail; a conflicting file is not installed and the step returns `Conflict`, with no health reason | `tests/open_questions.rs` `q39_conflicting_file_leaves_restore_in_progress` |
| A listed object lost or corrupted in the bucket | §8 Restore, Failures and end, §8 Corrupt block and No deletion, invariants 9 and 10 (Q-39 (e)): treated as Corrupt block with deep as the failed tier, in step 1 or later; compacted again from the other listed files, the positions no other listed file holds recorded as a lost range and reported as `data_lost`; the restore completes | Not modelled: the bucket never loses or corrupts a file, so no restore records a lost range or ends that way | — |
| End of `restoring` | §8 Restore step 3 and Failures and end: only files the restore record places on hot (and cold where configured) are brought back, and `restoring` ends when they are installed, replaced or recorded lost (Q-39 (e)); the label goes when the spans whose detail comes back are verified | Every file the restore restores from comes back, and `restoring` ends when all are local; the label goes when every span the checkpoint covers is verified | `tests/open_questions.rs` `q39_restoring_ends_when_every_file_is_local` |
| Query flags and fact reads | §11 Query; §8 Restore step 2 and Failures and end: every query result carries `restoring` while restoring, and fact reads fetch detail not yet local on demand | Only window results carry flags (`restoring`, `summary_stale`); while restoring, `current`, `facts` and `latest` return the local facts, unflagged, without fetching | — |
| Window edges over detail that is not local | §6 Old spans, §8 Restore step 2: an edge moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, the error `detail_not_local`; no bound is refused for its resolution | No window is flagged `approximate_edges` and no edge moves; while restoring, a window bound inside a minute is refused with `Restoring`. A series that stores no level, or only the 15-minute level, answers the unstored levels from its local facts: a minute the replay touches inside a quarter-hour the checkpoint holds is flagged `summary_stale`, but a window over part of a quarter-hour whose detail is not yet local is computed from the local facts without a flag (see Q-49 (b)) | `tests/open_questions.rs` `q39_no_sub_minute_query_or_checkpoint_while_restoring` |
| Replaying the log after a checkpoint | §8 Restore step 1: "replay them into the tree" | A minute for which the checkpoint holds a node and the replay (or new ingest) brings a fact, while the minute's earlier detail is not local, is recomputed from the local facts and flagged `summary_stale` until the detail is back (row 29); an engine that folds facts in `t` order into the checkpoint's node would hold it exact | `tests/rules.rs` `s8_replayed_minute_without_its_detail_is_stale` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap: lost-range records stay in the local manifest and the bucket does not carry them, so a restore sets aside the positions after a lost range | — |
