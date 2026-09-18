#!/usr/bin/env bash
# Plan 10-5 AC-5.1, kernel half: a cancellation transport crossing a real
# kernel binder transaction.
#
# The transport leaves the service as a binder object in a reply and
# comes back in another process as a proxy; the `oneway cancel()` sent to
# that proxy has to reach the signal the service still holds. One process
# cannot show this — a binder resolved inside the process that owns it is
# dispatched in-process and the driver's object marshalling never runs.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin cancel_probe
#   ./tests/scripts/run_cancel_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/cancel_probe
LOG=/tmp/rsb105-hub.log
OUT=/tmp/rsb105-out
SVCLOG=/tmp/rsb105-svc.log
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    pkill -f '[t]arget/debug/cancel_probe' 2>/dev/null
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

# Truncate here, not in the background job: the redirection below runs in
# the forked child, so the poll could otherwise read a `SERVING` line an
# interrupted earlier run left behind.
: > "$SVCLOG"
RUST_LOG=info nohup $PROBE serve rsb105.probe > "$SVCLOG" 2>&1 &
SVC_PID=$!; disown 2>/dev/null
started=0
for _ in $(seq 1 20); do
    grep -q '^SERVING rsb105.probe' "$SVCLOG" && { started=1; break; }
    sleep 0.5
done

note "AC-5.1  a caller cancels through the transport the service handed back"
if [ "$started" = 1 ]; then
    ok "the service registered and is serving"
    # `timeout`: a synchronous kernel transaction blocks in ioctl until a
    # reply or BR_DEAD_REPLY, so a regression would hang rather than fail.
    timeout 60 $PROBE cancel rsb105.probe > "$OUT" 2>/tmp/rsb105-cancel.err
    if grep -qx 'RESULT cancel false true' "$OUT"; then
        ok "not cancelled before, cancelled after — across two processes"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT cancel false true')"
    fi
    # The service must still be serving: a cancel is a notification, not
    # a teardown. A second run gets its own fresh signal, which is also
    # what proves the first `false` was not a stale reading.
    timeout 60 $PROBE cancel rsb105.probe > "$OUT" 2>>/tmp/rsb105-cancel.err
    if grep -qx 'RESULT cancel false true' "$OUT"; then
        ok "a second job gets a fresh signal and cancels too"
    else
        bad "second run: got '$(cat "$OUT")'"
    fi
else
    bad "the service did not start"; cat "$SVCLOG"
fi

printf '\n==== Plan 10-5 AC-5.1 (kernel): %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
