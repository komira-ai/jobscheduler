//! The job store suite: every behaviour of [`crate::JobStore`] stated as a function generic
//! over a `js_state::conformance::Backend`, so the fake, SQLite and Postgres run the same
//! checks. A backend's tests run them with [`crate::job_store_tests!`] (one test each) or
//! [`run_job_store_suite`]; the fake's tests also run a check against a fake with a
//! deliberate defect and require it to panic.
//!
//! Every assertion message starts with the name of its check, which is what a
//! `should_panic(expected = ...)` test matches. Time is the `ManualClock` the backend is
//! opened with; nothing reads the wall clock or sleeps, and a race is arranged by a hook that
//! commits just before the operation's own commit ([`Interfere::before_next`]).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{HostPort, JobMount, Layer, MountMode, MountSource, PortProtocol, Propagation};
use js_state::conformance::Backend;
use js_state::{CommitStats, Cond, ManualClock, Store, Txn, Versioned};
use js_types::{
    CpuSeries, ExecMode, HistoryKey, HostId, ImageDigestRef, Labels, ReservedKey, Resources,
};

use crate::interfere::Interfere;
use crate::{
    Booked, Constraints, DeclaredPort, Isolation, JobEnd, JobId, JobSpec, JobState, JobStore,
    Needs, NetworkPolicy, NewJob, Output, RandomIds, ResourceUsage,
};

mod boot;
mod claimed;
mod clock;
mod collect;
mod compat;
mod cordons;
mod dispatch;
mod duties;
mod failures;
mod guarded;
mod held;
mod host_rows;
mod hosts;
mod idempotency;
mod indexes;
mod lifecycle;
mod logs;
mod marks;
mod nodes;
mod paging;
mod placement;
mod pools;
mod prepare;
mod queue;
mod queued_index;
mod races;
mod retention;
mod retire;
mod rows;
mod scrub;
mod silence;
mod taints;
mod unclean;
mod whole;

pub use boot::*;
pub use claimed::*;
pub use clock::*;
pub use collect::*;
pub use compat::*;
pub use cordons::*;
pub use dispatch::*;
pub use duties::*;
pub use failures::*;
pub use guarded::*;
pub use held::*;
pub use host_rows::*;
pub use hosts::*;
pub use idempotency::*;
pub use indexes::*;
pub use lifecycle::*;
pub use logs::*;
pub use marks::*;
pub use nodes::*;
pub use paging::*;
pub use placement::*;
pub use pools::*;
pub use prepare::*;
pub use queue::*;
pub use queued_index::*;
pub use races::*;
pub use retention::*;
pub use retire::*;
pub use rows::*;
pub use scrub::*;
pub use silence::*;
pub use taints::*;
pub use unclean::*;
pub use whole::*;

