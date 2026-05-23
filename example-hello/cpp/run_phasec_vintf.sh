#!/usr/bin/env bash
# Plan 2-14 Phase C — VINTF-driven accessor routing end-to-end harness.
#
# Validates the *AOSP-canonical* deployment path that lives strictly
# above the 2-13 D.8 / 2-14 D.9 STAGE3 bypass:
#
#   rsbinder server publishes a `LocalAccessor` binder via
#   `hub::add_service("android.os.IAccessor/<iface>/<inst>", ...)`,
#   and a VINTF `<accessor>` entry on the device tells servicemanager
#   "service `<pkg>.<iface>/<inst>` is reachable via that accessor".
#   A real-libbinder client then asks for the *service* name and
#   `AServiceManager_waitForService` transparently returns the RPC
#   root — no manual `BpAccessor::from_binder` dance.
#
# Prereqs:
#   * Android_16 AVD booted with `-writable-system`:
#       emulator -avd Android_16 -port 5556 -writable-system -no-snapshot-load
#   * `adb root` works (userdebug image).
#   * `adb remount` was run once after boot to enable the
#     `/system_ext` overlay (this script's `bootstrap_overlay` step).
#   * NDK r29+ at $ANDROID_NDK_HOME, cargo-ndk + aarch64-linux-android
#     rustup target installed.
#
# Usage:
#   example-hello/cpp/run_phasec_vintf.sh [-s emulator-5556]
#
# Exits 0 on PASS, non-zero on FAIL. The expected sequence on a
# successful run is:
#   - `service check rsbinder.test.accessor.IInterop/default: found`
#     (VINTF arm gating — proves servicemanager resolved through the
#     `<accessor>` entry, since we never directly addService that name)
#   - `phasec_vintf_client` exits 0 with TX_ECHO + TX_GIVE_MARKER byte
#     matches.
#
# The VINTF entry and the server binary survive emulator reboots
# (overlay persists in `/data`), but `adb remount` must be re-run if
# the overlay gets disabled (e.g. cold start without
# `-writable-system`). The rsbinder server itself does NOT auto-start
# on reboot — this script restarts it after any reboot.

set -euo pipefail

DEVICE=emulator-5556
INSTANCE="android.os.IAccessor/IInterop/default"
SERVICE_NAME="rsbinder.test.accessor.IInterop/default"
SOCK=/data/local/tmp/rsacc-phasec.sock
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"
NDK_BIN="$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi

echo "==> [1/8] verifying device $DEVICE is userdebug + booted"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 36"; exit 1; }
build_type=$(adb -s "$DEVICE" shell getprop ro.build.type | tr -d '\r')
[[ "$build_type" == "userdebug" ]] || {
    echo "device $DEVICE is build type '$build_type', need userdebug"; exit 1; }
adb -s "$DEVICE" root >/dev/null
adb -s "$DEVICE" wait-for-device

echo "==> [2/8] ensuring /system_ext overlay is RW"
# `adb remount` enables the overlay; on emulator boot the overlay
# itself is mounted RO and needs an explicit remount to flip RW (the
# `/data` overlay scratch persists across reboots but the mount mode
# resets). Detect via the `(rw,...` vs `(ro,...` mount option.
mount_line=$(adb -s "$DEVICE" shell mount | grep -E '^overlay on /system_ext type overlay' || true)
if [[ -z "$mount_line" || "$mount_line" == *"(ro,"* ]]; then
    echo "  /system_ext overlay not RW yet — running adb remount"
    remount_out=$(adb -s "$DEVICE" remount 2>&1 || true)
    echo "$remount_out" | tail -3
    if echo "$remount_out" | grep -qE 'Device must be bootloader unlocked|Not a remountable device'; then
        echo "FATAL: overlay setup blocked by AVB. Restart the emulator with -writable-system."
        exit 1
    fi
    # remount may request a reboot to finalize.
    if echo "$remount_out" | grep -qi 'reboot'; then
        adb -s "$DEVICE" reboot
        adb -s "$DEVICE" wait-for-device
        for _ in $(seq 1 30); do
            if adb -s "$DEVICE" shell 'getprop sys.boot_completed' | grep -q 1; then break; fi
            sleep 5
        done
        adb -s "$DEVICE" root >/dev/null
        adb -s "$DEVICE" wait-for-device
        # Post-reboot, the overlay is RO again; one more remount.
        adb -s "$DEVICE" remount 2>&1 | tail -3
    fi
