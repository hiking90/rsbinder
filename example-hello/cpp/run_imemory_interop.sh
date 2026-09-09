#!/usr/bin/env bash
# Plan 4-7a E5 STAGE3 — shared memory (`android.utils.IMemory` /
# `android.utils.IMemoryHeap`) interop between rsbinder and the real
# libbinder on an Android emulator, both directions:
#
#   (a) rsbinder `test_service` serves `rsbinder.test.shm`; the C++
#       `imemory_interop client` (`interface_cast<IMemory>`) reads the
#       window and writes a marker; the rsbinder test
#       `stage3_cpp_imemory_client_marker_visible` then sees the marker.
#   (b) C++ `imemory_interop server` (`MemoryHeapBase` + `MemoryBase`)
#       serves `rsbinder.test.cppshm`; the rsbinder test
#       `stage3_cpp_memory_heap_server_readable` maps it via `BpMemory`.
#
# The C++ binary links against the *device's own* libbinder.so (pulled
# here) with AOSP headers — `IMemory` is libbinder's private C++ API,
# not in libbinder_ndk.
#
# Prereqs:
#   * a google_apis (rootable) emulator booted, e.g. Android_16;
#     `adb root` + `setenforce 0` are applied (test_service's addService
#     needs it, like the rest of the tests/ package).
#   * NDK at $ANDROID_NDK_HOME (default /opt/homebrew/share/android-ndk),
#     cargo-ndk, the rustup target for the device's ABI.
#   * AOSP checkout at $AOSP (default /Users/king/Workspace/android) for
#     the libbinder / libutils / libbase / liblog / libcutils headers.
#
# Usage: ./run_imemory_interop.sh [-s emulator-5554] [-t <abi>]
# The ABI defaults to the device's own; -t overrides it.
# Exit 0 on PASS (both directions).

set -euo pipefail

DEVICE=emulator-5554
ABI=""
AOSP="${AOSP:-/Users/king/Workspace/android}"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${TMPDIR:-/tmp}/rsbinder-imemory-interop"
DEV_DIR=/data/local/tmp

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
ADB=(adb -s "$DEVICE")

# For the two 64-bit ABIs the NDK clang prefix and the cargo target
# directory are both the Rust triple; the 32-bit ones differ and are not
# covered.
sdk=$("${ADB[@]}" shell getprop ro.build.version.sdk | tr -d '\r')
case "${ABI:-$("${ADB[@]}" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level, so walk
# down to the newest clang the NDK actually ships at or below it.
CXX=""
API=""
for a in $(seq "$sdk" -1 29); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

mkdir -p "$WORK/sys"

echo "==> [1/7] pulling system libs for host-side linking"
for l in libbinder.so libutils.so libcutils.so libbase.so liblog.so libc++.so; do
    "${ADB[@]}" pull "/system/lib64/$l" "$WORK/sys/$l" >/dev/null
done

echo "==> [2/7] compiling imemory_interop.cpp against AOSP headers + device libbinder"
"$CXX" -O2 -Wall -std=c++20 -DANDROID -D__ANDROID_API__="$API" \
    -I "$AOSP/frameworks/native/libs/binder/include" \
    -I "$AOSP/system/core/libutils/include" \
    -I "$AOSP/system/libbase/include" \
    -I "$AOSP/system/logging/liblog/include" \
    -I "$AOSP/system/core/libcutils/include" \
    -I "$AOSP/system/core/libsystem/include" \
    -I "$AOSP/frameworks/native/libs/binder/ndk/include_cpp" \
    -L "$WORK/sys" -lbinder -lutils -lcutils -llog -lbase -nostdlib++ -lc++ \
    -o "$WORK/imemory_interop" "$REPO_ROOT/example-hello/cpp/imemory_interop.cpp"

echo "==> [3/7] cross-compiling rsbinder test_service + tests binary"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" build -p tests --bin test_service >/dev/null 2>&1 )
TESTBIN=$( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" test --no-run -p tests 2>&1 \
    | grep -E "Executable unittests src/lib.rs" | sed -E 's/.*\((.*)\)/\1/' )

echo "==> [4/7] pushing"
"${ADB[@]}" push "$WORK/imemory_interop" "$DEV_DIR/imemory_interop" >/dev/null
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/debug/test_service" "$DEV_DIR/test_service" >/dev/null
"${ADB[@]}" push "$REPO_ROOT/$TESTBIN" "$DEV_DIR/tests_testbin" >/dev/null
"${ADB[@]}" shell "chmod 755 $DEV_DIR/imemory_interop $DEV_DIR/test_service $DEV_DIR/tests_testbin"

echo "==> [5/7] (re)starting rsbinder test_service"
"${ADB[@]}" shell "pkill -x test_service; pkill -x imemory_interop; sleep 1; cd $DEV_DIR && (TMPDIR=$DEV_DIR nohup ./test_service >ts.log 2>&1 &); sleep 4; grep -q TEST_SERVICE_READY ts.log"

echo "==> [6/7] (a) real libbinder client → rsbinder IMemory"
out_a=$("${ADB[@]}" shell "cd $DEV_DIR && ./imemory_interop client; echo client-exit=\$?; TMPDIR=$DEV_DIR ./tests_testbin --ignored --nocapture stage3_cpp_imemory_client_marker_visible 2>&1 | grep -E 'STAGE3|test result'")
echo "$out_a"

echo "==> [7/7] (b) real libbinder MemoryHeapBase server → rsbinder BpMemory"
out_b=$("${ADB[@]}" shell "cd $DEV_DIR && (nohup ./imemory_interop server >cpp_srv.log 2>&1 &); sleep 3; cat cpp_srv.log; TMPDIR=$DEV_DIR ./tests_testbin --ignored --nocapture stage3_cpp_memory_heap_server_readable 2>&1 | grep -E 'STAGE3|test result'; pkill -x imemory_interop")
echo "$out_b"

"${ADB[@]}" shell "pkill -x test_service" || true

echo "$out_a" | grep -q "STAGE3_4_7A_CLIENT_PASS"          || { echo "FAIL: (a) C++ client"; exit 1; }
echo "$out_a" | grep -q "client-exit=0"                    || { echo "FAIL: (a) client-exit"; exit 1; }
echo "$out_a" | grep -q "STAGE3_4_7A_RUST_SEES_CPP_MARKER" || { echo "FAIL: (a) rust marker check"; exit 1; }
echo "$out_b" | grep -q "STAGE3_4_7A_SERVER_READY"         || { echo "FAIL: (b) C++ server"; exit 1; }
echo "$out_b" | grep -q "STAGE3_4_7A_RUST_READS_CPP_HEAP"  || { echo "FAIL: (b) rust client"; exit 1; }
echo "==> PASS"
