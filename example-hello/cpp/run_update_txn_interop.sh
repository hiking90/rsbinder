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
# Prereqs: NDK at $ANDROID_NDK_HOME (default
# /opt/homebrew/share/android-ndk), cargo-ndk + the rustup target for the
# device's ABI.
#
# Usage: ./run_update_txn_interop.sh [-s emulator-5556] [-t <abi>]
# The ABI defaults to the device's own; -t overrides it.
# Exit 0 on PASS.

set -euo pipefail

DEVICE=emulator-5556
ABI=""
SERVICE_BIN=/data/local/tmp/update_txn_interop_service
CLIENT_BIN=/data/local/tmp/update_txn_interop_client
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done

sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')

# For the two 64-bit ABIs the NDK clang prefix and the cargo target
# directory are both the Rust triple; the 32-bit ones differ and are not
# covered.
case "${ABI:-$(adb -s "$DEVICE" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level, so walk
# down to the newest clang the NDK actually ships at or below it.
API=""
for a in $(seq "$sdk" -1 29); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { API="$a"; break 2; }
    done
done
[ -n "$API" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

echo "==> verifying device $DEVICE is Android 12+ (needs BINDER_GET_EXTENDED_ERROR / TF_UPDATE_TXN)"
[[ "$sdk" -ge 31 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 31 (Android 12)"; exit 1; }

echo "==> cross-compiling rsbinder STAGE3 binaries"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p example-hello \
        --bin update_txn_interop_service --bin update_txn_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/$TRIPLE/release/update_txn_interop_service" \
    "$SERVICE_BIN" >/dev/null
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/$TRIPLE/release/update_txn_interop_client" \
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
