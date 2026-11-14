# kvik-model

A deliberately simple, slow, in-memory implementation of the semantics in [`docs/spec.md`](../docs/spec.md). No
files, no performance work. It shows that the rules are consistent and is the oracle the engine will be tested
against. Sums and integrals use exact rationals, so tree nodes, merges and windows are compared with direct
computation for exact equality.

| Area | Module |
|---|---|
| Series registry, kinds (interval series with length and phase), aliases, retirement, versioned series, timing, expected interval, record on change, counter rules, lateness bound | `registry` |
| Facts, quality, flags, `t_source`, positions, rank | `types` |
| Current, as-of and history views (direct scans) | `view` |
| Summary nodes, direct computation, `merge`, window edge stretches, density rule, credit limit, counter steps and rate, apportioned interval amounts | `summary` |
| Summary tree (1-minute and 15-minute levels, stored where the density rule keeps them), windows (interval windows included), sliding cache | `tree` |
| Calendar days and months: spans in the site zone (an effective-dated log of zone entries) or a named zone, zone identity after links, the tzdata version, calendar files of stored day and month nodes, calendar windows and results as window evaluations | `calendar` |
| Direct window and calendar oracles | `oracle` |
| Conditions with an injected monotonic clock; condition state, transitions and resume from a latch | `condition` |
| Ingest (lateness, future readings, invalid values, the clock before synchronisation, retirement, interval phase), duplicates, commit with crash points, durable mark, recovery by boot id, delivery (run-time routes, condition funnels and timer deliveries, cursors and latches, catch-up, resync), compaction and compaction of compacted files, atomic manifest records, reader leases, retirement, tiers, backup, tree checkpoints, generations, stepwise restore | `engine` |

The model does not yet follow every rule of the specification; [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md) lists where
it differs, and the open questions in the specification itself.

## Tests

| File | Covers |
|---|---|
| `tests/invariants.rs` | Property tests for the invariants of spec §10, named after them (`invNN_…`), and guards (`…_coverage`) that check the generators reach the states each invariant is about |
| `tests/edge_cases.rs` | The edge-case rows of spec §9, named after them (`row_NN_…`); rows 28, 28a and 28b are in `tests/rules.rs` |
| `tests/rules.rs` | Other rules, named after their section; the `s5_7_…` and `s6_calendar_…` tests cover calendar results, spans pinned by hand, zone identity and calendar windows; the `s7_…` tests cover per-series chunks, atomic manifest records, merging compacted files, the grace period and leases; the `s3_low_mark_…`, `s8_eviction_…`, `s8_retention_…` and `s8_fill_…` tests cover tiers and eviction; the `s2_2_…` and `s2_5_…` tests cover lateness, future readings and the clock, `s5_1_density_rule` and `s6_rate_…` the density rule and rate; the `s2_…`, `s4_outranked_…`, `s5_6_…` and `s8_version_index_…` tests cover the duplicate rule and versioned series (rank, refusals, outranked facts in delivery and the tree, the version index during a restore); the `s4_…` and `s6_…` delivery tests cover run-time routes, `resync` on resume, after a recovery on a new boot, after a restore and in place of a replay that is too far behind, condition deliveries and latches, with properties for delivery identity, resync order and no gap within a session; the `s8_…` tests cover tree checkpoints, generations and restore steps 1–4, including rows 28 and 28b, and row 28a at three points: before the record, during step 1, and between later downloads (a download is atomic in the model) |
| `tests/open_questions.rs` | One test per open question that the model can show (`qNN_…`) |
| `tests/model.rs` | Model behaviour where it implements a simpler rule than the specification |

## Coverage

