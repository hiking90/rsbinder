#!/usr/bin/env bash
# rsb_hub's topic registry (plan 15 P5-3): AC-15.5.7, 5.8, 5.12, and 5.1b and
# 5.5 once more against a real hub. Needs a Linux host with a writable
# /dev/binderfs/binder and no rsb_hub running: the script starts its own.
#
#   ./tests/scripts/run_topic_registry_ac.sh
#
# It builds what it runs: `rsb_hub` and `rsb_service` with the `topic`
# feature, copied to target/debug/rsb_hub_topic and rsb_service_topic, then
# both again without it, so target/debug/rsb_hub stays the hub every other
# script runs (the feature is part of the artifact hash, so the dependencies
# are built once); and `topic_probe`, the client. The copies go when it ends.
#
# Runs as the invoking user. Allowed and refused are told apart by name, as
# in run_hub_policy_ac.sh; the subject sets themselves are the unit tests'.
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
HUB_TOPIC=./target/debug/rsb_hub_topic
SVC_TOPIC=./target/debug/rsb_service_topic
PROBE=./target/debug/topic_probe
MY_UID=$(id -u)
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

# AC-15.5.9: a hub already serving would answer in place of the one under test.
running=$(pgrep -x rsb_hub; pgrep -x rsb_hub_topic)
if [ -n "$running" ]; then
    echo "an rsb_hub is running (pid $(echo $running)); stop it, this script starts its own" >&2
    exit 1
fi

cargo build -q -p rsbinder-tools --features topic --bin rsb_hub --bin rsb_service || exit 1
cp -f target/debug/rsb_hub "$HUB_TOPIC" && cp -f target/debug/rsb_service "$SVC_TOPIC" || exit 1
cargo build -q -p rsbinder-tools --bin rsb_hub --bin rsb_service || exit 1
cargo build -q -p tests --bin topic_probe || exit 1

# Not under /tmp: rsb_hub refuses a configuration a world-writable directory holds.
WORK=$(mktemp -d -p "${XDG_RUNTIME_DIR:-${HOME:?}}" rsb-topic-ac.XXXXXX) || exit 1
CFG=$WORK/cfg
LOG=$WORK/hub.log
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
cleanup() { stop_probes; stop_hub; rm -f "$HUB_TOPIC" "$SVC_TOPIC"; rm -rf "$WORK"; }
trap cleanup EXIT

# start_hub BIN ARGS...: up once it logs that it serves, which follows the registry's registration.
start_hub() {
    local bin=$1; shift
    : > "$LOG"
    RUST_LOG=info "$bin" "$@" >"$LOG" 2>&1 &
    HUB_PID=$!
    local i
    for i in $(seq 100); do
        grep -q "rsb_hub: serving on" "$LOG" && return 0
        kill -0 "$HUB_PID" 2>/dev/null || { HUB_PID=; return 1; }
        sleep 0.05
    done
    return 1
}

# config FILE...: a fresh configuration directory of the files named, each `NAME=TEXT`.
config() {
    rm -rf "$CFG"; mkdir -p "$CFG"; chmod 755 "$CFG"
    local spec
    for spec in "$@"; do
        printf '%s\n' "${spec#*=}" > "$CFG/${spec%%=*}"
        chmod 644 "$CFG/${spec%%=*}"
    done
}
PERMISSIVE="10-permissive.toml=$(cat tests/policy/permissive.toml)"
PERMISSIVE_TOPIC="20-topic.toml=$(cat tests/policy/topic/permissive-topic.toml)"

# probe OUT ARGS...: in the background, kept until the section ends; LAST is its pid.
LAST=
probe() {
    local out=$1; shift
    "$PROBE" "$@" >"$out" 2>>"$WORK/probe.err" &
    LAST=$!
    PIDS+=("$LAST")
}
# probe_now OUT ARGS...: in the foreground, given 20 s.
probe_now() {
    local out=$1; shift
    timeout 20 "$PROBE" "$@" >"$out" 2>>"$WORK/probe.err"
}

