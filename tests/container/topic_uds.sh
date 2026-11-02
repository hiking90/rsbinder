#!/usr/bin/env bash
# Topics over a Unix socket in a container with no binder (plan 15-pubsub-uds §7): the tests that
# say nothing there needs a device, and the AC between real uids. It builds on the host and runs
# the binaries in the container, which mounts the repository read-only.
#
#   tests/container/topic_uds.sh
#
# Exit 77 when docker is not available, unless RSB_CONTAINER_REQUIRED=1.
set -euo pipefail

cd "$(dirname "$0")/../.."
RUN=tests/container/run.sh

executables() {
    "$@" --no-run 2>&1 | sed -n 's/.*Executable .*(\(target[^)]*\)).*/\1/p'
}

cargo build -p tests --features topic-uds --bin topic_uds_probe
mapfile -t tests < <(
    executables cargo test -p rsbinder-topic --features uds --lib --test uds
    executables cargo test -p rsbinder --features rpc --test rpc_unix_fd
)
for test in "${tests[@]}"; do
    echo "=== $test"
    set +e
    "$RUN" --no-binder -- "$PWD/$test"
    status=$?
    set -e
    [ "$status" = 0 ] || exit "$status"
done
"$RUN" --no-binder --as-root -- "$PWD/tests/container/ac_topic_uds.sh" target/debug
