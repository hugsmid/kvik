# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated. Identifiers are never reused.

| ID | Section | Question |
|---|---|---|
| Q-14 | §6 Edge stretches | State transition at a window's leading edge |
| Q-15 | §5.4, §6 | Rolling-window coverage after a forward jump of the system clock |
| Q-16 | §5.4, §4 | Which commits refresh live staleness |
| Q-17 | §2.4, §2.5 | Duplicate identity of a reading recorded without a value |
| Q-18 | §6 Aggregates | Rate's elapsed time against a window's coverage |

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
**Model.** As specified (`q15_forward_clock_jump_reduces_window_coverage`).
**Proposed.** State whether rolling windows over the present count as "history" (event time, as now), or whether
their trailing stretch is measured like live staleness, on the monotonic clock since the last commit.

## Q-16 — Which commits refresh live staleness
**Scenario.** S = 3 minutes. A series has been silent for 10 minutes and is stale. A late fact for last week is then
committed. Read literally, "since its last commit of a good fact" makes the series fresh again, although live state
(the greatest `t ≤ now`) still shows the 10-minute-old reading. Likewise, §4 says a future fact is ignored by
staleness until it is due, yet its commit is the series' last commit.
**Model.** Stale-for conditions measure monotonic time since the last arrival of any reading.
**Proposed.** "…since the commit of the fact that is the series' live state (§2.3)", so late and future facts do not
refresh it.

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

## Settled
| ID | Question | Settled in |
|---|---|---|
| Q-01 | Corruption before backup | §7 (inputs kept until verified), §8 Corrupt block, invariant 10 |
| Q-02 | Catch-up position on overflow | §4 Overflow |
| Q-03 | Low mark and tiering | §3 Disk space |
| Q-04 | Day nodes and edge stretches | §5.7 |
| Q-05 | Rate across a gap | §6 Aggregates |
| Q-06 | Days outside 92–100 quarter-hours | §5.7 |
| Q-07 | Forward clock jumps and staleness | §5.4 (window coverage remains open: Q-15) |
| Q-08 | Age of a delivery | §4 Replay |
| Q-09 | Quality and future stamping | §2.2, §2.4 |
| Q-10 | Coverage for text series | §5.2 |
| Q-11 | Counters and grace across reboots | §7, §11 |
| Q-12 | Invariant 9 and disaster loss | invariant 9 |
| Q-13 | Negative counter values | §2.5, §5.5 |

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4), the late and future
flags (§2.2), invalid values (§2.5), retirement at `t_r` (§2.1), recovery after a process crash or a power loss
(§3), catch-up after overflow (§4), and loss of a corrupt log segment with no other copy (§8). It does not yet
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
| Durable mark file | §3 | Process crash and power loss are distinguished directly | `tests/edge_cases.rs` `row_07_…`, `row_07a_…` |
| Low disk mark | §3: compaction pauses; tiering continues | Tiering pauses too | `low_mark_pauses_tiering_too` |
| Routing changes as log entries; new-consumer start; cycles | §4, rows 33–34 | Fixed selections from configuration | — |
| Age of a replayed delivery | §4 Replay: from `received_at` | Not modelled | — |
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
| Invariants 11–16 | §10 | Not checked yet | — |
