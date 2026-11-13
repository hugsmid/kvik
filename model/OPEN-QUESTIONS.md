# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-14 | §6 Edge stretches | State transition at a window's leading edge |
| Q-15 | §5.4, §6 | Rolling-window coverage after a forward jump of the system clock |
| Q-16 | §5.4, §4, §2.5 | Which commits refresh live staleness, and across a restart |
| Q-17 | §2.4, §2.5 | Duplicate identity of a reading recorded without a value |
| Q-18 | §6 Aggregates | Rate's elapsed time against a window's coverage |
| Q-19 | §7, §8 Corrupt block | A corrupt compacted file whose inputs are still local |
| Q-20 | §3 Recovery, invariant 10 | A corrupt block inside the open segment after a reboot |
| Q-21 | §7 | Which clock measures lease expiry |
| Q-22 | §2.5, §5.5, §12 D1 | Negative counter values if D1 changes |
| Q-23 | §5.1, §5.7 | Stored calendar nodes for days off 15-minute boundaries |
| Q-24 | §3 Write or sync failure, §3 Recovery, row 12 | A failed sync followed by a power loss |
| Q-25 | §3 Durable mark, §3 Recovery | When recovery writes the mark; a missing or invalid mark |

## Q-14 — Transitions at the leading edge
**Scenario.** State "idle" before a window and "running" at its first reading inside. §6 credits the counter step
to the window but says nothing about the state transition.
**Model.** The transition is credited to the window, like the step (`q14_leading_edge_state_transition`).
**Proposed.** "(counter: the step, state: the transition, to the first reading inside is credited to this window)".

## Q-15 — Window coverage after a forward clock jump
**Scenario.** Live staleness is measured on the monotonic clock (§5.4), so a forward jump of the system clock no
longer makes a series stale. The window clock `now` still follows the system clock forward at once (§6), and a
rolling window's trailing stretch ends at `last.t + S`. After a one-hour jump, a device-stamped series that reports
every second has no reading inside `[now − 5 min, now)`: its 5-minute window has coverage 0 and an empty average
while the series is live and not stale.
**Model.** As specified (`q15_forward_clock_jump_empties_rolling_window`).
**Proposed.** State whether rolling windows over the present count as "history" (event time, as now), or whether
their trailing stretch is measured like live staleness, on the monotonic clock since the last commit.

## Q-16 — Which commits refresh live staleness
**Scenario.** S = 3 minutes. A series has been silent for 10 minutes and is stale. A late fact for last week is then
committed. Read literally, "since its last commit of a fact that is not bad quality" makes the series fresh again,
although live state (the greatest `t ≤ now`) still shows the 10-minute-old reading. A future fact, by contrast, is
excluded until due (§4), but once due it is not said whether staleness counts from its commit or from the moment
it fell due. Two further gaps: "bad quality" may mean the adapter-supplied or the engine-assigned quality (§2.2), and the
monotonic clock restarts at boot, so after a restart there is no last-commit instant on it to measure from.
**Model.** Stale-for conditions measure monotonic time since the last arrival of any reading.
**Proposed.** "…since the commit of the fact that is the series' live state (§2.3), using the engine-assigned
quality. After a restart, every series is measured from the restart, as if its live-state fact had just been
committed."

## Q-17 — Identity of a reading recorded without a value
**Scenario.** An adapter sends NaN at `t`, then the same NaN again. NaN is recorded as bad quality with no value
(§2.5), and identity compares the value and the adapter-supplied quality (§2.4). The arriving value is NaN; the
recorded fact has no value. The same applies to a repeated negative counter value.
**Model.** The arriving reading is converted to "no value" first, so the repeat is a duplicate
(`q17_repeated_non_finite_reading`).
**Proposed.** "A reading that is recorded without a value is compared as 'no value', with its adapter-supplied
quality", or keep the supplied bits with the fact and compare those.

## Q-18 — Rate flagged in windows without a gap
**Scenario.** Counter readings every 60 s from 10:00:00, S = 180 s; window `[10:00:30, 10:05:10)`. The readings that
bound the increase are 10:00:00 (the step to 10:01:00 is credited to this window, §6) and 10:05:00, so the elapsed
time is 300 s. Coverage inside the window is 30 + 240 + 10 = 280 s, less than 300 s, so the rate is flagged although
nothing was missed. With no step in the window (no reading inside, or only one with no reading before it), no
readings bound the increase and the rate is undefined.
**Model.** Rate is increase ÷ covered seconds (`tests/model.rs` `rate_divides_by_covered_seconds`).
**Proposed.** "…flagged when a step it includes spans more than S", and "rate is empty when no step falls in the
window".

