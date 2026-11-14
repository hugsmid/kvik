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
| Q-39 (e) | §8 Restore (Failures and end), §8 No deletion, §8 Corrupt block | What ends a restore when the bucket has lost or corrupted a listed object (for the owner) |
| Q-39 (f) | §8 Restore (Failures and end, the paragraph after step 4) | A crash while the restore record is written |
| Q-44 (b)–(d) | §2.1 timing, §2.3 Views, §2.4, §11 | Withdrawn and far-future values on scheduled and forecast series; forecast accuracy ((d) for the owner) |
| Q-45 | §2.1 interval, §5.7 | Intervals of varying length (for the owner) |
| Q-46 | §3 Recovery rule 3, §7 Readers | Positions kept by a reader process, after a truncated tail |
| Q-47 | §4 At-least-once, Resync, §11 Consumers | How a consumer learns its recovered cursor, and what forgetting means |
| Q-48 | §3 Recovery rules 1 and 3, §4 Resync, §6 Restart and resume | The resync owed after a truncated tail is not durable |
| Q-49 (b) | §4 Resync, §8 Restore step 2, §6 Old spans, invariant 16 | Conditions and live state with no local detail during a restore (for the owner) |
| Q-52 | §8 Corrupt block, invariant 10, §3 Recovery | Manifest records as single copies |
| Q-60 | §5.1 density rule, §5.7, §2.1 parameters, invariant 8 | Calendar nodes when the expected interval changes |
| Q-61 | §4 Order, At-least-once, Consumer state, §6 Latch | Identity of a condition delivery, and where its sequence is kept |
| Q-62 | §4 Overflow, Replay, §6 Cadence | Condition transitions while a consumer catches up after overflow |
| Q-63 | §6 Restart and resume, Conditions, Latch, invariant 16 | Condition state now, for conditions with memory |
| Q-64 | §4 Routing, §8 Continuous backup, §8 Restore step 1 | Routing changes before a restore's checkpoint |
| Q-70 | §5.6, §8 Restore step 3, §8 Tiers | Chunk indexes after a restore |
| Q-71 | §7 Manifest, §7 inputs kept, §8 Tiers, invariant 14 | A log segment and the chunks compacted from it hold the same positions in one epoch |
| Q-72 | §2.4, §8 Restore step 2, row 2c, invariant 15 | Whether the revised-key index is consulted when the current fact is off the machine |

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
(b) are settled (§8 Corrupt block, §11 Health); part (d) is Q-52. The same question applies to the set-aside range
each restore record reports (§8 Restore step 1), which is derived from the record at every start.
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

## Q-39 (e) — What ends a restore when the bucket has lost or corrupted a listed object
**Scenario.** Parts (a)–(d) are settled (§8 Restore, Failures and end). A restore record lists a file that the bucket
no longer holds, or whose plaintext hash never verifies. In step 1 the engine stays down with `restore_failed`, and
every restart fails the same way; after step 1 the file is retried with degraded health and the store stays
`restoring`, so no tree checkpoint is taken, with no exit. This is a loss in the deep tier, which §8 No deletion
("kept forever in at least the deep tier") and §8 Corrupt block ("the only single copies are …") assume cannot
happen, and a definition entry lost with the file is Q-33. A content conflict between two listed files likewise has
no exit without an operator.
**Model.** Not modelled; downloads do not fail. A conflicting file keeps the restore in progress
(`q39_conflicting_file_leaves_restore_in_progress`).
**Proposal (for the owner).** Treat it as §8 Corrupt block with deep as the failed tier: compact again from the other
listed files, and positions that no other listed file holds become a lost range, reported as `data_lost`. This needs
Corrupt block's "the only single copies are …" and No deletion's "kept forever in at least the deep tier" amended,
and Q-33 for a lost definition.