/// Runs every check of the suite against `backend`, one after another.
pub fn run_job_store_suite<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    same_id_same_job_returns_the_existing_row(backend);
    same_id_different_job_is_a_conflict(backend);
    a_priority_only_difference_is_the_same_job(backend);
    a_retry_by_another_client_keeps_the_first(backend);
    the_same_job_is_found_by_value_not_by_bytes(backend);
    a_concurrent_insert_answers_from_the_job_it_made(backend);
    minted_ids_do_not_repeat_across_instances(backend);
    a_minted_id_collision_is_retried(backend);
    minting_stops_at_its_budget(backend);
    placement_and_allocation_commit_together(backend);
    a_failed_commit_changes_nothing(backend);
    a_failed_read_changes_nothing(backend);
    opening_after_another_store_wrote_the_schema_reads_its_row(backend);
    place_on_a_job_that_changed_is_stale(backend);
    place_refuses_a_job_that_is_not_queued_or_still_booked(backend);
    a_requeued_job_gets_a_new_allocation(backend);
    a_repeated_allocation_id_is_drawn_again(backend);
    booked_on_sums_the_live_allocations_of_a_host(backend);
    placement_books_the_jobs_host_ports(backend);
    live_on_lists_the_live_allocations_of_one_host(backend);
    two_servers_beating_one_host_at_once_apply_each_beat_once(backend);
    a_beat_or_hello_that_loses_its_version_reads_the_row_again(backend);
    a_guarded_write_commits_at_the_node_version_and_fails_after_it_moved(backend);
    a_conflict_on_another_row_is_not_the_node_moving(backend);
    a_conflict_on_a_job_row_when_the_node_moved_too_is_the_node_moving(backend);
    a_guarded_view_shares_the_write_count_of_its_store(backend);
    a_guarded_claimed_placement_honours_the_node_guard(backend);
    settle_node_writes_at_the_version_and_keeps_the_other_fields(backend);
    the_unclean_hosts_follow_the_rows_between_calls(backend);
    a_beat_of_an_old_epoch_after_a_newer_hello_is_stale(backend);
    a_beat_through_one_server_is_read_by_another(backend);
    heard_at_is_the_store_clock_and_judges_live_nodes(backend);
    a_node_row_without_the_optional_fields_reads_and_keeps_a_later_builds_field(backend);
    forget_node_deletes_only_the_row_it_read_and_a_failed_write_changes_nothing(backend);
    a_claimed_placement_books_only_what_the_host_has_room_for(backend);
    a_whole_host_and_a_share_exclude_each_other_in_the_placement(backend);
    a_job_another_writer_placed_is_not_placed_again(backend);
    a_claimed_placement_moves_the_hosts_reservation_row(backend);
    a_claimed_placement_locks_the_hosts_row_before_it_sums_the_host(backend);
    a_claimed_placement_deletes_the_marked_jobs_mark(backend);
    a_claimed_placement_of_a_missing_job_is_stale(backend);
    a_claimed_placement_calls_the_store_only_through_its_transaction(backend);
    a_requeued_job_is_placed_again_over_its_freed_allocation(backend);
    a_placement_counts_past_a_stray_entry_and_refuses_a_row_it_cannot_read(backend);
    a_duty_is_claimed_by_one_server_for_each_period(backend);
    a_failed_claim_is_an_error_and_claims_nothing(backend);
    a_duty_is_due_by_the_store_clock(backend);
    nothing_is_written_when_the_store_clock_cannot_be_read(backend);
    end_and_free_is_one_commit(backend);
    requeue_loses_to_a_change_of_either_row(backend);
    a_move_that_loses_its_job_version_is_stale(backend);
    a_cancel_that_loses_to_a_placement_is_stale(backend);
    a_release_that_loses_to_a_free_is_stale(backend);
    concurrent_frees_keep_the_first_committed_reason(backend);
    a_placement_over_a_changed_freed_allocation_is_stale(backend);
    a_freed_allocation_row_is_kept_as_history(backend);
    free_twice_keeps_the_first_reason(backend);
    an_ended_job_cannot_move_again(backend);
    ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit(backend);
    cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent(backend);
    cancel_twice_is_cancel_once(backend);
    releasing_needs_an_end_or_a_cancel(backend);
    every_time_is_the_store_clock_at_its_step(backend);
    every_supported_end_is_stored_with_its_time_and_release(backend);
    a_row_with_unknown_tags_is_read_and_rewritten_intact(backend);
    unknown_fields_survive_every_rewrite(backend);
    a_row_without_the_newest_tag_reads_with_defaults(backend);
    a_row_with_a_higher_format_byte_is_unknown_not_missing(backend);
    a_reserved_label_of_a_later_build_is_an_unknown_row(backend);
    a_row_this_build_cannot_read_refuses_every_write(backend);
    a_row_that_does_not_parse_is_corrupt(backend);
    a_row_under_another_jobs_key_is_refused(backend);
    list_jobs_pages_in_key_order_and_filters_by_state(backend);
    list_jobs_walks_past_pages_with_nothing_to_show(backend);
    list_jobs_is_capped_at_max_page(backend);
    list_jobs_follows_rows_that_change_between_pages(backend);
    a_result_is_readable_until_the_retention_end_and_not_after(backend);
    restart_keeps_results(backend);
    a_result_survives_the_end_of_its_allocation(backend);
    retention_ends_are_raised_at_boot_after_a_long_outage(backend);
    a_boot_raises_floors_and_retention_ends_a_row_to_a_commit(backend);
    a_boot_raise_that_other_writers_beat_reads_each_row_again_alone(backend);
    a_boot_raise_wider_than_a_batch_takes_a_batch_per_group(backend);
    retention_uses_the_injected_clock(backend);
    the_result_and_the_state_change_commit_together(backend);
    an_overlong_tail_is_cut_on_a_character_boundary(backend);
    ending_an_ended_job_keeps_the_first_result(backend);
    a_job_without_a_result_decodes(backend);
    sweep_never_lists_a_job_that_has_not_ended(backend);
    the_tick_deletes_in_batches_and_leaves_what_it_cannot_read(backend);
    a_failed_sweep_or_raise_changes_nothing(backend);
    a_sweep_that_loses_to_a_raise_is_stale(backend);
    queued_lists_the_queue_in_order(backend);
    live_allocations_lists_every_booked_or_releasing_allocation(backend);
    a_free_with_rows_beside_it_is_one_commit(backend);
    ended_job_record_holds_no_file_bytes(backend);
    live_job_keeps_file_bytes(backend);
    an_ended_job_is_matched_by_the_digests_of_its_files(backend);
    a_boot_digests_the_files_an_ended_row_still_holds(backend);
    the_log_lengths_are_kept_with_the_result_and_swept_with_it(backend);
    the_tick_deletes_only_freed_allocations_past_their_window(backend);
    a_backlog_of_freed_allocations_clears_over_several_ticks(backend);
    a_collection_that_loses_to_a_placement_keeps_the_booking(backend);
    every_write_keeps_the_indexes_equal_to_a_scan(backend);
    the_backfill_makes_the_indexes_agree_and_a_second_changes_nothing(backend);
    an_indexed_read_judges_the_row_not_the_entry(backend);
    an_indexed_read_reads_every_page_of_entries(backend);
    an_ended_row_without_a_retention_end_is_due_and_kept(backend);
    a_backfill_that_loses_a_row_reads_it_again_and_indexes_it(backend);
    a_backfill_whose_row_is_deleted_before_its_commit_deletes_its_entries(backend);
    a_store_an_earlier_build_wrote_reads_its_whole_queue_after_an_open(backend);
    a_queued_job_stays_under_the_active_space_an_earlier_build_reads(backend);
    every_write_moves_the_written_mark_and_no_read_does(backend);
    another_job_stores_write_moves_the_written_mark_where_the_store_tells_its_head(backend);
    marks_are_additive_to_an_existing_store(backend);
    a_mark_round_trips_with_its_exclusions_and_made(backend);
    a_mark_keeps_a_later_builds_field_only_on_its_host(backend);
    only_a_queued_whole_host_job_takes_a_mark(backend);
    a_mark_row_this_build_cannot_read_is_refused_or_left(backend);
    marks_are_raised_at_boot(backend);
    end_and_unmark_deletes_both_or_neither(backend);
    delete_mark_removes_the_mark_it_read(backend);
    a_mark_that_loses_to_an_end_or_another_mark_is_stale(backend);
    a_host_isolation_placement_taints_its_host_in_the_same_commit(backend);
    a_contained_placement_writes_no_taint(backend);
    a_failed_host_placement_writes_no_taint(backend);
    a_second_host_job_on_a_tainted_host_is_stale(backend);
    a_whole_host_job_in_host_isolation_taints_its_host(backend);
    host_taints_lists_every_taint_and_refuses_a_bad_row(backend);
    the_retention_tick_keeps_taint_rows(backend);
    a_cordon_is_placed_and_renewed_by_its_holder(backend);
    another_holders_cordon_is_left_as_it_was(backend);
    a_take_over_makes_the_cordon_the_askers(backend);
    a_cordon_lapses_with_the_store_clock(backend);
    a_lift_ends_a_cordon_by_the_rule(backend);
    a_cordon_request_that_races_or_fails_writes_nothing(backend);
    a_cordon_row_that_cannot_be_read_is_refused_and_kept_in_force(backend);
    restore_cordon_writes_once_and_never_overwrites(backend);
    restore_cordons_carries_the_bindings_once(backend);
    a_placement_on_a_cordoned_host_commits_nothing(backend);
    a_cordon_taken_during_a_placement_makes_it_stale(backend);
    drop_taints_deletes_the_named_rows_in_one_commit(backend);
    place_with_writes_the_prepare_deadline_and_drops_the_mark(backend);
    a_placement_that_drops_a_mark_loses_to_a_change_of_the_job_or_the_mark(backend);
    a_cancel_of_a_queued_job_drops_its_mark_in_the_same_commit(backend);
    a_failed_prepare_requeues_the_job_with_its_count_and_bars(backend);
    a_held_refusal_requeues_the_job_without_counting_a_failure(backend);
    a_capacity_refusal_requeues_the_job_and_names_the_host_in_its_own_list(backend);
    a_bundle_pending_refusal_requeues_the_job_and_names_the_host_in_no_list(backend);
    a_held_refusal_refuses_what_it_cannot_requeue(backend);
    a_held_refusal_loses_to_a_change_of_either_row(backend);
    the_last_failed_prepare_ends_the_job_failed_to_start(backend);
    a_failed_prepare_refuses_what_the_requeue_refuses(backend);
    a_failed_prepare_loses_to_a_change_of_either_row(backend);
    a_failed_prepare_drops_its_mark_or_writes_nothing(backend);
    a_restarted_prepare_moves_only_the_deadline(backend);
    a_restarted_prepare_loses_to_a_change_of_either_row(backend);
    a_claimed_placement_refuses_a_tainted_host(backend);
    a_plain_placement_of_a_contained_job_on_a_tainted_host_is_stale(backend);
    a_hostmark_refuses_every_placement(backend);
    a_live_allocations_host_port_refuses_a_claimed_placement(backend);
    stray_on_host_entries_hold_no_port_and_an_unreadable_row_refuses(backend);
    host_marks_are_written_and_deleted_at_the_version_read(backend);
    drop_taints_keeps_other_jobs_rows_and_reads_again_after_a_lost_version(backend);
    a_pool_admits_new_hosts_up_to_its_cap_and_a_known_host_always(backend);
    an_admission_that_loses_the_pool_row_counts_again(backend);
    pool_admits_judges_as_the_admission_does_and_writes_nothing(backend);
    two_servers_admitting_hosts_one_slot_from_the_cap_admit_exactly_one(backend);
    live_nodes_reusing_takes_only_rows_at_the_stored_version(backend);
    nodes_lists_every_readable_row_heard_or_not(backend);
    forget_silent_nodes_deletes_only_the_rows_that_hold_nothing(backend);
    forget_silent_nodes_keeps_a_row_that_moved_and_refuses_an_unreadable_binding(backend);
    retire_node_deletes_a_lost_row_so_a_full_pool_admits_a_new_id(backend);
    retire_node_keeps_the_row_of_a_host_that_holds_or_is_owed_work(backend);
    retire_node_judges_again_a_row_that_moved_or_went_and_deletes_nothing_on_a_failure(backend);
    two_servers_claiming_one_host_at_once_hand_out_each_message_once(backend);
    a_message_whose_claim_lapses_is_handed_out_again_by_the_store_clock(backend);
    an_ack_deletes_only_what_was_handed_out_and_a_repeat_changes_nothing(backend);
    every_placement_that_books_a_host_queues_its_start_and_a_refused_one_none(backend);
    a_whole_host_job_queues_its_start_when_it_moves_to_starting(backend);
    an_end_leaves_a_stop_only_where_the_start_may_have_reached_the_host(backend);
    an_end_that_loses_to_a_claim_of_its_start_is_stale(backend);
    the_sweep_deletes_a_jobs_messages_with_its_row(backend);
    a_dispatch_row_of_another_build_is_read_or_left(backend);
    a_failed_claim_or_ack_changes_nothing_and_says_so(backend);
}

