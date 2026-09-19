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
#   (c) rsbinder `shm_probe serve` publishes a $SHM_LARGE-byte heap
#       (default 64 MB, far above the 4 MB transaction limit); the C++
#       `client-large` checks every byte and writes the tail, which
#       `shm_probe tail` reads back from a third process.
#   (d) C++ `server-large` publishes a heap of the same size;
#       `shm_probe read` checks every byte, writes the tail and reads it
#       back through a second proxy. Both sides must report the same
#       checksum.
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
LARGE="${SHM_LARGE:-$((64 * 1024 * 1024))}"
RS_LARGE=rsbinder.test.shm.large
CPP_LARGE=rsbinder.test.cppshm.large

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

# Poll a device log for the SERVING line; filling a large heap takes a while.
wait_serving() {
    for _ in $(seq 1 60); do
        "${ADB[@]}" shell "grep -q SERVING $DEV_DIR/$1" 2>/dev/null && return 0
        sleep 1
    done
    "${ADB[@]}" shell cat "$DEV_DIR/$1"
    return 1
}

echo "==> [1/9] pulling system libs for host-side linking"
for l in libbinder.so libutils.so libcutils.so libbase.so liblog.so libc++.so; do
    "${ADB[@]}" pull "/system/lib64/$l" "$WORK/sys/$l" >/dev/null
done

echo "==> [2/9] compiling imemory_interop.cpp against AOSP headers + device libbinder"
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

echo "==> [3/9] cross-compiling rsbinder test_service + shm_probe + tests binary"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" build -p tests --bin test_service --bin shm_probe >/dev/null 2>&1 )
TESTBIN=$( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" test --no-run -p tests 2>&1 \
    | grep -E "Executable unittests src/lib.rs" | sed -E 's/.*\((.*)\)/\1/' )

echo "==> [4/9] pushing"
"${ADB[@]}" push "$WORK/imemory_interop" "$DEV_DIR/imemory_interop" >/dev/null
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/debug/test_service" "$DEV_DIR/test_service" >/dev/null
"${ADB[@]}" push "$REPO_ROOT/$TESTBIN" "$DEV_DIR/tests_testbin" >/dev/null
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/debug/shm_probe" "$DEV_DIR/shm_probe" >/dev/null
"${ADB[@]}" shell "chmod 755 $DEV_DIR/imemory_interop $DEV_DIR/test_service $DEV_DIR/tests_testbin $DEV_DIR/shm_probe"

echo "==> [5/9] (re)starting rsbinder test_service"
"${ADB[@]}" shell "pkill -x test_service; pkill -x imemory_interop; sleep 1; cd $DEV_DIR && (TMPDIR=$DEV_DIR nohup ./test_service >ts.log 2>&1 &); sleep 4; grep -q TEST_SERVICE_READY ts.log"

echo "==> [6/9] (a) real libbinder client → rsbinder IMemory"
out_a=$("${ADB[@]}" shell "cd $DEV_DIR && ./imemory_interop client; echo client-exit=\$?; TMPDIR=$DEV_DIR ./tests_testbin --ignored --nocapture stage3_cpp_imemory_client_marker_visible 2>&1 | grep -E 'STAGE3|test result'")
echo "$out_a"

echo "==> [7/9] (b) real libbinder MemoryHeapBase server → rsbinder BpMemory"
out_b=$("${ADB[@]}" shell "cd $DEV_DIR && (nohup ./imemory_interop server >cpp_srv.log 2>&1 &); sleep 3; cat cpp_srv.log; TMPDIR=$DEV_DIR ./tests_testbin --ignored --nocapture stage3_cpp_memory_heap_server_readable 2>&1 | grep -E 'STAGE3|test result'; pkill -x imemory_interop")
echo "$out_b"

"${ADB[@]}" shell "pkill -x test_service" || true

echo "==> [8/9] (c) rsbinder serves a $LARGE-byte heap → real libbinder client"
"${ADB[@]}" shell "pkill -x shm_probe; cd $DEV_DIR && rm -f shm_srv.log && (nohup ./shm_probe serve $RS_LARGE $LARGE >shm_srv.log 2>&1 &)"
out_c=""
if wait_serving shm_srv.log; then
    out_c=$("${ADB[@]}" shell "cd $DEV_DIR && ./imemory_interop client-large $RS_LARGE $LARGE; ./shm_probe tail $RS_LARGE $LARGE" | tr -d '\r') || true
fi
echo "$out_c"
"${ADB[@]}" shell "pkill -x shm_probe" || true

echo "==> [9/9] (d) real libbinder serves a $LARGE-byte heap → rsbinder client"
"${ADB[@]}" shell "pkill -x imemory_interop; cd $DEV_DIR && rm -f cpp_large.log && (nohup ./imemory_interop server-large $CPP_LARGE $LARGE >cpp_large.log 2>&1 &)"
out_d=""
if wait_serving cpp_large.log; then
    out_d=$("${ADB[@]}" shell "cd $DEV_DIR && ./shm_probe read $CPP_LARGE $LARGE" | tr -d '\r') || true
fi
echo "$out_d"
"${ADB[@]}" shell "pkill -x imemory_interop" || true

echo "$out_a" | grep -q "STAGE3_4_7A_CLIENT_PASS"          || { echo "FAIL: (a) C++ client"; exit 1; }
echo "$out_a" | grep -q "client-exit=0"                    || { echo "FAIL: (a) client-exit"; exit 1; }
echo "$out_a" | grep -q "STAGE3_4_7A_RUST_SEES_CPP_MARKER" || { echo "FAIL: (a) rust marker check"; exit 1; }
echo "$out_b" | grep -q "STAGE3_4_7A_SERVER_READY"         || { echo "FAIL: (b) C++ server"; exit 1; }
echo "$out_b" | grep -q "STAGE3_4_7A_RUST_READS_CPP_HEAP"  || { echo "FAIL: (b) rust client"; exit 1; }
sum_c=$(echo "$out_c" | sed -nE "s/^RESULT cpp-shm $LARGE OK ([0-9]+)$/\1/p")
[ -n "$sum_c" ]                                            || { echo "FAIL: (c) C++ client-large"; exit 1; }
echo "$out_c" | grep -qx "RESULT shm-tail $LARGE cpp-large-tail" || { echo "FAIL: (c) tail not visible to rsbinder"; exit 1; }
sum_d=$(echo "$out_d" | sed -nE "s/^RESULT shm $LARGE OK ([0-9]+)$/\1/p")
[ -n "$sum_d" ]                                            || { echo "FAIL: (d) rsbinder read"; exit 1; }
[ "$sum_c" = "$sum_d" ]                                    || { echo "FAIL: checksums differ ($sum_c vs $sum_d)"; exit 1; }
echo "==> PASS"