else
    echo "  $mount_line"
fi
# Final sanity — must be RW now.
mount_line=$(adb -s "$DEVICE" shell mount | grep -E '^overlay on /system_ext type overlay' || true)
if [[ "$mount_line" != *"(rw,"* ]]; then
    echo "FATAL: /system_ext overlay is still not RW after remount:"
    echo "  $mount_line"
    exit 1
fi
echo "  $mount_line"

echo "==> [3/8] pulling libbinder_ndk.so for host-side linking"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null

echo "==> [4/8] building phasec_vintf_client (NDK)"
"$NDK_BIN/aarch64-linux-android35-clang++" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp -lbinder_ndk -llog \
    "$SCRIPT_DIR/phasec_vintf_client.cpp" \
    -o "$SCRIPT_DIR/phasec_vintf_client"

echo "==> [5/8] cross-compiling rsbinder accessor server"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK_HOME" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --features rpc,android_16 \
        --bin rpc_accessor_register_interop_server )

echo "==> [6/8] pushing artifacts (VINTF XML, server, client)"
adb -s "$DEVICE" push "$REPO_ROOT/example-hello/android/rsbinder_phasec_accessor.xml" \
    /system_ext/etc/vintf/manifest/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/aarch64-linux-android/release/rpc_accessor_register_interop_server" \
    /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$SCRIPT_DIR/phasec_vintf_client" /data/local/tmp/ >/dev/null

# Reboot only if VINTF XML wasn't already loaded — detected via
# `service check`. The VINTF parse happens at servicemanager startup,
# so a fresh push (overlay write before this run) needs a reboot to
# take effect.
echo "==> [7/8] checking if VINTF entry is already loaded by servicemanager"
sc=$(adb -s "$DEVICE" shell "service check '$SERVICE_NAME'" 2>&1 | tr -d '\r')
case "$sc" in
    *found*)
        echo "  VINTF entry already loaded: $sc"
        ;;
    *)
        echo "  VINTF entry NOT loaded yet ('$sc') — rebooting to apply"
        adb -s "$DEVICE" reboot
        adb -s "$DEVICE" wait-for-device
        for _ in $(seq 1 30); do
            if adb -s "$DEVICE" shell 'getprop sys.boot_completed' | grep -q 1; then break; fi
            sleep 5
        done
        adb -s "$DEVICE" root >/dev/null
        adb -s "$DEVICE" wait-for-device
        ;;
esac

echo "==> [8/8] starting rsbinder server + running Phase C client"
adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor_register_interop_server 2>/dev/null; rm -f $SOCK; sleep 1" || true
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_accessor_register_interop_server '$INSTANCE' '$SOCK' 2 > /data/local/tmp/phasec_srv.stdout 2> /data/local/tmp/phasec_srv.stderr &" &
sleep 3
ready=$(adb -s "$DEVICE" shell "cat /data/local/tmp/phasec_srv.stdout 2>/dev/null" | tr -d '\r')
if [[ "$ready" != *"READY"* ]]; then
    echo "rsbinder server failed to come up. stderr:"
    adb -s "$DEVICE" shell "cat /data/local/tmp/phasec_srv.stderr" || true
    exit 1
fi

# Quick servicemanager sanity (VINTF-bridged lookup).
adb -s "$DEVICE" shell "service check '$SERVICE_NAME'" 2>&1
adb -s "$DEVICE" shell "service check 'android.os.IAccessor/IInterop/default'" 2>&1

# The real Phase C gate.
client_out=$(adb -s "$DEVICE" shell "/data/local/tmp/phasec_vintf_client; echo client-exit=\$?" 2>&1)
echo "$client_out"

adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor_register_interop_server 2>/dev/null; rm -f $SOCK" || true

if [[ "$client_out" == *"client-exit=0"* ]] && [[ "$client_out" == *"PASS"* ]]; then
    echo "==> Phase C PASS"
    exit 0
fi
echo "==> Phase C FAIL"
exit 1