## Q-19 — A corrupt compacted file whose inputs are still local
**Scenario.** A compacted file is sealed and verified; before it is uploaded, a block in it fails its checksum.
Its input log segments are still on the hot tier (§7 keeps them until the file is verified elsewhere), but §8
Corrupt block only fetches a copy from cold or deep, so queries over the range return an error until the upload
that can no longer happen.
**Model.** Restore of a quarantined file needs a verified copy in another tier; there is no re-derivation.
**Proposed.** "If no other tier holds a copy, the file is compacted again from its kept inputs, verified, and
swapped in by a manifest record."

## Q-20 — A corrupt block inside the open segment after a reboot
**Scenario.** After a reboot (different boot id), recovery keeps blocks "up to the last complete commit block with
valid checksums" (§3). If a block in the middle of the open segment has decayed, every later batch, though synced,
acknowledged and possibly delivered, is dropped. Invariant 10 now says the only loss short of a disaster is a
corrupt sealed log segment with no other copy; the open segment is also a single copy.
**Model.** Recovery distinguishes only a torn last batch; mid-segment corruption is not modelled.
**Proposed.** Either list the open segment in invariant 10 and §8 as a single-copy window (and extend the mirror path,
now limited to sealed log segments, to the open segment), or keep every later batch whose own checksums are valid
and report the damaged range as fatal.

## Q-21 — Which clock measures lease expiry
**Scenario.** The retirement grace period is wall-clock time recorded in the manifest (§7). If a lease's expiry
("not refreshed for 1 hour") is also wall-clock time, a forward jump of the system clock of more than an hour
ends the grace period and expires every lease at once, and retired files are removed under a live reader.
**Model.** Leases are not modelled (see the table below).
**Proposed.** "A lease is refreshed and expired on the monotonic clock of the machine; leases are void after a
reboot."

## Q-22 — Negative counter values if D1 changes
**Scenario.** A negative counter value is recorded like NaN (§2.5), whose treatment is open decision D1. If D1 is
decided as "refused and counted", it is not stated whether negative counter values are refused too.
**Model.** Not modelled (see the table below).
**Proposed.** Name negative counter values in D1, or state that they are recorded as bad quality whatever D1 says.

## Q-23 — Stored calendar nodes for days off 15-minute boundaries
**Scenario.** §5.1 says calendar nodes are stored, assembled from 15-minute nodes. A day whose edges are not on
15-minute boundaries (a historical offset such as UTC−0:44:30) cannot be assembled that way. The result is a window
evaluation either way (§5.7); only the stored node is undefined.
**Model.** Such days are refused (`tests/model.rs` `unaligned_days_are_refused`).
**Proposed.** "No calendar node is stored for such a day; its result is evaluated from the 15-minute nodes inside it
and facts at its edges."

## Q-24 — A failed sync followed by a power loss
**Scenario.** A sync returns an error; the engine is read-only, and "recovery by restart … truncates to the mark"
(§3, row 12). Before the restart, the machine loses power. On the new boot, Recovery's different-boot rule keeps
"blocks up to the last complete commit block with valid checksums", which includes the batch whose sync failed if
it reached the disk whole. Recovery cannot know that the sync failed.
**Model.** The boot rule applies; the batch is kept and becomes durable (`q24_sync_error_then_power_loss_keeps_whole_batch`).
**Proposed.** "A batch whose sync failed may be kept by recovery after a reboot; it was never acknowledged, so
adapters may also replay it, which the duplicate rule absorbs." Or record the failure durably so recovery truncates.

## Q-25 — When recovery writes the mark
**Scenario.** The mark is updated "after each successful sync" (§3). After a reboot with nothing past the mark,
recovery syncs; if it did not also write the mark with the new boot id, a later process crash on the same boot
would find a different boot id in the mark and keep an unsynced batch, against row 7. Also unstated: what recovery
does when the mark file is missing or unreadable, and whether a failed recovery sync leaves the old mark.
**Model.** Recovery after a reboot always syncs and writes the mark with the current boot id; a failed recovery
sync leaves the old mark, so the next restart tries again; a missing mark is treated as a different boot
(`q25_recovery_stamps_mark_after_reboot`).
**Proposed.** "Recovery after a reboot ends by writing the mark with the current boot id. A missing or unreadable
mark is treated as a different boot."

