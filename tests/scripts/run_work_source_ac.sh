#!/usr/bin/env bash
# Plan 10-9 AC-9.1/9.2, kernel half: the work source crossing real kernel
# binder transactions.
#
# The value rides in the request header, so one process sets it and
# another reads it; the rule that a received value is not forwarded unless
# the handler sets it again takes a third process. The RPC half is
# `tests/tests/work_source_rpc.rs`.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin work_source_probe
#   ./tests/scripts/run_work_source_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0; it refuses to start while any `target/debug/rsb_hub` runs.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/work_source_probe
LOG=/tmp/rsb109-hub.log
OUT=/tmp/rsb109-out
FAIL=0

cleanup() {
    pkill -f '[t]arget/debug/work_source_probe' 2>/dev/null
    [ -n "${HUB_PID:-}" ] && kill "$HUB_PID" 2>/dev/null
}
trap cleanup EXIT
if pgrep -f '[t]arget/debug/rsb_hub' >/dev/null; then
    echo "an rsb_hub is already running; stop it first (this script starts its own)"
    exit 1
fi
cleanup; sleep 1

for b in "$HUB" "$PROBE"; do
    [ -x "$b" ] || { echo "missing $b — see the header for the build commands"; exit 1; }
done

RUST_LOG=info nohup $HUB --insecure-allow-all > "$LOG" 2>&1 &
HUB_PID=$!; disown 2>/dev/null; sleep 2
kill -0 "$HUB_PID" 2>/dev/null || { echo "hub did not start"; cat "$LOG"; exit 1; }

# $1 = service name, $2 = optional next service. Waits for `SERVING`.
start() {
    local log="/tmp/rsb109-svc-$1.log"
    : > "$log"
    RUST_LOG=info nohup $PROBE serve "$@" > "$log" 2>&1 &
    disown 2>/dev/null
    for _ in $(seq 1 20); do
        grep -q "^SERVING $1" "$log" && return 0
        sleep 0.5
    done
    echo "service $1 did not start"; cat "$log"
    return 1
}

start rsb109.back && start rsb109.front rsb109.back || exit 1

# `timeout`: a synchronous kernel transaction blocks in ioctl until a reply
# or BR_DEAD_REPLY, so a regression would hang rather than fail.
timeout 60 $PROBE check rsb109.front > "$OUT" 2>/tmp/rsb109-check.err
cat "$OUT"
grep -qx 'RESULT ok' "$OUT" || FAIL=1

printf '\n==== Plan 10-9 AC-9.1/9.2 (kernel): %s ====\n' "$([ $FAIL -eq 0 ] && echo PASS || echo FAIL)"
[ "$FAIL" -eq 0 ]
