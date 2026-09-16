#!/usr/bin/env bash
# Plan 10-1 AC-1.4: the receive mapping is what bounds a transaction.
#
# A 3 MB payload must reach a service that mapped 4 MB and must be refused
# for one that kept the ~1 MB default. Both halves need real kernel binder
# and two processes — the driver is the only thing that enforces the size,
# and it is only reached when sender and receiver are different processes.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin mmap_probe
#   ./tests/scripts/run_mmap_size_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding handle 0.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/mmap_probe
LOG=/tmp/rsb101-hub.log
OUT=/tmp/rsb101-out
SVCLOG=/tmp/rsb101-svc.log
PAYLOAD=$((3 * 1024 * 1024))
BIG=$((4 * 1024 * 1024))
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    pkill -f '[t]arget/debug/mmap_probe' 2>/dev/null
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

SVC_PID=""
# start_service <bytes|default> <name>
start_service() {
    RUST_LOG=info nohup $PROBE serve "$1" "$2" > "$SVCLOG" 2>&1 &
    SVC_PID=$!; disown 2>/dev/null
    for _ in $(seq 1 20); do
        grep -q "^SERVING $2 " "$SVCLOG" && return 0
        sleep 0.5
    done
    return 1
}
stop_service() {
    [ -n "$SVC_PID" ] && kill "$SVC_PID" 2>/dev/null
    SVC_PID=""
    sleep 1
}
# want <expected-outcome> <label>
want() {
    if grep -qx "RESULT call $PAYLOAD $1" "$OUT"; then ok "$2"
    else bad "$2 (got '$(cat "$OUT")')"; fi
}

######################################################################
note "AC-1.4a  a service that mapped 4 MB receives a 3 MB payload"
if start_service "$BIG" rsb101.big; then
    got=$(sed -n 's/^SERVING rsb101.big mmap=//p' "$SVCLOG")
    [ "$got" = "$BIG" ] && ok "the service reports the mapping it asked for ($got)" \
                        || bad "mapping is $got, want $BIG"
    $PROBE call "$PAYLOAD" rsb101.big > "$OUT" 2>/tmp/rsb101-call.err
    want OK "3 MB accepted"
else
    bad "the 4 MB service did not start"; cat "$SVCLOG"
fi
stop_service

######################################################################
note "AC-1.4b  the same payload is refused by a default-sized service"
if start_service default rsb101.small; then
    got=$(sed -n 's/^SERVING rsb101.small mmap=//p' "$SVCLOG")
    [ "$got" -lt "$PAYLOAD" ] && ok "the default mapping ($got) is smaller than the payload" \
                              || bad "default mapping is $got, expected below $PAYLOAD"
    $PROBE call "$PAYLOAD" rsb101.small > "$OUT" 2>/tmp/rsb101-call.err
    want FAILEDTXN "3 MB refused, and refused as FailedTransaction"
    # The refusal must not take the service down with it: the driver
    # rejects the transaction, it does not kill the target.
    kill -0 "$SVC_PID" 2>/dev/null && ok "the service survived the refusal" \
                                   || bad "the service died on the oversized transaction"
else
    bad "the default-sized service did not start"; cat "$SVCLOG"
fi
stop_service

printf '\n==== Plan 10-1 AC-1.4: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