# wait_line FILE REGEX SECS
wait_line() {
    local i
    for i in $(seq $(($3 * 20))); do
        grep -qE "$2" "$1" 2>/dev/null && return 0
        sleep 0.05
    done
    return 1
}
# want FILE LINE LABEL
want() {
    if grep -qxF "$2" "$1"; then ok "$3"; else bad "$3 (want '$2', got: $(grep -E '^(RESULT|GOT)' "$1" | tr '\n' ';'))"; fi
}
reject() { if grep -qxF "$2" "$1"; then bad "$3 (got '$2')"; else ok "$3"; fi; }
# fast FILE OP ARG LABEL: the last such call ended within a second.
fast() {
    local ms
    ms=$(awk -v op="$2" -v arg="$3" '$1=="TIME" && $2==op && $3==arg {ms=$4} END {print ms}' "$1")
    if [ -n "$ms" ] && [ "$ms" -lt 1000 ]; then ok "$4 (${ms} ms)"; else bad "$4 (${ms:-no answer} ms)"; fi
}
# stop_until_stopped PID...: SIGSTOP, then wait for the kernel to report each stopped.
stop_until_stopped() {
    local pid i
    for pid in "$@"; do
        kill -STOP "$pid"
        for i in $(seq 100); do
            [ "$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)" = T ] && break
            sleep 0.01
        done
    done
}
dump_value() { "$SVC_TOPIC" dump manager 2>/dev/null | sed -n "s/.* $1=\([0-9]*\).*/\1/p"; }
# reload: SIGHUP, then wait for the hub to log one more reload, done or failed.
reload() {
    local before i
    before=$(grep -c "SIGHUP reload" "$LOG")
    kill -HUP "$HUB_PID"
    for i in $(seq 100); do
        [ "$(grep -c "SIGHUP reload" "$LOG")" -gt "$before" ] && return 0
        sleep 0.05
    done
    return 1
}

