#!/usr/bin/env bash
# STAGE3: `LazyServiceRegistrar` against Android's **real** servicemanager.
#
# The Linux gate (`tests/scripts/run_lazy_service_ac.sh`) drives the same path
# against `rsb_hub`, which is our own port of AOSP `ServiceManager.cpp`. This
# one removes that circularity: the peer here is the AOSP binary, so what it
# proves is that the wire rsbinder speaks — `addService` with
# `FLAG_IS_LAZY_SERVICE`, `registerClientCallback`, the `onClients` a real
# `handleClientCallbacks` timer sends, and `tryUnregisterService` — is the
# wire AOSP expects.
#
#   ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
#     cargo ndk -t aarch64-linux-android build -p example-hello --bins
#   ./example-hello/cpp/run_lazy_service_stage3.sh [serial]
#
# Needs a userdebug/google_apis AVD: `addService` from the shell domain is an
# SELinux denial otherwise, so the script needs `adb root` + `setenforce 0`.
# It restores the previous enforcing mode on exit.
set -u

cd "$(dirname "$0")/../.." || exit 1

SERIAL=${1:-}
ADB=(adb)
[ -n "$SERIAL" ] && ADB=(adb -s "$SERIAL")

DEV=/data/local/tmp
BIN=target/aarch64-linux-android/debug
SERVICE=my.hello
PASS=0; FAIL=0

# `pkill -x` matches the 15-char-truncated comm. `pkill -f` would match the
# adb shell's own command line and kill the shell instead — see the trap
# notes in tests/scripts and the emulator memory.
DEMO_COMM=hello_callback_
CLIENT_COMM=hello_client

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
sh_() { "${ADB[@]}" shell "$@"; }

for f in hello_callback_demo hello_client; do
    [ -x "$BIN/$f" ] || { echo "missing $BIN/$f — see the build line in this script's header"; exit 2; }
done

"${ADB[@]}" root >/dev/null 2>&1
sh_ 'true' >/dev/null 2>&1 || { echo "no device (serial: ${SERIAL:-default})"; exit 2; }

PREV_ENFORCE=$(sh_ 'getenforce' | tr -d '\r')
cleanup() {
    sh_ "pkill -x $CLIENT_COMM; pkill -x $DEMO_COMM" >/dev/null 2>&1
    [ "$PREV_ENFORCE" = "Enforcing" ] && sh_ 'setenforce 1' >/dev/null 2>&1
}
trap cleanup EXIT
sh_ 'setenforce 0' >/dev/null 2>&1

sh_ "pkill -x $CLIENT_COMM; pkill -x $DEMO_COMM" >/dev/null 2>&1
# The L3-4 verdict reads servicemanager's own log, so only this run's lines
# may be in it.
"${ADB[@]}" logcat -c >/dev/null 2>&1
for f in hello_callback_demo hello_client; do
    "${ADB[@]}" push "$BIN/$f" "$DEV/$f" >/dev/null 2>&1 || { echo "push $f failed"; exit 2; }
done
sh_ "chmod 755 $DEV/hello_callback_demo $DEV/hello_client" >/dev/null 2>&1

######################################################################
note "L3-1  register_service is accepted by AOSP servicemanager"

sh_ "cd $DEV && (TMPDIR=$DEV RUST_LOG=info nohup ./hello_callback_demo > lazy.log 2>&1 &)" >/dev/null 2>&1
# Hold a client straight away: with none, the servicemanager's own 5-second
# timer would report zero clients and take the process down before the later
# gates could look at it. That is the behaviour L3-3 measures deliberately.
sh_ "cd $DEV && (TMPDIR=$DEV RUST_LOG=warn nohup ./hello_client > lazyclient.log 2>&1 &)" >/dev/null 2>&1
sleep 4

if [ -n "$(sh_ "pgrep -x $DEMO_COMM" | tr -d '\r')" ]; then
    ok "the lazy service is running"
else
    bad "it exited early: $(sh_ "cat $DEV/lazy.log" | tail -5)"
fi

# `service check` prints "Service <name>: found" or "... : not found", so the
# test has to be for the *absence* of "not found" — grepping for "found"
# matches both.
if sh_ "service check $SERVICE" | grep -q 'not found'; then
    bad "service check says: $(sh_ "service check $SERVICE")"
