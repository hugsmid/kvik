# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-33 | §3 Recovery, §2.1, invariants 1 and 8 | Registry entries and derived files after a lost range |
| Q-34 | §3, §8 Corrupt block, §11 | What `data_lost` and `possible_loss` mean beyond the report |
| Q-37 | §8 Generations, §8 Restore, §2.2 | Which manifest records a restore reads, and how generations begin |
| Q-38 | §8 Restore step 1, §2.1, §7 | Segments beyond the restored end |
| Q-39 | §8 Restore steps 2–4, §8 Deep | Failures during a restore, and when it ends |
| Q-40 | §2.4 Version index, §8 Restore | Ranking facts of versioned series during a restore |
| Q-44 (d) | §2.3 Views, §2.1 timing | Forecast accuracy, and what was known as of an issue time (for the owner) |
| Q-45 | §2.1 interval, §5.7 | Intervals of varying length (for the owner) |
| Q-46 | §3 Recovery rule 3, §7 Readers | Positions kept by a reader process, after a truncated tail |
| Q-47 | §4 At-least-once, Resync, §11 Consumers | How a consumer learns its recovered cursor, and what forgetting means |
| Q-48 | §3 Recovery rules 1 and 3, §4 Resync, §6 Restart and resume | The resync owed after a truncated tail is not durable |
| Q-49 | §8 Restore step 2, §5.1 density rule | What a series with no stored level serves during a restore |
| Q-50 | §6 Old spans, §5.1 density rule, §12 D7 | Window edges over moved detail of a series with no stored 1-minute level |

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

## Q-34 — What `data_lost` means beyond the report
**Scenario.** (a) §8 Corrupt block says queries over a quarantined range return an error "until then", but a sealed
log segment with no other copy is never replaced, so the error never ends; its valid batches stay unreadable. (b) A
fact in a lost range may have superseded an earlier one, which then quietly becomes current again; current-view
results carry no flag. (c) It is not said whether `data_lost` clears, and how. (d) Manifest records are single
copies until uploaded, but neither §8 Corrupt block nor invariant 10 lists them.
**Model.** A corrupt sealed segment with no other copy loses its whole range (`tests/rules.rs`
`s8_corrupt_log_segment_with_no_other_copy_loses_range`).
**Needed.** Whether valid batches of such a segment are kept and the quarantine lifted, how results over or after a
lost range are flagged, and whether the state is latched until acknowledged.

## Q-37 — Which manifest records a restore reads, and how generations begin
**Scenario.** (a) A restore claims generation g and crashes before writing its restore record. The next restore
claims g + 1 and reads the records of g, the highest claimed before its own, which has none. (b) Records are keyed by
generation, so a second restore reads only the first restore's generation and not the files of the generations before
it. (c) The first store's generation (0) is never claimed, so "one above the highest claimed" can equal it while that
store is still writing, and two facts get the same position.
**Model.** A restore claims one above the highest generation claimed or present in the bucket, so it never takes
generation 0, and reads every file in the bucket whatever its generation; there is no restore record. So a second
restore finds the first restore's gap again: with `0:1`–`0:5`, `0:7`, `1:1` and `1:2` backed up, its restored end is
`0:5` and all of generation 1 is set aside.
**Needed.** Which records a restore reads, how a restore record carries forward the files it restored, and whether a
new store claims its generation.

## Q-38 — Segments beyond the restored end
**Scenario.** Segments beyond the first gap are kept but quarantined. (a) A definition in them is outside the restored
registry, which may then assign that id again, against "never reused" (§2.1). (b) Nothing keeps them out of
compaction (§7) or out of step 3's "log segments", where they would re-enter the views. (c) "Quarantined" means
"queries return an error" in §8 Corrupt block, and it is not said who triggers their re-ingest.
**Model.** Positions of older generations beyond the restored end are kept and excluded by position from views,
trees, the registry and delivery, wherever compaction moves them. They are not reported, their series ids are not
reserved (so (a) applies), reader epochs still read them, and re-ingest is not modelled. A drill sets aside what
the restore it rehearses would set aside.
**Needed.** How these segments are held, named and reported, which ids they reserve, and how re-ingest is started.

