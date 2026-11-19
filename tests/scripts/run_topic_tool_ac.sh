#!/usr/bin/env bash
# `rsb_service topic` against a real hub and a publisher of another process
# (plans/15-pubsub-gaps.md §5.3): `stat`, `hz`, `echo` and `record` through the
# hub's topic registry, `stat` of a stopped publisher, and the `--topic-socket`
# path. Needs a Linux host with a writable /dev/binderfs/binder and no rsb_hub
# running: the script starts its own.
#
#   ./tests/scripts/run_topic_tool_ac.sh
#
# It builds `rsb_hub` and `rsb_service` with the `topic` and `rpc` features,
# copied to target/debug/rsb_hub_tool and rsb_service_tool, then both again
# without them, so target/debug/rsb_hub stays the hub every other script runs;
# and `topic_probe`, whose `publish:NAME:VALUE` publishes VALUE every 50 ms.
# The copies go when it ends.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB_TOOL=./target/debug/rsb_hub_tool
SVC_TOOL=./target/debug/rsb_service_tool
PROBE=./target/debug/topic_probe
PASS=0; FAIL=0

note() { SECTION=$*; printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

running=$(pgrep -x rsb_hub; pgrep -x rsb_hub_tool)
if [ -n "$running" ]; then
    echo "an rsb_hub is running (pid $(echo $running)); stop it, this script starts its own" >&2
    exit 1
fi

PLAIN=0
built() { stat -c '%i %z' "target/debug/$1" 2>/dev/null; }
PRE_HUB=$(built rsb_hub); PRE_SVC=$(built rsb_service)
trap '[ "$PLAIN" -eq 1 ] || { [ "$(built rsb_hub)" = "$PRE_HUB" ] || rm -f target/debug/rsb_hub
      [ "$(built rsb_service)" = "$PRE_SVC" ] || rm -f target/debug/rsb_service; }
      rm -f "$HUB_TOOL" "$SVC_TOOL"' EXIT
cargo build -q -p rsbinder-tools --features topic,rpc --bin rsb_hub --bin rsb_service || exit 1
cp -f target/debug/rsb_hub "$HUB_TOOL" && cp -f target/debug/rsb_service "$SVC_TOOL" || exit 1
cargo build -q -p rsbinder-tools --bin rsb_hub --bin rsb_service || exit 1
PLAIN=1
cargo build -q -p tests --bin topic_probe || exit 1

# Not under /tmp: rsb_hub refuses a configuration a world-writable directory holds.
WORK=$(mktemp -d -p "${XDG_RUNTIME_DIR:-${HOME:?}}" rsb-topic-tool-ac.XXXXXX) || exit 1
CFG=$WORK/cfg
LOG=
SECTION=
HUB_PID=
PIDS=()

