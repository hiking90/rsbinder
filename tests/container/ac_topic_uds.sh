#!/bin/sh
# Topics over a Unix socket between real uids (plan 15-pubsub-uds §7). Runs as root inside
# `run.sh --no-binder --as-root`, which gives this script no binder device, and lowers each
# process to a uid of its own with setpriv. Not named run_*_ac.sh: the host gate and CI run
# that glob as it is, and this one needs the container.
#
#   tests/container/run.sh --no-binder --as-root -- tests/container/ac_topic_uds.sh TARGET HUBS
#
# TARGET holds topic_uds_probe (`cargo build -p tests --features topic-uds`) and HUBS rsb_hub and
# rsb_service built with the `topic` and `rpc` features.
set -u

TARGET=${1:-target/debug}
HUBS=${2:-target/uds/debug}
PROBE=$PWD/$TARGET/topic_uds_probe
HUB_BIN=$PWD/$HUBS/rsb_hub
SERVICE_BIN=$PWD/$HUBS/rsb_service
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
    # Always in `$(...)` or `&`, a shell of its own to replace: the pid `$!` gives is the probe.
    exec setpriv --reuid="$uid" --regid="$uid" --clear-groups -- "$@"
}
expect() { # WHAT WANTED OUTPUT
    if printf '%s\n' "$3" | grep -qF "$2"; then
        ok "$1"
    else
        bad "$1: wanted '$2' in: $3"
    fi
}
same() { # WHAT WANTED GOT
    if [ -n "$2" ] && [ "$2" = "$3" ]; then
        ok "$1"
    else
        bad "$1: wanted '$2', got '$3'"
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

echo "=== a hub of uid 1000, started with umask 0077 and a low descriptor limit"
HUB_UID=1000
RUN=/run/rsbinder
SOCKET=$RUN/topic.sock
CONFIG=/etc/rsbinder/hub.d
mkdir -p "$RUN" "$CONFIG"
chown "$HUB_UID" "$RUN"
chmod 0755 "$RUN"
# Owned by root, as the hub's trust check asks; uid 1001 publishes, 1002 subscribes, 1003 neither.
cat >"$CONFIG/10-topics.toml" <<'TOML'
[global]
topics = { uid = [1002] }

[[topic]]
name = "sensors.*"
advertise = { uid = [1001] }
subscribe = { uid = [1002] }

[[topic]]
name = "open.*"
advertise = "any"
subscribe = "any"
TOML
start_hub() {
    (
        ulimit -Sn 256
        [ -z "${HUB_HARD_LIMIT:-}" ] || ulimit -Hn "$HUB_HARD_LIMIT"
        umask 0077
        exec setpriv --reuid="$HUB_UID" --regid="$HUB_UID" --clear-groups -- \
            env RUST_LOG=warn "$HUB_BIN" --topic-socket "$SOCKET" --config "$CONFIG" "$@"
    ) >>"$DIR/hub.log" 2>&1 &
    HUB=$!
    tries=0
    # The directory stays between hubs and comes before the bind; a hub that ended took its socket.
    while [ "$(stat -c %a "$SOCKET" 2>/dev/null)" != 666 ] && [ "$tries" -lt 1000 ]; do
        tries=$((tries + 1))
        sleep 0.01
    done
}
start_hub

same "the hub socket is 0666 whatever the umask" 666 "$(stat -c %a "$SOCKET")"
same "the hub's directory is private" 700 "$(stat -c %a "$RUN/topic.d")"
same "the hub raised its descriptor limit" "$(awk '/open files/ { print $5 }' "/proc/$HUB/limits")" \
    "$(awk '/open files/ { print $4 }' "/proc/$HUB/limits")"
case "$(cat "$DIR/hub.log")" in
*NameNotFound*) bad "a socket hub warned of the kernel registry's rule: $(cat "$DIR/hub.log")" ;;
*) ok "a socket hub with no [[rule]] does not warn of the kernel registry's rule" ;;
esac