Every invariant has a property test and every edge-case row a test whose doc comment cites it, except those whose "Not shown in the model" cell says they are not yet modelled. Properties over
`arb_op` are guarded by `generator_coverage`; those over `arb_lossy_op` (the properties whose names mention lost
ranges, kept beside their plain versions) by `lossy_generator_coverage`. "Pins" names the
open question ([`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md)) whose answer a test fixes for now; "Not shown in the model"
names the clauses the model cannot show, why, and the roadmap item that tests them in the engine.

### Invariants (spec §10)

| # | Property tests | File | Generator guards | Pins | Not shown in the model |
|---|---|---|---|---|---|
| 1 | `inv01_positions_gap_free_across_restarts`, `inv01_only_restore_starts_generation`, `inv01_gap_free_except_lost_ranges` | `invariants.rs` | `generator_coverage`, `restore_generator_coverage`, `lossy_generator_coverage` |  | A lost range described by a recovery entry (§3 Recovery rule 3): the model's mark is exact and it writes no recovery entry (power-loss rig; model support before roadmap 4.5) |
| 2 | `inv02_delivered_iff_durable`, `inv02_routed_positions_delivered_or_resynced`, `inv02_resync_excused_by_locality_when_issued`, `inv02_delivered_iff_durable_except_lost_ranges` | `invariants.rs` | `generator_coverage`, `delivery_generator_coverage`, `lossy_generator_coverage` |  | The exception for a tail truncated by rule 3, and orphaned facts routed to none (§2.1), as for 1 (model support before roadmap 4.5) |
| 3 | `inv03_delivery_in_position_order`, `inv03_fact_deliveries_in_order_among_other_deliveries`, `inv03_delivery_in_position_order_with_lost_ranges` | `invariants.rs` | `generator_coverage`, `delivery_generator_coverage`, `lossy_generator_coverage` |  |  |
| 4 | `inv04_current_view_over_all_tiers`, `inv04_current_view_over_all_tiers_with_lost_ranges` | `invariants.rs` | `generator_coverage`, `lossy_generator_coverage` |  | Orphaned facts (§2.1, Q-33): a lost range never holds a definition in the model, so none arises (model support before roadmap 4.5) |
| 5 | `inv05_tree_nodes_equal_direct`, `inv05_tree_nodes_equal_direct_with_lost_ranges`, `inv05_stored_levels_follow_the_density_rule`, `inv05_interval_levels_stored_only_where_no_interval_crosses_an_edge`, `inv05_calendar_nodes_equal_direct` | `invariants.rs` | `generator_coverage`, `params_generator_covers_every_density_level`, `interval_generator_coverage`, `calendar_generator_coverage`, `lossy_generator_coverage` |  |  |
| 6 | `inv06_merge_associative_and_direct` | `invariants.rs` | `params_generator_covers_every_density_level` |  |  |
| 7 | `inv07_windows_equal_direct`, `inv07_calendar_results_equal_direct`, `inv07_window_examples_at_quarter_edges` | `invariants.rs` | `params_generator_covers_every_density_level`, `calendar_generator_coverage` |  | Results flagged `approximate_edges`: no edge moves in the model (see `OPEN-QUESTIONS.md`; roadmap 7.3 and 8.3) |
| 8 | `inv08_rebuild_equals_live`, `inv08_rebuild_equals_live_with_lost_ranges`, `inv08_calendar_rebuild_equals_live` | `invariants.rs` | `generator_coverage`, `calendar_generator_coverage`, `lossy_generator_coverage` |  | A lost range holding a registry entry (Q-33): refused by the fault injection. Adopting a tzdata release at open (Q-76, settled): the model differs (see `OPEN-QUESTIONS.md`); `q76_…` in `open_questions.rs` expects the model's result |
| 9 | `inv09_tiering_backup_restore_preserve_facts`, `inv09_stepwise_restore_from_checkpoint`, `inv09_files_preserve_positions_outside_lost_ranges`; `s10_9_restore_preserves_backed_up_positions` | `invariants.rs`; `rules.rs` | `generator_coverage`, `restore_generator_coverage`, `lossy_generator_coverage` |  | A restore across a lost range: the bucket does not carry lost-range records, so the model reads a gap (see `OPEN-QUESTIONS.md`; restore, roadmap 8.3); a listed file lost or corrupted in the bucket during a restore (Q-39 (e)): not modelled |
| 10 | `inv10_no_operation_reduces_facts`, `inv10_only_lost_ranges_remove_facts` | `invariants.rs` | `generator_coverage`, `lossy_generator_coverage` |  | A tail truncated by rule 3 and `possible_loss`, as for 1 (model support before roadmap 4.5); a corrupt manifest record (Q-52) and a loss in the bucket during a restore (Q-39 (e)): not modelled |
| 11 | `inv11_nothing_observed_beyond_mark`, `inv11_nothing_observed_beyond_mark_with_lost_ranges`, `inv11_no_delivery_of_any_kind_beyond_mark` | `invariants.rs` | `generator_coverage`, `delivery_generator_coverage`, `lossy_generator_coverage` |  |  |
| 12 | `inv12_no_file_leaves_a_tier_without_a_verified_copy` | `invariants.rs` | `files_generator_coverage` |  |  |
| 13 | `inv13_cursor_and_latch_never_ahead_of_confirmation`; `s6_latch_moves_on_confirmation_only` | `invariants.rs`; `rules.rs` | `delivery_generator_coverage` |  | The cursor file synced at most every 1 s: the model writes it at once (roadmap 5.2) |
| 14 | `inv14_no_epoch_lists_two_files_with_one_position`; `q71_log_segment_and_its_chunks_listed_in_one_epoch` | `invariants.rs`; `open_questions.rs` | `files_generator_coverage` |  |  |
| 15 | `inv15_dropped_iff_identical_without_revise` | `invariants.rs` | `dup_generator_coverage` |  | The revised-key index outside a restore (Q-72, settled): the model differs (see `OPEN-QUESTIONS.md`); `q72_…` in `open_questions.rs` expects the model's result |
| 16 | `inv16_resume_delivers_each_condition_once_in_resync`; `s6_held_for_true_while_down_delivered_in_resync` | `invariants.rs`; `rules.rs` | `delivery_generator_coverage` |  | The exception for a condition unknown during a restore (Q-49 (b)): not modelled |
| 17 | `inv17_counter_increase_adds_over_adjacent_windows`, `inv17_state_transitions_add_over_adjacent_windows`, `inv17_counter_days_add_up_to_their_month` | `invariants.rs` | `params_generator_covers_every_density_level`; DST months by construction |  |  |
| 18 | `inv18_nothing_credited_past_now_or_staleness`, `inv18_interval_reading_valid_for_its_interval_only` | `invariants.rs` | `params_generator_covers_every_density_level`, `interval_generator_coverage` |  |  |
| 19 | `inv19_resubmission_never_changes_a_current_fact`, `inv19_resubmission_across_a_restore` | `invariants.rs` | `inv19_generator_coverage`, `dup_generator_coverage` |  |  |
| 20 | `inv20_scheduled_and_forecast_facts_never_future_or_out_of_order_late` | `invariants.rs` | `timed_generator_coverage` |  |  |
| 21 | `inv21_scheduled_and_forecast_windows_independent_of_now`, `inv21_tree_windows_independent_of_now` | `invariants.rs` | `timed_generator_coverage`, `params_generator_covers_every_density_level` |  | The clause for a calendar-length interval series (Q-45): not yet modelled (follow-up item) |
| 22 | `inv22_interval_sums_add_over_adjacent_windows`, `inv22_interval_days_add_up_to_their_month` | `invariants.rs` | `interval_generator_coverage`; zones and months by construction |  | Displaced readings (Q-45): not yet modelled (follow-up item). The counts clause (Q-81): no property yet (follow-up item) |
| 23 | — | — | — |  | Calendar-length interval series (Q-45): not yet modelled (follow-up item) |
| 24 | — | — | — |  | The lateness reference (Q-74): not yet modelled (follow-up item); `q74_…` in `open_questions.rs` expects the model's result |
| 25 | — | — | — |  | Reset points and `forget_above` (Q-47, Q-48): not yet modelled; the mark is exact (model support before roadmap 4.5) |
| 26 | — | — | — |  | The timed mark sync: not yet modelled; the mark is exact, so nothing can name a position above it (model support before roadmap 4.5) |
| 27 | — | — | — |  | Acknowledgement and `possible_loss` (Q-34 (c)): not yet modelled; the model's lost ranges are read from the manifest and never shortened (`row_11a_corrupt_block_inside_the_open_segment`) (model support before roadmap 4.5) |

### Edge-case rows (spec §9)

| Row | Tests | File | Pins | Not shown in the model |
|---|---|---|---|---|
| 1 | `row_01_same_t_different_value_becomes_current`, `row_01_property` | `edge_cases.rs` |  |  |
| 2 | `row_02_identical_repeat_dropped_and_counted` | `edge_cases.rs` |  |  |
| 2a | `row_02a_replay_of_earlier_value_dropped_unless_revise`, `row_01_property` | `edge_cases.rs` |  |  |
| 2b | `row_02b_repeat_whose_current_fact_is_off_every_local_tier` | `edge_cases.rs` |  |  |
| 2c | `row_02c_replay_never_reverts_a_versioned_correction` | `edge_cases.rs` |  | A replay whose facts are off every tier while the revised-key index holds them (Q-72, settled): the model records it unchecked, and the test's off-tier case and `q72_…` expect the model's result |
| 2d | `row_02d_lower_changed_at_is_outranked` | `edge_cases.rs` |  |  |
| 3 | `row_03_late_reading_flagged_tree_updated_no_retroactive_trigger` | `edge_cases.rs` |  |  |
| 4 | `row_04_future_reading_recorded_suspect_excluded_until_its_time` | `edge_cases.rs` |  | "Does not raise the lateness reference" (Q-74, settled): not yet modelled (follow-up item); `q74_…` in `open_questions.rs` expects the model's result |
| 4a | — | — |  | The lateness reference (Q-74): not yet modelled (follow-up item) |
| 5 | `row_05_stuck_or_past_clock_is_late_and_reported` | `edge_cases.rs` |  |  |
| 6 | `row_06_two_readings_in_one_second` | `edge_cases.rs` |  |  |
| 7 | `row_07_process_crash_after_write_before_sync`; `s3_missing_mark_counts_as_different_boot`, `s3_recovery_writes_mark_after_reboot` | `edge_cases.rs`; `rules.rs` |  | `possible_loss`, the recovery entry and the timed mark sync: the model's mark is exact (kill loop; model support before roadmap 4.5) |
| 7a | `row_07a_power_loss_after_write_before_sync`; `s4_resync_after_rule_3_recovery_before_later_positions` | `edge_cases.rs`; `rules.rs` |  | `possible_loss`, the recovery entry and `resync` with `forget_above` on every start until the consumer's synced cursor has passed it: the mark is exact; `s4_resync_after_rule_3_recovery_before_later_positions` now contradicts the specification (see `OPEN-QUESTIONS.md`; power-loss rig, model support before roadmap 4.5) |
| 7b | — | — |  | The rule is open (Q-46): how a reader checks a kept position after a truncation (power-loss rig; roadmap 4.5 with 4.6) |
| 7c | — | — |  | Recovery entries (Q-48): not yet modelled (kill loop; model support before roadmap 4.5) |
| 8 | `row_08_crash_after_sync_before_delivery`, `row_08_crash_during_delivery_after_mark` | `edge_cases.rs` |  |  |
| 9 | `row_09_crash_during_compaction`, `row_09_failed_recovery_sync_discards_nothing` | `edge_cases.rs` |  | Crash points in real files (kill loop, roadmap 6.2) |
| 10 | `row_10_crash_around_the_manifest_record` | `edge_cases.rs` |  | Crash points in real files (kill loop, roadmap 6.3) |
| 11 | `row_11_corrupt_sealed_file_quarantined_and_restored_from_tier`; `s8_corrupt_log_segment_with_no_other_copy_loses_range`, `s8_corrupt_log_segment_found_while_read_only_or_stopped_is_recorded_later` | `edge_cases.rs`; `rules.rs` |  | Losing only the damaged region and copying the valid batches to a new segment, which ends the error: the model loses the whole segment (recorded as a lost range, `data_lost`) and its quarantine stays fatal; compacting again from kept inputs (roadmap 6.3 and 8.1) |
| 11a | `row_11a_corrupt_block_inside_the_open_segment`, `row_11a_corrupt_block_below_an_old_boots_mark`, `row_11a_failed_recovery_sync_reports_the_range_as_found`, `row_11a_fault_injection_limits` | `edge_cases.rs` | Q-33 | A block found corrupt at run time (reported when found; for the open segment, recorded by the next start that can write); the lost range as a reset point before the kept facts above it, and the recovery entry after a power loss: `row_11a_corrupt_block_inside_the_open_segment`, `row_11a_corrupt_block_below_an_old_boots_mark` and `row_11a_failed_recovery_sync_reports_the_range_as_found` now contradict the specification (see `OPEN-QUESTIONS.md`); damaged bytes in real files (fault injection, roadmap 4.5) |
| 12 | `row_12_sync_error_read_only_fatal_restart_truncates` | `edge_cases.rs` |  | Injected sync errors in real files (roadmap 4.5) |
| 13 | `row_13_disk_fills_degrade_then_ingest_waits_nothing_deleted` | `edge_cases.rs` |  | ENOSPC from compaction: the model pauses at the low mark first (fault injection, roadmap 4.5) |
| 14 | `row_14_consumer_slower_than_stream_catches_up_from_log`; `s4_no_gap_no_repeat_within_a_session` | `edge_cases.rs`; `rules.rs` |  | Throughput under load (roadmap 5.5) |
| 15 | `row_15_consumer_down_for_a_day` | `edge_cases.rs` |  |  |
| 16 | `row_16_condition_true_while_consumer_down`; `s6_held_for_true_while_down_delivered_in_resync` | `edge_cases.rs`; `rules.rs` |  |  |
| 17 | `row_17_series_goes_silent` | `edge_cases.rs` |  |  |
| 17a | `row_17a_record_on_change_keep_alive_never_stale` | `edge_cases.rs` | Q-73 |  |
| 18 | `row_18_counter_reset_increase_across_nodes` | `edge_cases.rs` |  |  |
| 18a | `row_18a_counter_wraps_glitches_low_or_is_silent_for_hours` | `edge_cases.rs` |  |  |
| 19 | `row_19_correction_changes_min_max` | `edge_cases.rs` |  |  |
| 20 | `row_20_dst_days`, `row_20_days_with_midnight_in_a_gap_or_twice`, `row_20_dst_transition_days_explicit` | `edge_cases.rs` |  | Days around a site-zone change to another offset (Q-75, settled): the model differs (see `OPEN-QUESTIONS.md`); `q75_…` in `open_questions.rs` expects the model's result |
| 20a | `row_20a_no_calendar_node_without_a_stored_15_minute_level`, `row_20a_interval_series_of_30_minutes_has_no_calendar_node`, `row_20a_calendar_results_without_a_stored_level_are_windows_over_facts` | `edge_cases.rs` |  |  |
| 21 | `row_21_site_zone_change_rebuilds_from_its_effective_time`, `row_21_tzdata_change_rebuilds_the_affected_days` | `edge_cases.rs` |  | A real tzdata release: the model replaces one zone's rules (see `OPEN-QUESTIONS.md`; roadmap 7.3). Adopting a release at open (Q-76, settled): the model differs, and `row_21_tzdata_…` expects the model's result between the install and the rebuild |
| 21a | `row_21a_named_zone_with_a_historical_offset`, `row_21a_interval_series_in_named_zones` | `edge_cases.rs` |  |  |
| 22 | `row_22_system_clock_jump_timers_unaffected` | `edge_cases.rs` |  |  |
| 22a | `row_22a_host_clock_not_yet_synchronised` | `edge_cases.rs` |  |  |
| 23 | `row_23_series_renamed_alias_fact` | `edge_cases.rs` |  |  |
| 24 | `row_24_unit_or_kind_change_is_new_series` | `edge_cases.rs` |  |  |
| 25 | `row_25_series_retired_at_t_r` | `edge_cases.rs` |  |  |
| 26 | `row_26_reader_holds_an_old_epoch` | `edge_cases.rs` |  | A reader in a second process (roadmap 4.6) |
| 27 | `row_27_bucket_unreachable_for_days` | `edge_cases.rs` |  | A real bucket (roadmap 8.2) |
| 28 | `s8_restore_claims_next_generation_from_sequence_1`, `s8_restore_from_latest_checkpoint_at_or_below_restored_end`, `s8_restoring_queries_at_one_minute_resolution`, `s8_restored_end_before_first_gap`, `s8_detail_back_verifies_checkpoint_and_removes_label` | `rules.rs` |  | Positions set aside are not reported and their ids not reserved; fact reads do not fetch on demand (restore drill, roadmap 8.3) |
| 28a | `s8_restart_after_record_resumes_in_its_generation`, `s8_crash_before_record_restarts_with_next_generation`, `s8_crash_during_step_1_finishes_it_before_ingest`; `inv09_stepwise_restore_from_checkpoint` | `rules.rs`; `invariants.rs` |  | A crash inside a download: a download is atomic in the model (kill loop, roadmap 8.3) |
| 28b | `s8_two_restores_from_one_bucket_claim_different_generations`, `s8_superseded_writer_stops_uploading`, `s8_drill_is_read_only_and_claims_nothing` | `rules.rs` |  | Two concurrent restores: the model does not read the claims again after uploading its record (fault injection, roadmap 8.3) |
| 29 | `row_29_late_fact_for_span_without_local_detail`; `s8_replayed_minute_without_its_detail_is_stale` | `edge_cases.rs`; `rules.rs` |  |  |
| 30 | `row_30_series_ceiling` | `edge_cases.rs` |  | Memory per series (load test, roadmap 10.1) |
| 31 | `row_31_nan_or_infinite_recorded_as_bad_without_value` | `edge_cases.rs` |  |  |
| 32 | `row_32_newer_format_refused` | `edge_cases.rs` |  | Golden files (roadmap 4.2) |
| 33 | `row_33_emit_from_a_delivery_does_not_block_and_a_cycle_is_not_rejected` | `edge_cases.rs` |  | **Partial.** Emitting off the writer thread: the model is synchronous (roadmap 5.2). `emits` and the cycle test (Q-78, settled): not yet modelled (follow-up item), and the test pins the model's gap, a cycle accepted |
| 34 | `row_34_route_added_while_catching_up` | `edge_cases.rs` |  |  |
| 35 | `row_35_window_with_no_new_data_or_edge_in_open_minute` | `edge_cases.rs` |  |  |
| 36 | `row_36_scheduled_value_for_tomorrow`; `s5_4_stale_for_on_a_scheduled_series` | `edge_cases.rs`; `rules.rs` |  |  |
| 37 | `row_37_forecast_reissued` | `edge_cases.rs` |  |  |
| 37b | `row_37b_forecast_source_falls_silent` | `edge_cases.rs` |  |  |
| 38 | `row_38_window_over_tomorrow_on_scheduled_or_forecast_series` | `edge_cases.rs` |  |  |
| 39 | `row_39_window_ending_after_now_on_an_observed_series` | `edge_cases.rs` |  |  |
| 40 | `row_40_interval_reading_off_its_phase` | `edge_cases.rs` |  | The calendar-length clauses and the duplicate before the phase check (Q-45): not yet modelled (follow-up item) |
| 41 | `row_41_window_edge_inside_an_interval` | `edge_cases.rs` |  |  |
| 42 | `row_42_interval_missing` | `edge_cases.rs` |  |  |
| 43 | — | — |  | Calendar-length interval series (Q-45): not yet modelled (follow-up item) |
| 44 | — | — |  | Calendar-length interval series (Q-45): not yet modelled (follow-up item) |
| 45 | — | — |  | Calendar-length interval series (Q-45), and an interval zone dropped or become a link (Q-80): not yet modelled (follow-up item) |
| 46 | — | — |  | Calendar-length interval series (Q-45): not yet modelled (follow-up item) |

```bash
cargo test --release                         # faster
PROPTEST_CASES=10000 cargo test --release    # the deep CI cases on pull requests (40,000 nightly)
../.github/deep-shards.sh run invariants-5   # one shard of that run; set PROPTEST_CASES the same way
cargo run --example basic
```
