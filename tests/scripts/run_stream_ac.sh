#!/usr/bin/env bash
# Plan 10-7 Phase C, kernel half: a stream crossing real kernel binder
# transactions.
#
# `rsbinder::stream`'s unit tests dispatch straight into a local object
# and `tests/tests/stream_rpc.rs` runs over a session. Three things live
# only in the driver and are checked here:
#
#   (1) `oneway` batches to one node keep their order, and the terminator
#       that follows them cannot overtake (`binder.c`, `async_todo`);
#   (2) the default window and batch cap fit inside the asynchronous
#       space a receiving process has (half its mapping), so a long
#       stream is paced by credit rather than refused with
#       FAILED_TRANSACTION;
#   (3) a peer whose process is gone ends the stream on both sides —
#       including when nothing is in flight to fail, which is what the
#       death link on the sink and on the source exist for.
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin stream_probe
#   ./tests/scripts/run_stream_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0; it refuses to start while any `target/debug/rsb_hub` runs.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/stream_probe
LOG=/tmp/rsb107-hub.log
OUT=/tmp/rsb107-out
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    pkill -f '[t]arget/debug/stream_probe' 2>/dev/null
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

# $1 = service name. Each case gets a service of its own: the probe keeps
# one producer's counters, so two streams through one service would read
# each other's. Echoes the pid of the serving process.
start() {
    local log="/tmp/rsb107-svc-$1.log"
    # Truncated here, not in the background job: that redirection runs in
    # the forked child, so the poll below could otherwise match a
    # `SERVING` line an interrupted earlier run left behind.
    : > "$log"
    RUST_LOG=info nohup $PROBE serve "$1" > "$log" 2>&1 &
    local pid=$!
    disown 2>/dev/null
    for _ in $(seq 1 20); do
        grep -q "^SERVING $1" "$log" && { echo "$pid"; return 0; }
        sleep 0.5
    done
    echo "service $1 did not start" >&2; cat "$log" >&2
    return 1
}

# `timeout` throughout: a synchronous kernel transaction blocks in ioctl
# until a reply or BR_DEAD_REPLY, so a regression hangs rather than fails.

note "AC-7.1/7.2 (kernel)  a long stream arrives in order, paced by credit"
if start rsb107.bulk > /dev/null; then
    # 200000 integers is 800 KB through 16 KB batches — 49 of them, so
    # the four-batch opening window cannot cover it and every further
    # batch is paid for by a grant. 800 KB is also well past the ~512 KB
    # of asynchronous space the receiver has, which the window is what
    # keeps the stream inside of.
    timeout 180 $PROBE consume rsb107.bulk 200000 16384 4 > "$OUT" 2>/tmp/rsb107-bulk.err
    if grep -qx 'RESULT consume 200000 true ok' "$OUT"; then
        ok "200000 items, in order, clean terminator"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT consume 200000 true ok')"
    fi
else
    bad "the bulk service did not start"
fi

note "AC-7.4 (kernel)  a consumer that dies is seen by a producer holding credit"
if start rsb107.holding > /dev/null; then
    # Four-byte batches, 100000 opening credits and 2 ms between items:
    # when the consumer goes, the producer still has credit, so its next
    # batch is what reports the death.
    timeout 60 $PROBE die rsb107.holding 3 4 100000 2000 > "$OUT" 2>/tmp/rsb107-holding.err
    if grep -qx 'RESULT die 3' "$OUT"; then
        sleep 2
        timeout 60 $PROBE status rsb107.holding > "$OUT" 2>>/tmp/rsb107-holding.err
        if grep -q 'finished=true dead=true' "$OUT"; then
            ok "the producer's next batch reported DeadObject and it stopped"
        else
            bad "got '$(cat "$OUT")' (want finished=true dead=true)"
        fi
    else
        bad "the consumer did not take three items: '$(cat "$OUT")'"
    fi
else
    bad "the holding service did not start"
fi

note "AC-7.4 (kernel)  a consumer that dies is seen by a producer parked for credit"
if start rsb107.parked > /dev/null; then
    # One opening credit and nothing to replenish it: the producer is
    # parked in `wait_credit` with no batch in flight. Nothing can fail,
    # so only the death link on the sink ends this wait — without it the
    # producer thread is there for the life of the process.
    timeout 60 $PROBE die rsb107.parked 1 4 1 0 > "$OUT" 2>/tmp/rsb107-parked.err
    if grep -qx 'RESULT die 1' "$OUT"; then
        sleep 2
        timeout 60 $PROBE status rsb107.parked > "$OUT" 2>>/tmp/rsb107-parked.err
        if grep -q 'finished=true dead=true' "$OUT"; then
            ok "the parked producer was released with DeadObject"
        else
            bad "got '$(cat "$OUT")' (want finished=true dead=true)"
        fi
    else
        bad "the consumer did not take an item: '$(cat "$OUT")'"
    fi
else
    bad "the parked service did not start"
fi

note "AC-7.4 (kernel)  a producer that dies is seen by a blocked consumer"
SVC_PID=$(start rsb107.orphan)
if [ -n "$SVC_PID" ]; then
    # The mirror of the case above. The producer paces itself at 5 ms an
    # item and holds credit to spare, so the consumer is blocked in
    # `recv` with nothing in flight when the service is killed.
    timeout 60 $PROBE orphan rsb107.orphan > "$OUT" 2>/tmp/rsb107-orphan.err &
    CONSUMER=$!
    sleep 2
    kill -9 "$SVC_PID" 2>/dev/null
    wait "$CONSUMER"
    if grep -q '^RESULT orphan [0-9]* err:DeadObject$' "$OUT"; then
        ok "the blocked consumer was released with DeadObject: $(cat "$OUT")"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT orphan <n> err:DeadObject')"
    fi
else
    bad "the orphan service did not start"
fi

printf '\n==== Plan 10-7 Phase C (kernel): %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
