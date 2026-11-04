#!/usr/bin/env bash
# What tests/container/run.sh promises (plan 6-7 X1, plan 15-pubsub-uds §4.9).
#
#   tests/container/ac_run.sh
#
# Exit 77 when docker is not available, unless RSB_CONTAINER_REQUIRED=1. The runs with a device
# need target/debug/rsb_device and a kernel with binderfs; without those they are skipped by name.
set -u

cd "$(dirname "$0")/../.." || exit 1
RUN=tests/container/run.sh
# This run's containers alone: another run.sh may be running beside this one.
export RSB_CONTAINER_RUN=ac-run-$$
LABEL=rsbinder.test.container.run=$RSB_CONTAINER_RUN
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
same() { # WHAT WANTED GOT
    if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: wanted '$2', got '$3'"; fi
}
running_container() { # the name, once this run's one container is up; nothing after 10 s
    local tries=0 name=
    until name=$(docker ps --filter "label=$LABEL" --format '{{.Names}}'); [ -n "$name" ]; do
        tries=$((tries + 1))
        [ "$tries" -lt 500 ] || break
        sleep 0.02
    done
    echo "$name"
}

"$RUN" --no-binder -- true
status=$?
if [ "$status" = 77 ]; then
    echo "docker is not available"
    exit 77
fi
same "exit code of true" 0 "$status"
"$RUN" --no-binder -- false
same "exit code of false" 1 $?
"$RUN" --no-binder -- sh -c 'exit 42'
same "any other exit code" 42 $?

same "--uid" 1000 "$("$RUN" --no-binder --uid 1000 -- id -u)"
same "--as-root" 0 "$("$RUN" --no-binder --as-root -- id -u)"
same "the default is the repository's owner" "$(stat -c %u .)" "$("$RUN" --no-binder -- id -u)"

out=$("$RUN" --no-binder --as-root -- sh -c '
    [ -e /dev/binderfs ] && echo device
    mkdir /tmp/b; mount -t binder binder /tmp/b 2>/dev/null && echo mounted
    grep -q "^Seccomp:[[:space:]]*2" /proc/self/status || echo no-seccomp
    echo done')
same "--no-binder: no device, no mount, seccomp on" done "$out"

"$RUN" --no-binder -- sh -c 'touch "$PWD/.rsb-container-probe" 2>/dev/null && exit 1; exit 0'
same "the repository is read-only" 0 $?

# Far longer than the wait below: only the trap can end this run in time, not the command.
"$RUN" --no-binder -- sleep 300 >/dev/null 2>&1 &
runner=$!
running_container >/dev/null
kill -TERM "$runner"
tries=0
while kill -0 "$runner" 2>/dev/null && [ "$tries" -lt 3000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
if kill -0 "$runner" 2>/dev/null; then
    bad "a signalled run ends at once: it still ran after 30 s"
    docker ps -q --filter "label=$LABEL" | xargs -r docker rm -f >/dev/null
else
    ok "a signalled run ends at once"
fi
wait "$runner"
same "a signalled run ends as the signal says" 143 $?
same "no container left after a run that was signalled" "" \
    "$(docker ps -a --filter "label=$LABEL" --format '{{.Names}}')"

"$RUN" --no-binder --as-root -- sh -c 'exit 3'
same "no container left after a failed command" "" \
    "$(docker ps -a --filter "label=$LABEL" --format '{{.Names}}')"

# The command ends as a `sleep`, which reaps nothing: only a PID 1 above it can reap the orphan.
"$RUN" --no-binder -- sh -c 'sh -c "sleep 0.1 >/dev/null 2>&1 &"; exec sleep 300' >/dev/null 2>&1 &
runner=$!
name=$(running_container)
sleep 1
out=$(docker exec "$name" sh -c 'grep -l "^State:[[:space:]]*Z" /proc/[0-9]*/status 2>/dev/null
    echo done')
same "an orphan does not stay a zombie" done "$out"
kill -TERM "$runner"
wait "$runner"

if [ -x target/debug/rsb_device ] && grep -q binder /proc/filesystems; then
    device_mode=$(stat -c '%a %t:%T' /dev/binderfs/binder 2>/dev/null)
    out=$("$RUN" -- sh -c 'test -w /dev/binderfs/binder && id -u' | tail -1)
    same "a device of its own, as the repository's owner" "$(stat -c %u .)" "$out"
    same "the host's device is as it was" "$device_mode" \
        "$(stat -c '%a %t:%T' /dev/binderfs/binder 2>/dev/null)"
    out=$("$RUN" --uid 1000 -- id -u | tail -1)
    same "a device and --uid" 1000 "$out"
    # Without the device rule: a setup failure, not a test result.
    rule=$(sed -e '/--device-cgroup-rule/d' "$RUN")
    copy=tests/container/.run-mutant.$$.sh
    printf '%s\n' "$rule" >"$copy"
    bash "$copy" -- true >/dev/null 2>&1
    same "a device that cannot be made is a setup failure" 125 $?
    rm -f "$copy"
else
    echo "  SKIP  the runs with a device: no target/debug/rsb_device or no binderfs"
fi

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