/// One `#[test]` per check of the suite, named as the check is, each on a fresh backend from
/// `$backend()`: `$backend` is a function (or closure) that makes a
/// `js_state::conformance::Backend`.
#[macro_export]
macro_rules! job_store_tests {
    (@each $backend:expr; $($check:ident),+ $(,)?) => {
        $(
            #[test]
            fn $check() {
                $crate::suite::$check(&$backend());
            }
        )+
    };
    ($backend:expr) => {
        $crate::job_store_tests!(
            @each $backend;
            same_id_same_job_returns_the_existing_row,
            same_id_different_job_is_a_conflict,
            a_priority_only_difference_is_the_same_job,
            a_retry_by_another_client_keeps_the_first,
            the_same_job_is_found_by_value_not_by_bytes,
            a_concurrent_insert_answers_from_the_job_it_made,
            minted_ids_do_not_repeat_across_instances,
            a_minted_id_collision_is_retried,
            minting_stops_at_its_budget,
            placement_and_allocation_commit_together,
            a_failed_commit_changes_nothing,
            a_failed_read_changes_nothing,
            opening_after_another_store_wrote_the_schema_reads_its_row,
            place_on_a_job_that_changed_is_stale,
            place_refuses_a_job_that_is_not_queued_or_still_booked,
            a_requeued_job_gets_a_new_allocation,
            a_repeated_allocation_id_is_drawn_again,
            booked_on_sums_the_live_allocations_of_a_host,
            placement_books_the_jobs_host_ports,
            live_on_lists_the_live_allocations_of_one_host,
            two_servers_beating_one_host_at_once_apply_each_beat_once,
            a_beat_or_hello_that_loses_its_version_reads_the_row_again,
            a_guarded_write_commits_at_the_node_version_and_fails_after_it_moved,
            a_conflict_on_another_row_is_not_the_node_moving,
            a_conflict_on_a_job_row_when_the_node_moved_too_is_the_node_moving,
            a_guarded_view_shares_the_write_count_of_its_store,
            a_guarded_claimed_placement_honours_the_node_guard,
            settle_node_writes_at_the_version_and_keeps_the_other_fields,
            the_unclean_hosts_follow_the_rows_between_calls,
            a_beat_of_an_old_epoch_after_a_newer_hello_is_stale,
            a_beat_through_one_server_is_read_by_another,
            heard_at_is_the_store_clock_and_judges_live_nodes,
            a_node_row_without_the_optional_fields_reads_and_keeps_a_later_builds_field,
            forget_node_deletes_only_the_row_it_read_and_a_failed_write_changes_nothing,
            a_claimed_placement_books_only_what_the_host_has_room_for,
            a_whole_host_and_a_share_exclude_each_other_in_the_placement,
            a_job_another_writer_placed_is_not_placed_again,
            a_claimed_placement_moves_the_hosts_reservation_row,
            a_claimed_placement_locks_the_hosts_row_before_it_sums_the_host,
            a_claimed_placement_deletes_the_marked_jobs_mark,
            a_claimed_placement_of_a_missing_job_is_stale,
            a_claimed_placement_calls_the_store_only_through_its_transaction,
            a_requeued_job_is_placed_again_over_its_freed_allocation,
            a_placement_counts_past_a_stray_entry_and_refuses_a_row_it_cannot_read,
            a_duty_is_claimed_by_one_server_for_each_period,
            a_failed_claim_is_an_error_and_claims_nothing,
            a_duty_is_due_by_the_store_clock,
            nothing_is_written_when_the_store_clock_cannot_be_read,
            end_and_free_is_one_commit,
            requeue_loses_to_a_change_of_either_row,
            a_move_that_loses_its_job_version_is_stale,
            a_cancel_that_loses_to_a_placement_is_stale,
            a_release_that_loses_to_a_free_is_stale,
            concurrent_frees_keep_the_first_committed_reason,
            a_placement_over_a_changed_freed_allocation_is_stale,
            a_freed_allocation_row_is_kept_as_history,
            free_twice_keeps_the_first_reason,
            an_ended_job_cannot_move_again,
            ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit,
            cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent,
            cancel_twice_is_cancel_once,
            releasing_needs_an_end_or_a_cancel,
            every_time_is_the_store_clock_at_its_step,
            every_supported_end_is_stored_with_its_time_and_release,
            a_row_with_unknown_tags_is_read_and_rewritten_intact,
            unknown_fields_survive_every_rewrite,
            a_row_without_the_newest_tag_reads_with_defaults,
            a_row_with_a_higher_format_byte_is_unknown_not_missing,
            a_reserved_label_of_a_later_build_is_an_unknown_row,
            a_row_this_build_cannot_read_refuses_every_write,
            a_row_that_does_not_parse_is_corrupt,
            a_row_under_another_jobs_key_is_refused,
            list_jobs_pages_in_key_order_and_filters_by_state,
            list_jobs_walks_past_pages_with_nothing_to_show,
            list_jobs_is_capped_at_max_page,
            list_jobs_follows_rows_that_change_between_pages,
            a_result_is_readable_until_the_retention_end_and_not_after,
            restart_keeps_results,
            a_result_survives_the_end_of_its_allocation,
            retention_ends_are_raised_at_boot_after_a_long_outage,
            a_boot_raises_floors_and_retention_ends_a_row_to_a_commit,
            a_boot_raise_that_other_writers_beat_reads_each_row_again_alone,
            a_boot_raise_wider_than_a_batch_takes_a_batch_per_group,
            retention_uses_the_injected_clock,
            the_result_and_the_state_change_commit_together,
            an_overlong_tail_is_cut_on_a_character_boundary,
            ending_an_ended_job_keeps_the_first_result,
            a_job_without_a_result_decodes,
            sweep_never_lists_a_job_that_has_not_ended,
            the_tick_deletes_in_batches_and_leaves_what_it_cannot_read,
            a_failed_sweep_or_raise_changes_nothing,
            a_sweep_that_loses_to_a_raise_is_stale,
            queued_lists_the_queue_in_order,
            live_allocations_lists_every_booked_or_releasing_allocation,
            a_free_with_rows_beside_it_is_one_commit,
            ended_job_record_holds_no_file_bytes,
            live_job_keeps_file_bytes,
            an_ended_job_is_matched_by_the_digests_of_its_files,
            a_boot_digests_the_files_an_ended_row_still_holds,
            the_log_lengths_are_kept_with_the_result_and_swept_with_it,
            the_tick_deletes_only_freed_allocations_past_their_window,
            a_backlog_of_freed_allocations_clears_over_several_ticks,
            a_collection_that_loses_to_a_placement_keeps_the_booking,
            every_write_keeps_the_indexes_equal_to_a_scan,
            the_backfill_makes_the_indexes_agree_and_a_second_changes_nothing,
            an_indexed_read_judges_the_row_not_the_entry,
            an_indexed_read_reads_every_page_of_entries,
            an_ended_row_without_a_retention_end_is_due_and_kept,
            a_backfill_that_loses_a_row_reads_it_again_and_indexes_it,
            a_backfill_whose_row_is_deleted_before_its_commit_deletes_its_entries,
            a_store_an_earlier_build_wrote_reads_its_whole_queue_after_an_open,
            a_queued_job_stays_under_the_active_space_an_earlier_build_reads,
            every_write_moves_the_written_mark_and_no_read_does,
            another_job_stores_write_moves_the_written_mark_where_the_store_tells_its_head,
            marks_are_additive_to_an_existing_store,
            a_mark_round_trips_with_its_exclusions_and_made,
            a_mark_keeps_a_later_builds_field_only_on_its_host,
            only_a_queued_whole_host_job_takes_a_mark,
            a_mark_row_this_build_cannot_read_is_refused_or_left,
            marks_are_raised_at_boot,
            end_and_unmark_deletes_both_or_neither,
            delete_mark_removes_the_mark_it_read,
            a_mark_that_loses_to_an_end_or_another_mark_is_stale,
            a_host_isolation_placement_taints_its_host_in_the_same_commit,
            a_contained_placement_writes_no_taint,
            a_failed_host_placement_writes_no_taint,
            a_second_host_job_on_a_tainted_host_is_stale,
            a_whole_host_job_in_host_isolation_taints_its_host,
            host_taints_lists_every_taint_and_refuses_a_bad_row,
            the_retention_tick_keeps_taint_rows,
            a_cordon_is_placed_and_renewed_by_its_holder,
            another_holders_cordon_is_left_as_it_was,
            a_take_over_makes_the_cordon_the_askers,
            a_cordon_lapses_with_the_store_clock,
            a_lift_ends_a_cordon_by_the_rule,
            a_cordon_request_that_races_or_fails_writes_nothing,
            a_cordon_row_that_cannot_be_read_is_refused_and_kept_in_force,
            restore_cordon_writes_once_and_never_overwrites,
            restore_cordons_carries_the_bindings_once,
            a_placement_on_a_cordoned_host_commits_nothing,
            a_cordon_taken_during_a_placement_makes_it_stale,
            drop_taints_deletes_the_named_rows_in_one_commit,
            place_with_writes_the_prepare_deadline_and_drops_the_mark,
            a_placement_that_drops_a_mark_loses_to_a_change_of_the_job_or_the_mark,
            a_cancel_of_a_queued_job_drops_its_mark_in_the_same_commit,
            a_failed_prepare_requeues_the_job_with_its_count_and_bars,
            a_held_refusal_requeues_the_job_without_counting_a_failure,
            a_capacity_refusal_requeues_the_job_and_names_the_host_in_its_own_list,
            a_bundle_pending_refusal_requeues_the_job_and_names_the_host_in_no_list,
            a_held_refusal_refuses_what_it_cannot_requeue,
            a_held_refusal_loses_to_a_change_of_either_row,
            the_last_failed_prepare_ends_the_job_failed_to_start,
            a_failed_prepare_refuses_what_the_requeue_refuses,
            a_failed_prepare_loses_to_a_change_of_either_row,
            a_failed_prepare_drops_its_mark_or_writes_nothing,
            a_restarted_prepare_moves_only_the_deadline,
            a_restarted_prepare_loses_to_a_change_of_either_row,
            a_claimed_placement_refuses_a_tainted_host,
            a_plain_placement_of_a_contained_job_on_a_tainted_host_is_stale,
            a_hostmark_refuses_every_placement,
            a_live_allocations_host_port_refuses_a_claimed_placement,
            stray_on_host_entries_hold_no_port_and_an_unreadable_row_refuses,
            host_marks_are_written_and_deleted_at_the_version_read,
            drop_taints_keeps_other_jobs_rows_and_reads_again_after_a_lost_version,
            a_pool_admits_new_hosts_up_to_its_cap_and_a_known_host_always,
            an_admission_that_loses_the_pool_row_counts_again,
            pool_admits_judges_as_the_admission_does_and_writes_nothing,
            two_servers_admitting_hosts_one_slot_from_the_cap_admit_exactly_one,
            live_nodes_reusing_takes_only_rows_at_the_stored_version,
            nodes_lists_every_readable_row_heard_or_not,
            forget_silent_nodes_deletes_only_the_rows_that_hold_nothing,
            forget_silent_nodes_keeps_a_row_that_moved_and_refuses_an_unreadable_binding,
            retire_node_deletes_a_lost_row_so_a_full_pool_admits_a_new_id,
            retire_node_keeps_the_row_of_a_host_that_holds_or_is_owed_work,
            retire_node_judges_again_a_row_that_moved_or_went_and_deletes_nothing_on_a_failure,
            two_servers_claiming_one_host_at_once_hand_out_each_message_once,
            a_message_whose_claim_lapses_is_handed_out_again_by_the_store_clock,
            an_ack_deletes_only_what_was_handed_out_and_a_repeat_changes_nothing,
            every_placement_that_books_a_host_queues_its_start_and_a_refused_one_none,
            a_whole_host_job_queues_its_start_when_it_moves_to_starting,
            an_end_leaves_a_stop_only_where_the_start_may_have_reached_the_host,
            an_end_that_loses_to_a_claim_of_its_start_is_stale,
            the_sweep_deletes_a_jobs_messages_with_its_row,
            a_dispatch_row_of_another_build_is_read_or_left,
            a_failed_claim_or_ack_changes_nothing_and_says_so,
        );
    };
}