## Q-39 (f) — A crash while the restore record is written
**Scenario.** A restore claims generation g and crashes while writing its restore record locally, leaving a torn
record file. §8 Restore (Failures and end) says `restore` refuses a directory that holds files and no restore record,
and the paragraph after step 4 says a crash before the record starts the restore again with the next generation. It
is not said whether a torn record counts as no record (the directory then holds a file, so `restore` refuses it, and
a restart has no record to resume from) or as a record (which cannot be read).
**Model.** Not modelled; writing the record is atomic.
**Needed.** How a torn local restore record is recognised, and whether the restore then starts again with the next
generation or the directory is refused.

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

## Q-49 (b) — Conditions and live state with no local detail during a restore
**Scenario.** Part (a) is settled: a query whose detail is not local is flagged `approximate_edges` or is the error
`detail_not_local`, never empty (§6 Old spans, §8 Restore step 2). Invariant 16 and §4 Resync promise every
condition's present state after a restore, and two consumer-facing points remain. (i) A funnel with a rolling window
over a gauge with a 5-minute expected interval (no stored level), evaluated during step 2 before its detail is back,
has no local span, so its evaluation is that error: what the funnel delivers, and what its `resync` carries, is not
said. (ii) A state or text series with no staleness limit and no stored level, whose last fact predates the log
replayed from the checkpoint, has neither node nor fact: `resync` would report no live state for a valve that is
open.
**Model.** Not modelled; both levels are stored for every series.
**Proposal (for the owner).** For both, the state is unknown, not false or none: the evaluation is skipped and
counted, the condition keeps its state, `resync` says "unknown" for that funnel, and the first evaluation with detail
is delivered as a transition. This changes what `resync` guarantees (§4 Resync, invariant 16).

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
**Proposal.** (a) A node of any level is stored only for a span throughout which the expected interval in force
qualifies; a minute or quarter-hour in which a change crosses the threshold stores no node and is answered from its
facts (§5.1, §5.7 already say "throughout" for calendar nodes; row 20a for days). (b) A parameter entry with a past
effective time is applied to the tree like late data (§5.6): every stored node of the series from its effective time
on is recomputed from facts under the parameters now in force, removed where its span no longer qualifies and created
where it newly does, with results flagged `summary_stale` until that is complete and the tree file's checkpoint kept
below the entry's position; nodes over detail that is not local are flagged individually and recomputed when their
detail is fetched. A new row 20b and an exception in invariant 8 ("once no recompute is pending") would follow.
**Objections.** (1) While the recompute is pending only results are flagged, not nodes, so invariant 5 fails for nodes
still computed under the old parameters; invariant 8's "once no recompute is pending" conflicts with counting the
recompute complete when nodes over non-local detail are only flagged, and a row 20b "equal to a rebuild" fails for
them. (2) "Created where it newly does" is undefined over detail that is not local: a 120 s gauge corrected to 30 s
with an effective time six months back, its last year of detail in deep, cannot have its new 15-minute nodes built
without fetching. (3) Per-span density leaves holes: a quarter-hour with no node whose detail is in deep, between two
window edges whose detail is local, cannot be evaluated without off-machine data, while §6 Old spans only moves
edges. (4) Already so in the current text: §5.6's "every higher node is the left-to-right merge of its children" is
undefined for a quarter-hour whose 1-minute children are stored for only part of its span.