stop_hub() {
    [ -n "$HUB_PID" ] && kill "$HUB_PID" 2>/dev/null && wait "$HUB_PID" 2>/dev/null
    HUB_PID=
}
# By pid: `pkill -f` would match this script's own shells too.
stop_probes() {
    local pid
    for pid in "${PIDS[@]}"; do
        kill -CONT "$pid" 2>/dev/null; kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    done
    PIDS=()
}
cleanup() {
    stop_probes; stop_hub; rm -f "$HUB_TOOL" "$SVC_TOOL"
    [ "$FAIL" -eq 0 ] || { echo "--- hub log"; tail -50 "$LOG"; echo "--- probe stderr";
        cat "$WORK/probe.err"; } >&2 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# start_hub ARGS...: up once it logs that it serves; retried while a stopped one holds handle 0.
start_hub() {
    local try i
    LOG=$WORK/hub.log
    for try in $(seq 5); do
        : > "$LOG"
        RUST_LOG=info "$HUB_TOOL" "$@" >"$LOG" 2>&1 &
        HUB_PID=$!
        for i in $(seq 100); do
            # `serving on` the binder device, or `serving topics on` the socket.
            grep -qE "rsb_hub: serving (topics )?on" "$LOG" && return 0
            kill -0 "$HUB_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill -0 "$HUB_PID" 2>/dev/null && return 1
        wait "$HUB_PID" 2>/dev/null; HUB_PID=
        sleep 0.2
    done
    return 1
}
config() {
    rm -rf "$CFG"; mkdir -p "$CFG"; chmod 755 "$CFG"
    printf '%s\n' "$(cat tests/policy/permissive.toml)" > "$CFG/10-permissive.toml"
    printf '%s\n' "$(cat tests/policy/topic/permissive-topic.toml)" > "$CFG/20-topic.toml"
    chmod 644 "$CFG"/*.toml
}
# probe OUT ARGS...: in the background, kept until the section ends; LAST is its pid.
LAST=
probe() {
    local out=$1; shift
    : > "$out"
    echo "--- $SECTION: $*" >> "$WORK/probe.err"
    "$PROBE" "$@" >"$out" 2>>"$WORK/probe.err" &
    LAST=$!
    PIDS+=("$LAST")
}
wait_line() {
    local i
    for i in $(seq $(($3 * 20))); do
        grep -qE "$2" "$1" 2>/dev/null && return 0
        sleep 0.05
    done
    return 1
}
# tool OUT ARGS...: rsb_service in the foreground, given 20 s; RC is its exit status.
RC=
tool() {
    local out=$1; shift
    timeout 20 "$SVC_TOOL" "$@" >"$out" 2>"$out.err"; RC=$?
}

######################################################################
note "stat, hz, echo and record of a publisher in another process"
config
if start_hub --config "$CFG"; then
    probe "$WORK/pub" publish:tool.a:v say:up hold
    PUB=$LAST
    wait_line "$WORK/pub" "^SAY up" 10 || bad "the publisher did not start: $(cat "$WORK/pub")"

    tool "$WORK/stat0" topic stat tool.a
    [ "$RC" -eq 0 ] && ok "stat exits 0" || bad "stat exit $RC: $(cat "$WORK/stat0.err")"
    head -1 "$WORK/stat0" | grep -qE '^bytes event topic: ring [0-9]+ bytes, lossy, published [0-9]+ .*, 0 subscribers$' \
        && ok "stat: the topic's line" || bad "stat: $(head -1 "$WORK/stat0")"

    probe "$WORK/sub" subscribe:tool.a hold
    wait_line "$WORK/sub" "^GOT tool.a v" 10 || bad "the subscriber got nothing: $(cat "$WORK/sub")"
    tool "$WORK/stat1" topic stat tool.a
    [ "$(wc -l < "$WORK/stat1")" -eq 2 ] && grep -qE "^  #[0-9]+ uid $(id -u) pid [0-9]+: lag" "$WORK/stat1" \
        && ok "stat: a line for the subscriber" || bad "stat: $(cat "$WORK/stat1")"

    tool "$WORK/hz" topic hz tool.a --count 30
    [ "$RC" -eq 0 ] && ok "hz exits 0 after its count" || bad "hz exit $RC: $(cat "$WORK/hz.err")"
    # 50 ms apart: 20 Hz, within what a loaded host does.
    awk '/ Hz\)/ { gsub(/[()]/, "", $5); if ($5 + 0 >= 5 && $5 + 0 <= 40) f = 1 } END { exit !f }' "$WORK/hz" \
        && ok "hz: about 20 Hz" || bad "hz: $(cat "$WORK/hz")"

    tool "$WORK/echo" topic echo tool.a --count 2
    [ "$RC" -eq 0 ] && ok "echo exits 0" || bad "echo exit $RC: $(cat "$WORK/echo.err")"
    head -1 "$WORK/echo" | grep -qx 'type "bytes", event topic' && ok "echo: the type and kind" \
        || bad "echo: $(head -1 "$WORK/echo")"
    [ "$(grep -cx '1 bytes: 76 "v"' "$WORK/echo")" -eq 2 ] && ok "echo: each record in hex and text" \
        || bad "echo: $(cat "$WORK/echo")"

    tool "$WORK/rec.out" topic record tool.a "$WORK/rec" --count 5
    [ "$RC" -eq 0 ] && ok "record exits 0" || bad "record exit $RC: $(cat "$WORK/rec.out.err")"
    # A header of 4 + 4 + 4 + 4 + 5 bytes, and 5 entries of 8 + 4 + 1.
    head=$(od -An -tx1 -N21 "$WORK/rec" | tr -d ' \n')
    [ "$head" = "52535452010000000000000005000000""6279746573" ] && ok "record: the header" \
        || bad "record header: $head"
    [ "$(stat -c %s "$WORK/rec")" -eq $((21 + 5 * 13)) ] && ok "record: five entries" \
        || bad "record: $(stat -c %s "$WORK/rec") bytes"

    tool "$WORK/svc" topic stat tool.a --service
    [ "$RC" -eq 1 ] && ok "--service: a registry name is no service (exit 1)" || bad "--service exit $RC"
    tool "$WORK/none" topic stat tool.none
    [ "$RC" -eq 1 ] && ok "a name with no topic: exit 1" || bad "no topic exit $RC"
    tool "$WORK/list" topic list tool.
    [ "$RC" -eq 0 ] && awk 'NR == 2 { found = ($1 == "tool.a") } END { exit !found }' "$WORK/list" \
        && ok "topic list is topics" || bad "list: $(cat "$WORK/list")"

    kill -STOP "$PUB"
    tool "$WORK/stopped" topic stat tool.a --timeout 1
    [ "$RC" -eq 2 ] && grep -q "no answer within" "$WORK/stopped.err" \
        && ok "stat of a stopped publisher: exit 2 at its timeout" \
        || bad "stopped: exit $RC: $(cat "$WORK/stopped.err")"
    kill -CONT "$PUB"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "--topic-socket: the hub's registry over a Unix socket"
SOCK=$WORK/topic.sock
if start_hub --config "$CFG" --topic-socket "$SOCK"; then
    tool "$WORK/sock" topic stat tool.none --topic-socket "$SOCK"
    [ "$RC" -eq 1 ] && ok "a name with no topic on the socket: exit 1" \
        || bad "socket exit $RC: $(cat "$WORK/sock.err")"
    tool "$WORK/sock-list" topic list --topic-socket "$SOCK"
    [ "$RC" -eq 0 ] && ok "topic list over the socket" || bad "socket list exit $RC"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_hub

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