/// A store under test: its clock, the wrapper that interferes with its commits, and the job
/// store over the wrapper.
pub(crate) struct Rig<S> {
    /// The store's clock.
    pub(crate) clock: Arc<ManualClock>,
    /// The wrapped store.
    pub(crate) store: Arc<Interfere<S>>,
    /// The job store under test.
    pub(crate) jobs: JobStore<Interfere<S>>,
}

/// A fresh store of `backend` with a job store over it.
pub(crate) fn rig<B: Backend>(backend: &B) -> Rig<B::Store> {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Interfere::new(backend.open(Arc::clone(&clock))));
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("the job store opens");
    Rig { clock, store, jobs }
}

impl<S: Store> Rig<S> {
    /// Moves the clock on, so the next write's time differs from the last one's.
    pub(crate) fn tick(&self) {
        self.clock.advance(Duration::from_secs(1));
    }

    /// Records a queued job `text` with the full spec.
    pub(crate) fn queue(&self, text: &str) -> JobId {
        self.queue_spec(text, full_spec())
    }

    /// Records a queued job `text` with `spec`.
    pub(crate) fn queue_spec(&self, text: &str, spec: JobSpec) -> JobId {
        let inserted = self
            .jobs
            .insert_queued(NewJob {
                id: Some(job_id(text)),
                spec,
                priority: 0,
                client: None,
            })
            .expect("a fresh job is recorded");
        assert!(inserted.created, "a fresh job is created");
        inserted.record.id
    }

