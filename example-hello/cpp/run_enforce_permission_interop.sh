#!/usr/bin/env bash
# Plan 4-2 Phase D STAGE3 — `@EnforcePermission` codegen interop with
# real `PermissionManagerService` on the emulator.
#
# Pure-Rust (no C++ launcher) because both server and client live
# inside the emulator. The client MUST run as the `shell` (uid 2000)
# domain rather than root — Android's `PermissionManagerService`
# bypasses permission checks for uid 0, which would mask the deny path.
# We coerce that via `su shell sh -c "..."`.
#
# Pair of binaries:
#   * enforce_permission_interop_service — registers `rsbinder.test.permcheck` with
#     `BinderFeatures::set_requesting_sid = true`. Every method body
#     returns `Ok(true)`; only the generated `@EnforcePermission` arm
#     emits `EX_SECURITY`.
#   * enforce_permission_interop_client  — verifies doSingle/doAllOf/doAnyOf succeed
#     (real INTERNET/BLUETOOTH permissions) and doDenied fails with
#     `ExceptionCode::Security` (fabricated permission name).
#
# Prereqs:
#   * emulator-5556 booted (Android 16, SDK 36, SELinux Enforcing).
#   * NDK r29+ at $ANDROID_NDK_HOME (default /opt/homebrew/share/android-ndk).
#   * cargo-ndk + aarch64-linux-android rustup target.
#
# Usage:
#   ./run_enforce_permission_interop.sh [-s emulator-5556]
#
# Exit 0 on PASS (4 STAGE3_4_2_PASS markers + client-exit=0).

set -euo pipefail

DEVICE=emulator-5556
SERVICE_BIN=/data/local/tmp/enforce_permission_interop_service
CLIENT_BIN=/data/local/tmp/enforce_permission_interop_client
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi

echo "==> verifying device $DEVICE is Android 16"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 36"; exit 1; }

echo "==> verifying SELinux is enforcing"
mode=$(adb -s "$DEVICE" shell getenforce | tr -d '\r')
[[ "$mode" == "Enforcing" ]] || { echo "device $DEVICE getenforce=$mode, expected Enforcing"; exit 1; }

echo "==> cross-compiling rsbinder STAGE3 binaries"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --bin enforce_permission_interop_service --bin enforce_permission_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/enforce_permission_interop_service" \
    "$SERVICE_BIN" >/dev/null
adb -s "$DEVICE" push \
    "$REPO_ROOT/target/aarch64-linux-android/release/enforce_permission_interop_client" \
    "$CLIENT_BIN" >/dev/null

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f enforce_permission_interop_service 2>/dev/null; rm -f /data/local/tmp/svc42.*; sleep 1" || true

echo "==> starting server (background)"
adb -s "$DEVICE" shell "nohup $SERVICE_BIN > /data/local/tmp/svc42.stdout 2> /data/local/tmp/svc42.stderr < /dev/null &" &
sleep 4

echo "==> running client as shell uid (root bypass would mask deny path)"
out=$(adb -s "$DEVICE" shell "su shell sh -c '$CLIENT_BIN; echo client-exit=\$?'" 2>&1)
echo "$out"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f enforce_permission_interop_service 2>/dev/null" || true

# Verify pass markers.
echo "$out" | grep -q "STAGE3_4_2_PASS doSingle=true"        || { echo "FAIL: doSingle not PASS"; exit 1; }
echo "$out" | grep -q "STAGE3_4_2_PASS doAllOf=true"         || { echo "FAIL: doAllOf not PASS"; exit 1; }
echo "$out" | grep -q "STAGE3_4_2_PASS doAnyOf=true"         || { echo "FAIL: doAnyOf not PASS"; exit 1; }
echo "$out" | grep -q "STAGE3_4_2_PASS doDenied=Security"    || { echo "FAIL: doDenied deny path did not fire"; exit 1; }
echo "$out" | grep -q "client-exit=0"                        || { echo "FAIL: client-exit != 0"; exit 1; }

echo "==> PASS"
