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
| Q-37 | §8 Generations, §8 Restore, §2.2 | Which manifest records a restore reads, and how generations begin |
| Q-38 | §8 Restore step 1, §2.1, §7 | Segments beyond the restored end |
| Q-39 | §8 Restore steps 2–4, §8 Deep | Failures during a restore, and when it ends |
| Q-40 | §2.4 Version index, §8 Restore | Ranking facts of versioned series during a restore |
| Q-44 (b)–(d) | §2.1 timing, §2.3 Views, §2.4, §11 | Withdrawn and far-future values on scheduled and forecast series; forecast accuracy ((d) for the owner) |
| Q-45 | §2.1 interval, §5.7 | Intervals of varying length (for the owner) |
| Q-46 | §3 Recovery rule 3, §7 Readers | Positions kept by a reader process, after a truncated tail |
| Q-47 | §4 At-least-once, Resync, §11 Consumers | How a consumer learns its recovered cursor, and what forgetting means |
| Q-48 | §3 Recovery rules 1 and 3, §4 Resync, §6 Restart and resume | The resync owed after a truncated tail is not durable |
| Q-49 | §8 Restore step 2, §5.1 density rule | What a series with no stored level serves during a restore |
| Q-50 | §6 Old spans, §5.1 density rule, §12 D7 | Window edges over moved detail of a series with no stored 1-minute level |
| Q-52 | §8 Corrupt block, invariant 10, §3 Recovery | Manifest records as single copies |
| Q-60 | §5.1 density rule, §5.7, §2.1 parameters, invariant 8 | Calendar nodes when the expected interval changes |

## Q-16 — Which commits refresh live staleness
**Scenario.** S = 3 minutes. (a) A series has been silent for 10 minutes and is stale; a late fact for last week is
committed. Read literally, "since its last commit of a fact that is not bad quality" makes the series fresh again,
although live state still shows the 10-minute-old reading. (b) A revision for the same `t` as the live-state fact,
committed after S of silence, likewise refreshes it. (c) A future fact is excluded until due (§4), but it is not
said whether staleness counts from its commit or from when it fell due, nor whether "due" follows `now` (which a
forward clock jump moves) or the monotonic clock (§4 Timers). (d) The monotonic clock restarts at boot, so after a
restart there is no last-commit instant to measure from. (e) "Bad quality" may mean the supplied or the engine
quality.
**Model.** Stale-for conditions measure monotonic time since the last arrival of any reading.
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
**Scenario.** A lost range (§3 Recovery) holds the definition of a series, a parameter change, an alias or a routing
change; later complete batches, which are kept, hold facts of that series. The registry file is compacted from the
log and may not hold the lost entry. Tree files and the revised-key index were built over facts that are now lost,
so rebuilding from the log no longer equals the live structures (invariant 8).
**Model.** Not modelled; a lost range is not produced.
**Tried and rejected.** Syncing every non-fact entry to the registry file and re-appending lost ones to the log with
their original positions; keeping facts of undefined series apart until a definition names their id. Review found
that re-appended entries put lower sequences after higher ones (§2.2, `format.md` A.4), that a definition taking
effect from its earliest fact contradicts §2.1, that new ids "above every id seen" can reuse a lost id, and that the
site-zone entry was not covered.
**Needed.** What happens to kept facts whose registry entries are lost, how derived files are re-verified, and how
invariants 1 and 8 read with lost ranges.
**Proposal.** After a lost range or a truncated tail the kept log is the whole truth: entries in the lost positions
(definitions, parameter and site-zone changes, aliases, retirements, routing changes) no longer apply and are not
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
(b) are settled (§8 Corrupt block, §11 Health); part (d) is Q-52.
**Model.** Not modelled; the model records no lost ranges and does not report a truncated tail.
**Proposal.** Both reports are read from manifest records at every start (for `possible_loss`, the truncated-tail
record proposed in Q-48), so a restart does not clear them, the list is never shortened and the counts only rise;
neither makes the store read-only. For the owner: whether they also hold health degraded until an operator
acknowledges each loss. That needs a new manifest record type and a call such as `acknowledge_loss`, and changes
operability rather than guarantees; since a power cut under load usually leaves a torn batch, most power cuts would
need an acknowledgement.
**Objection.** A reboot before any new batch commits recomputes the same K and F (the earlier record names positions
up to F − 1), so a second record with the same lost range is written and `possible_loss` is counted twice for one
loss.

