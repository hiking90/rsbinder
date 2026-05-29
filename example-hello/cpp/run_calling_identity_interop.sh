#!/usr/bin/env bash
# Plan 4-1 Phase E STAGE3 — calling-identity + IPermissionController
# smoke against real Android emulator (kernel binder + servicemanager
# + system_server).
#
# Pure-Rust (no C++ launcher) because both halves of the test live
# inside the emulator: the server-side service exercises Phase A/B
# (`get_calling_*` / `clear_calling_identity` / `restore_calling_identity`
# / `has_explicit_identity`), and the client exercises Phase D
# (`permission_controller::default()` + `checkPermission`).
#
# Pairs of binaries:
#   * calling_identity_interop_service — registers `rsbinder.test.calling_identity` with
#     `BinderFeatures { set_requesting_sid: true, .. }`.
#   * calling_identity_interop_client  — calls describeCaller() and checkPermission().
#
# Prereqs (mirrors run_rpc_accessor_interop.sh):
#   * emulator-5556 booted (Android 16, SDK 36, SELinux Enforcing).
#   * NDK r29+ at $ANDROID_NDK_HOME (default
#     /opt/homebrew/share/android-ndk).
#   * cargo-ndk installed; aarch64-linux-android rustup target added.
#
# Usage:
#   ./run_calling_identity_interop.sh [-s emulator-5556]
#
# Exit 0 on PASS (describe round-trip + permission round-trip both
# print PASS markers and client-exit=0).

set -euo pipefail

DEVICE=emulator-5556
SERVICE_BIN=/data/local/tmp/calling_identity_interop_service
CLIENT_BIN=/data/local/tmp/calling_identity_interop_client
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi

echo "==> verifying device $DEVICE is Android 16"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 36"; exit 1; }

echo "==> verifying SELinux is enforcing (SID extraction needs it)"
mode=$(adb -s "$DEVICE" shell getenforce | tr -d '\r')
[[ "$mode" == "Enforcing" ]] || { echo "device $DEVICE getenforce=$mode, expected Enforcing"; exit 1; }

echo "==> cross-compiling rsbinder STAGE3 binaries"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --bin calling_identity_interop_service --bin calling_identity_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/calling_identity_interop_service" \
    "$SERVICE_BIN" >/dev/null
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/calling_identity_interop_client" \
    "$CLIENT_BIN" >/dev/null

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f calling_identity_interop_service 2>/dev/null; rm -f /data/local/tmp/calling_identity_interop_*.{stdout,stderr,log}; sleep 1" || true

echo "==> starting server (background)"
adb -s "$DEVICE" shell "nohup $SERVICE_BIN > /data/local/tmp/calling_identity_interop_service.stdout 2> /data/local/tmp/calling_identity_interop_service.stderr &" &
sleep 3

echo "==> running client (describe + check_permission)"
out=$(adb -s "$DEVICE" shell "$CLIENT_BIN; echo client-exit=\$?" 2>&1)
echo "$out"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f calling_identity_interop_service 2>/dev/null" || true

# Verify pass markers.
echo "$out" | grep -q "STAGE3_4_1_DESCRIBE_PASS"   || { echo "FAIL: missing STAGE3_4_1_DESCRIBE_PASS"; exit 1; }
echo "$out" | grep -q "STAGE3_4_1_PERMISSION_PASS" || { echo "FAIL: missing STAGE3_4_1_PERMISSION_PASS"; exit 1; }
echo "$out" | grep -q "client-exit=0"              || { echo "FAIL: client-exit != 0"; exit 1; }

echo "==> PASS"
