#!/usr/bin/env bash
# Plan 10-7b P3, kernel half: a stream whose endpoint crosses a real
# kernel binder transaction and whose items cross a shared ring.
#
# `rsbinder::stream`'s unit tests put both ends in one process and
# `tests/tests/stream_rpc.rs` runs over a session. What only two
# processes and a real driver show is checked here:
#
#   (1) the ring's memfd crosses as a file descriptor in the endpoint —
#       in an argument for a download, in a reply for an upload — and the
#       producer maps what the consumer allocated and sealed
#       (AC-7b.1, AC-7b.4 upload);
#   (2) the ring is the whole of the flow control: a producer stops at
#       the ring's capacity and moves again on the consumer's futex wake,
#       and nothing but the opening call goes through the receiver's
#       binder mapping, so a page of it carries twelve streams
#       (AC-7b.2, AC-7b.3);
#   (3) a peer whose process is gone ends the stream on both sides —
#       including when nothing is in flight to fail, which is what the
#       death links exist for. The consumer's link on an upload is made
#       inside the `upload` handler, on a binder thread, which only a
#       real driver exercises (AC-7b.4). A cancel is the same wait ended
#       by the ring's own flag bit instead (AC-7b.5);
#   (4) on an enforcing SELinux device the endpoint crosses a domain
#       boundary and the other domain maps the ring (AC-7b.8, `--adb`
#       only).
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
# Needs a userdebug image: `adb root` for the cases that register a
# service from the shell (refused under enforcing SELinux, so the script
# goes permissive for them and restores the mode it found), and the `su`
# binary for AC-7b.8, which runs enforcing with the service in the `su`
# domain and the client in `shell` — see that case for why.
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

OUT=/tmp/rsb107b-out
PASS=0; FAIL=0; SKIP=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
skip() { SKIP=$((SKIP+1)); printf '  SKIP  %s\n' "$*"; }

# Both modes supply the same four operations, so the gates below are
# written once:
#   probe <secs> <args…>   run the probe to completion, stdout = its output
#   start <name>           serve <name>; prints a handle for `stop`
#   stop <handle>          kill -9 that one service
#   cleanup                whatever this script started