else
    ok "registered with the platform servicemanager"
fi

# AOSP `ServiceManager::addService` only *warns* when no priority bit is set
# and never rejects an unknown one, so a rejected registration here would
# mean the parcel itself was wrong, not the flag value.
if sh_ "cat $DEV/lazy.log" | grep -q 'Registered lazy service'; then
    ok "addService with FLAG_IS_LAZY_SERVICE returned ok"
else
    bad "registration log: $(sh_ "cat $DEV/lazy.log" | tail -5)"
fi

# Gate this explicitly. A client that died early would leave L3-2 passing for
# the wrong reason — "still alive" means nothing if nothing was holding it.
if [ -n "$(sh_ "pgrep -x $CLIENT_COMM" | tr -d '\r')" ]; then
    ok "a client is holding the service"
else
    bad "the client is not running: $(sh_ "cat $DEV/lazyclient.log" | tail -3)"
fi

######################################################################
note "L3-2  a held client keeps it alive across servicemanager timer ticks"

# AOSP `kClientCallbackCheckInterval` is 5s, same as the rsb_hub poller.
sleep 12
if [ -z "$(sh_ "pgrep -x $CLIENT_COMM" | tr -d '\r')" ]; then
    bad "the client died during the wait — this gate proved nothing"
elif [ -n "$(sh_ "pgrep -x $DEMO_COMM" | tr -d '\r')" ]; then
    ok "still running after two timer ticks with a client attached"
else
    bad "shut down while a client held it: $(sh_ "cat $DEV/lazy.log" | tail -10)"
fi

######################################################################
note "L3-3  the last client leaving shuts it down"

sh_ "pkill -x $CLIENT_COMM" >/dev/null 2>&1

waited=0
while [ -n "$(sh_ "pgrep -x $DEMO_COMM" | tr -d '\r')" ] && [ "$waited" -lt 25 ]; do
    sleep 1
    waited=$((waited + 1))
done

if [ -n "$(sh_ "pgrep -x $DEMO_COMM" | tr -d '\r')" ]; then
    bad "still running 25s after the last client left: $(sh_ "cat $DEV/lazy.log" | tail -10)"
else
    ok "shut down on its own after ${waited}s"
fi

if sh_ "cat $DEV/lazy.log" | grep -q 'has_clients=false'; then
    ok "the real servicemanager delivered onClients(false)"
else
    bad "no has_clients=false: $(sh_ "cat $DEV/lazy.log" | tail -10)"
fi

######################################################################
note "L3-4  AOSP honoured tryUnregisterService"

# The verdict is servicemanager's own statement about which branch it took:
# `Unregistering <name>` is the erase, and it is the only path that returns
# `Status::ok()`. Reading the registry alone cannot tell that apart from the
# entry being dropped later by the process's death notification, which is
# what would happen if the unregister had been refused.
SM_LOG=$("${ADB[@]}" logcat -d 2>/dev/null | grep "servicemanager" | grep -- "$SERVICE")
if grep -q "Unregistering $SERVICE" <<<"$SM_LOG"; then
    ok "servicemanager logged the unregister (not a death-driven cleanup)"
else
    bad "no 'Unregistering $SERVICE' from servicemanager: $(tail -5 <<<"$SM_LOG")"
fi
if grep -q "Tried to unregister $SERVICE" <<<"$SM_LOG"; then
    bad "servicemanager refused it: $(grep "Tried to unregister" <<<"$SM_LOG" | tail -2)"
else
    ok "no refusal logged"
fi

# The erase is synchronous inside the call, so this is settled by the time the
# process is gone; the retry only absorbs the gap between the process dying
# and this shell observing it.
gone=0
for _ in 1 2 3 4 5; do
    sh_ "service check $SERVICE" | grep -q 'not found' && { gone=1; break; }
    sleep 1
done
if [ "$gone" = 1 ]; then ok "the name is gone from the platform registry"
else bad "still registered: $(sh_ "service check $SERVICE")"; fi

######################################################################
printf '\n===== %d passed, %d failed =====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