out=$(as_uid 1003 "$PROBE" advertise:"$SOCKET":sensors.a:hello 2>&1)
expect "the policy refuses an advertise by uid" "RESULT advertise sensors.a SECURITY" "$out"

as_uid 1001 "$PROBE" advertise:"$SOCKET":sensors.a:hello say:serving hold:60 >"$DIR/hub_pub.log" 2>&1 &
HUB_PUBLISHER=$!
tries=0
while ! grep -q serving "$DIR/hub_pub.log"; do
    tries=$((tries + 1))
    [ "$tries" -lt 1000 ] || break
    sleep 0.01
done
expect "a publisher of an allowed uid advertises" "RESULT advertise sensors.a OK" "$(cat "$DIR/hub_pub.log")"
same "the directory holds no socket file" 0 "$(find "$RUN/topic.d" -mindepth 1 | wc -l)"

out=$(as_uid 1002 "$PROBE" named:"$SOCKET":sensors.a nrecv 2>&1)
expect "a subscriber of another uid receives over the ring" "RESULT nrecv hello OK" "$out"

out=$(as_uid 1003 "$PROBE" lookup:"$SOCKET":sensors.a 2>&1)
expect "the policy gives a refused subscriber no descriptor" "RESULT lookup sensors.a NULL" "$out"

out=$(as_uid 1002 "$PROBE" list:"$SOCKET":sensors 2>&1)
expect "a uid [global] topics allows lists" "LISTED sensors.a 1001" "$out"
out=$(as_uid 1003 "$PROBE" list:"$SOCKET":sensors 2>&1)
expect "a uid it does not is refused" "RESULT list sensors SECURITY" "$out"

out=$(as_uid 1002 "$SERVICE_BIN" topics --topic-socket "$SOCKET" 2>&1)
expect "rsb_service asks a hub on a socket, with no binder device" "sensors.a" "$out"

out=$(as_uid 1002 "$PROBE" lookup:"$SOCKET":sensors.a peer 2>&1)
expect "who listens is the publisher, not the hub" "PEER 1001 " "$out"

out=$(as_uid 1002 "$PROBE" named:"$SOCKET":sensors.a:1001 nrecv 2>&1)
expect "a publisher of the expected uid is subscribed to" "RESULT nrecv hello OK" "$out"
out=$(as_uid 1002 "$PROBE" named:"$SOCKET":sensors.a:1003 nrecv 2>&1)
expect "a publisher of another uid is not" "refused the subscription: Security" "$out"

echo "=== who holds a name"
as_uid 1001 "$PROBE" advertise:"$SOCKET":open.a:one say:serving hold:60 >"$DIR/open_pub.log" 2>&1 &
OPEN_PUBLISHER=$!
tries=0
while ! grep -q serving "$DIR/open_pub.log"; do
    tries=$((tries + 1))
    [ "$tries" -lt 1000 ] || break
    sleep 0.01
done
out=$(as_uid 1003 "$PROBE" advertise:"$SOCKET":open.a:two 2>&1)
expect "a name another uid holds is refused" "RESULT advertise open.a TAKEN" "$out"
out=$(as_uid 1003 "$PROBE" advertise:"$SOCKET":open.other:two 2>&1)
expect "another name is not" "RESULT advertise open.other OK" "$out"
tries=0
while [ "$tries" -lt 500 ]; do
    out=$(as_uid 1003 "$PROBE" lookup:"$SOCKET":open.other 2>&1)
    case "$out" in *NULL*) break ;; esac
    tries=$((tries + 1))
    sleep 0.01
done
expect "a publisher that ended has taken its name back" "RESULT lookup open.other NULL" "$out"

echo "=== a publisher's end"
as_uid 1002 "$PROBE" lookup:"$SOCKET":open.a say:looked wait:"$DIR/open_killed" connect \
    >"$DIR/connect.log" 2>&1 &