## Q-61 — Identity of a condition delivery, and where its sequence is kept
**Scenario.** (a) A threshold funnel is evaluated on a routed fact at position 0:7 (§6 Cadence) and fires. §4 Order
gives an identity only to fact deliveries (the position) and timer deliveries (the last committed position plus a
per-funnel sequence); `api.md` gives "position, or funnel and sequence for timer deliveries". Identified by 0:7, the
delivery would share its identity with the fact delivery at 0:7, and two funnels firing on one fact would share one.
(b) A timer delivery `(F, 5)` is taken but not confirmed, and the engine crashes. The latch (§6) holds the state
and position, not the sequence, and is never ahead of confirmation (invariant 13), so a sequence restored from it
starts again at 5: the next, different transition is `(F, 5)`, which a consumer de-duplicating by `(funnel,
sequence)` (§4 At-least-once) drops. Nothing else on disk is said to hold the sequence. (c) A store is restored
(§8): cursor files are not backed up, so nothing tells the new store which sequences were issued, and its first
transition is `(F, 1)` again, which a consumer that kept its de-duplication memory across the restore drops.
**Model.** Every condition delivery, on a routed fact or on the timer, is identified by `(generation, funnel,
sequence)`, the generation being the store's at issue, and carries the last committed position at evaluation. The
sequence is kept on disk when the delivery is issued, so it is never issued twice for a funnel within a generation
(`q61_condition_delivery_sequence_survives_a_crash`), and a restore, which starts a generation, starts the sequences
again without repeating an identity (`q61_condition_delivery_identity_includes_the_generation`).
**Needed.** The identity of a delivery from an evaluation on a fact, where the per-funnel sequence is persisted (with
the mark, in the cursor file at issue, or a sequence that a consumer forgets on `resync`), and how it continues after
a restore.

## Q-62 — Condition transitions while a consumer catches up after overflow
**Scenario.** A consumer has a fact funnel and a threshold funnel; its queue is full, so it is catching up from the
log (§4 Overflow). Meanwhile a routed fact makes the threshold fire. The transition is not in the log, so catch-up
cannot deliver it, the queue has no room, and overflow is not among the `resync` triggers (§4 Resync); delivered
later it would follow facts committed after it. Separately, catch-up reads "from after the last position already
enqueued"; if that position's segment has left the local tiers meanwhile (§8), §8 Behind consumers ("never downloads
from deep for catch-up") speaks only of a cursor.
**Model.** The condition keeps being evaluated on live facts and on the timer, but nothing is sent while the
consumer is catching up and the last emitted state is unchanged; on re-attaching to live the consumer receives the
present state as a delivery if it differs from the last state emitted to it
(`q62_transition_during_overflow_sent_on_reattach`). A transition that finds a live consumer's queue full switches it
to catch-up in the same way. Catch-up after overflow reads an entry wherever it is held.
**Needed.** What a consumer receives for transitions during catch-up after overflow, and what catch-up does when its
position is no longer local.

## Q-63 — Condition state now, for conditions with memory
**Scenario.** §6 Restart and resume rebuilds the operator "as of now" and compares "the condition state now" with the
latch. (a) A threshold with activation 50 and clear 40 whose value now is 45: its state depends on the last crossing,
which the value now does not show, and finding that crossing in the tree means replaying past evaluations, which
depend on the cadence and on data that arrived late; after a restore there is no latch either. (b) For an on-change
condition, "differs from the latch" is not said to use the dead-band. (c) A condition funnel added at run time has no
latch; it is not said whether it announces its present state.
**Model.** `Runner::resume`: a threshold is true above its activation level, false below its clear level and, between
them, keeps the latched state (false with no latch) (`q63_threshold_between_levels_resumes_from_the_latch`); an
on-change condition reports the value now, a transition if it moved beyond the dead-band from the latched value. A
funnel added at run time starts with a fresh condition and announces nothing until it is first evaluated; a removed
funnel's latch is dropped, and a latch belongs to one incarnation of a funnel id. Held-for is rebuilt from the tree as
of now (§6): true when the value was above throughout the last N, judged exactly on the facts and the values they hold
(`tests/rules.rs` `s6_held_for_true_while_down_delivered_in_resync`, `s6_held_for_rebuilt_exactly_from_the_facts`);
stale-for follows Q-16.
**Needed.** How a threshold's state between its levels is determined on resume, the dead-band rule for on-change, and
whether a new funnel announces its state.

