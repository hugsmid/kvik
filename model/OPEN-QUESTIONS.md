# Open questions

Questions in [`docs/spec.md`](../docs/spec.md) found by building the reference model, and the places where the
model does not yet follow the specification. Section numbers refer to `docs/spec.md`. Each question gives a
concrete scenario, what the model does for now, and proposed wording. Where a test pins the model's behaviour, it
is named; tests are in `tests/open_questions.rs` unless stated.

| ID | Severity | Section | Question |
|---|---|---|---|
| Q-01 | High | §7, §8, §10.10 | A sealed file corrupted before any other tier holds a copy is lost |
| Q-02 | Medium | §4 Overflow, §10.3 | Catch-up "from its cursor" re-reads positions already queued |
| Q-03 | Medium | §3 Disk space, §8 | The low mark pauses tiering, the operation that frees space |
| Q-04 | Medium | §5.2, §5.7, §6 | Stored day nodes omit the stretches at the day's edges |
| Q-05 | Medium | §5.5, §6 | Rate divides an uncapped step by capped coverage |
| Q-06 | Low | §5.7 | Days with no quarter-hours, or not on 15-minute boundaries |
| Q-07 | Low | §5.4, §6 | A forward jump of the system clock makes healthy series stale |
| Q-08 | Low | §4 Replay | What "older than 60 s" is measured from |
| Q-09 | Low | §2.2, §2.4 | Is quality compared before or after future stamping? |
| Q-10 | Low | §5.2 | Coverage for text series |
| Q-11 | Low | §1.5, §7 | Durability of counters; the grace period across reboots |
| Q-12 | Low | §8, §10.9 | Invariant 9 and facts lost in a disaster |
| Q-13 | Low | §5.5 | Negative counter values and the reset ceiling |
| Q-14 | Low | §6 Edge stretches | State transition at a window's leading edge |

## Q-01 — Corruption before backup (High)
**Scenario.** A segment is sealed; before its upload completes, a block in it fails its checksum. §8 says to fetch
the copy from cold, else deep. Neither has it, so its facts are lost, against §10.10 and principle 1.4.
**Model.** Facts lost (`q01_corrupt_before_backup_loses_facts`).
**Proposed.** "Until a sealed file is verified in another tier, it is not the only copy: the data is also kept in
the log, or a second local copy is written to a different device. A corrupt file with no verified copy is
reported as data loss with the affected positions."

## Q-02 — Catch-up position on overflow (Medium)
**Scenario.** Queue capacity 2; positions 2 and 3 are queued but not confirmed (cursor 0); position 4 overflows.
Reading "from its cursor" re-delivers 2 and 3 after the consumer has taken them: 2, 3, 2, 3, 4.
**Model.** Catch-up starts at the first position not queued; after a restart, at cursor + 1
(`q02_overflow_catch_up_position`).
**Proposed.** "…reads the log from the first position not yet queued for it (after a restart: from its cursor)."

## Q-03 — Low mark and tiering (Medium)
**Scenario.** Free space falls below the low mark; compaction and tiering pause. Moving verified files out of the
hot tier is what frees space, so the store fills to the critical mark although it need not.
**Model.** Pauses both (`q03_low_mark_pauses_tiering`).
**Proposed.** "Below the low mark, compaction pauses; removal of files already verified in another tier continues
and is prioritised."

## Q-04 — Day nodes and edge stretches (Medium)
**Scenario.** Nothing is held past a node's last reading, and a day node is the merge of its quarter-hours. With
readings an hour before midnight, an hour after, and an hour before the next midnight, the day node covers 22
hours while the window over the same day covers 24.
**Model.** As specified; the two disagree (`q04_day_node_differs_from_day_window`).
**Proposed.** "Calendar results are window evaluations over the day's span, edge stretches included; stored day
nodes are an input to that evaluation, not the result."

## Q-05 — Rate across a gap (Medium)
**Scenario.** Counter readings 100 and 250 an hour apart, S = 3 minutes. The step (150) is never capped, but
coverage is about 181 s, so the rate is about 0.83 per second against a true average of about 0.04.
**Model.** As specified (`q05_rate_across_gap`).
**Proposed.** "rate = increase ÷ window seconds", or "rate is empty when a step spans a gap longer than S", and
flag such windows.

## Q-06 — Days outside 92–100 quarter-hours (Low)
**Scenario.** `Pacific/Apia` skipped 2011-12-30 (0 quarter-hours). `Africa/Monrovia` used UTC−0:44:30 until 1972,
so its midnights are not on 15-minute boundaries and its days cannot be built from 15-minute nodes.
**Model.** An empty day; an error for unaligned days (`q06_days_outside_the_rule`).
**Proposed.** "A skipped date is an empty day. A zone whose offset is not a multiple of 15 minutes at the time is
not supported for calendar levels and is refused with an error."

## Q-07 — Forward clock jumps (Low)
**Scenario.** The window clock is `max(system clock, previous value)`. A forward correction of an hour moves it an
hour ahead at once: every series looks stale and coverage drops, until real time catches up.
**Model.** As specified (`q07_forward_clock_jump_makes_series_stale`).
**Proposed.** State whether this is accepted, or advance the window clock only by the monotonic clock once the
clock is synchronised.