CONNECT=$!
tries=0
while ! grep -q looked "$DIR/connect.log"; do
    tries=$((tries + 1))
    [ "$tries" -lt 1000 ] || break
    sleep 0.01
done
kill -KILL "$OPEN_PUBLISHER" 2>/dev/null
wait "$OPEN_PUBLISHER" 2>/dev/null
touch "$DIR/open_killed"
wait "$CONNECT"
expect "the hub keeps no copy of a socket whose publisher died" "RESULT connect - ECONNREFUSED" \
    "$(cat "$DIR/connect.log")"
tries=0
while [ "$tries" -lt 500 ]; do
    out=$(as_uid 1002 "$PROBE" lookup:"$SOCKET":open.a 2>&1)
    case "$out" in *NULL*) break ;; esac
    tries=$((tries + 1))
    sleep 0.01
done
expect "a publisher's death takes its name back" "RESULT lookup open.a NULL" "$out"

echo "=== a second hub on the socket"
# A hub that is not refused serves for good: past the deadline it is a failure, not a hang.
second=$(timeout 10 setpriv --reuid="$HUB_UID" --regid="$HUB_UID" --clear-groups -- \
    "$HUB_BIN" --topic-socket "$SOCKET" --config "$CONFIG" 2>&1)
status=$?
if [ "$status" != 0 ] && [ "$status" != 124 ]; then
    ok "a second hub is refused"
else
    bad "a second hub, exit $status: $second"
fi
expect "the refusal is the directory's, before the socket's path" "in use by another host" "$second"
out=$(as_uid 1002 "$PROBE" named:"$SOCKET":sensors.a nrecv 2>&1)
expect "the first goes on" "RESULT nrecv hello OK" "$out"

kill "$HUB_PUBLISHER" 2>/dev/null
wait "$HUB_PUBLISHER" 2>/dev/null

echo "=== what a hub on a socket does not use"
cat >"$CONFIG/20-services.toml" <<TOML
[global]
list = "any"

[[rule]]
name = "*"
add = "any"
find = "any"