if [ "$MODE" = host ]; then
    HUB=./target/debug/rsb_hub
    PROBE=./target/debug/stream_probe
    LOG=/tmp/rsb107b-hub.log

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
    # reply or BR_DEAD_REPLY, and a ring wait is a futex with no bound,
    # so a regression hangs rather than fails.
    probe() { local secs=$1; shift; timeout "$secs" $PROBE "$@"; }

    start() {
        local log="/tmp/rsb107b-svc-$1.log"
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
    DEV=/data/local/tmp/rsb107b-ac
    BIN=target/$TARGET/debug/stream_probe
    [ -f "$BIN" ] || { echo "missing $BIN — see the header for the build command"; exit 1; }

    # Every `adb shell` gets `</dev/null`: it reads stdin otherwise, and
    # inside a loop or a `$(…)` that silently eats the caller's input.
    dev() { "${ADB[@]}" shell "$@" </dev/null | tr -d '\r'; }

    # `adb root` / `adb unroot` restart adbd, which also ends every shell
    # it was carrying — a service started before the switch is gone
    # after it. Each waits for the device to come back.
    as_root()  { "${ADB[@]}" root   >/dev/null 2>&1; sleep 3; "${ADB[@]}" wait-for-device; }
    as_shell() { "${ADB[@]}" unroot >/dev/null 2>&1; sleep 3; "${ADB[@]}" wait-for-device; }

    as_root
    [ "$(dev id -u)" = 0 ] || { echo "adb root did not take; use an image that allows it"; exit 1; }
    SELINUX_WAS=$(dev getenforce)
    dev setenforce 0

    cleanup() {
        # Back to root first: AC-7b.8 leaves adbd unrooted, and only root
        # can kill the `su` service and put the SELinux mode back.
        as_root
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

    # $1 = name, $2 = a command prefix that runs the service in another
    # domain (`su root sh -c` for AC-7b.8), empty otherwise.
    start_as() {
        local prefix=${2:-}
        # The host-side `adb shell` is what goes to the background; a job
        # backgrounded inside the device shell dies with that shell.
        # `exec` keeps the pid written to the file the probe's own.
        if [ -n "$prefix" ]; then
            "${ADB[@]}" shell "$prefix 'cd $DEV && echo \$\$ > $1.pid && RUST_LOG=info exec ./stream_probe serve $1 > $1.log 2>&1'" </dev/null >/dev/null 2>&1 &
        else
            "${ADB[@]}" shell "cd $DEV && echo \$\$ > $1.pid && RUST_LOG=info exec ./stream_probe serve $1 > $1.log 2>&1" </dev/null >/dev/null 2>&1 &
        fi
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
    start() { start_as "$1"; }

    stop() { dev kill -9 "$1" >/dev/null 2>&1; }
fi

# Poll a `status` line for a pattern: the gate's predicate is readable
# directly, and how long a death notification takes to reach a binder
# thread is the device's business, not a sleep's.
#   status_reaches <name> <pattern>   -> 0 once seen; the last line is in $OUT
status_reaches() {
    for _ in $(seq 1 20); do
        probe 60 status "$1" > "$OUT" 2>>"/tmp/rsb107b-$1.err"
        grep -q "$2" "$OUT" && return 0
        sleep 0.5
    done
    return 1
}

# $1 = service name. Each case gets a service of its own: the probe keeps
# one producer's counters, so two streams through one service would read
# each other's.

note "AC-7b.1  a long stream arrives in order through a small ring"
if start rsb107b.bulk > /dev/null; then
    # 200000 integers are 1.6 MB of records through a 64 KB ring: the
    # ring wraps about 25 times, and the producer stops and restarts on
    # the consumer's wakes throughout.
    probe 180 consume rsb107b.bulk 200000 65536 > "$OUT" 2>/tmp/rsb107b-bulk.err
    if grep -qx 'RESULT consume 200000 true ok' "$OUT"; then
        ok "200000 items, in order, clean end"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT consume 200000 true ok')"
    fi
else
    bad "the bulk service did not start"
fi

note "AC-7b.2  a consumer that stops reading stops the producer at the ring's capacity"
if start rsb107b.pause > /dev/null; then
    # A 4096-byte ring holds (4096 - 256) / 8 = 480 eight-byte records.
    # The consumer takes 100 items and stops; the producer must come to
    # rest with exactly 580 sent, unfinished, and then finish the 20000
    # once reading resumes. No clock decides it: the probe polls the
    # service's counter for that value.
    probe 120 pause rsb107b.pause 20000 4096 100 > "$OUT" 2>/tmp/rsb107b-pause.err
    if grep -qx 'RESULT pause 20000 true parked=true ok' "$OUT"; then
        ok "the producer stopped at 100 + 480 and finished after the consumer resumed"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT pause 20000 true parked=true ok')"
    fi
else
    bad "the pause service did not start"
fi

note "AC-7b.3  twelve streams into a consumer with a one-page binder mapping"
if start rsb107b.crowd > /dev/null; then
    # 12 × 20000 items, default 64 KB rings, and the consumer's binder
    # mapping at the smallest size `?mmap=` accepts. Every item goes
    # through a ring; the mapping carries twelve `subscribe` replies.
    probe 180 crowd rsb107b.crowd 12 20000 > "$OUT" 2>/tmp/rsb107b-crowd.err
    if grep -qx 'RESULT crowd 12 240000 true ok' "$OUT"; then
        ok "12 streams, 240000 items, in order, nothing refused"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT crowd 12 240000 true ok')"
    fi
else
    bad "the crowd service did not start"
fi

note "AC-7b.4  a consumer that dies is seen by a producer with room to write"
if start rsb107b.holding > /dev/null; then
    # A 64 KB ring and 2 ms between items: the ring is never near full,
    # so when the consumer goes the producer is between sends, and it is
    # the next `send` that reports the death.
    probe 60 die rsb107b.holding 3 65536 2000 > "$OUT" 2>/tmp/rsb107b-holding.err
    if grep -qx 'RESULT die 3' "$OUT"; then
        if status_reaches rsb107b.holding 'finished=true dead=true'; then
            ok "the producer's next send reported DeadObject and it stopped"
        else
            bad "got '$(cat "$OUT")' (want finished=true dead=true)"
        fi
    else
        bad "the consumer did not take three items: '$(cat "$OUT")'"
    fi
else
    bad "the holding service did not start"
fi

note "AC-7b.4  a consumer that dies is seen by a producer parked on a full ring"
if start rsb107b.parked > /dev/null; then
    # A 512-byte ring (32 records) and no pacing: the consumer takes one
    # item and leaves, and the producer is in the futex wait for room
    # with nothing in flight. Nothing can fail, so only the death link on
    # the sink ends this wait — without it the producer thread is there
    # for the life of the process.
    probe 60 die rsb107b.parked 1 512 0 > "$OUT" 2>/tmp/rsb107b-parked.err
    if grep -qx 'RESULT die 1' "$OUT"; then
        if status_reaches rsb107b.parked 'finished=true dead=true'; then
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

note "AC-7b.5  a consumer that cancels releases a producer parked on a full ring"
if start rsb107b.cancel > /dev/null; then
    # Same ring as above, but the consumer drops its receiver instead of
    # dying: the `CANCEL` bit on the ring's flag word ends the producer's
    # wait, and its next `send` reports InvalidOperation.
    probe 60 cancel rsb107b.cancel 1 512 > "$OUT" 2>/tmp/rsb107b-cancel.err
    if grep -qx 'RESULT cancel 1' "$OUT"; then
        if status_reaches rsb107b.cancel 'finished=true dead=false canceled=true'; then
            ok "the parked producer was released with InvalidOperation"
        else
            bad "got '$(cat "$OUT")' (want finished=true dead=false canceled=true)"
        fi
    else
        bad "the consumer did not take an item: '$(cat "$OUT")'"
    fi
else
    bad "the cancel service did not start"
fi

note "AC-7b.4  a producer that dies is seen by a consumer parked on an empty ring"
SVC=$(start rsb107b.orphan)
if [ -n "$SVC" ]; then
    # The mirror of the case above. The producer paces itself at 5 ms an
    # item into a 64 KB ring, so the consumer is parked on an empty ring
    # with nothing in flight when the service is killed.
    probe 60 orphan rsb107b.orphan > "$OUT" 2>/tmp/rsb107b-orphan.err &
    CONSUMER=$!
    # Waited on the service's own counter, not a clock: the gate below
    # asks for at least one item, and a fixed sleep decides that gate on
    # an `adb` round trip or a cold device. `sent` counts records the
    # producer has committed to the ring, which the consumer reads before
    # it can see the death.
    for _ in $(seq 1 40); do
        probe 20 status rsb107b.orphan 2>/dev/null | grep -q 'sent=[1-9]' && break
        sleep 0.5
    done
    stop "$SVC"
    wait "$CONSUMER"
    # At least one item: `[0-9]*` would also pass a run where no record
    # ever arrived and only the death link fired.
    if grep -q '^RESULT orphan [1-9][0-9]* err:DeadObject$' "$OUT"; then
        ok "the blocked consumer was released with DeadObject: $(cat "$OUT")"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT orphan <n> err:DeadObject')"
    fi
else
    bad "the orphan service did not start"
fi

note "AC-7b.4 (upload)  the ring crosses in a reply and the client pushes"
if start rsb107b.upload > /dev/null; then
    # The service allocates a 4096-byte ring inside the `upload` handler
    # and returns it; the client maps it and pushes 5000 items through
    # its 480 records.
    probe 120 upload rsb107b.upload 5000 4096 > "$OUT" 2>/tmp/rsb107b-upload.err
    if grep -qx 'RESULT upload 5000 true ok' "$OUT"; then
        ok "5000 items uploaded, in order, clean end"
    else
        bad "got '$(cat "$OUT")' (want 'RESULT upload 5000 true ok')"
    fi
else
    bad "the upload service did not start"
fi

note "AC-7b.4 (upload)  a producer that dies with the endpoint in hand is seen by the consumer"
if start rsb107b.vanish > /dev/null; then
    # The client takes the endpoint and exits before it maps the ring.
    # The service's consumer is parked on the empty ring it just made,
    # and the death link — made in the handler, on the token the client
    # passed — is the only thing that can release it.
    probe 60 vanish rsb107b.vanish 0 > "$OUT" 2>/tmp/rsb107b-vanish.err
    if grep -qx 'RESULT vanish 0' "$OUT"; then
        if status_reaches rsb107b.vanish 'up_finished=true up_dead=true'; then
            ok "the consumer was released with DeadObject: $(cat "$OUT")"
        else
            bad "got '$(cat "$OUT")' (want up_finished=true up_dead=true)"
        fi
    else
        bad "the client did not get an endpoint: '$(cat "$OUT")'"
    fi
else
    bad "the vanish service did not start"
fi

note "AC-7b.4 (upload)  a producer that dies mid-stream is seen after its items"
if start rsb107b.vanish2 > /dev/null; then
    # Three records are in the ring and no end follows them: the consumer
    # takes the three, parks, and is released by the death link.
    probe 60 vanish rsb107b.vanish2 3 > "$OUT" 2>/tmp/rsb107b-vanish2.err
    if grep -qx 'RESULT vanish 3' "$OUT"; then
        if status_reaches rsb107b.vanish2 'up_received=3 up_ordered=true up_finished=true up_dead=true'; then
            ok "three items, then DeadObject: $(cat "$OUT")"
        else
            bad "got '$(cat "$OUT")' (want up_received=3 … up_finished=true up_dead=true)"
        fi
    else
        bad "the client did not push three items: '$(cat "$OUT")'"
    fi
else
    bad "the vanish2 service did not start"
fi

note "AC-7b.8  an enforcing SELinux device: the endpoint crosses a domain boundary"
if [ "$MODE" != adb ]; then
    skip "SELinux domains are an Android case (run with --adb)"
elif [ "$SELINUX_WAS" != Enforcing ]; then
    skip "the device was not enforcing to begin with ($SELINUX_WAS)"
elif [ -z "$(dev su root id -u 2>/dev/null)" ]; then
    skip "no usable su binary on this image"
else
    # What the platform policy (system/sepolicy, android17-release)
    # allows decides the shape of this case:
    #
    #   - `shell` has no `service_manager add` at all, so under enforcing
    #     no service can be registered from a shell. The service runs in
    #     the `su` domain (permissive on userdebug; `su root` from an
    #     unrooted adb shell transitions there) under a name whose type
    #     `shell` may `find` — an unknown name is `default_android_service`,
    #     which shell.te excludes. `aidl_lazy_test_1` is AOSP's own lazy
    #     service test name and is registered on no shipping image.
    #   - the client is the `shell` domain, enforcing. Both directions
    #     rest on userdebug rules in domain.te: `allow domain su:binder
    #     { call transfer }`, `allow domain su:fd use`, `allow domain
    #     su:memfd_file { getattr read write map }`. A memfd is otherwise
    #     `self` only — a service that hands rings to apps needs a rule
    #     of its own, as audioserver and hal_sensors have for
    #     `appdomain:memfd_file`.
    #
    # The download is the shell consumer's memfd crossing into `su` and
    # being mapped there; the upload is a `su` memfd crossing back into
    # `shell`, where the mapping is the enforced check.
    dev setenforce 1
    as_shell
    if [ "$(dev id -u)" = 0 ]; then
        bad "adb unroot did not take"
    else
        SVC=$(start_as aidl_lazy_test_1 "su root sh -c")
        if [ -n "$SVC" ] && [ "$(dev "ps -o LABEL -p $SVC | tail -1")" = u:r:su:s0 ]; then
            printf '  client domain %s, service domain u:r:su:s0, %s\n' "$(dev id -Z)" "$(dev getenforce)"
            probe 120 consume aidl_lazy_test_1 20000 4096 > "$OUT" 2>/tmp/rsb107b-selinux-down.err
            if grep -qx 'RESULT consume 20000 true ok' "$OUT"; then
                ok "download: a shell ring mapped by a su producer"
            else
                bad "download: got '$(cat "$OUT")' (want 'RESULT consume 20000 true ok')"
                dev "logcat -d -b all 2>/dev/null | grep -i 'avc:.*denied' | tail -5" | sed 's/^/        /'
            fi
            probe 120 upload aidl_lazy_test_1 20000 4096 > "$OUT" 2>/tmp/rsb107b-selinux-up.err
            if grep -qx 'RESULT upload 20000 true ok' "$OUT"; then
                ok "upload: a su ring mapped by a shell producer"
            else
                bad "upload: got '$(cat "$OUT")' (want 'RESULT upload 20000 true ok')"
                dev "logcat -d -b all 2>/dev/null | grep -i 'avc:.*denied' | tail -5" | sed 's/^/        /'
            fi
            dev su root kill -9 "$SVC" >/dev/null 2>&1
        else
            bad "the su service did not start under enforcing"
            dev "su root cat $DEV/aidl_lazy_test_1.log" 2>/dev/null | sed 's/^/        /'
        fi
    fi
    # `cleanup` re-roots and restores the SELinux mode.
fi

printf '\n==== Plan 10-7b P3 (kernel, %s): %d passed, %d failed, %d skipped ====\n' "$MODE" "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" -eq 0 ]