## Q-37 — Which manifest records a restore reads, and how generations begin
**Scenario.** (a) A restore claims generation g and crashes before writing its restore record. The next restore
claims g + 1 and reads the records of g, the highest claimed before its own, which has none. (b) Records are keyed by
generation, so a second restore reads only the first restore's generation and not the files of the generations before
it. (c) The first store's generation (0) is never claimed, so "one above the highest claimed" can equal it while that
store is still writing, and two facts get the same position.
**Model.** A restore claims one above the highest generation claimed or present in the bucket, so it never takes
generation 0, and reads every file in the bucket whatever its generation. Its restore record (generation, checkpoint,
restored end) is local and carries no files forward; the list of bucket files it reads is kept with it, standing in
for the manifest records it reads. So a second restore finds the first restore's gap again, a known defect: with
`0:1`–`0:8`, `0:10` and `1:1` backed up, its restored end is `0:8` and all of generation 1 is set aside
(`q37_second_restore_finds_the_old_gap_again`).
**Needed.** Which records a restore reads, how a restore record carries forward the files it restored, and whether a
new store claims its generation.

## Q-38 — Segments beyond the restored end
**Scenario.** Segments beyond the first gap are kept but quarantined. (a) A definition in them is outside the restored
registry, which may then assign that id again, against "never reused" (§2.1). (b) Nothing keeps them out of
compaction (§7) or out of step 3's "log segments", where they would re-enter the views. (c) "Quarantined" means
"queries return an error" in §8 Corrupt block, and it is not said who triggers their re-ingest.
**Model.** Segments beyond the restored end are downloaded in step 1 with the log after the checkpoint. Positions of
older generations beyond the restored end are kept and excluded by position from views, trees, the registry and
delivery, wherever compaction moves them (`q38_segments_beyond_restored_end_downloaded_in_step_1_and_set_aside`).
They are not reported, their series ids are not reserved (so (a) applies), reader epochs still read them, and
re-ingest is not modelled. A drill sets aside what the restore it rehearses would set aside.
**Needed.** How these segments are held, named and reported, which ids they reserve, and how re-ingest is started.