## Q-64 — Routing changes before a restore's checkpoint
**Scenario.** A route is added at 0:2; a tree checkpoint is taken at 0:5 and the log before it is compacted. A restore
downloads the checkpoint, with the registry file, and the log segments after 0:5 (§8 Restore step 1), and accepts
ingest. Routing changes are log entries (§4 Routing), and the checkpoint is not said to carry them, so until the chunk
holding 0:2 is back (step 3) the route is unknown and new facts are not delivered by it.
**Model.** Routes are taken from the entries that are local; a route added before the checkpoint is not in force until
the file holding its entry is downloaded
(`q64_route_before_checkpoint_not_in_force_until_its_entry_is_back`).
**Needed.** Whether a tree checkpoint (or the registry file it carries) holds the routing table as of its position.

## Q-70 — Chunk indexes after a restore
**Scenario.** §5.6 relies on chunk indexes always being on hot, so the engine knows whether a minute has facts off the
machine. After a restore, step 3 leaves files that the restore record does not place on hot or cold in deep, and the
tree checkpoint carries no chunk indexes. Once `restoring` ends, a late fact for a minute whose chunk stayed in deep is
folded from the local facts only, giving a wrong node that is not flagged `summary_stale`; an identical repeat of a
fact in that chunk is recorded without being counted `duplicates_unchecked`; and §6 Old spans cannot tell that an
edge's detail is off the machine.
**Model.** Not modelled; a restore brings every file back.
**Needed.** Where chunk indexes come from after a restore (carried by the tree checkpoint, listed with the files in
the restore record, or downloaded in step 1 or step 3), and what holds until they are local.

