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
#       death link on the sink and on the source exist for. The
#       consumer's link is made inside the `oneway onStart` handler, on
#       a binder thread, which only a real driver exercises.
#
# On this machine, against `rsb_hub`:
#
#   cargo build -p rsbinder-tools --bin rsb_hub
#   cargo build -p tests --bin stream_probe
#   ./tests/scripts/run_stream_ac.sh
#
# Starts and stops its own hub, so run it with nothing else holding
# handle 0; it refuses to start while any `target/debug/rsb_hub` runs.
#
# On an Android device or emulator, against the platform's own
# servicemanager and its driver:
#
#   cargo ndk -t x86_64-linux-android -p 35 build -p tests --bin stream_probe
#   ./tests/scripts/run_stream_ac.sh --adb [-s <serial>] [--target <triple>]
#
# Needs an image where `adb root` works: registering a service from the
# shell is refused under enforcing SELinux, so the script goes permissive
# for the run and restores the mode it found.
set -u

cd "$(dirname "$0")/../.." || exit 1

MODE=host
SERIAL=
TARGET=x86_64-linux-android
while [ $# -gt 0 ]; do
    case "$1" in
        --adb) MODE=adb ;;
        -s) SERIAL=$2; shift ;;
        --target) TARGET=$2; shift ;;
        *) echo "usage: $0 [--adb [-s <serial>] [--target <triple>]]"; exit 2 ;;
    esac
    shift
done

OUT=/tmp/rsb107-out
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

# Both modes supply the same four operations, so the gates below are
# written once:
#   probe <secs> <args…>   run the probe to completion, stdout = its output
#   start <name>           serve <name>; prints a handle for `stop`
#   stop <handle>          kill -9 that one service
#   cleanup                whatever this script started

if [ "$MODE" = host ]; then
    HUB=./target/debug/rsb_hub
    PROBE=./target/debug/stream_probe
    LOG=/tmp/rsb107-hub.log

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

    # `timeout`: a synchronous kernel transaction blocks in ioctl until a
    # reply or BR_DEAD_REPLY, so a regression hangs rather than fails.
    probe() { local secs=$1; shift; timeout "$secs" $PROBE "$@"; }

    start() {
        local log="/tmp/rsb107-svc-$1.log"
        # Truncated here, not in the background job: that redirection runs
        # in the forked child, so the poll below could otherwise match a
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

    stop() { kill -9 "$1" 2>/dev/null; }
else
    ADB=(adb)
    [ -n "$SERIAL" ] && ADB=(adb -s "$SERIAL")
    DEV=/data/local/tmp/rsb107-ac
    BIN=target/$TARGET/debug/stream_probe
    [ -f "$BIN" ] || { echo "missing $BIN — see the header for the build command"; exit 1; }

    # Every `adb shell` gets `</dev/null`: it reads stdin otherwise, and
    # inside a loop or a `$(…)` that silently eats the caller's input.
    dev() { "${ADB[@]}" shell "$@" </dev/null | tr -d '\r'; }

    "${ADB[@]}" root >/dev/null 2>&1; sleep 2; "${ADB[@]}" wait-for-device
    [ "$(dev id -u)" = 0 ] || { echo "adb root did not take; use an image that allows it"; exit 1; }
    SELINUX_WAS=$(dev getenforce)
    dev setenforce 0

    cleanup() {
        # `-x` on the comm name: `pkill -f` would match the adb shell
        # that carries the pattern on its own command line.
        dev "pkill -9 -x stream_probe; rm -rf $DEV" >/dev/null 2>&1
        [ "${SELINUX_WAS:-}" = Enforcing ] && dev setenforce 1
    }
    trap cleanup EXIT

    dev "pkill -9 -x stream_probe; rm -rf $DEV; mkdir -p $DEV" >/dev/null 2>&1
    "${ADB[@]}" push "$BIN" "$DEV/stream_probe" >/dev/null || { echo "push failed"; exit 1; }
    dev chmod 755 "$DEV/stream_probe"

    probe() { local secs=$1; shift; dev "cd $DEV && timeout $secs ./stream_probe $*"; }

    start() {
        # The host-side `adb shell` is what goes to the background; a job
        # backgrounded inside the device shell dies with that shell.
        # `exec` keeps the pid written to the file the probe's own.
        "${ADB[@]}" shell "cd $DEV && echo \$\$ > $1.pid && RUST_LOG=info exec ./stream_probe serve $1 > $1.log 2>&1" </dev/null >/dev/null 2>&1 &
        disown 2>/dev/null
        for _ in $(seq 1 20); do
            if [ -n "$(dev "grep '^SERVING $1' $DEV/$1.log 2>/dev/null")" ]; then
                dev cat "$DEV/$1.pid"
                return 0
            fi
            sleep 0.5
        done
        echo "service $1 did not start" >&2; dev cat "$DEV/$1.log" >&2
        return 1
    }

    stop() { dev kill -9 "$1" >/dev/null 2>&1; }
fi

# $1 = service name. Each case gets a service of its own: the probe keeps
# one producer's counters, so two streams through one service would read
# each other's.

note "AC-7.1/7.2 (kernel)  a long stream arrives in order, paced by credit"
if start rsb107.bulk > /dev/null; then
    # 200000 integers is 800 KB through 16 KB batches — 49 of them, so
    # the four-batch opening window cannot cover it and every further
    # batch is paid for by a grant. 800 KB is also well past the ~512 KB
    # of asynchronous space the receiver has, which the window is what
    # keeps the stream inside of.
    probe 180 consume rsb107.bulk 200000 16384 4 > "$OUT" 2>/tmp/rsb107-bulk.err
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
    probe 60 die rsb107.holding 3 4 100000 2000 > "$OUT" 2>/tmp/rsb107-holding.err
    if grep -qx 'RESULT die 3' "$OUT"; then
        sleep 2
        probe 60 status rsb107.holding > "$OUT" 2>>/tmp/rsb107-holding.err
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
    # One opening credit, and a consumer that leaves as soon as it has the
    # one item — before it would wait, which is when it grants what it
    # owes. The producer is parked in `wait_credit` with no batch in
    # flight. Nothing can fail, so only the death link on the sink ends
    # this wait — without it the producer thread is there for the life of
    # the process.
    probe 60 die rsb107.parked 1 4 1 0 > "$OUT" 2>/tmp/rsb107-parked.err
    if grep -qx 'RESULT die 1' "$OUT"; then
        sleep 2
        probe 60 status rsb107.parked > "$OUT" 2>>/tmp/rsb107-parked.err
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
SVC=$(start rsb107.orphan)
if [ -n "$SVC" ]; then
    # The mirror of the case above. The producer paces itself at 5 ms an
    # item and holds credit to spare, so the consumer is blocked in
    # `recv` with nothing in flight when the service is killed.
    probe 60 orphan rsb107.orphan > "$OUT" 2>/tmp/rsb107-orphan.err &
    CONSUMER=$!
    sleep 2
    stop "$SVC"
    wait "$CONSUMER"
    if grep -q '^RESULT orphan [0-9]* err:DeadObject$' "$OUT"; then
        ok "the blocked consumer was released with DeadObject: $(cat "$OUT")"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT orphan <n> err:DeadObject')"
    fi
else
    bad "the orphan service did not start"
fi

printf '\n==== Plan 10-7 Phase C (kernel, %s): %d passed, %d failed ====\n' "$MODE" "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
