#!/usr/bin/env bash
# LazyServiceRegistrar acceptance gates (AC-L.1 .. AC-L.5).
#
# The registrar's state machine has hermetic unit tests; what those cannot
# reach is the half that only exists on a wire: does `FLAG_IS_LAZY_SERVICE`
# arrive at the service manager, does `registerClientCallback` land, does the
# hub's 5-second poller actually drive `onClients(false)` into this process,
# and does the process then take itself down. Until now the only thing
# covering that was a demo someone had to read the output of.
#
# Needs a Linux host with a working binder device and nothing else already
# holding handle 0.
#
#   cargo build -p rsbinder-tools --bin rsb_hub --bin rsb_service
#   cargo build -p example-hello --bin hello_callback_demo --bin hello_client
#   ./tests/scripts/run_lazy_service_ac.sh
set -u

cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
SVC=./target/debug/rsb_service
DEMO=./target/debug/hello_callback_demo
CLIENT=./target/debug/hello_client
POLDIR=/tmp/rsb-lazy-policy
HUBLOG=/tmp/rsb-lazy-hub.log
DEMOLOG=/tmp/rsb-lazy-demo.log
CLIENTLOG=/tmp/rsb-lazy-client.log
OUT=/tmp/rsb-lazy-out
PASS=0; FAIL=0
MY_UID=$(id -u)
SERVICE=my.hello

# One poller tick is 5s (rsb_hub `run_client_callback_poller`). Give the
# shutdown three of them before calling it a failure — the gate is "it
# happens on its own", not "it happens in exactly one tick".
TICK=5
SHUTDOWN_TIMEOUT=$((TICK * 3 + 5))

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
want_out()   { if grep -q "$1" "$OUT"; then ok "$2"; else bad "$2 (missing '$1' in: $(head -20 "$OUT"))"; fi; }
reject_out() { if grep -q "$1" "$OUT"; then bad "$2 (leaked '$1')"; else ok "$2"; fi; }

cleanup() {
    pkill -f 'target/debug/hello_client' 2>/dev/null
    pkill -f 'target/debug/hello_callback_demo' 2>/dev/null
    pkill -f 'target/debug/rsb_hub' 2>/dev/null
    rm -rf "$POLDIR"
}
trap cleanup EXIT
cleanup; sleep 1

for bin in "$HUB" "$SVC" "$DEMO" "$CLIENT"; do
    [ -x "$bin" ] || { echo "missing $bin — see the build lines in this script's header"; exit 2; }
done

mkdir -p "$POLDIR"
cat > "$POLDIR/10.toml" <<EOF
[global]
list = { uid = [$MY_UID] }

[[rule]]
name = "$SERVICE"
add  = { uid = [$MY_UID] }
find = { uid = [$MY_UID] }

# rsb_service looks the hub up by name like any other client.
[[rule]]
name = "manager"
find = { uid = [$MY_UID] }
EOF

RUST_LOG=info nohup $HUB --config "$POLDIR" > "$HUBLOG" 2>&1 &
disown 2>/dev/null
sleep 2
pgrep -f 'target/debug/rsb_hub' >/dev/null || { echo "rsb_hub did not start:"; cat "$HUBLOG"; exit 2; }

######################################################################
note "AC-L.1  register_service reaches the service manager"

RUST_LOG=info nohup $DEMO > "$DEMOLOG" 2>&1 &
DEMO_PID=$!
disown 2>/dev/null

# Hold a client immediately: with none, the first poller tick would take the
# process down before the later gates could look at it. That is the correct
# behaviour, and AC-L.4 measures it deliberately.
RUST_LOG=warn nohup $CLIENT > "$CLIENTLOG" 2>&1 &
CLIENT_PID=$!
disown 2>/dev/null
sleep 3

if kill -0 "$DEMO_PID" 2>/dev/null; then ok "demo is running"
else bad "demo exited early (rc=$?): $(tail -5 "$DEMOLOG")"; fi

$SVC dump manager > "$OUT" 2>&1
want_out "$SERVICE" "the service is registered"
# The whole point of the flag: without it the hub cannot tell a lazy service
# from an ordinary one, and `rsb_service dump` reports it as ordinary.
if grep -A2 "^  $SERVICE\$" "$OUT" | grep -q 'lazy=yes'; then
    ok "registered with FLAG_IS_LAZY_SERVICE"
else
    bad "lazy=no — FLAG_IS_LAZY_SERVICE did not reach the hub: $(grep -A2 "^  $SERVICE\$" "$OUT")"
fi
if grep -A3 "^  $SERVICE\$" "$OUT" | grep -q 'client=1$'; then
    ok "registerClientCallback landed (client=1)"
else
    bad "client callback count: $(grep -A3 "^  $SERVICE\$" "$OUT" | grep client)"
fi

######################################################################
note "AC-L.2  a held client keeps the process alive across poller ticks"

sleep $((TICK * 2 + 2))
if kill -0 "$DEMO_PID" 2>/dev/null; then
    ok "still running after two poller ticks with a client attached"
else
    bad "shut down while a client was still holding it: $(tail -10 "$DEMOLOG")"
fi

######################################################################
note "AC-L.3  the last client leaving shuts the process down, exit 0"

kill "$CLIENT_PID" 2>/dev/null
wait "$CLIENT_PID" 2>/dev/null

waited=0
while kill -0 "$DEMO_PID" 2>/dev/null && [ "$waited" -lt "$SHUTDOWN_TIMEOUT" ]; do
    sleep 1
    waited=$((waited + 1))
done

if kill -0 "$DEMO_PID" 2>/dev/null; then
    bad "still running ${SHUTDOWN_TIMEOUT}s after the last client left"
    kill "$DEMO_PID" 2>/dev/null
else
    ok "shut down on its own after ${waited}s"
    wait "$DEMO_PID" 2>/dev/null
    rc=$?
    if [ "$rc" -eq 0 ]; then ok "exit 0 (AOSP exit(EXIT_SUCCESS))"; else bad "exit $rc, want 0"; fi
fi

grep -q 'has_clients=false' "$DEMOLOG" && ok "the process saw the transition" \
    || bad "no has_clients=false in the demo log: $(tail -5 "$DEMOLOG")"

######################################################################
note "AC-L.4  tryUnregisterService really removed it from the registry"

$SVC dump manager > "$OUT" 2>&1
reject_out "^  $SERVICE\$" "the name is gone from the hub"

$SVC check "$SERVICE" > "$OUT" 2>&1; rc=$?
if [ "$rc" -eq 1 ]; then ok "checkService reports it as absent"; else bad "check exit $rc, want 1"; fi

######################################################################
note "AC-L.5  the hub logged the unregister rather than a death"

# A lazy shutdown and a crash look the same from the outside if the only
# evidence is "the name is gone". The hub distinguishes them.
if grep -q 'Tried to unregister' "$HUBLOG"; then
    bad "the hub refused the unregister: $(grep 'Tried to unregister' "$HUBLOG" | head -2)"
else
    ok "no refusal logged"
fi

######################################################################
printf '\n===== %d passed, %d failed =====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
