#!/usr/bin/env bash
# The topic fleet over Unix sockets, in a container with no binder (plan 15-pubsub-uds U6): every
# node, the relay and the registries included, runs where binder cannot be opened.
#
#   tests/container/topic_fleet_uds.sh [--release]
#
# --release for a runner that has no cores to spare: a debug build's nodes take about three.
# FLEET_SECS sets the chaos layer's length, and every other FLEET_* reaches the test as set here.
# Exit 77 when docker is not available, unless RSB_CONTAINER_REQUIRED=1.
set -euo pipefail

cd "$(dirname "$0")/../.."

# No color, as CI's CARGO_TERM_COLOR would put an escape between `Executable` and its space.
built=$(cargo test "$@" -p tests --features topic-uds --test topic_fleet --no-run \
    --color never 2>&1) || {
    printf '%s\n' "$built" >&2
    exit 1
}
exe=$(printf '%s\n' "$built" | sed -n 's/.*Executable .*(\(target[^)]*\)).*/\1/p')
# The node the test binary names, built with the feature; the build above makes it.
cargo build "$@" -p tests --features topic-uds --bin topic_node

script='mkdir -p /tmp/sockets && exec "$0" "$@"'
mapfile -t fleet < <(env | sed -n '/^FLEET_SECS=/d; /^FLEET_/p')
# Each run's own, as another may run beside it. On the host, as a run's files say why it failed
# after its container is gone.
runs=$PWD/target/tmp/topic-fleet-uds/$$
log=$runs.log
mkdir -p "$runs"
for test in steady_state_fleet scripted_faults chaos_faults; do
    echo "=== $test"
    tests/container/run.sh --no-binder --rw "$runs" -- env \
        TOPIC_FLEET_SOCKETS=/tmp/sockets TOPIC_FLEET_DIR="$runs" \
        FLEET_SECS="${FLEET_SECS:-30}" "${fleet[@]}" \
        sh -c "$script" "$PWD/$exe" --ignored --exact --test-threads=1 --nocapture "$test" |
        tee "$log"
    # Zero tests is a pass to cargo: a name that changed or an `#[ignore]` removed would be one.
    grep -q 'test result: ok\. 1 passed' "$log" || {
        echo "topic_fleet_uds.sh: $test did not run" >&2
        exit 1
    }
done
rm -rf "$log" "$runs"