## Q-39 — Failures during a restore, and when it ends
**Scenario.** (a) After ingest has opened, a segment at or below the restored end fails to download or fails its hash
check. (b) "A crash before the record starts the restore again" assumes an empty disk, but files may already be local.
(c) The end of `restoring` is not defined (step 3 complete, or step 4's label removed). (d) Step 3 downloads the
revised-key index, which the deep tier does not list.
**Model.** Downloads arrive one file per step (`Model::restore_step`) and do not fail, except that (a) a file holding
a position whose content differs from the local copy is not installed: the restore stays in progress and the step
fails again (`q39_conflicting_file_leaves_restore_in_progress`). (b) The record precedes every download, so a restore
that stops before it has nothing local (`q39_nothing_local_before_the_record`); a retry after such a stop that fails during step 1 leaves the engine down with
fatal health, keeps its claim and stays pending, so the next restart claims the next generation, although its record
was written (`q39_failed_restart_of_a_restore_is_reported`). (c) The store is `restoring` until
every file the restore reads is local; the label goes earlier, once every span the checkpoint covers is verified
(`q39_restoring_ends_when_every_file_is_local`). (d) The revised-key index counts as back once every current chunk is
local, and is taken from the restored facts (`q39_revised_key_index_back_with_current_chunks`). While `restoring`, a
window bound inside a minute is refused and no tree checkpoint is taken
(`q39_no_sub_minute_query_or_checkpoint_while_restoring`); only window queries are guarded, and `current`, `facts`
and `latest` return the local facts, unflagged. A restore in progress is no health condition of its own
(`q39_restoring_is_no_health_condition`). If starting a restore again, or finishing its step 1, fails at a restart,
the engine stays down and health reports `RestoreFailed` (`q39_failed_restart_of_a_restore_is_reported`).
**Needed.** What each failure does, what a restart clears, and when the store stops being flagged `restoring`.

## Q-40 — Ranking facts of versioned series during a restore
**Scenario.** Until the version index is back during a restore, the engine cannot tell whether an arriving fact on a
versioned series outranks the facts already recorded for its key. A rule that holds such facts as "undetermined"
until a download finishes was withdrawn: its effect would depend on download timing, not on the log (principle 2),
and live state and windows would differ from a rebuild meanwhile (invariants 7 and 8). The version index is also not
yet listed in the deep tier.
**Model.** Not modelled; every series ranks by position.
**Proposed.** Each tree checkpoint carries the version index as of its position, like the registry and latest `t`
(§8), so step 1's replay brings it up to the restored end and the rank is always known.

## Q-44 (b)–(d) — Withdrawn and far-future values, and forecast accuracy
**Scenario.** (b) A newer issue that no longer covers a valid time cannot withdraw the older issue's value there, so
the current view can mix issues. (c) A wrong value far in the future on a scheduled or forecast series is never
flagged, since those series are exempt from `future`. (d) How forecast accuracy is served: comparing each issue with
the outcome series, and asking what was known as of an issue time rather than as of a position. Part (a), a silent
feed, is settled (see Settled).
**Model.** Not modelled.
**Proposal for (b) and (c).** The engine sets no horizon and infers no withdrawal. A source withdraws a value by a
reading for that `t` with a higher `changed_at` (on a series that is not versioned, a later reading) and quality bad,
carrying the value it withdraws, which summaries, windows and live state ignore (§2.5, §2.3). A wrong value far ahead
is corrected by a revision or withdrawn the same way, and per-series statistics show the latest recorded `t`.
**Objections to the proposal.** (b) Under §12 D6 the withdrawing bad reading is ignored and the previous value's hold
continues up to S, so withdrawing the 14:00 value of a half-hourly price credits the 13:30 price across 14:00 with
full coverage; "coverage ends where the remaining values' validity ends" holds only at the tail of an issue. On a
series that is not versioned, a later unchecked replay of the withdrawn value (§12 D5) can revert the withdrawal,
which is the hazard §2.4 gives for declaring a series versioned. (c) Neither a revision nor a withdrawal lowers the
latest recorded `t`, so a wrong value for 2099 disables the duplicate fast path (§2.4) for good, makes every fact
during §8 Restore step 2 `duplicates_unchecked`, and keeps the latest-`t` statistic wrong.
**Proposal for (d) (for the owner).** Not served by the engine in v1. History carries every issue with its
`changed_at`, so a reader can compute "known as of issue X" (per key, the fact of highest rank with
`changed_at ≤ X`) from `View::History` and compare it with the outcome series. A view by `changed_at`, or
forecast-accuracy operators, is new scope. If confirmed, `api.md` Not in v1 gains "a view as of an issue time
(`changed_at`), and forecast-accuracy operators (computed by the reader from `View::History`)".

## Q-45 — Intervals of varying length
**Scenario.** An interval series has a fixed length d. A source that reports one amount per local day or per month
has intervals of 23–25 hours or 28–31 days, which no fixed d and phase describe.
**Model.** Not modelled.
**Needed.** Whether a reading may carry its own interval length, or whether such amounts are stored at a fixed finer
length or as calendar results.
**Proposal (for the owner).** Keep d fixed and decline such sources in v1. A reading does not carry its own length,
and the engine never splits an amount into finer intervals, which would invent facts. No existing kind stores such
amounts faithfully:
- *Counter* (cumulative, read at the period end): the step is credited to the window holding the later reading
  (§5.5); a reading at a month's end lies in the next half-open window, so each amount lands one period late, and a
  revision of one period rewrites every later value.
- *Gauge* (`t` at the period start, aggregate sum): exact only for windows made of whole periods. A window edge
  inside a period, or a query in another zone, credits the whole amount to the window holding `t`, with no
  `apportioned` flag, which is a silent misattribution (principle 5); coverage is hold-based, so a missing day shows
  only beyond S; avg and integral are meaningless but not refused. It serves only a consumer that queries whole
  source periods in the source's zone, and would be documented as a workaround with these defects, not as the rule.
- *A fixed finer d* (the adapter splits the amount): invented values, and one revision becomes many.

If confirmed, §2.1 `interval length` gains: "The length is the same for every reading: a source whose intervals vary
in length (an amount per local day in a zone with clock changes, or per month) cannot be an interval series in this
version, and the engine never splits an amount into finer intervals"; and `api.md` Not in v1 gains "interval
readings of varying length (amounts per local day or month)". A daily amount of 23–25 hours with d = 24 h is then
refused off its phase after a clock change (row 40), so the limit is visible. Accepting such sources needs a new
mechanism: a length per reading (in the fact, `format.md` and duplicate identity; a rule for overlapping intervals,
affecting sum, coverage and invariant 22; a declared maximum length bounding edge lookups and the recent window;
readiness at `t + d` per reading), or a calendar length (local day or month in a zone fixed for the series), whose
intervals tile but whose phase check and lengths then depend on tzdata, and which still does not cover irregular
billing periods. Since Q-36 such a series would store no level, so node edges raise no problem. If wanted, the
feature is best opened as its own question, starting from the calendar length.

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

## Q-49 — What a series with no stored level serves during a restore
**Scenario.** A gauge has an expected interval of 5 minutes, so the density rule (§5.1) stores no 1-minute or
15-minute level and, since Q-36, no calendar node; the tree checkpoint holds no node for it. After a restore onto an
empty disk, §8 Restore step 2 serves queries "at ≥ 1-minute resolution" from the checkpoint, flagged `restoring`,
while its facts come back only in step 3. A query for yesterday's daily mean of this series during step 2 has
neither a node nor a fact to answer from, and row 28 ("queries at ≥ 1 min from the checkpoint") does not say what it
returns.
**Model.** Not modelled; a restore completes in one step (see Q-39).
**Needed.** What such a series serves during step 2 (empty with zero coverage, an error, or a result that waits for
its detail), how the result is flagged, and whether the detail of series with no stored level is downloaded first.

## Q-50 — Window edges over moved detail of a series with no stored 1-minute level
**Scenario.** §6 Old spans evaluates an edge whose raw detail is not local "at 1-minute precision", flagged
`approximate_edges`, and §12 D7 keeps the 1-minute level hot. The density rule (§5.1) stores the 1-minute level only
for series whose expected interval is 5 s or shorter. A gauge with a 60 s expected interval has only the 15-minute
level; one with a 5-minute expected interval has none. When their detail has moved off the machine (§8 Fill
pressure, or a diagnostic retention class), a rolling 24-hour window whose start falls in that span has no 1-minute
node to evaluate its edge from.
**Model.** Not modelled; both levels are stored for every series, and no window is flagged `approximate_edges`.
**Needed.** Whether such an edge is evaluated at the finest stored level (and with no level, how), or its detail is
fetched on demand as for other reads of moved detail (§8), and how the result is flagged in each case.

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

## Q-60 — Calendar nodes when the expected interval changes
**Scenario.** A calendar node is stored only where the series' expected interval in force qualifies it for a stored
15-minute level (§5.1, §5.7). (a) The expected interval changes from 30 s to 5 minutes by a parameter entry effective
at 14:07, which is not on a 15-minute boundary: the quarter-hour 14:00–14:15 straddles the change, and it is not said
whether it qualifies, nor so whether that day stores a calendar node. (b) A parameter change with an effective time
in the past (§2.1) moves the threshold for days whose calendar nodes are already built: it would create nodes for
spans that had none, or delete nodes already stored, and a rebuild from the log must give the same set (invariant 8).
**Model.** Not modelled; both levels and calendar nodes are stored for every series.
**Needed.** Whether a quarter-hour qualifies by the expected interval in force at its start, throughout, or by some
other rule, and how a change with a past effective time rebuilds the calendar nodes it affects.

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
- **F-7.** `format.md`: a segment's first sequence is at least 1, and a last sequence of 0 means empty (§2.2).
  Part B needs a "restore" manifest record (generation, checkpoint, restored end) and manifest records keyed by
  generation; the deep tier holds generation claim markers; a tree checkpoint bundles the registry file and each
  series' latest `t` (§8).
- **F-8.** `api.md`: the backup sink needs a create-if-absent write for generation claims; restore needs a read-only
  drill mode; a consumer can register with its last confirmed position after a restore, so the application must
  persist its confirmed positions; restore returns the restored end; health names a writer superseded by a higher
  generation; re-ingest needs an entry point; a store's identity across restores and drills (§8).
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
  `min_t` and `max_t` must allow for a reading's reach of d (§2.1, §6).
- **F-15.** `format.md`: a log segment may follow, or be a copy that omits, a lost range (§8 Corrupt block); the
  lost-range record carries the commit times of the batches on either side (§11 Health).

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
| Q-28 | Two restores from one bucket; a restore from an empty bucket | §8 Generations, §8 Restore drill, row 28b (open parts: Q-37) |
| Q-29 | The restored end, and what a continuing cursor receives | §8 Restore steps 1–2, §8 Continuous backup, invariants 4 and 9, row 28 (open parts: Q-38, Q-39) |
| Q-30 | `duplicates_unchecked` during a restore compared a time with a position | §8 Restore step 2; versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 1, 2c and 2d), so that a replay never reverts a correction (open part: Q-40) |
| Q-31 | Accepting ingest before the registry is back | §8 Continuous backup, §8 Restore step 1 |
| Q-32 | A crash or reboot during a restore | §8 Restore, row 28a (open parts: Q-37, Q-39) |
| Q-34 | What `data_lost` and `possible_loss` mean beyond the report, parts (a) and (b) | §8 Corrupt block (only the damaged region is lost; the valid batches of a sealed segment are copied to a new segment by one manifest record, which ends the error), §11 Health (each report carries what is known of its range; no flag on query results), row 11 (open parts: Q-34 (c), Q-52) |
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (open parts: Q-49, Q-50, Q-60) |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (b)–(d)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (open part: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (on `resync` consumers forget positions seen above their recovered cursor), §4 Resync (every consumer, after every §3 Recovery rule 3 recovery), §3 Recovery rule 3, row 7a; no margin of skipped positions (open parts: Q-46 to Q-48) |
| Q-44 | Gaps left by scheduled and forecast series, part (a): a silent feed | §2.1 `timing` (a silent forecast source is reported by stale-for on the observed series its adapter emits on each successful fetch), row 37b (open parts: Q-44 (b)–(d)) |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4), the late and future
flags (§2.2), invalid values (§2.5), retirement at `t_r` (§2.1), the durable mark and recovery by boot id (§3),
catch-up after overflow (§4), loss of a corrupt log segment with no other copy (§8; the whole range, see the table), and generations, tree checkpoints
and restore (§2.2, §8 Continuous backup, Generations, Restore drill, Restore steps 1–4 and the paragraph after them,
rows 28, 28a and 28b; for 28a, a crash before the record, during step 1 or between later downloads, a download
being atomic): a tree checkpoint carries the stored nodes, the registry and each series' latest recorded `t`
as of its position, and is uploaded only once log segments in the bucket hold every position up to it; a restore
claims the generation one above the highest claimed or present in the bucket by a create-if-absent marker, taking the
next if the marker exists, and starts it at sequence 1; a bucket with nothing backed up is refused; the restored end
is the last position before the first gap, and later positions of older generations are set aside, outside views,
trees, the registry and delivery; the restore writes its record (generation, checkpoint, restored end) and downloads
the latest checkpoint at or below the restored end and the log segments after it (with no checkpoint, the whole log)
before it accepts ingest; window queries at 1-minute resolution are then flagged `restoring`; until the current chunks (and with them the
revised-key index) are back, a fact at or before the latest known `t` of its series that matches no local fact is
recorded and counted `duplicates_unchecked`; a consumer whose kept cursor is at or below the restored end catches up
to it, flagged `replayed`, if log segments after it are local, and otherwise only receives `resync`; every consumer
then receives `generation_changed` and `resync`; current chunks, then revision chunks, then log segments come back
one per step, a position in two files must have identical content, and as each span's detail is back its nodes are
recomputed from the facts as of the checkpoint's position and compared with the checkpoint's, a difference is
counted and the recomputed nodes win, and the `derived-unverified` label goes when every span the checkpoint covers
is verified; a restart after the record resumes the restore in its generation, finishing an interrupted step 1
before it accepts ingest, and one before it starts again with the next generation; a drill is read-only, claims nothing and raises no health condition of its own; a writer that
finds a higher generation claimed stops uploading and stays read-only across restarts, with fatal health
(`tests/rules.rs` `s8_…`, `tests/invariants.rs`
`inv01_only_restore_starts_generation` and `inv09_stepwise_restore_from_checkpoint`). It does not yet implement:

