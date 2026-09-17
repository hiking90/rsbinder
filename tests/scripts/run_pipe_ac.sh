#!/usr/bin/env bash
# Plan 10-3 AC-3.1, kernel half: 4 MB through a pipe whose fd crossed a
# real kernel binder transaction.
#
# The parcel carries one descriptor; the payload goes through the kernel
# pipe. Two processes are what make that a stream — in one process the
# binder is dispatched in-process and nothing is proven.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin pipe_probe
#   ./tests/scripts/run_pipe_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/pipe_probe
LOG=/tmp/rsb103-hub.log
OUT=/tmp/rsb103-out
SVCLOG=/tmp/rsb103-svc.log
FOUR_MB=$((4 * 1024 * 1024))
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    pkill -f '[t]arget/debug/pipe_probe' 2>/dev/null
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

# Truncate before forking: the redirection below runs in the child, so
# the poll could otherwise read a `SERVING` line an interrupted earlier
# run left behind.
: > "$SVCLOG"
RUST_LOG=info nohup $PROBE serve rsb103.probe > "$SVCLOG" 2>&1 &
disown 2>/dev/null
started=0
for _ in $(seq 1 20); do
    grep -q '^SERVING rsb103.probe' "$SVCLOG" && { started=1; break; }
    sleep 0.5
done

note "AC-3.1  4 MB through a pipe whose fd crossed a kernel transaction"
if [ "$started" = 1 ]; then
    ok "the service registered and is serving"
    # `timeout`: a stalled reader or a writer that never fills would hang
    # instead of failing — the pipe buffer is ~64 KB, so both sides have
    # to make progress for this to finish at all.
    timeout 120 $PROBE read rsb103.probe "$FOUR_MB" > "$OUT" 2>/tmp/rsb103-read.err
    if grep -q "^RESULT pipe $FOUR_MB OK " "$OUT"; then
        ok "4 MB arrived intact ($(cat "$OUT"))"
    else
        bad "got '$(cat "$OUT")'"
    fi
    # A second stream on the same service: the first pipe's fds must not
    # have leaked into it, and the service is still serving.
    timeout 120 $PROBE read rsb103.probe 65536 > "$OUT" 2>>/tmp/rsb103-read.err
    if grep -q '^RESULT pipe 65536 OK ' "$OUT"; then
        ok "a second stream works too"
    else
        bad "second stream: got '$(cat "$OUT")'"
    fi
else
    bad "the service did not start"; cat "$SVCLOG"
fi

printf '\n==== Plan 10-3 AC-3.1 (kernel): %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
