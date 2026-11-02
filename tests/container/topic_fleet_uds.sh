#!/usr/bin/env bash
# The topic fleet over Unix sockets, in a container with no binder (plan 15-pubsub-uds U6): every
# node, the relay and the registries included, runs where binder cannot be opened.
#
#   tests/container/topic_fleet_uds.sh [--release]
#
# --release for a runner that has no cores to spare: a debug build's nodes take about three.
# FLEET_SECS sets the chaos layer's length. Exit 77 when docker is not available, unless
# RSB_CONTAINER_REQUIRED=1.
set -euo pipefail

cd "$(dirname "$0")/../.."

exe=$(cargo test "$@" -p tests --features topic-uds --test topic_fleet --no-run 2>&1 |
    sed -n 's/.*Executable .*(\(target[^)]*\)).*/\1/p')
# The node the test binary names, built with the feature; the build above makes it.
cargo build "$@" -p tests --features topic-uds --bin topic_node

script='mkdir -p /tmp/sockets /tmp/fleet && exec "$0" "$@"'
for test in steady_state_fleet scripted_faults chaos_faults; do
    echo "=== $test"
    tests/container/run.sh --no-binder -- env \
        TOPIC_FLEET_SOCKETS=/tmp/sockets TOPIC_FLEET_DIR=/tmp/fleet \
        FLEET_SECS="${FLEET_SECS:-30}" \
        sh -c "$script" "$PWD/$exe" --ignored --exact --test-threads=1 --nocapture "$test"
done