## Q-39 — Failures during a restore, and when it ends
**Scenario.** (a) After ingest has opened, a segment at or below the restored end fails to download or fails its hash
check. (b) "A crash before the record starts the restore again" assumes an empty disk, but files may already be local.
(c) The end of `restoring` is not defined (step 3 complete, or step 4's label removed). (d) Step 3 downloads the
revised-key index, which the deep tier does not list.
**Model.** Downloads do not fail; a restore completes in one step.
**Needed.** What each failure does, what a restart clears, and when the store stops being flagged `restoring`.

## Q-40 — Ranking facts of versioned series during a restore
**Scenario.** Until the version index is back during a restore, the engine cannot tell whether an arriving fact on a
versioned series outranks the facts already recorded for its key. A rule that holds such facts as "undetermined"
until a download finishes was withdrawn: its effect would depend on download timing, not on the log (principle 2),
and live state and windows would differ from a rebuild meanwhile (invariants 7 and 8). The version index is also not
yet listed in the deep tier.
**Model.** Not modelled.
**Proposed.** Each tree checkpoint carries the version index as of its position, like the registry and latest `t`
(§8), so step 1's replay brings it up to the restored end and the rank is always known.

## Q-44 (d) — Forecast accuracy, and what was known as of an issue time
**Scenario.** How forecast accuracy is served: comparing each issue with the outcome series, and asking what was
known as of an issue time rather than as of a position. Parts (a)–(c), on gaps left by scheduled and forecast
series, are settled (see Settled).
**Model.** Not modelled.
**Proposal (for the owner).** Not served by the engine in v1. History carries every issue with its `changed_at`, so
a reader can compute "known as of issue X" (per key, the fact of highest rank with `changed_at ≤ X`) from
`View::History` and compare it with the outcome series. A view by `changed_at`, or forecast-accuracy operators, is
new scope. If confirmed, `api.md` Not in v1 gains "a view as of an issue time (`changed_at`), and forecast-accuracy
operators (computed by the reader from `View::History`)".

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

## Q-47 — How a consumer learns its recovered cursor, and what forgetting means
**Scenario.** A consumer confirms position p; its cursor file, synced at most every second (§4 Consumer state), still
names an earlier position when power is lost. The tail holding p is truncated and p is assigned to a new fact
(§3 Recovery rule 3). On `resync` the consumer must forget the positions it has seen above its recovered cursor
(§4 At-least-once), p included, but nothing tells it which cursor was recovered; the `cursor` event in §11 is not
defined. Separately, "forget" is undefined for a consumer that keeps state outside the engine keyed by position:
positions in the truncated tail may be the only copy of acknowledged facts that are now lost, and with an upsert
keyed by position the new fact at p overwrites the lost one (see Q-34).
**Model.** Not modelled; the mark is always exact.
**Proposed.** `resync` after a truncated tail carries the consumer's recovered cursor. Alternatively, the engine
delivers one value to every consumer: the first new position after recovery, since only positions at or above it
(above the last kept batch, the synced cursors and the manifest records) can be reused; this is tighter than each
consumer's recovered cursor and also serves readers (Q-46). What a consumer with outside state does with the lost
facts is not proposed.

## Q-48 — The resync owed after a truncated tail is not durable
**Scenario.** Recovery under §3 Recovery rule 3 resumes, and a new fact commits at a reused position p. The process
crashes on the same boot before consumer C has received its `resync`. On restart rule 1 applies, so no `resync` for
a truncated tail is sent, and §6 Restart and resume delivers the facts after C's cursor, p included, before its
`resync`; C may drop the new fact at p as a repeat.
**Model.** Not modelled; the mark is always exact.
**Proposed.** Persist a "resync owed" flag, or a reuse floor, with the mark until each consumer has confirmed its
`resync`.

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
  tests need the stale-mark case. Whether `possible_loss` is latched until acknowledged is part of Q-34.
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
| Q-16 | Which commits refresh live staleness, when a fact falls due, and across a restart | §5.4 (the refresh instant of the live-state key; nothing else refreshes it; seeded from `received_at` and `t` after a restart), rows 3, 17b and 17c, invariant 23 |
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
| Q-35 | Loss above a stale mark after a power loss is not reported | §3 Durable mark (timed sync), §3 Recovery rule 3 (`possible_loss`, no reuse of positions), §11 Health, rows 7 and 7a, invariant 10 |
| Q-36 | Calendar nodes for a series without a stored 15-minute level | §5.1 (calendar row and density rule), §5.4, §5.7: no calendar node without a stored 15-minute level, the result being a window over facts; row 20a, §13 (open parts: Q-49, Q-50) |
| Q-41 | Forecasts: values issued now about future times | §2.1 `timing`, §2.2 flags, §2.3 Live state, §4 Future facts and Timers, §5.4, §6 Windows, Evaluation and Edge stretches (credit limit), invariants 18, 20 and 21, rows 4 and 36–39, §13 (open part: Q-44 (d)) |
| Q-42 | Readings that cover an interval, not an instant | §2.1 `kind` `interval`, `interval length` and `phase`; §2.2; §5.2; §5.4; §6 Aggregates (`apportioned`); invariants 18 and 22; rows 40–42 (open part: Q-45) |
| Q-43 | Positions delivered but not confirmed, after a truncated tail | §4 At-least-once (on `resync` consumers forget positions seen above their recovered cursor), §4 Resync (every consumer, after every §3 Recovery rule 3 recovery), §3 Recovery rule 3, row 7a; no margin of skipped positions (open parts: Q-46 to Q-48) |
| Q-44 | Gaps left by scheduled and forecast series, parts (a)–(c) | §2.1 `timing` (no horizon and no inferred withdrawal; a source withdraws a value by a bad-quality reading; a silent source is reported by stale-for on the adapter's companion series), §11 Health and `api.md` Health and statistics (each series' latest recorded `t`), rows 37a, 37b and 38a (open part: Q-44 (d)) |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4), the late and future
flags (§2.2), invalid values (§2.5), retirement at `t_r` (§2.1), the durable mark and recovery by boot id (§3),
catch-up after overflow (§4), loss of a corrupt log segment with no other copy (§8), and generations (§2.2, §8
Generations, Restore drill, Restore steps 1–2, row 28b): a restore claims the generation one above the highest
claimed or present in the bucket by a create-if-absent marker, taking the next if the marker exists, and starts it at
sequence 1; a bucket with nothing backed up is refused; the restored end is the last position before the first gap,
and later positions of older generations are set aside, outside views, trees, the registry and delivery; a consumer
whose kept cursor is at or below the restored end catches up to it, flagged `replayed` (whether or not its segments
are local; see the table), and every consumer receives `generation_changed` and `resync`; a drill is read-only,
claims nothing and raises no health condition of its own; a writer that finds a higher generation claimed stops
uploading and stays read-only across restarts, with fatal health (`tests/rules.rs` `s8_…`, `tests/invariants.rs`
`inv01_only_restore_starts_generation`). It does not yet implement:

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
| Live staleness for stale-for conditions | §2.5, §5.4, rows 17b and 17c, invariant 23: only the refresh instant of the live-state key counts, so late, same-`t`, bad-quality, outranked and not-yet-due facts refresh nothing; after a restart the time passed is `now − max(received_at, t)` and the latch holds until it is taken | Any reading passed to the condition refreshes it; a rebuilt condition counts from the restart and starts unfired | `tests/edge_cases.rs` `row_17_series_goes_silent` |
| Clock jumps in health | §6 Windows, §11 | Not modelled | — |
| Latch and `resync` | §4, §6, rows 15–16, invariant 16 | Operator rebuilt from the tree; a true condition counts as fired | `restart_rebuilds_threshold_without_firing_again`, `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Timer deliveries with per-funnel sequence | §4 | Conditions are tested apart from delivery | — |
| Tree checkpoints, density rule | §5.1, row 20a | Tree rebuilt from facts at restart; both levels, and calendar nodes, stored for every series | — |
| Calendar results as windows | §5.7: a result is a window evaluation, also in a named zone; no stored node for a span off 15-minute boundaries or without a stored 15-minute level; rows 20a and 21a | Day node is the merge of quarter-hours; unaligned days refused | `day_node_omits_edge_stretches`, `unaligned_days_are_refused` |
| Rate | §6: increase ÷ elapsed time; flagged when a step spans more than S; empty with no step | Increase ÷ covered seconds | `rate_divides_by_covered_seconds` |
| Atomic manifest records | §7, row 10, invariant 14 | Publish and retirement are two records | `compaction_crash_after_publish_resumes_retirement` |
| Inputs kept until verified; log segments kept 90 days | §3, §7, §8 | Compaction retires its input segments | `compaction_crash_after_publish_resumes_retirement` |
| Reader leases: refresh counter and boot id, expiry and grace on the writer's monotonic clock | §7, row 26 | Files leave hot after the grace period; old epochs read remotely | `reader_epoch_survives_grace_then_reads_remotely` |
| Revised-key index, recent window | §2.4, §5.1 | A full in-memory history | — |
| Retention class | §2.1, §8 Retention class | Not modelled; detail moves only under the model's tier rules | — |
| Series timing: scheduled and forecast series, credit limit, withdrawal | §2.1, §2.2, §2.3, §4, §5.4, §6, invariants 18, 20 and 21, rows 37a and 38a | Not modelled; every series is observed | — |
| Interval series | §2.1, §2.2, §5.2, §6, invariant 22 | Not modelled; readings are instants | — |
| Versioned series | §2.1–§2.4, §4, §5.6, §6 Cadence, §7, §11, invariants 4, 15 and 19, rows 2c and 2d | Not modelled; every series ranks by position | — |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Compacting a corrupt file again from its kept inputs | §8 Corrupt block, row 11 | Quarantine needs a verified copy in another tier | — |
| Corruption inside the open segment; lost ranges, `data_lost` | §3 Recovery, §11, rows 11 and 11a, invariant 10 | Only a torn last batch is modelled | — |
| Timed mark sync; `possible_loss`; no reuse of positions after a truncation; `resync` to every consumer after it | §3 Durable mark, §3 Recovery rule 3, §4 Resync, row 7a | The mark is always exact; a truncation after a reboot is not reported | — |
| Restore record and resuming a restore | §8 Generations, §8 Restore step 1 and the paragraph after step 4, row 28a (see Q-32, Q-37, Q-39) | No restore record; manifest records are not keyed by generation, and a restore reads every file in the bucket whatever its generation, so a second restore finds the first restore's gap again and sets aside the generation that restore started (see Q-37); a restore completes in one call, so a restart has nothing to resume | — |
| Restore from a checkpoint, step by step | §8 Continuous backup, §8 Restore steps 1–4, row 28: tree checkpoint with registry and latest `t`; queries at ≥ 1 minute flagged `restoring`; detail back-filled and compared with the checkpoint | No checkpoints; restore downloads files (current chunks first, optionally) and rebuilds; no `restoring` flag | `restore_onto_empty_disk_current_first` |
| Segments beyond the restored end | §8 Restore step 1: quarantined, reported in health like a lost range, available for re-ingest (see Q-38) | Kept and excluded by position from views, trees, the registry and delivery; not reported; their series ids are not reserved; reader epochs (§7) still read them | `tests/rules.rs` `s8_restored_end_before_first_gap` |
| Lost ranges and the restored end | §8 Restore step 1: a recorded lost range is not a gap | Every missing position is a gap (the model records no lost ranges) | — |
| Consumers after a restore | §8 Restore step 2: catch-up to the restored end only "if their segments are local; otherwise §4 Resync applies"; every consumer "then" receives `generation_changed` and `resync` | The local-segments condition is not yet modelled: catch-up replays whatever the restore brought back, so with `current_only` it replays from current chunks only. Signals and fact deliveries are kept in separate lists (`signals`, `observed`), so the model cannot show that `generation_changed` follows the catch-up | `tests/rules.rs` `s8_cursor_at_or_below_restored_end_continues` |
| `duplicates_unchecked` during a restore | §2.4, §8 Restore step 2 | Not modelled: all detail is back before ingest is accepted, so the duplicate check always runs | — |
| Invariants 12–16 | §10 | Not checked yet | — |