    /// Records a queued job `text` with the full spec but no host port, so that a claimed
    /// placement of it is not refused for a port another job on the host holds.
    pub(crate) fn queue_portless(&self, text: &str) -> JobId {
        self.queue_spec(
            text,
            JobSpec {
                ports: Vec::new(),
                ..full_spec()
            },
        )
    }

    /// Records the job `text` and places it on `host-a` with [`share`].
    pub(crate) fn placed(&self, text: &str) -> JobId {
        let id = self.queue(text);
        self.jobs
            .place(&id, host("host-a"), share())
            .expect("a queued job is placed");
        id
    }

    /// Records the job `text`, places it and moves it to running.
    pub(crate) fn running(&self, text: &str) -> JobId {
        let id = self.placed(text);
        self.jobs
            .transition(&id, JobState::Placed, JobState::Starting)
            .expect("placed to starting");
        self.jobs
            .transition(&id, JobState::Starting, JobState::Running)
            .expect("starting to running");
        id
    }

    /// The raw row `key`.
    pub(crate) fn raw(&self, key: &str) -> Option<Versioned> {
        self.store.get(key).expect("the store is reachable")
    }

    /// Writes `value` under `key` as it is, past the job store, and then indexes the tables
    /// as a job store's open does ([`Rig::reindex`]): the row is what a later build, an earlier
    /// release or a hand left in the tables, which the backfill indexes at every start.
    pub(crate) fn put_raw(&self, key: &str, value: Vec<u8>) {
        self.store
            .inner()
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("the store is reachable")
            .expect("an unconditional put commits");
        self.reindex();
    }

