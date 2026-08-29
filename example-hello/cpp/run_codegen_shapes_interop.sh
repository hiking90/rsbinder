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
#   * A booted Android device/emulator (arm64), API >= 29.
#   * NDK at $ANDROID_NDK_HOME (default /opt/homebrew/share/android-ndk).
#   * cargo-ndk + the aarch64-linux-android rustup target.
#
# Usage: ./run_codegen_shapes_interop.sh [-s <device>]
# Exit 0 on PASS.

set -euo pipefail

DEVICE=""
while getopts "s:" opt; do
    case "$opt" in
        s) DEVICE="$OPTARG" ;;
        *) echo "usage: $0 [-s <device>]" >&2; exit 2 ;;
    esac
done
adb_() { if [ -n "$DEVICE" ]; then adb -s "$DEVICE" "$@"; else adb "$@"; fi; }

CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
TOP_DIR="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"
REMOTE=/data/local/tmp/rsb_shapes
API=35

CXX=$(ls "$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android${API}-clang++ | head -1)
[ -x "$CXX" ] || { echo "NDK clang++ not found under $NDK" >&2; exit 2; }

echo "==> [1/5] building the C++ client (NDK only)"
"$CXX" -O2 -Wall -Wextra -std=c++17 -static-libstdc++ \
    "$CPP_DIR/codegen_shapes_interop.cpp" \
    -lbinder_ndk -llog -ldl \
    -o "$CPP_DIR/codegen_shapes_interop"

echo "==> [2/5] cross-building the rsbinder service"
(cd "$TOP_DIR" && cargo ndk -t arm64-v8a -p "$API" build \
    --bin codegen_shapes_interop_service >/dev/null)

echo "==> [3/5] pushing to $REMOTE"
adb_ shell "mkdir -p $REMOTE"
adb_ push "$CPP_DIR/codegen_shapes_interop" "$REMOTE/" >/dev/null
adb_ push "$TOP_DIR/target/aarch64-linux-android/debug/codegen_shapes_interop_service" \
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
