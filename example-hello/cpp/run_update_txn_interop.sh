#!/usr/bin/env bash
# Plan 4-4 Phase D STAGE3 — extended-error ioctl + `TF_UPDATE_TXN`
# `TF_COLLECT_NOTED_APP_OPS` wire surface against real Android binder
# driver on emulator.
#
# Server: `update_txn_interop_service` registers `rsbinder.test.update_txn` with
# `max_threads=1` and *without* `start_thread_pool()` so the main
# thread is the sole worker. The single-worker setup keeps async
# transactions queueing in `node->async_todo` long enough that any
# kernel that implements the dedup walk will be able to coalesce them.
#
# Client: `update_txn_interop_client` runs three checks:
#   1. Baseline `FLAG_ONEWAY` burst — every payload must survive.
#   2. `FLAG_ONEWAY | FLAG_UPDATE_TXN` burst — the kernel must at
#      least *accept* every transaction (no `EINVAL`). Whether it
#      actually collapses the queue is kernel-implementation
#      dependent (Android 12+ does, older kernels silently ignore),
#      so the client logs which behavior it observed and PASSes
#      either way.
#   3. `get_extended_error()` — must return `Ok` on Android 12+ /
#      Linux 5.14+; the API itself surfaces `InvalidOperation` on
#      older drivers, which counts as the documented fallback (also
#      PASS).
#
# Usage: ./run_update_txn_interop.sh [-s emulator-5556]
# Exit 0 on PASS.

set -euo pipefail

DEVICE=emulator-5556
SERVICE_BIN=/data/local/tmp/update_txn_interop_service
CLIENT_BIN=/data/local/tmp/update_txn_interop_client
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi

echo "==> verifying device $DEVICE is Android 12+ (needs BINDER_GET_EXTENDED_ERROR / TF_UPDATE_TXN)"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 31 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 31 (Android 12)"; exit 1; }

echo "==> cross-compiling rsbinder STAGE3 binaries"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --bin update_txn_interop_service --bin update_txn_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/update_txn_interop_service" \
    "$SERVICE_BIN" >/dev/null
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/update_txn_interop_client" \
    "$CLIENT_BIN" >/dev/null

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f update_txn_interop_service 2>/dev/null; rm -f /data/local/tmp/svc44.*; sleep 1" || true

echo "==> starting server (background)"
adb -s "$DEVICE" shell "nohup $SERVICE_BIN > /data/local/tmp/svc44.stdout 2> /data/local/tmp/svc44.stderr < /dev/null &" &
sleep 4

echo "==> running client (baseline + dedup + extended_error)"
out=$(adb -s "$DEVICE" shell "$CLIENT_BIN; echo client-exit=\$?" 2>&1)
echo "$out"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f update_txn_interop_service 2>/dev/null" || true

echo "$out" | grep -q "STAGE3_4_4_BASELINE recorded=\[10, 11, 12, 13\]" \
    || { echo "FAIL: baseline burst lost data"; exit 1; }
echo "$out" | grep -q "STAGE3_4_4_DEDUP recorded="            || { echo "FAIL: dedup round did not run"; exit 1; }
echo "$out" | grep -q "STAGE3_4_4_EXTENDED_ERROR"             || { echo "FAIL: get_extended_error() not exercised"; exit 1; }
echo "$out" | grep -q "STAGE3_4_4_PASS"                       || { echo "FAIL: missing STAGE3_4_4_PASS"; exit 1; }
echo "$out" | grep -q "client-exit=0"                         || { echo "FAIL: client-exit != 0"; exit 1; }

echo "==> PASS"