    /// Removes the row `key`, past the job store, and then indexes the tables as
    /// [`Rig::put_raw`] does.
    pub(crate) fn delete_raw(&self, key: &str) {
        self.store
            .inner()
            .txn(Txn::new().delete(key, Cond::Any))
            .expect("the store is reachable")
            .expect("an unconditional delete commits");
        self.reindex();
    }

    /// Runs the index backfill a job store's open runs, past the interference, over rows
    /// written past the job store.
    pub(crate) fn reindex(&self) {
        JobStore::new(self.store.inner(), RandomIds).expect("the job store opens and backfills");
    }

    /// A second writer over the wrapped store: its commits pass the interference by, so a
    /// hook can use it to commit between an operation's read and its commit.
    pub(crate) fn other(&self) -> JobStore<S> {
        JobStore::new(self.store.inner(), RandomIds).expect("a second writer opens")
    }

    /// A job store over the wrapped store (interference included) minting from `ids`.
    pub(crate) fn minting<I: crate::IdSource>(&self, ids: I) -> JobStore<Interfere<S>, I> {
        JobStore::new(Arc::clone(&self.store), ids).expect("a job store opens")
    }

    /// Every row and the commit counters.
    pub(crate) fn snapshot(&self) -> (Vec<(String, Versioned)>, CommitStats) {
        let rows = self.store.scan_prefix("").expect("the store is reachable");
        (rows, self.store.commit_stats())
    }