## Q-71 — A log segment and the chunks compacted from it hold the same positions in one epoch
**Scenario.** Log segment L holds positions 1–10 and is sealed. Compaction writes a current chunk C and a revision chunk
R from it and publishes them in one record (§7). Log segments are never retired by compaction (§7) and stay on hot for
90 days (§8 Tiers, Eviction at 90 days), so the epoch of that record lists L, C and R, and position 3 is held by L and
by C or R. §7 Manifest and invariant 14 say that an epoch never lists two live files holding the same position. A
restore installs log segments and chunks compacted from them in the same way (§8 Restore step 3: "A position present
in two files is one fact").
**Model.** Invariant 14 is checked among compacted files (current and revision chunks) at every epoch; log segments
stay listed beside the chunks compacted from them. A sealed segment whose positions live chunks hold, or after a
restore the chunks it will bring back, is not compacted again (`q71_log_segment_and_its_chunks_listed_in_one_epoch`).
**Proposal.** §7 Manifest and invariant 14: "No manifest epoch lists two live compacted files holding the same
position. A log segment holds the same positions as the chunks compacted from it until it leaves (§8)."

## Q-72 — Whether the revised-key index is consulted when the current fact is off the machine
**Scenario.** (a) A series that is not versioned records A and then a correction B for one `(series, t)`; fill
pressure moves the detail holding both off every local tier. An adapter replays A. §2.4 says that if the current fact
for a key is on no local tier the fact is recorded and counted `duplicates_unchecked`, and that such a repeat "can
revert a correction that differs in value"; so A is recorded and becomes current. But the revised-key index, which
is hot and never moves, holds A's value and quality, so the check could run and drop A; and §8 Restore step 2 drops a
fact that matches an entry of the index, so the same replay is dropped during a restore, from the checkpoint's index.
(b) The versioned twin: A (`changed_at` 1), B (2), both off every local tier; a replay of A matches "a fact off every
local tier" by `changed_at`, so §2.4 records it unchecked, although the index holds it. Row 2c allows both outcomes.
**Model.** Outside a restore it follows §2.4 as written: (a) records A unchecked and A becomes current; (b) records A
unchecked, outranked, and B stays current. During a restore the replay is dropped
(`q72_replay_off_the_machine_recorded_unchecked_although_the_index_holds_it`,
`tests/rules.rs` `s8_checkpoint_carries_the_revised_key_index`).
**Needed.** Whether the check compares with the revised-key index whenever it holds the key, so that "on no local
tier" applies only to a key with a single fact, or whether the unchecked rule applies whatever the index holds.

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
  latest `t`, the revised-key index and the version index (§8 Continuous backup, Generations).
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
| Q-28 | Two restores from one bucket; a restore from an empty bucket | §8 Generations, §8 Restore drill, row 28b |
| Q-29 | The restored end, and what a continuing cursor receives | §8 Restore steps 1–2, §8 Continuous backup, invariants 4 and 9, row 28 (open parts: Q-39 (e) and (f)) |
| Q-30 | `duplicates_unchecked` during a restore compared a time with a position | §8 Restore step 2; versioned series (§2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 1, 2c and 2d), so that a replay never reverts a correction |
| Q-31 | Accepting ingest before the registry is back | §8 Continuous backup, §8 Restore step 1 |
| Q-32 | A crash or reboot during a restore | §8 Restore, row 28a (open parts: Q-39 (e) and (f)) |
| Q-34 | What `data_lost` and `possible_loss` mean beyond the report, parts (a) and (b) | §8 Corrupt block (only the damaged region is lost; the valid batches of a sealed segment are copied to a new segment by one manifest record, which ends the error), §11 Health (each report carries what is known of its range; no flag on query results), row 11 (open parts: Q-34 (c), Q-52) |
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (open parts: Q-49 (b), Q-60) |
| Q-37 | Which manifest records a restore reads, and how generations begin | §8 Generations (generation 0 counts as claimed; each generation above 0 begins with a self-contained restore record listing the files it restores from and carrying the earlier restored ends, uploaded before any other file of its generation, after which the claims are read again; a restore reads the highest generation below its own that has records, and keeps the store id), §8 Restore step 1 (a position set aside once stays so) and the paragraph after step 4, invariant 9, row 28b |
| Q-38 | Segments beyond the restored end | §8 Restore step 1 (positions set aside, a term distinct from Corrupt block's quarantine: excluded by position everywhere, also in reader processes, with no query error; a set-aside definition keeps its id reserved; the set-aside range reported in health from the record at every start; re-ingested only by the operator, through export and Re-ingest), §2.1 `id`, §11 Health, invariants 4 and 9, row 28, `api.md` Backup and restore (open part: Q-34 (c), for clearing the report) |
| Q-39 | Failures during a restore, and when it ends, parts (a)–(d) | §8 Restore, Failures and end (any failure before step 1 is complete is fatal and resumed at restart; later failures are retried with degraded health; the record precedes every download; `restoring` is derived and ends when every file step 3 brings back is installed), §8 Restore steps 1–4, §8 Continuous backup and Deep (each tree checkpoint carries the revised-key index), §2.4, §11 Health, §13, `api.md` (open parts: Q-39 (e) and (f)) |
| Q-40 | Ranking facts of versioned series during a restore | §2.4 Version index, §8 Continuous backup and Deep, §8 Restore steps 1–2 (each tree checkpoint carries the version index, and step 1's replay brings it to the restored end before ingest opens), §13 |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (b)–(d)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (open part: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (on `resync` consumers forget positions seen above their recovered cursor), §4 Resync (every consumer, after every §3 Recovery rule 3 recovery), §3 Recovery rule 3, row 7a; no margin of skipped positions (open parts: Q-46 to Q-48) |
| Q-44 | Gaps left by scheduled and forecast series, part (a): a silent feed | §2.1 `timing` (a silent forecast source is reported by stale-for on the observed series its adapter emits on each successful fetch), row 37b (open parts: Q-44 (b)–(d)) |
| Q-49 | What a series with no stored level serves during a restore, part (a): queries | §8 Restore step 2 and §6 Old spans (window edges move to where detail is local, the error `detail_not_local` only when no span is left, never an empty result; reads of facts fetch on demand; no priority download), row 28 (open part: Q-49 (b)) |
| Q-50 | Window edges over moved detail of a series with no stored 1-minute level | §6 Old spans (an edge whose detail is not local moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, `detail_not_local`; no fetch on the evaluation path), invariants 7, 17 and 22, `api.md` Reading |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4: identity by value and
adapter-supplied quality; a fact whose key's current fact is on no local tier, hot or cold, is recorded and counted
`duplicates_unchecked`; invariant 15, rows 2, 2a and 2b), versioned series (§2.1–§2.4: `changed_at` required on
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
a restore leaves out files retired by a record whose added files are in the bucket; invariant 14 (see Q-71), rows 10
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
flagged `no_value` and counted), counters and rate (§5.5, §6: resets, wraps and regressions, each regression
contributing 0 and counted; rate as the increase over the elapsed time between the readings that bound its steps,
empty with no step, flagged when a step spans more than S; row 18a, `tests/rules.rs` `s6_rate_…`), retirement at `t_r` (§2.1, also for a reading that waited in the queue while the
retirement committed), the durable mark and recovery by boot id (§3),
delivery (§4, §6: routing changes as log entries taking effect at their position, in live delivery and in catch-up;
condition funnels evaluated on routed facts and on the timer, never on replayed facts, their deliveries carrying the
last committed position and a per-funnel sequence; one latch per condition funnel, moved when the consumer confirms;
on every resume, after a restart, a recovery on a new boot or a consumer restart, a replay up to the last committed
position followed by `resync` with the live state and the condition state now, a transition where it differs from
the latch or there is no latch, or `resync` alone when the cursor is too far behind the local log segments; and
catch-up after overflow from the last position enqueued in the session; rows 14–16 and 34, invariants 2, 3, 11, 13
and 16; `tests/rules.rs` `s4_…` and `s6_…`), loss of a corrupt log segment with no other copy (§8; the whole range, see the table), and generations, tree checkpoints
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
| Record on change | §2.6, row 17a | Keep-alive facts are ordinary facts from the adapter | `tests/edge_cases.rs` `row_17a_…` |
| Effective-dated parameters and site zone | §2.1, §5.7, row 21 | One staleness limit per series; days computed per zone on demand | `calendar_depends_on_zone_utc_levels_do_not` |
| New-consumer start; cycles; name patterns | §4 New consumer, Routing, row 33 | Consumers are declared when the model is created and start before the first position; no consumer emits readings; a selection is a list of ids or every series | — |
| Funnels with an operator and no condition; conditions over a selection of several series; held-for over a windowed operator | §4 Funnel, §6 | Refused when routed (§6 Multi-series leaves composition to the consumer); held-for is modelled on the live-state value only, so that its resume can be rebuilt exactly from the facts | — |
| Timer cadence | §6 Cadence: every 1 s, configurable per funnel | Condition funnels are evaluated on the timer when `Model::tick` is called | — |
| Cursor file synced at most every 1 s | §4 Consumer state | The cursor and the latches are durable at once on confirmation | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled; replayed deliveries keep their original times | `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Back-pressure wait | §3, §11: the wait is measured | Waits are counted, not timed (the model is synchronous) | `tests/rules.rs` `s3_full_ingest_queue_waits`, `tests/edge_cases.rs` `row_13_…` |
| Live staleness for stale-for conditions | §2.5, §5.4: bad-quality facts do not refresh it (see Q-16) | Any routed reading refreshes it; a resume takes the time since the last reading's `received_at` | `tests/open_questions.rs` `q16_stale_for_true_now_reported_true_on_resync` |
| Clock jumps in health | §6 Windows, §11 | Not modelled | — |
| Condition state on resume | §6 Restart and resume, invariant 16 (see Q-63) | The engine resumes conditions with `Runner::resume`, from the value now, the history rebuilt from the tree (held-for, stale-for) and the latch; a standalone `Runner::rebuild` scans a history of values and counts a true condition as fired | `restart_rebuilds_threshold_without_firing_again`, `tests/open_questions.rs` `q63_…` |
| Cursor of a consumer after a restore | §4 Consumer state, invariant 13: a cursor is never ahead of the consumer's confirmation | A consumer without a kept cursor, or whose catch-up segments are not local, gets the restored end written as its cursor, which it never confirmed, standing in for "treated as beyond the restored end" (§8 Restore step 2) | `tests/rules.rs` `s8_cursor_catches_up_only_if_its_segments_are_local` |
| Tree files and their checkpoint, density rule | §5.1, row 20a | Tree rebuilt from facts at restart (tree checkpoints are taken only for backup, by an explicit call); both levels, and calendar nodes, stored for every series | — |
| Calendar results as windows | §5.7: a result is a window evaluation, also in a named zone; no stored node for a span off 15-minute boundaries or without a stored 15-minute level; rows 20a and 21a | Day node is the merge of quarter-hours; unaligned days refused | `day_node_omits_edge_stretches`, `unaligned_days_are_refused` |
| Compaction of compacted files | §7 Policy: merge when a series' chunks overlapping any 15-minute span exceed 4 | Counts and merges the live current chunks of a series on the fast disk only; revision chunks are never merged | `tests/rules.rs` `s7_merge_publishes_outputs_and_retires_inputs_in_one_record` |
| Revised-key index, recent window | §2.4, §5.1, §8 Restore step 2: "A key that gains a second fact meanwhile, in the replay or by ingest, has its index entry completed when its chunk is back" | A full in-memory history of every key stands in for the index and the recent window; while restoring, a key revised only after the checkpoint has its entry completed when every current chunk is back, not when its own chunk is | `tests/rules.rs` `s8_index_entry_completed_when_the_current_chunks_are_back` |
| Age of a compacted file | §8 Eviction at 90 days: no rule for the age of a merged or compacted file | A compacted file takes the seal time of its oldest input, so a revision made today, merged with a 60-day-old chunk, leaves hot after 30 days | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Clock of the eviction age | §8 Eviction at 90 days: no clock named | The engine clock `now`, as for the retention class, from the seal of a log segment, or of a compacted file's oldest input, so a merge does not reset it | `tests/rules.rs` `s8_merge_keeps_the_age_of_its_oldest_input` |
| Verification of compaction outputs | §7 Procedure: fact count, per-series checksum of facts, each chunk's summary equals the summary recomputed from its input facts | The outputs hold exactly the input entries; chunks carry no summary | — |
| A restore without the log segments after its checkpoint | §8 Restore step 1 | Where no backed-up log segment holds a position after the checkpoint, step 1 takes the first file in download order that holds it | — |
| Series timing: scheduled and forecast series, credit limit | §2.1, §2.2, §2.3, §4, §5.4, §6, invariants 18, 20 and 21 | Timing is recorded with the definition, and a forecast series must be versioned with `changed_at` kind `time`; otherwise every series is treated as observed (`future` and `late` flags, live state, staleness, delivery, cadence and the credit limit `now`) | — |
| Interval series | §2.1, §2.2, §5.2, §6, invariant 22 | Not modelled; readings are instants | — |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Compacting a corrupt file again from its kept inputs | §8 Corrupt block, row 11 | Quarantine needs a verified copy in another tier | — |
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery, §4 Resync, §11, rows 11 and 11a, invariant 10 | Only a torn last batch is modelled, so no consumer receives `resync` for a cursor or `start` position below or inside a lost range | — |
| A corrupt sealed log segment with no other copy loses only its damaged region | §8 Corrupt block, row 11: valid batches copied to a new segment by one manifest record, which ends the error | The whole segment's range is lost and the quarantine stays | `tests/rules.rs` `s8_corrupt_log_segment_with_no_other_copy_loses_range` |
| Timed mark sync; `possible_loss`; no reuse of positions after a truncation; forgetting positions above the recovered cursor | §3 Durable mark, §3 Recovery rule 3, §4 At-least-once, Resync, row 7a | The mark is always exact, so no delivered position is reused and no cursor is at or above a truncated tail; a truncation after a reboot is not reported. The `resync` owed to every consumer after a recovery under rule 3 is the one every resume sends, after the replay up to the last kept batch | `tests/rules.rs` `s4_resync_after_rule_3_recovery_before_later_positions` |
| Positions set aside beyond the restored end | §8 Restore step 1: excluded by position also in reader processes; a set-aside definition keeps its id reserved; the set-aside range reported in health from the restore record at every start; exported for re-ingest (`api.md` `export_set_aside`) | Downloaded in step 1, kept and excluded by position from views, trees, the registry and delivery in the writer; not reported; their series ids are not reserved; reader epochs (§7) still read them; no export | `tests/rules.rs` `s8_restored_end_before_first_gap`, `tests/open_questions.rs` `q38_…` |
| Restore record and the files a restore reads | §8 Generations, §8 Restore step 1: the restore record lists every file it restores from, set-aside segments included, each with its position range, and carries the earlier restored ends; it is uploaded before any other file of its generation, and the claims are then read again; a restore reads only the records of the highest generation below its own that has any, and a position set aside once stays so | The record is local and holds generation, checkpoint and restored end; a restore reads every file in the bucket whatever its generation, except files retired by a record whose added files are all in the bucket, and does not read the claims again. A second restore after a restore with a gap therefore finds the old gap again and sets aside the generation that restore started; under the specification its restored end is `1:1` and generation 1 is kept | `tests/open_questions.rs` `q37_second_restore_finds_the_old_gap_again` (expects the model's result) |
| A failed retry of a restore | §8 Restore, the paragraph after step 4: a restart after the record resumes the restore in its generation | In `restart_restore`, a retry after a crash before the record that writes its record and then fails in step 1 is discarded and stays pending, so the next restart claims another generation | `tests/open_questions.rs` `q39_failed_restart_of_a_restore_is_reported` |
| Failures after step 1 | §8 Restore, Failures and end: a file that cannot be fetched, that the bucket no longer holds or whose hash fails is retried with degraded health naming it; a content conflict is named in health with both files | Downloads do not fail; a conflicting file is not installed and the step returns `Conflict`, with no health reason | `tests/open_questions.rs` `q39_conflicting_file_leaves_restore_in_progress` |
| End of `restoring` | §8 Restore step 3 and Failures and end: only files the restore record places on hot (and cold where configured) are brought back, and `restoring` ends when they are installed; the label goes when the spans whose detail comes back are verified | Every file the restore restores from comes back, and `restoring` ends when all are local; the label goes when every span the checkpoint covers is verified | `tests/open_questions.rs` `q39_restoring_ends_when_every_file_is_local` |
| Query flags and fact reads | §11 Query; §8 Restore step 2 and Failures and end: every query result carries `restoring` while restoring, and fact reads fetch detail not yet local on demand | Only window results carry flags (`restoring`, `summary_stale`); while restoring, `current`, `facts` and `latest` return the local facts, unflagged, without fetching | — |
| Window edges over detail that is not local | §6 Old spans, §8 Restore step 2: an edge moves inward to a node boundary of the finest stored level, or with none to where its detail is local, flagged `approximate_edges`; with no span left, the error `detail_not_local`; no bound is refused for its resolution | No window is flagged `approximate_edges` and no edge moves; while restoring, a window bound inside a minute is refused with `Restoring` | `tests/open_questions.rs` `q39_no_sub_minute_query_or_checkpoint_while_restoring` |
| Replaying the log after a checkpoint | §8 Restore step 1: "replay them into the tree" | A minute for which the checkpoint holds a node and the replay (or new ingest) brings a fact, while the minute's earlier detail is not local, is recomputed from the local facts and flagged `summary_stale` until the detail is back (row 29); an engine that folds facts in `t` order into the checkpoint's node would hold it exact | `tests/rules.rs` `s8_replayed_minute_without_its_detail_is_stale` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap (the model records no lost ranges) | — |
