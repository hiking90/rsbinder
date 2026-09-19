#!/usr/bin/env bash
# Plan 10-9 AC-9.5/9.6, kernel half: a transaction observer around kernel
# binder transactions, including one that makes a binder call of its own.
# The RPC half is `tests/tests/observer_rpc.rs`, with the same line format.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin observer_probe
#   ./tests/scripts/run_observer_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0 — an already running `rsb_hub` is stopped too.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/observer_probe
LOG=/tmp/rsb109-obs-hub.log
SVCLOG=/tmp/rsb109-obs-svc.log
OUT=/tmp/rsb109-obs-out
FAIL=0

cleanup() {
    pkill -f '[t]arget/debug/observer_probe' 2>/dev/null
    pkill -f '[t]arget/debug/rsb_hub' 2>/dev/null
}
trap cleanup EXIT
cleanup; sleep 1

for b in "$HUB" "$PROBE"; do
    [ -x "$b" ] || { echo "missing $b — see the header for the build commands"; exit 1; }
done

RUST_LOG=info nohup $HUB --insecure-allow-all > "$LOG" 2>&1 &
HUB_PID=$!; disown 2>/dev/null; sleep 2
kill -0 "$HUB_PID" 2>/dev/null || { echo "hub did not start"; cat "$LOG"; exit 1; }

: > "$SVCLOG"
RUST_LOG=info nohup $PROBE serve rsb109.obs > "$SVCLOG" 2>&1 &
disown 2>/dev/null
started=0
for _ in $(seq 1 20); do
    grep -q '^SERVING rsb109.obs' "$SVCLOG" && { started=1; break; }
    sleep 0.5
done
[ "$started" = 1 ] || { echo "service did not start"; cat "$SVCLOG"; exit 1; }

# `timeout`: a synchronous kernel transaction blocks in ioctl until a reply
# or BR_DEAD_REPLY, so a deadlock inside the observer would hang, not fail.
timeout 60 $PROBE check rsb109.obs > "$OUT" 2>/tmp/rsb109-obs-check.err
cat "$OUT"
grep -qx 'RESULT ok' "$OUT" || FAIL=1
# The panic is expected; the service must have logged it rather than died.
grep -q 'TransactionObserver::on_transact panicked' "$SVCLOG" \
    || { echo "FAIL the observer's panic was not logged"; FAIL=1; }

printf '\n==== Plan 10-9 AC-9.5/9.6 (kernel): %s ====\n' "$([ $FAIL -eq 0 ] && echo PASS || echo FAIL)"
[ "$FAIL" -eq 0 ]
