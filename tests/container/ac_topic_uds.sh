#!/bin/sh
# Topics over a Unix socket between real uids (plan 15-pubsub-uds §7). Runs as root inside
# `run.sh --no-binder --as-root`, which gives this script no binder device, and lowers each
# process to a uid of its own with setpriv. Not named run_*_ac.sh: the host gate and CI run
# that glob as it is, and this one needs the container.
#
#   tests/container/run.sh --no-binder --as-root -- tests/container/ac_topic_uds.sh TARGET_DIR
#
# TARGET_DIR holds topic_uds_probe, built with `cargo build -p tests --features topic-uds`.
set -u

TARGET=${1:-target/debug}
PROBE=$PWD/$TARGET/topic_uds_probe
DIR=/tmp/ac
mkdir -p "$DIR"
chmod 0777 "$DIR"
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
as_uid() {
    uid=$1
    shift
    setpriv --reuid="$uid" --regid="$uid" --clear-groups -- "$@"
}
expect() { # WHAT WANTED OUTPUT
    if printf '%s\n' "$3" | grep -qF "$2"; then
        ok "$1"
    else
        bad "$1: wanted '$2' in: $3"
    fi
}
wait_file() {
    tries=0
    while [ ! -e "$1" ]; do
        tries=$((tries + 1))
        [ "$tries" -lt 1000 ] || return 1
        sleep 0.01
    done
}

echo "=== no binder device here"
if [ -e /dev/binderfs ] || [ -e /dev/binder ]; then
    bad "a binder device in the container"
else
    ok "no /dev/binder"
fi
if mkdir -p /tmp/nobinder && mount -t binder binder /tmp/nobinder 2>/dev/null; then
    bad "binderfs mounts, so this run proves nothing"
else
    ok "binderfs does not mount"
fi

echo "=== a publisher of uid 1001 and subscribers of other uids (expect_uid)"
SOCK=$DIR/plain.sock
as_uid 1001 "$PROBE" publish:"$SOCK":hello "say:serving" hold:30 >"$DIR/publisher.log" 2>&1 &
PUBLISHER=$!
tries=0
while ! grep -q serving "$DIR/publisher.log"; do
    tries=$((tries + 1))
    [ "$tries" -lt 1000 ] || break
    sleep 0.01
done

out=$(as_uid 1002 "$PROBE" subscribe:"$SOCK" recv 2>&1)
expect "uid 1002 subscribes to a publisher of uid 1001" "RESULT subscribe $SOCK OK" "$out"
expect "uid 1002 receives over the ring" "RESULT recv hello OK" "$out"

out=$(as_uid 1002 "$PROBE" subscribe:"$SOCK":1001 recv 2>&1)
expect "expect_uid takes the publisher's own uid" "RESULT subscribe $SOCK OK" "$out"

out=$(as_uid 1002 "$PROBE" subscribe:"$SOCK":1003 2>&1)
expect "expect_uid refuses another uid" "RESULT subscribe $SOCK SECURITY" "$out"

kill "$PUBLISHER" 2>/dev/null
wait "$PUBLISHER" 2>/dev/null

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