## For the format and interface documents
The specification wins where the documents differ. These follow from it, for the checks of `docs/format.md` and
`docs/api.md`:
- **F-1.** `format.md` A.5: the fact row has one `quality` column, and a `no_value` row is forced to quality bad.
  Duplicate identity (§2.4) compares the adapter-supplied quality, which §2.2 keeps with the fact. Without it, the
  duplicate check and the revised-key index cannot be rebuilt from the log (invariants 8, 15).
- **F-2.** `api.md`: back-pressure is listed as a refusal; §3 says ingest waits. `try_emit` returning `Busy` fits.
- **F-3.** `api.md`: windowed results need a place for the rate flag (§6).

## Settled
| ID | Question | Settled in |
|---|---|---|
| Q-01 | Corruption before backup | §7 (inputs kept until verified), §8 Corrupt block, invariant 10 (gaps remain: Q-19, Q-20) |
| Q-02 | Catch-up position on overflow | §4 Overflow |
| Q-03 | Low mark and tiering | §3 Disk space |
| Q-04 | Day nodes and edge stretches | §5.7 |
| Q-05 | Rate across a gap | §6 Aggregates |
| Q-06 | Days outside 92–100 quarter-hours | §5.7 (stored nodes remain open: Q-23) |
| Q-07 | Forward clock jumps and staleness | §5.4 (window coverage remains open: Q-15) |
| Q-08 | Age of a delivery | §4 Replay |
| Q-09 | Quality and future stamping | §2.2, §2.4 |
| Q-10 | Coverage for text series | §5.2 |
| Q-11 | Counters and grace across reboots | §7, §11 (lease clock remains open: Q-21) |
| Q-12 | Invariant 9 and disaster loss | invariant 9 |
| Q-13 | Negative counter values | §2.5, §5.5 |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4), the late and future
flags (§2.2), invalid values (§2.5), retirement at `t_r` (§2.1), the durable mark and recovery by boot id (§3),
catch-up after overflow (§4), and loss of a corrupt log segment with no other copy (§8). It does not yet
implement:

| Rule | Specification | Model now | Test (`tests/model.rs` unless stated) |
|---|---|---|---|
| Generations | §2.2, §8: a restore starts a new generation | Positions continue after a restore | `restore_onto_empty_disk_current_first` |
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
| Live staleness for stale-for conditions | §2.5, §5.4: bad-quality facts do not refresh it | Any reading passed to the condition refreshes it | — |
| Latch and `resync` | §4, §6, rows 15–16, invariant 16 | Operator rebuilt from the tree; a true condition counts as fired | `restart_rebuilds_threshold_without_firing_again`, `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Timer deliveries with per-funnel sequence | §4 | Conditions are tested apart from delivery | — |
| Tree checkpoints, density rule | §5.1 | Tree rebuilt from facts at restart; both levels stored for every series | — |
| Calendar results as windows | §5.7 | Day node is the merge of quarter-hours; unaligned days refused | `day_node_omits_edge_stretches`, `unaligned_days_are_refused` |
| Rate | §6: increase ÷ elapsed time, flagged | Increase ÷ covered seconds | `rate_divides_by_covered_seconds` |
| Atomic manifest records | §7, row 10, invariant 14 | Publish and retirement are two records | `compaction_crash_after_publish_resumes_retirement` |
| Inputs kept until verified; log segments kept 90 days | §3, §7, §8 | Compaction retires its input segments | `compaction_crash_after_publish_resumes_retirement` |
| Reader leases; void after reboot | §7, row 26 | Files leave hot after the grace period; old epochs read remotely | `reader_epoch_survives_grace_then_reads_remotely` |
| Revised-key index, recent window | §2.4, §5.1 | A full in-memory history | — |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Restore from a checkpoint | §8 Restore | Restore downloads files and rebuilds | `restore_onto_empty_disk_current_first` |
| Invariants 12–16 | §10 | Not checked yet | — |
