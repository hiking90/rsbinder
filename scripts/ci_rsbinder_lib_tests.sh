#!/usr/bin/env bash
# Run every `rsbinder` lib unit test a runner without a binder device can run.
#
# `--features rpc` compiles a superset of the default and `--no-default-features`
# lib test sets, so one run covers them all, and a new module is covered without
# a workflow line of its own. The tests below open `/dev/binderfs/binder` (or
# create a binderfs device) and are skipped by exact name, since hermetic tests
# share their modules; integration-test.yml runs them through
# `cargo test --package rsbinder` on a runner that has binderfs. A new device
# test fails here with `ENOENT` until it is added to this list.
#
# Usage: ci_rsbinder_lib_tests.sh
set -euo pipefail

binder_device=(
    binderfs::tests::test_add_device
    process_state::tests::test_concurrent_strong_proxy_case_b_resurrection
    process_state::tests::test_concurrent_strong_proxy_same_handle_returns_same_arc
    process_state::tests::test_native_dedup_reserves_against_concurrent_removal
    process_state::tests::test_native_dedup_same_arc
    process_state::tests::test_native_distinct_arcs_get_distinct_ids
    process_state::tests::test_native_lookup_does_not_change_counts
    process_state::tests::test_native_uaf_window_closed
    process_state::tests::test_process_state
    process_state::tests::test_process_state_context_object
    process_state::tests::test_process_state_disable_background_scheduling
    process_state::tests::test_process_state_start_thread_pool
    process_state::tests::test_process_state_strong_proxy_for_handle
    process_state::tests::test_strong_proxy_under_same_thread_dead_binder_no_deadlock
    tests::process_state
    thread_state::tests::nested_kernel_transaction_answers_for_the_kernel_caller
    thread_state::tests::test_clear_and_restore_calling_identity_round_trip
    thread_state::tests::test_get_calling_inside_transaction_extracts_fields
    thread_state::tests::test_get_calling_outside_transaction_returns_defaults
    thread_state::tests::test_get_calling_sid_null_secctx_returns_none
    thread_state::tests::test_process_pending_derefs_handles_reentrant_push_from_drop
    thread_state::tests::test_strict_mode_policy_round_trip
)

skip=()
for name in "${binder_device[@]}"; do
    skip+=(--skip "$name")
done

out="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/rsbinder_lib_tests.out"
cargo test -p rsbinder --features rpc --lib -- --exact "${skip[@]}" 2>&1 | tee "$out"
# A zero-test run exits 0 too; require at least one pass.
grep -qE 'test result: ok\. [1-9][0-9]* passed' "$out"
