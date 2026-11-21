#!/usr/bin/env bash
# rsb_hub on a device that had a service manager of another uid (plan 6-7 O6).
#
#   tests/container/ac_hub_uid.sh
#
# The kernel keeps a binder device to the euid of its first context manager, so this needs two
# uids and a device of its own: a container, as root. Exit 77 when docker is not available,
# unless RSB_CONTAINER_REQUIRED=1. Needs target/debug/{rsb_device,rsb_hub,rsb_service}.
set -u

cd "$(dirname "$0")/../.." || exit 1
RUN=tests/container/run.sh
PASS=0
FAIL=0

ok() {
    PASS=$((PASS + 1))
    echo "  PASS  $*"
}
bad() {
    FAIL=$((FAIL + 1))
    echo "  FAIL  $*"
}
want() { # WHAT PATTERN
    if grep -q "$2" <<<"$OUT"; then ok "$1"; else bad "$1 (no '$2')"; fi
}

# Every step prints one `STEP name: ...` line, so one container serves the whole script.
OUT=$("$RUN" --as-root -- sh -c '
    HUB=target/debug/rsb_hub
    SVC=target/debug/rsb_service
    as_user() { setpriv --reuid=1000 --regid=1000 --clear-groups -- "$@"; }
    up() { # wait for a hub to answer; 10 s
        n=0
        until $SVC check manager >/dev/null 2>&1; do
            n=$((n + 1)); [ "$n" -lt 100 ] || return 1; sleep 0.1
        done
    }
    stop() { kill -TERM "$1"; wait "$1"; }

    $HUB --insecure-allow-all 2>/tmp/root-hub.err &
    pid=$!
    up && echo "STEP root hub: up" || echo "STEP root hub: DOWN $(cat /tmp/root-hub.err)"
    as_user $HUB --insecure-allow-all >/tmp/busy.out 2>&1
    echo "STEP while it runs: rc=$? $(tr "\n" " " </tmp/busy.out)"
    stop "$pid"

    as_user $HUB --insecure-allow-all >/tmp/perm.out 2>&1
    echo "STEP after it stopped: rc=$? $(tr "\n" " " </tmp/perm.out)"

    $HUB --insecure-allow-all 2>/tmp/root-hub.err &
    pid=$!
    up && echo "STEP root again: up" || echo "STEP root again: DOWN $(cat /tmp/root-hub.err)"
    stop "$pid"

    rm /dev/binderfs/binder && target/debug/rsb_device binder --mode 0666 >/dev/null 2>&1
    echo "STEP device made again: rc=$?"
    as_user $HUB --insecure-allow-all 2>/tmp/user-hub.err &
    pid=$!
    if up; then echo "STEP user hub on the new device: up"; else
        echo "STEP user hub on the new device: DOWN $(tr "\n" " " </tmp/user-hub.err)"
    fi
    kill -TERM "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    echo "STEP done"
' 2>&1)
status=$?
if [ "$status" = 77 ]; then
    echo "docker is not available"
    exit 77
fi
if [ "$status" != 0 ] || ! grep -q "^STEP done" <<<"$OUT"; then
    echo "$OUT"
    echo "the container run failed (exit $status)"
    exit 1
fi

want "a root hub starts on a new device" "^STEP root hub: up"
want "another uid while it runs: exit 1" "^STEP while it runs: rc=1 "
want "and is told one is running" "^STEP while it runs: .*one is running on"
want "another uid after it stopped: exit 1" "^STEP after it stopped: rc=1 "
want "and is told no hub runs, the device keeps the first uid" \
    "^STEP after it stopped: .*No service manager is running on /dev/binderfs/binder"
want "with its own uid" "^STEP after it stopped: .*rsb_hub runs as uid 1000"
want "the first uid starts again" "^STEP root again: up"
want "the advice works: the device removed and made again" "^STEP device made again: rc=0"
want "takes the other uid" "^STEP user hub on the new device: up"

echo "==== hub uid AC: $PASS passed, $FAIL failed ===="
[ "$FAIL" = 0 ] || { echo "$OUT"; exit 1; }
