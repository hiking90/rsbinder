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
    local built
    # No color, as CI's CARGO_TERM_COLOR would put an escape between `Executable` and its space.
    built=$("$@" --no-run --color never 2>&1) || {
        printf '%s\n' "$built" >&2
        return 1
    }
    printf '%s\n' "$built" | sed -n 's/.*Executable .*(\(target[^)]*\)).*/\1/p'
}

cargo build -p tests --features topic-uds --bin topic_uds_probe
# A target dir of its own: these features would replace the rsb_hub every other script runs.
CARGO_TARGET_DIR=target/uds cargo build -p rsbinder-tools --features topic,rpc \
    --bin rsb_hub --bin rsb_service
# Not a process substitution: its exit status goes unseen, and a test that does not build would
# leave nothing to run and nothing to fail.
topic_tests=$(executables cargo test -p rsbinder-topic --features uds --lib --test uds)
fd_tests=$(executables cargo test -p rsbinder --features rpc --test rpc_unix_fd)
mapfile -t tests <<<"$topic_tests"$'\n'"$fd_tests"
if [ "${#tests[@]}" -ne 3 ]; then
    echo "topic_uds.sh: wanted 3 test binaries, got: ${tests[*]}" >&2
    exit 1
fi
for test in "${tests[@]}"; do
    echo "=== $test"
    set +e
    timeout --foreground 600 "$RUN" --no-binder -- "$PWD/$test"
    status=$?
    set -e
    [ "$status" = 0 ] || exit "$status"
done
# A stuck hub or probe is a failure, not a runner held for hours: `run.sh`'s trap ends it.
# `--foreground` keeps it in this group, or a terminal's Ctrl-C would not reach that trap.
timeout --foreground 600 "$RUN" --no-binder --as-root -- "$PWD/tests/container/ac_topic_uds.sh" target/debug target/uds/debug