######################################################################
note "AC-15.5.7  a hub without the feature refuses topic rules"
config "$PERMISSIVE" "$PERMISSIVE_TOPIC"
out=$(timeout 5 "$HUB" --config "$CFG" 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "does not start (exit 1)" || bad "exit $rc"
echo "$out" | grep -q "TOML parse error" && ok "names the refused file" || bad "message: $out"

config "$PERMISSIVE"
if start_hub "$HUB" --config "$CFG"; then
    config "$PERMISSIVE" "$PERMISSIVE_TOPIC"
    reload
    grep -q "reload of .* failed, keeping" "$LOG" && ok "SIGHUP keeps the configuration in force" \
        || bad "no failed reload: $(tail -2 "$LOG")"
    probe_now "$WORK/out" find
    want "$WORK/out" "RESULT find  NOTFOUND" "no registry to find"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_hub

######################################################################
note "AC-15.5.7  --insecure-allow-all: everything but the reserved name"
if start_hub "$HUB_TOPIC" --insecure-allow-all; then
    grep -q "serving the topic registry as 'rsbinder.topic.registry'" "$LOG" \
        && ok "logs the registry" || bad "no registry in the log"
    # Right after the hub says it serves, which it says after READY=1's precondition.
    probe "$WORK/a" find publish:ia.a:v lookup:ia.a list:ia. addreserved say:done hold
    wait_line "$WORK/a" "^SAY done" 10
    want "$WORK/a" "RESULT find  OK" "the registry is there once the hub serves"
    want "$WORK/a" "RESULT publish ia.a OK" "advertise allowed"
    want "$WORK/a" "RESULT lookup ia.a FOUND" "lookup allowed"
    want "$WORK/a" "LISTED ia.a $MY_UID 0" "list allowed"
    want "$WORK/a" "RESULT addreserved  SECURITY" "the registry's name is refused even so"
    "$SVC_TOPIC" dump manager > "$WORK/dump" 2>&1
    grep -q "topic registry: registered as rsbinder.topic.registry, access control DISABLED" \
        "$WORK/dump" && ok "the dump shows the registry" || bad "dump: $(head -8 "$WORK/dump")"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "AC-15.5.7/8  permissive-topic.toml, --allow-cross-uid-overwrite; rsb_service topics"
config "$PERMISSIVE" "$PERMISSIVE_TOPIC"
if start_hub "$HUB_TOPIC" --config "$CFG" --allow-cross-uid-overwrite; then
    probe "$WORK/a" addreserved publish:ls.b:v publish:ls.a:v publish:other.a:v say:done hold
    wait_line "$WORK/a" "^SAY done" 10
    want "$WORK/a" "RESULT addreserved  SECURITY" "the registry's name is refused"
    "$SVC_TOPIC" topics ls. > "$WORK/topics" 2>&1; rc=$?
    [ "$rc" -eq 0 ] && ok "topics exits 0" || bad "topics exit $rc"
    head -1 "$WORK/topics" | grep -qE "^NAME +UID +PID +AGE +WATCHERS$" && ok "a header of the columns" \
        || bad "header: $(head -1 "$WORK/topics")"
    rows=$(awk 'NR > 1 {print $1}' "$WORK/topics" | tr '\n' ' ')
    [ "$rows" = "ls.a ls.b " ] && ok "the names under the prefix, sorted" || bad "rows: $rows"
    awk -v uid="$MY_UID" 'NR == 2 && $2 == uid && $3 > 0 && $4 ~ /s$/ && $5 == 0 {f=1} END {exit !f}' \
        "$WORK/topics" && ok "uid pid age watchers" || bad "row: $(sed -n 2p "$WORK/topics")"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "AC-15.5.7  [[topic]] rules decide by name"
# ac_rules GLOBAL: the rules below, after a `[global]` that holds GLOBAL.
ac_rules() {
    config "$PERMISSIVE" "20-topic.toml=[global]
$1

[[topic]]
name = \"ac.open.*\"
advertise = { uid = [$MY_UID] }
subscribe = { uid = [$MY_UID] }

[[topic]]
name = \"ac.hidden.*\"
advertise = { uid = [$MY_UID] }
subscribe = \"none\"

[[topic]]
name = \"ac.closed.*\"
subscribe = { uid = [$MY_UID] }"
}
ac_rules "topics = { uid = [$MY_UID] }"
if start_hub "$HUB_TOPIC" --config "$CFG"; then
    grep -q "loaded 1 rule(s), 3 topic rule(s), 0 service declaration(s)" "$LOG" \
        && ok "the start log counts the topic rules" || bad "log: $(grep loaded "$LOG")"
    probe "$WORK/a" publish:ac.open.a:v publish:ac.hidden.a:v publish:ac.closed.a:v \
        publish:zz.a:v lookup:ac.open.a lookup:ac.hidden.a list:ac. say:done hold
    wait_line "$WORK/a" "^SAY done" 10
    want "$WORK/a" "RESULT publish ac.open.a OK" "advertise allowed"
    want "$WORK/a" "RESULT publish ac.closed.a SECURITY" "advertise refused: EX_SECURITY"
    want "$WORK/a" "RESULT publish zz.a SECURITY" "a name no rule covers is refused"
    want "$WORK/a" "RESULT lookup ac.open.a FOUND" "subscribe allowed"
    want "$WORK/a" "RESULT lookup ac.hidden.a NULL" "subscribe refused: a null topic"
    want "$WORK/a" "LISTED ac.open.a $MY_UID 0" "list shows what the caller may subscribe to"
    reject "$WORK/a" "LISTED ac.hidden.a $MY_UID 0" "and leaves out what it may not"
    "$SVC_TOPIC" dump manager > "$WORK/dump" 2>&1
    grep -q "topic registry: registered as rsbinder.topic.registry, 3 topic rule(s)" "$WORK/dump" \
        && ok "the dump counts the topic rules" || bad "dump: $(grep 'topic registry' "$WORK/dump")"
    grep -q " notices_failed=0 " "$WORK/dump" && ok "the dump carries the counters" \
        || bad "dump: $(grep topics= "$WORK/dump")"

    note "AC-15.5.7/8  [global] topics: undefined and \"none\" refuse list"
    stop_probes
    ac_rules ""
    reload
    probe_now "$WORK/b" list:ac.
    want "$WORK/b" "RESULT list ac. SECURITY" "undefined: refused"
    ac_rules 'topics = "none"'
    reload
    probe_now "$WORK/b" list:ac.
    want "$WORK/b" "RESULT list ac. SECURITY" "\"none\": refused"
    "$SVC_TOPIC" topics > "$WORK/topics" 2>&1; rc=$?
    [ "$rc" -eq 2 ] && ok "topics exits 2" || bad "topics exit $rc"
    grep -q "policy denies" "$WORK/topics" && grep -q '\[global\] topics' "$WORK/topics" \
        && ok "and says which policy" || bad "topics: $(cat "$WORK/topics")"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

note "AC-15.5.7  [global] topics in two files"
config "$PERMISSIVE" "$PERMISSIVE_TOPIC" "30-again.toml=[global]
topics = \"none\""
out=$(timeout 5 "$HUB_TOPIC" --config "$CFG" 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "does not start (exit 1)" || bad "exit $rc"
echo "$out" | grep -q "\[global\] topics is defined in both" && ok "names both files" || bad "$out"

######################################################################
note "AC-15.5.7  a reload that takes subscribe away and gives it back (D4 B')"
# rt_rules SUBJECTS: who may subscribe to `rt.*`.
rt_rules() {
    config "$PERMISSIVE" "20-topic.toml=[global]
topics = \"any\"

[[topic]]
name = \"rt.*\"
advertise = \"any\"
subscribe = $1"
}
rt_rules '"any"'
if start_hub "$HUB_TOPIC" --config "$CFG"; then
    probe "$WORK/w" subscribe:rt.a say:watching hold; watcher=$LAST
    wait_line "$WORK/w" "^SAY watching" 10
    probe "$WORK/p1" publish:rt.a:one hold
    wait_line "$WORK/w" "^GOT rt.a one$" 10 && ok "the watcher gets the first publisher" \
        || bad "no first value: $(cat "$WORK/w")"
    rt_rules '"none"'; reload
    probe "$WORK/p2" publish:rt.a:two say:up hold
    wait_line "$WORK/p2" "^SAY up" 10
    want "$WORK/p2" "RESULT publish rt.a OK" "the second publisher advertises"
    sleep 3
    reject "$WORK/w" "GOT rt.a two" "a watcher that lost subscribe is told nothing"
    probe_now "$WORK/b" lookup:rt.a
    want "$WORK/b" "RESULT lookup rt.a NULL" "and finds nothing"
    rt_rules '"any"'; reload
    probe "$WORK/p3" publish:rt.a:three hold
    wait_line "$WORK/w" "^GOT rt.a three$" 10 && ok "given it back, the next advertise reaches it" \
        || bad "no third value: $(cat "$WORK/w")"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "AC-15.5.1b  a stopped publisher holds up no call of the hub"
config "$PERMISSIVE" "$PERMISSIVE_TOPIC"
if start_hub "$HUB_TOPIC" --config "$CFG"; then
    probe "$WORK/a" publish:sp.a:x say:up hold; publisher=$LAST
    wait_line "$WORK/a" "^SAY up" 10
    # The client looks the name up first and keeps the proxy: one with none would ask the
    # stopped process for its descriptor, a wait of the client's own (plan 15 §12.7).
    probe "$WORK/c" lookup:sp.a say:looked wait:"$WORK/go1" lookup:sp.a list:sp. \
        publish:sp.b:y say:done hold
    wait_line "$WORK/c" "^SAY looked" 10
    stop_until_stopped "$publisher"
    touch "$WORK/go1"
    if wait_line "$WORK/c" "^SAY done" 10; then
        fast "$WORK/c" lookup sp.a "lookup of the stopped publisher's name"
        fast "$WORK/c" list sp. "list"
        fast "$WORK/c" publish sp.b "another publisher's advertise"
        probe_now "$WORK/d" lookup:sp.b
        want "$WORK/d" "RESULT lookup sp.b FOUND" "another client finds it"
        fast "$WORK/d" lookup sp.b "another client's lookup"
    else
        bad "a call is held up: $(cat "$WORK/c")"
    fi
    kill -CONT "$publisher"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "AC-15.5.5  stopped watchers hold up no call, and meet the last publisher once resumed"
if start_hub "$HUB_TOPIC" --config "$CFG"; then
    watchers=()
    for i in $(seq 8); do
        probe "$WORK/w$i" subscribe:sw.a say:watching hold; watchers+=("$LAST")
    done
    for i in $(seq 8); do wait_line "$WORK/w$i" "^SAY watching" 10; done
    stop_until_stopped "${watchers[@]}"
    # Past a watcher's async space (half its 1 MiB - 8 KiB mapping): ~3500 notices of this name.
    probe "$WORK/f" flood:sw.a:6000 publish:sw.a:last say:published hold
    if wait_line "$WORK/f" "^SAY published" 120; then
        want "$WORK/f" "RESULT flood sw.a OK" "6000 advertises"
        fast "$WORK/f" flood sw.a "the longest of them"
        fast "$WORK/f" publish sw.a "the last publisher's advertise"
        probe_now "$WORK/d" lookup:sw.a
        fast "$WORK/d" lookup sw.a "another client's lookup"
        failed=$(dump_value notices_failed); owed=$(dump_value owed)
        [ "${failed:-0}" -ge 1 ] && ok "the hub's dump counts failed notices ($failed, $owed owed)" \
            || bad "notices_failed=${failed:-none}"
    else
        bad "the hub is held up: $(cat "$WORK/f")"
    fi
    for pid in "${watchers[@]}"; do kill -CONT "$pid"; done
    met=0
    for i in $(seq 8); do wait_line "$WORK/w$i" "^GOT sw.a last$" 10 && met=$((met+1)); done
    [ "$met" -eq 8 ] && ok "all 8 meet the last publisher within 10 s" || bad "$met of 8"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

######################################################################
note "AC-15.5.12  a notice lost for want of room arrives once the watcher reads again"
if start_hub "$HUB_TOPIC" --config "$CFG"; then
    # 4 KiB mapped, no thread pool: the notices of its 40 watches stay unread and fill its 2 KiB.
    probe "$WORK/s" --mmap 4096 watchmany:fill.:40 subscribe:sm.a say:ready \
        wait:"$WORK/go2" pool hold
    wait_line "$WORK/s" "^SAY ready" 10
    want "$WORK/s" "RESULT watchmany fill.:40 OK" "40 names watched"
    probe_now "$WORK/b" fill:fill.:40
    want "$WORK/b" "RESULT fill fill.:40 OK" "their notices sent"
    probe "$WORK/p" publish:sm.a:small say:up hold
    wait_line "$WORK/p" "^SAY up" 10
    owed=$(dump_value owed)
    [ "${owed:-0}" -ge 1 ] && ok "the publisher's notice is owed ($owed owed)" || bad "owed=${owed:-none}"
    touch "$WORK/go2"
    wait_line "$WORK/s" "^GOT sm.a small$" 35 && ok "it arrives within 30 s of the pool" \
        || bad "never arrived: $(cat "$WORK/s")"
else
    bad "hub did not start: $(tail -3 "$LOG")"
fi
stop_probes; stop_hub

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