    /// How many job rows there are.
    pub(crate) fn job_rows(&self) -> usize {
        self.store
            .scan_prefix(crate::keys::JOB)
            .expect("the store is reachable")
            .len()
    }
}

/// The job id `text`.
pub(crate) fn job_id(text: &str) -> JobId {
    JobId::parse(text).expect("a valid job id")
}

/// The host id `text`.
pub(crate) fn host(text: &str) -> HostId {
    HostId::new(text).expect("a valid host id")
}

/// A digest of 64 hex digits that repeat `digit`.
pub(crate) fn digest(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

/// A spec with every field set to something other than its default.
pub(crate) fn full_spec() -> JobSpec {
    let caller = BTreeMap::from([("team".to_owned(), "infra".to_owned())]);
    JobSpec {
        image: Some(
            ImageDigestRef::parse(&format!("team/app:1@{}", digest('a'))).expect("an image"),
        ),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned(), "--seed=7".to_owned()],
        env: BTreeMap::from([
            ("MODE".to_owned(), "test".to_owned()),
            ("LEVEL".to_owned(), "2".to_owned()),
        ]),
        needs: Needs::Shape(
            Resources::new(2000, 1 << 30, 1, Some("accel-a".to_owned()))
                .expect("resources")
                .with_disk_bytes(1 << 32),
        ),
        constraints: Constraints {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            exec: Some(ExecMode::Container),
            capabilities: BTreeMap::from([("driver".to_owned(), "550".to_owned())]),
            isolation: Isolation::Contained,
        },
        wall_limit_secs: 600,
        network: NetworkPolicy::AllowList,
        ports: vec![DeclaredPort {
            host: 8080,
            container: 80,
        }],
        files: crate::JobFiles::Contents(BTreeMap::from([(
            "inputs/upload".to_owned(),
            b"upload-target".to_vec(),
        )])),
        labels: Labels::from_caller(&caller)
            .and_then(|labels| labels.with_reserved(ReservedKey::Run, "run-1"))
            .expect("labels"),
        layers: vec![
            Layer::Image {
                image: format!("team/base@{}", digest('d')),
            },
            Layer::HostPath {
                path: "/srv/trees/sdk".to_owned(),
            },
        ],
        mounts: vec![
            JobMount {
                source: MountSource::HostPath {
                    path: "/srv/trees/src".to_owned(),
                },
                target: "/work".to_owned(),
                mode: MountMode::CopyOnWrite,
                sub_path: Some("lib".to_owned()),
                no_exec: true,
                propagation: Propagation::Private,
            },
            JobMount {
                source: MountSource::HostPath {
                    path: "/srv/cache".to_owned(),
                },
                target: "/work/cache".to_owned(),
                mode: MountMode::ReadWrite,
                sub_path: None,
                no_exec: false,
                propagation: Propagation::Private,
            },
        ],
        host_ports: vec![
            HostPort {
                host_port: 7070,
                container_port: Some(7171),
                protocol: PortProtocol::Tcp,
            },
            HostPort {
                host_port: 9000,
                container_port: Some(9000),
                protocol: PortProtocol::Tcp,
            },
        ],
        history_key: Some(
            HistoryKey::parse("root//crates/js-sched:js-sched rust_compile build")
                .expect("a size class"),
        ),
    }
}

