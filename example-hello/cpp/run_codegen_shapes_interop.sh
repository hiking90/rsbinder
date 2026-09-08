#!/usr/bin/env bash
# Real-libbinder cross-check for the argument shapes whose generated Rust
# changed with the 2026-08 AIDL review. See `codegen_shapes_interop.cpp`
# for what is asserted and why an rsbinder-to-rsbinder round trip cannot
# assert it.
#
# Self-contained: the C++ client needs only the NDK — its own
# `android/binder_{ibinder,parcel,status}.h` and `libbinder_ndk.so` stub.
# Nothing is pulled off the device and no AOSP checkout is consulted; the
# platform-only `AServiceManager_*` entry points are resolved with `dlsym`
# against the device's libbinder_ndk at run time.
#
# Prereqs:
#   * A booted Android device/emulator, API >= 29.
#   * NDK at $ANDROID_NDK_HOME (default /opt/homebrew/share/android-ndk).
#   * cargo-ndk + the rustup target for the device's ABI.
#
# Usage: ./run_codegen_shapes_interop.sh [-s <device>] [-t <abi>]
# The ABI defaults to the device's own; -t overrides it.
# Exit 0 on PASS.

set -euo pipefail

DEVICE=""
ABI=""
while getopts "s:t:" opt; do
    case "$opt" in
        s) DEVICE="$OPTARG" ;;
        t) ABI="$OPTARG" ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
adb_() { if [ -n "$DEVICE" ]; then adb -s "$DEVICE" "$@"; else adb "$@"; fi; }

CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
TOP_DIR="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"
REMOTE=/data/local/tmp/rsb_shapes

# For the two 64-bit ABIs the NDK clang prefix and the cargo target
# directory are both the Rust triple; the 32-bit ones differ and are not
# covered.
case "${ABI:-$(adb_ shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level, so walk
# down to the newest clang the NDK actually ships at or below it.
DEV_API=$(adb_ shell getprop ro.build.version.sdk | tr -d '\r')
CXX=""
API=""
for a in $(seq "$DEV_API" -1 29); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $DEV_API under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $DEV_API)"

echo "==> [1/5] building the C++ client (NDK only)"
"$CXX" -O2 -Wall -Wextra -std=c++17 -static-libstdc++ \
    "$CPP_DIR/codegen_shapes_interop.cpp" \
    -lbinder_ndk -llog -ldl \
    -o "$CPP_DIR/codegen_shapes_interop"

echo "==> [2/5] cross-building the rsbinder service"
(cd "$TOP_DIR" && cargo ndk -t "$TRIPLE" -p "$API" build \
    --bin codegen_shapes_interop_service >/dev/null)

echo "==> [3/5] pushing to $REMOTE"
adb_ shell "mkdir -p $REMOTE"
adb_ push "$CPP_DIR/codegen_shapes_interop" "$REMOTE/" >/dev/null
adb_ push "$TOP_DIR/target/$TRIPLE/debug/codegen_shapes_interop_service" \
    "$REMOTE/" >/dev/null
adb_ shell "chmod 755 $REMOTE/codegen_shapes_interop $REMOTE/codegen_shapes_interop_service"

cleanup() {
    # Bracket the first character so the pattern cannot match the `adb shell`
    # wrapper that carries it.
    adb_ shell "pkill -f '[c]odegen_shapes_interop_service'" >/dev/null 2>&1 || true
    adb_ shell "rm -rf $REMOTE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> [4/5] starting the service and waiting for registration"
adb_ shell "cd $REMOTE && (nohup ./codegen_shapes_interop_service > service.log 2>&1 &)"
ready=""
for _ in $(seq 1 30); do
    if adb_ shell "grep -c SHAPES_SERVICE_READY $REMOTE/service.log 2>/dev/null" \
        | tr -d '\r' | grep -qv '^0$'; then
        ready=1
        break
    fi
    sleep 1
done
if [ -z "$ready" ]; then
    echo "service did not register:" >&2
    adb_ shell "cat $REMOTE/service.log" >&2 || true
    exit 3
fi

echo "==> [5/5] running the real-libbinder client"
out=$(adb_ shell "cd $REMOTE && ./codegen_shapes_interop 2>&1; echo rc=\$?" | tr -d '\r')
echo "$out"
echo "$out" | grep -q "^rc=0$" || { echo "client exit != 0" >&2; exit 1; }
echo "$out" | grep -q CODEGEN_SHAPES_INTEROP_PASS || { echo "PASS marker missing" >&2; exit 1; }
echo "==> PASS"
