#!/usr/bin/env bash
# The session bus between two uids (plan 20 E1a): a name one uid registered under the rule
# `user/{uid}/*` is not found, listed, watched or replaced by another uid, root included.
#
#   tests/container/ac_session_uid.sh
#
# `run_hub_policy_ac.sh` runs as one uid, so it refuses another uid's names by their spelling
# only; this has the name registered for real. Two uids need a container, as root. Exit 77 when
# docker is not available, unless RSB_CONTAINER_REQUIRED=1. Needs
# target/debug/{rsb_device,rsb_hub,rsb_service,ac61_probe}.
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
want() { # WHAT LINE
    if grep -qxF "$2" <<<"$OUT"; then ok "$1"; else bad "$1 (no '$2')"; fi
}
reject() { # WHAT LINE
    if grep -qxF "$2" <<<"$OUT"; then bad "$1 (has '$2')"; else ok "$1"; fi
}

# Each probe line comes out as `<who> <line>`, so one container serves the whole script.
OUT=$("$RUN" --as-root -- sh -c '
    HUB=target/debug/rsb_hub
    SVC=target/debug/rsb_service
    PROBE=target/debug/ac61_probe
    as_uid() { uid=$1; shift; setpriv --reuid="$uid" --regid="$uid" --clear-groups -- "$@"; }
    tagged() { sed "s/^/$1 /"; }
    # Not under /tmp: rsb_hub refuses a configuration a world-writable directory holds.
    CFG=/run/rsb-session-ac
    mkdir -p "$CFG"
    # The catch-all grants everything, so a refusal below is the namespace rule'"'"'s.
    printf "%s\n" "[global]" "list = \"any\"" "" \
        "[[rule]]" "name = \"user/{uid}/*\"" "add = \"any\"" "find = \"any\"" "" \
        "[[rule]]" "name = \"*\"" "add = \"any\"" "find = \"any\"" >"$CFG/10-rules.toml"
    $HUB --config "$CFG" 2>/tmp/hub.err &
    hub=$!
    n=0
    until $SVC check manager >/dev/null 2>&1; do
        n=$((n + 1))
        [ "$n" -lt 100 ] || { echo "STEP hub: DOWN $(tr "\n" " " </tmp/hub.err)"; exit 1; }
        sleep 0.1
    done
    echo "STEP hub: up"

    as_uid 1000 $PROBE sadd:player hold:30 >/tmp/owner.out 2>/dev/null &
    owner=$!
    n=0
    until grep -q "^RESULT hold" /tmp/owner.out; do
        n=$((n + 1)); [ "$n" -lt 100 ] || break; sleep 0.1
    done
    tagged owner </tmp/owner.out

    as_uid 1000 $PROBE sfind:player find:user/1000/player slist 2>/dev/null | tagged same
    as_uid 1001 $PROBE find:user/1000/player sfind:player notify:user/1000/player \
        add:user/1000/player add:user/1000/other sadd:player slist list 2>/dev/null | tagged other
    $PROBE find:user/1000/player add:user/1000/player list 2>/dev/null | tagged root
    as_uid 1000 $PROBE sfind:player 2>/dev/null | tagged after

    kill "$owner" 2>/dev/null; wait "$owner" 2>/dev/null
    kill -TERM "$hub"; wait "$hub"
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

want "uid 1000 adds a session name" "owner RESULT sadd player OK"
want "another process of uid 1000 finds it on the session bus" "same RESULT sfind player OK"
want "and under the name the hub holds" "same RESULT find user/1000/player OK"
want "its session list has the name" "same SLIST player"

want "uid 1001 does not find uid 1000's name" "other RESULT find user/1000/player NOTFOUND"
want "its own session has no such name" "other RESULT sfind player NOTFOUND"
want "it cannot watch the name" "other RESULT notify user/1000/player SECURITY"
want "it cannot replace the name" "other RESULT add user/1000/player SECURITY"
want "nor add another under uid 1000" "other RESULT add user/1000/other SECURITY"
want "it adds the same session name for itself" "other RESULT sadd player OK"
want "its session list has its own" "other SLIST player"
reject "its list of every name leaves uid 1000's out" "other LIST user/1000/player"
want "and has its own" "other LIST user/1001/player"

want "root does not find uid 1000's name" "root RESULT find user/1000/player NOTFOUND"
want "root cannot replace it" "root RESULT add user/1000/player SECURITY"
reject "root's list leaves it out" "root LIST user/1000/player"

want "uid 1000's name is still its own binder" "after RESULT sfind player OK"

echo "==== session uid AC: $PASS passed, $FAIL failed ===="
[ "$FAIL" = 0 ] || { echo "$OUT"; exit 1; }