/// [`full_spec`], run on the host itself: placing it taints the host.
pub(crate) fn host_spec() -> JobSpec {
    let spec = full_spec();
    JobSpec {
        constraints: Constraints {
            isolation: Isolation::Host,
            ..spec.constraints.clone()
        },
        ..spec
    }
}

/// A spec that needs the whole host.
pub(crate) fn whole_host_spec() -> JobSpec {
    JobSpec {
        needs: Needs::WholeHost,
        ..full_spec()
    }
}

/// What a host reports of a job that stopped: short tails, the resources it used and its CPU
/// series.
pub(crate) fn output() -> Output {
    Output {
        stdout: "done\n".to_owned(),
        stderr: "warning: slow disk\n".to_owned(),
        truncated: false,
        usage: Some(ResourceUsage {
            cpu_time: Duration::from_millis(2500),
            peak_memory_bytes: 64 << 20,
            cpu_throttle: None,
            cpu_pressure_some: None,
            cpu_peak: None,
        }),
        logs: None,
        cpu_series: Some(series()),
        memory_series: None,
    }
}

/// The CPU series [`output`] carries: two points at 2 s a point.
pub(crate) fn series() -> CpuSeries {
    CpuSeries::new(2, vec![16, 40], 3_000_000, false).expect("a well-formed series")
}

/// An exit with `exit_code` and [`output`].
pub(crate) fn exited(exit_code: i32) -> JobEnd {
    JobEnd::Exited {
        exit_code,
        output: output(),
    }
}

/// A cancel with no exit code and no output, as of a job that never ran.
pub(crate) fn cancelled() -> JobEnd {
    JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    }
}

/// A start body for the job `job` and the allocation `allocation`, as the agent protocol reads
/// one: the fields it requires, and every other at its default.
pub(crate) fn start_body(job: &str, allocation: &str) -> js_agent_wire::JobStartSpec {
    let text = format!(
        r#"{{"job_id":"{job}","allocation_id":"{allocation}","image":"{}","booked":{{"cpu_millis":2000,"memory_bytes":1073741824}},"wall_ms":600000}}"#,
        digest('a')
    );
    serde_json::from_str(&text).expect("a start body")
}

/// The share of a host [`Rig::placed`] books.
pub(crate) fn share() -> Booked {
    Booked::Shape {
        cpu_millis: 2000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 32,
        accelerators: 1,
    }
}