## Q-08 — Age of a delivery (Low)
**Scenario.** A consumer that acts on devices must not act on deliveries "older than 60 s". A late fact delivered
live has an old `t` and a fresh `received_at`. Which one counts?
**Model.** Not modelled; both times are on every delivery.
**Proposed.** "…whose `received_at` is more than 60 s before now, or whose `t` is more than 60 s before now."

## Q-09 — Quality and future stamping (Low)
**Scenario.** A reading stamped two minutes ahead is stored as suspect. Resent as good once it is due, is it a
duplicate?
**Model.** Quality is compared after stamping, so it is a new fact (`q09_quality_compared_after_stamping`).
**Proposed.** "The duplicate check compares quality after stamping."

## Q-10 — Text coverage (Low)
**Scenario.** §5.2's "all" row includes coverage; the text row says "count, first, last only".
**Model.** Text nodes carry coverage (`q10_text_coverage`).
**Proposed.** "text: count, first, last and coverage."

## Q-11 — Counters and grace across reboots (Low)
**Scenario.** Are `duplicates_dropped` and the other counters durable across restarts? The retirement grace period
is a timer, but monotonic clocks restart at boot.
**Model.** Counters survive restarts (`q11_counters_across_restart`); grace is measured in wall time.
**Proposed.** State both.

## Q-12 — Invariant 9 and disaster loss (Low)
**Scenario.** §8 accepts losing facts committed after the last uploaded sealed file; §10.9 says restore preserves
the set of positions.
**Model.** A restore loses them (`q12_restore_loses_unsealed_facts`).
**Proposed.** "…preserve the set of positions held in sealed, backed-up files."

## Q-13 — Negative counters (Low)
**Scenario.** An `i64` counter moves from −10 to −20. `n < reset_ceiling × p` holds (−20 < −1), so an ordinary
decrease counts as a reset.
**Model.** As specified (`q13_negative_counter_values`).
**Proposed.** "Counter values are non-negative", or apply the rule to magnitudes.

## Q-14 — Transitions at the leading edge (Low)
**Scenario.** State "idle" before a window and "running" at its first reading inside. §6 credits the counter step
to the window but says nothing about the state transition.
**Model.** The transition is credited to the window, like the step (`q14_leading_edge_state_transition`).
**Proposed.** "(counter: the step, state: the transition, to the first reading inside is credited to this window)".

## Where the model differs from the specification
The model implements the summary semantics (§5, §6), the duplicate rule with `revise` (§2.4), the late and future
flags (§2.2), invalid values (§2.5), retirement at `t_r` (§2.1), and recovery after a process crash or a power
loss (§3). It does not yet implement:

| Rule | Specification | Model now | Test |
|---|---|---|---|
| Generations | §2.2, §8: a restore starts a new generation | Positions continue after a restore | `tests/model.rs` `restore_onto_empty_disk_current_first` |
| Commit time and as-of time | §2.2, §2.3 | As-of by `received_at` | — |
| `t_source`, `host_time`, `unsynced_clock` | §2.2, §2.5, row 22a | Not modelled | — |
| Record on change | §2.6, row 17a | Keep-alive facts are ordinary facts from the adapter | `row_17a_…` |
| Effective-dated parameters and site zone | §2.1, §5.7, row 21 | One staleness limit per series; days computed per zone on demand | `calendar_depends_on_zone_utc_levels_do_not` |
| Durable mark file | §3 | Process crash and power loss are distinguished directly | `row_07_…`, `row_07a_…` |
| Routing changes as log entries; new-consumer start; cycles | §4, rows 33–34 | Fixed selections from configuration | — |
| Latch and `resync` | §4, §6, rows 15–16, invariant 16 | Operator rebuilt from the tree; a true condition counts as fired | `restart_rebuilds_threshold_without_firing_again`, `consumer_down_for_a_day_resumes_from_cursor_replayed` |
| Timer deliveries with per-funnel sequence | §4 | Conditions are tested apart from delivery | — |
| Tree checkpoints, density rule | §5.1 | Tree rebuilt from facts at restart; both levels stored for every series | — |
| Atomic manifest records | §7, row 10, invariant 14 | Publish and retirement are two records | `compaction_crash_after_publish_resumes_retirement` |
| Reader leases | §7, row 26 | Files leave hot after the grace period; old epochs read remotely | `reader_epoch_survives_grace_then_reads_remotely` |
| Log segments kept 90 days | §3, §7, §8 | Compaction retires its input segments | `compaction_crash_after_publish_resumes_retirement` |
| Revised-key index, recent window | §2.4, §5.1 | A full in-memory history | — |
| Queries over a quarantined range return an error | §8 Corrupt block | The range reads from another tier | — |
| Restore from a checkpoint | §8 Restore | Restore downloads files and rebuilds | `restore_onto_empty_disk_current_first` |
| Invariants 11–16 | §10 | Not checked yet | — |