| Rule | Specification | Model now | Test (`tests/model.rs` unless stated) |
|---|---|---|---|
| Commit time and as-of time | §2.2, §2.3 | As-of by `received_at` | — |
| `t_source`, `host_time`, `unsynced_clock` | §2.2, §2.5, row 22a | Not modelled | — |
| Duplicate identity | §2.4: adapter-supplied quality | Quality after future stamping | `duplicate_check_compares_engine_assigned_quality` |
| Negative counter values | §2.5, §5.5: bad quality, no value | Reset rule applied | `negative_counter_decrease_counts_as_reset` |
| Record on change | §2.6, row 17a | Keep-alive facts are ordinary facts from the adapter | `tests/edge_cases.rs` `row_17a_…` |
| Effective-dated parameters and site zone | §2.1, §5.7, row 21 | One staleness limit per series; days computed per zone on demand | `calendar_depends_on_zone_utc_levels_do_not` |
| Low disk mark | §3: compaction pauses; tiering continues | Tiering pauses too | `low_mark_pauses_tiering_too` |
| Routing changes as log entries; new-consumer start; cycles | §4, rows 33–34 | Fixed selections from configuration | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled | — |
| Back-pressure wait | §3, §11: the wait is measured | Waits are counted, not timed (the model is synchronous) | `tests/rules.rs` `s3_full_ingest_queue_waits`, `tests/edge_cases.rs` `row_13_…` |
| Live staleness for stale-for conditions | §2.5, §5.4: bad-quality facts do not refresh it (see Q-16) | Any reading passed to the condition refreshes it | — |
| Clock jumps in health | §6 Windows, §11 | Not modelled | — |
| Latch and `resync` | §4, §6, rows 15–16, invariant 16 | Operator rebuilt from the tree; a true condition counts as fired | `restart_rebuilds_threshold_without_firing_again`, `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Timer deliveries with per-funnel sequence | §4 | Conditions are tested apart from delivery | — |
| Tree files and their checkpoint, density rule | §5.1, row 20a | Tree rebuilt from facts at restart (tree checkpoints are taken only for backup, by an explicit call); both levels, and calendar nodes, stored for every series | — |
| Calendar results as windows | §5.7: a result is a window evaluation, also in a named zone; no stored node for a span off 15-minute boundaries or without a stored 15-minute level; rows 20a and 21a | Day node is the merge of quarter-hours; unaligned days refused | `day_node_omits_edge_stretches`, `unaligned_days_are_refused` |
| Rate | §6: increase ÷ elapsed time; flagged when a step spans more than S; empty with no step | Increase ÷ covered seconds | `rate_divides_by_covered_seconds` |
| Atomic manifest records | §7, row 10, invariant 14 | Publish and retirement are two records | `compaction_crash_after_publish_resumes_retirement` |
| Inputs kept until verified; log segments kept 90 days | §3, §7, §8 | Compaction retires its input segments; so where no backed-up log segment holds a position after the checkpoint, step 1 of a restore takes the first file in download order that holds it | `compaction_crash_after_publish_resumes_retirement` |
| Reader leases: refresh counter and boot id, expiry and grace on the writer's monotonic clock | §7, row 26 | Files leave hot after the grace period; old epochs read remotely | `reader_epoch_survives_grace_then_reads_remotely` |
| Revised-key index, recent window | §2.4, §5.1 | A full in-memory history; during a restore, the history of local facts, plus every value of each key with more than one restored fact once the current chunks are back (see Q-39) | — |
| Retention class | §2.1, §8 Retention class | Not modelled; detail moves only under the model's tier rules | — |
| Series timing: scheduled and forecast series, credit limit | §2.1, §2.2, §2.3, §4, §5.4, §6, invariants 18, 20 and 21 | Not modelled; every series is observed | — |
| Interval series | §2.1, §2.2, §5.2, §6, invariant 22 | Not modelled; readings are instants | — |
| Versioned series | §2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 2c and 2d | Not modelled; every series ranks by position | — |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Compacting a corrupt file again from its kept inputs | §8 Corrupt block, row 11 | Quarantine needs a verified copy in another tier | — |
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery, §11, rows 11 and 11a, invariant 10 | Only a torn last batch is modelled | — |
| A corrupt sealed log segment with no other copy loses only its damaged region | §8 Corrupt block, row 11: valid batches copied to a new segment by one manifest record, which ends the error | The whole segment's range is lost and the quarantine stays | `tests/rules.rs` `s8_corrupt_log_segment_with_no_other_copy_loses_range` |
| Timed mark sync; `possible_loss`; no reuse of positions after a truncation; `resync` to every consumer after it | §3 Durable mark, §3 Recovery rule 3, §4 Resync, row 7a | The mark is always exact; a truncation after a reboot is not reported | — |
| Segments beyond the restored end | §8 Restore step 1: quarantined, reported in health like a lost range, available for re-ingest (see Q-38) | Downloaded in step 1, kept and excluded by position from views, trees, the registry and delivery; not reported; their series ids are not reserved; reader epochs (§7) still read them | `tests/rules.rs` `s8_restored_end_before_first_gap` |
| Restore record and the files a restore reads | §8 Generations, §8 Restore step 1 (see Q-37) | The record is local; a restore reads every file in the bucket whatever its generation, so a second restore finds the first restore's gap again and sets aside the generation that restore started | `tests/open_questions.rs` `q37_…` |
| Query flags | §11 Query: each result carries coverage and flags | Only window results carry flags (`restoring`, `summary_stale`); while restoring, `current`, `facts` and `latest` return the local facts, unflagged (see Q-39) | — |
| Replaying the log after a checkpoint | §8 Restore step 1: "replay them into the tree" | A minute for which the checkpoint holds a node and the replay (or new ingest) brings a fact, while the minute's earlier detail is not local, is recomputed from the local facts and flagged `summary_stale` until the detail is back (row 29); an engine that folds facts in `t` order into the checkpoint's node would hold it exact | `tests/rules.rs` `s8_replayed_minute_without_its_detail_is_stale` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap (the model records no lost ranges) | — |
| Consumers after a restore | §8 Restore step 2: every consumer "then" receives `generation_changed` and `resync` | Signals and fact deliveries are kept in separate lists (`signals`, `observed`), so the model cannot show that `generation_changed` follows the catch-up | `tests/rules.rs` `s8_cursor_catches_up_only_if_its_segments_are_local` |
| Invariants 12–16 | §10 | Not checked yet | — |