[[service]]
name = "com.example.IFoo/default"
start = { exec = ["/usr/bin/true"] }
TOML
: >"$DIR/hub.log"
kill -HUP "$HUB"
tries=0
while ! grep -q 'does not use' "$DIR/hub.log" && [ "$tries" -lt 1000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
log=$(cat "$DIR/hub.log")
expect "a reload warns of [[rule]]" "1 [[rule]]" "$log"
expect "a reload warns of [[service]], and of its start" "whose \`start\` is not run" "$log"
expect "a reload warns of [global] list" "[global] list" "$log"
out=$(as_uid 1002 "$PROBE" list:"$SOCKET":sensors 2>&1)
expect "the hub goes on with its topics" "RESULT list sensors OK" "$out"
kill "$HUB" 2>/dev/null
wait "$HUB" 2>/dev/null

: >"$DIR/hub.log"
start_hub
tries=0
while ! grep -q 'does not use' "$DIR/hub.log" && [ "$tries" -lt 1000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
# Past the bind, or the file this judges is not the one a SIGTERM takes: a hub that answers is.
tries=0
until as_uid 1002 "$PROBE" list:"$SOCKET":none 2>&1 | grep -q 'RESULT list none OK'; do
    tries=$((tries + 1))
    [ "$tries" -lt 200 ] || break
    sleep 0.05
done
if [ "$tries" -lt 200 ]; then ok "a hub with what it does not use serves"; else bad "a hub with what it does not use never answered"; fi
kill "$HUB" 2>/dev/null
wait "$HUB" 2>/dev/null
expect "a start warns as a reload does" "does not use" "$(cat "$DIR/hub.log")"
if [ -e "$SOCKET" ]; then bad "a hub that ended left its socket"; else ok "a hub that ends takes its socket's file"; fi
rm -f "$CONFIG/20-services.toml"

echo "=== a directory that is not the hub's own"
SHARED=$DIR/shared.d
mkdir -m 0700 "$SHARED"
chown "$HUB_UID" "$SHARED"
: >"$SHARED/kept.conf"
kept=$(timeout 10 setpriv --reuid="$HUB_UID" --regid="$HUB_UID" --clear-groups -- \
    "$HUB_BIN" --topic-socket "$DIR/shared.sock" --config "$CONFIG" 2>&1)
status=$?
if [ "$status" != 0 ] && [ "$status" != 124 ]; then
    ok "a hub refuses a directory with anything else in it"
else
    bad "a hub took a directory with something else in it, exit $status: $kept"
fi
expect "the refusal names what it found" "kept.conf" "$kept"
if [ -e "$SHARED/kept.conf" ]; then ok "and removes nothing"; else bad "the hub removed what it found"; fi
rm -rf "$SHARED" "$DIR/shared.sock"

echo "=== who connects cannot take the descriptors of what the hub has"
FRESH=$DIR/fresh/parent
setpriv --reuid="$HUB_UID" --regid="$HUB_UID" --clear-groups -- sh -c \
    "umask 0077; exec '$HUB_BIN' --topic-socket '$FRESH/t.sock' --config '$CONFIG'" >/dev/null 2>&1 &
FRESH_HUB=$!
tries=0
while [ ! -e "$FRESH/t.sock" ] && [ "$tries" -lt 1000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
kill "$FRESH_HUB" 2>/dev/null
wait "$FRESH_HUB" 2>/dev/null
same "a parent it makes can be reached whatever the umask" "755 755" \
    "$(stat -c %a "$DIR/fresh") $(stat -c %a "$FRESH")"
rm -rf "$DIR/fresh"

rm -f "$SOCKET" "$DIR/flooded"
HUB_HARD_LIMIT=512
: >"$DIR/hub.log"
start_hub
tries=0
until as_uid 1002 "$PROBE" list:"$SOCKET":none 2>&1 | grep -q 'RESULT list none OK'; do
    tries=$((tries + 1))
    [ "$tries" -lt 200 ] || break
    sleep 0.05
done
same "this hub has 512 descriptors" 512 "$(awk '/open files/ { print $4 }' "/proc/$HUB/limits")"
as_uid 1001 "$PROBE" advertise:"$SOCKET":sensors.first:one say:serving wait:"$DIR/flooded" \
    readvertise:sensors.second:two hold:60 >"$DIR/early.log" 2>&1 &
EARLY=$!
tries=0
while ! grep -q serving "$DIR/early.log"; do
    tries=$((tries + 1))
    [ "$tries" -lt 1000 ] || break
    sleep 0.01
done
as_uid 1003 "$PROBE" flood:"$SOCKET":300 hold:60 >"$DIR/flood.log" 2>&1 &
FLOOD=$!
tries=0
while ! grep -q 'RESULT flood' "$DIR/flood.log" && [ "$tries" -lt 3000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
flooded=$(sed -n 's/RESULT flood \([0-9]*\) OK/\1/p' "$DIR/flood.log")
# 512 descriptors take 51 serve workers, one to a session, and the earlier session has one.
if [ -n "$flooded" ] && [ "$flooded" -ge 40 ] && [ "$flooded" -le 50 ]; then
    ok "a uid that asked for 300 sessions got $flooded"
else
    bad "the sessions a uid got: '$flooded'"
fi
touch "$DIR/flooded"
tries=0
while ! grep -q 'RESULT advertise sensors.second' "$DIR/early.log" && [ "$tries" -lt 1000 ]; do
    tries=$((tries + 1))
    sleep 0.01
done
expect "a session the hub has still gets a descriptor from it" "RESULT advertise sensors.second OK" \
    "$(cat "$DIR/early.log")"
kill "$FLOOD" "$EARLY" "$HUB" 2>/dev/null
wait "$FLOOD" 2>/dev/null
wait "$EARLY" 2>/dev/null
wait "$HUB" 2>/dev/null

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
