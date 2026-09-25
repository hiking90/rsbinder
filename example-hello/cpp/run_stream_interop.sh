#!/usr/bin/env bash
# Plan 10-7b P4/P5 STAGE3 — the kernel-binder stream between rsbinder and two
# C++ NDK peers:
#
#   * stream_interop (P4) knows the stream only through the two C headers:
#     rsbinder-fmq/c/rsbinder_fmq.h (the ring) and contrib/ndk/rsbinder_stream.h
#     (the records on it). No libfmq, no rsbinder on the C++ side.
#   * stream_libfmq_interop (P5) runs the ring on AOSP's own libfmq
#     (`AidlMessageQueue`, `EventFlag`, libcutils memory) and writes only the
#     records and the event bits itself.
#
# rsbinder serves `streamdemo.IStreamDemo` (`stream_probe serve`); the C
# header client covers both directions:
#
#   S1  download: the C++ consumer makes the ring, 200000 items, clean end
#   S2  download: the end record carries a service-specific failure
#   S3  download: the C++ side stops reading; the producer stops at the
#       ring's capacity and finishes once reading resumes
#   S4  download: the C++ side cancels; the parked producer is released
#   S5  download: the service is killed; the C++ consumer's death link wakes it
#   S6  upload: the C++ producer writes into the service's ring
#   S7  upload: the service is killed; the C++ producer's death link wakes it
#   S8  upload: an item past the ring is refused locally, the stream goes on
#
# and the libfmq client the same ground (plans/10-7b §13.1):
#
#   L1  download: libfmq makes the ring (libcutils ashmem on a device whose
#       kernel has no ashmem-memfd compat), 200000 items, clean end
#   L2  download: the end record carries a service-specific failure
#   L3  download: libfmq stops reading; the producer stops at capacity
#   L4  download: libfmq sets CANCEL; the parked producer is released
#   L5  download: the service is killed; the death link wakes NOT_EMPTY
#   L6  upload: libfmq attaches to rsbinder's sealed memfd and produces
#   L7  upload: the service is killed; the death link wakes NOT_FULL
#
# Prereqs:
#   * a booted AVD (AOSP `default` or google_apis, rootable), SDK >= 33
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-ndk), `cargo ndk`,
#     the rustup target for the device's ABI
#   * SDK at $ANDROID_HOME (default /opt/android-sdk) with a build-tools
#     `aidl` and a platform that has `optional/libbinder_ndk_cpp`
#   * AOSP checkout at $AOSP (default ~/Workspace/aosp): the frozen V1
#     `android.hardware.common{,.fmq}` AIDL under hardware/interfaces/common,
#     frameworks/native for the platform binder_ndk headers, and for the
#     libfmq peer system/libfmq, system/core (libcutils, libutils),
#     system/libbase, system/logging — see CLAUDE.md
#   * `adb root` + `setenforce 0`, applied here and restored on exit: the
#     service registers under a name SELinux does not know.
#
# Usage: ./run_stream_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when every case passes.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
SDK="${ANDROID_HOME:-/opt/android-sdk}"
AOSP="${AOSP:-$HOME/Workspace/aosp}"
WORK="$REPO_ROOT/target/stream_interop"
DEV_DIR=/data/local/tmp
RS_LOG=$DEV_DIR/stream_probe.log
SVC=rsb10.stream

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
ADB=(adb -s "$DEVICE")

FMQ_API="$AOSP/hardware/interfaces/common/fmq/aidl/aidl_api/android.hardware.common.fmq/1"
COMMON_API="$AOSP/hardware/interfaces/common/aidl/aidl_api/android.hardware.common/1"
NDK_PLATFORM="$AOSP/frameworks/native/libs/binder/ndk/include_platform"
FMQ="$AOSP/system/libfmq"
for d in "$FMQ_API" "$COMMON_API" "$NDK_PLATFORM" "$FMQ/include" "$FMQ/base" \
         "$AOSP/system/core/libcutils/include" "$AOSP/system/core/libutils/include" \
         "$AOSP/system/libbase/include" "$AOSP/system/logging/liblog/include"; do
    [ -d "$d" ] || { echo "missing $d (see CLAUDE.md, Local AOSP Reference Checkout)" >&2; exit 2; }
done

echo "==> checking device $DEVICE"
sdk=$("${ADB[@]}" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 33 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 33"; exit 1; }

case "${ABI:-$("${ADB[@]}" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level.
CXX=""
API=""
for a in $(seq "$sdk" -1 33); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

AIDL=$(ls "$SDK"/build-tools/*/aidl 2>/dev/null | sort -V | tail -1 || true)
[ -x "$AIDL" ] || { echo "no build-tools aidl under $SDK" >&2; exit 2; }
NDK_CPP=$(ls -d "$SDK"/platforms/android-*/optional/libbinder_ndk_cpp 2>/dev/null | sort -V | tail -1 || true)
[ -d "$NDK_CPP" ] || { echo "no optional/libbinder_ndk_cpp under $SDK/platforms" >&2; exit 2; }

echo "==> compiling the .aidl files with $AIDL"
rm -rf "$WORK"
mkdir -p "$WORK/sys"
# The frozen V1 VINTF types (the build-tools `aidl` does not parse the
# current files' `@FixedSize T`; the wire is the same), then the stream
# endpoint rsbinder ships and the demo interface.
"$AIDL" --lang=ndk --structured --stability vintf -I "$FMQ_API" -I "$COMMON_API" \
    -o "$WORK/src" -h "$WORK/include" \
    "$FMQ_API"/android/hardware/common/fmq/*.aidl "$COMMON_API"/android/hardware/common/*.aidl
"$AIDL" --lang=ndk --structured -I "$FMQ_API" -I "$COMMON_API" \
    -I "$REPO_ROOT/rsbinder/aidl/stream" -I "$REPO_ROOT/tests/aidl" \
    -o "$WORK/src" -h "$WORK/include" \
    "$REPO_ROOT/rsbinder/aidl/stream/rsbinder/stream/StreamEndpoint.aidl" \
    "$REPO_ROOT/tests/aidl/streamdemo/IStreamDemo.aidl"

echo "==> pulling the device's libraries to link against"
# C-ABI libraries only: the device's libfmq.so is `std::__1` against the
# NDK's `std::__ndk1`, so the libfmq peer compiles EventFlag.cpp in (the
# ring code is header-only); libcutils is the real ashmem.
for l in libbinder_ndk.so libcutils.so; do
    "${ADB[@]}" pull "/system/lib64/$l" "$WORK/sys/$l" >/dev/null
done
GENERATED=("$WORK"/src/android/hardware/common/fmq/*.cpp "$WORK"/src/android/hardware/common/*.cpp
           "$WORK"/src/rsbinder/stream/*.cpp "$WORK"/src/streamdemo/*.cpp)

echo "==> building the C header peer (NDK clang, the two rsbinder C headers, device libbinder_ndk)"
"$CXX" -O2 -Wall -Wextra -Wno-unused-parameter -std=c++17 -DANDROID -static-libstdc++ \
    -I "$WORK/include" -I "$NDK_CPP" -I "$NDK_PLATFORM" \
    -I "$REPO_ROOT/contrib/ndk" -I "$REPO_ROOT/rsbinder-fmq/c" \
    -L "$WORK/sys" -lbinder_ndk -llog \
    "$CPP_DIR/stream_interop.cpp" "${GENERATED[@]}" \
    -o "$WORK/stream_interop"

echo "==> building the libfmq peer (NDK clang, AOSP libfmq sources, device libbinder_ndk + libcutils)"
# libfmq's headers as system headers: their -Wdeprecated-copy is AOSP's, not this peer's.
"$CXX" -O2 -Wall -Wextra -Wno-unused-parameter -std=c++17 -DANDROID -static-libstdc++ \
    -I "$WORK/include" -I "$NDK_CPP" -I "$NDK_PLATFORM" \
    -isystem "$FMQ/include" -isystem "$FMQ/base" \
    -I "$AOSP/system/core/libcutils/include" -I "$AOSP/system/core/libutils/include" \
    -I "$AOSP/system/libbase/include" -I "$AOSP/system/logging/liblog/include" \
    -L "$WORK/sys" -lbinder_ndk -lcutils -llog \
    "$CPP_DIR/stream_libfmq_interop.cpp" "$FMQ/EventFlag.cpp" "${GENERATED[@]}" \
    -o "$WORK/stream_libfmq_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin stream_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
ENFORCE=$("${ADB[@]}" shell getenforce | tr -d '\r')
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/stream_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$WORK/stream_interop" "$WORK/stream_libfmq_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/stream_probe" "$DEV_DIR/stream_interop" \
    "$DEV_DIR/stream_libfmq_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

# `pidof` matches the process name, so the shell running it is never a match.
stop_service() {
    "${ADB[@]}" shell 'for p in $(pidof stream_probe) $(pidof stream_interop) $(pidof stream_libfmq_interop); do kill $p; done' \
        >/dev/null 2>&1 || true
}
cleanup() {
    stop_service
    [ "$ENFORCE" = "Enforcing" ] && "${ADB[@]}" shell setenforce 1 >/dev/null 2>&1 || true
}
trap cleanup EXIT
stop_service

start_service() {
    "${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
    "${ADB[@]}" shell "$DEV_DIR/stream_probe serve $SVC > $RS_LOG 2>&1" &
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^SERVING $SVC' $RS_LOG 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
    return 1
}

CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 120"; fi
# The client the cases below run; each section sets it.
PEER=stream_interop
# `|| true`: the printed lines are what is judged.
run() { { $CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/$PEER $SVC $* 2>&1 || true" || true; } | tr -d '\r'; }

judge() {   # <output> <expected RESULT regex> <label>
    echo "$1" | sed 's/^/      /'
    if echo "$1" | grep -qE "^$2\$"; then ok "$3"; else bad "$3"; fi
}

# A case whose service is killed mid-stream: the client runs in the
# background until it prints READY and has moved some items, then the
# service goes.
killed_case() {   # <case args> <expected regex> <label>
    local out="$WORK/killed.out"
    # Straight to the file: a `tr` in between would hold READY until the client exits.
    { $CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/$PEER $SVC $1 2>&1" || true; } > "$out" &
    local client=$!
    for _ in $(seq 1 20); do grep -q '^READY' "$out" 2>/dev/null && break; sleep 0.5; done
    sleep 2
    "${ADB[@]}" shell 'kill -9 $(pidof stream_probe)' >/dev/null 2>&1 || true
    wait "$client" || true
    judge "$(tr -d '\r' < "$out")" "$2" "$3"
}

# Ring sizes are not multiples of the 8-byte record, so records and their
# headers straddle the ring's end on both sides' copy paths.
echo
echo "=== the C++ NDK peer against rsbinder's stream_probe"
if start_service; then
    judge "$(run download 200000 65540)" \
        "RESULT download 200000 ordered rc=0 end=0" \
        "S1 download: 200000 items in order through a 65540-byte ring the C++ side made, clean end"
    judge "$(run failing 3 77 quota-exhausted)" \
        "RESULT failing 3 rc=0 exception=-8 code=77 message=quota-exhausted" \
        "S2 download: the end record carries EX_SERVICE_SPECIFIC, the code and the message"
    judge "$(run pause 20000 4100)" \
        "RESULT pause stalled=480 20000 ordered rc=0 end=0" \
        "S3 download: the producer stopped at (4100 - 256) / 8 = 480 items and finished after reading resumed"
    judge "$(run cancel 100 4100)" \
        "RESULT cancel 100 finished=1 error=-38" \
        "S4 download: the C++ cancel released the parked producer with InvalidOperation"
    judge "$(run upload 50000 4100)" \
        "RESULT upload 50000 ordered finished=1 error=0 rc=0" \
        "S6 upload: 50000 items from the C++ producer into a 4100-byte rsbinder ring, clean end"
    judge "$(run toobig 4100)" \
        "RESULT toobig -90 10 ordered finished=1 error=0 rc=0" \
        "S8 upload: an item past the ring is -EMSGSIZE and the next ten still arrive"
    killed_case "orphan 65540" "RESULT orphan [1-9][0-9]* rc=-32" \
        "S5 download: the killed producer's death reached the waiting C++ consumer as -EPIPE"
fi
stop_service; sleep 1
if start_service; then
    killed_case "vanish 4100" "RESULT vanish [1-9][0-9]* rc=-32" \
        "S7 upload: the killed consumer's death reached the C++ producer as -EPIPE"
fi
stop_service; sleep 1

echo
echo "=== the libfmq peer against rsbinder's stream_probe"
PEER=stream_libfmq_interop
if start_service; then
    judge "$(run download 200000 65540)" \
        "RESULT download 200000 ordered rc=0 end=0 mem=(ashmem|memfd:sealed)" \
        "L1 download: 200000 items in order through a 65540-byte ring libfmq made, clean end"
    judge "$(run failing 3 77 quota-exhausted)" \
        "RESULT failing 3 rc=0 exception=-8 code=77 message=quota-exhausted" \
        "L2 download: libfmq reads the end record's EX_SERVICE_SPECIFIC, code and message"
    judge "$(run pause 20000 4100)" \
        "RESULT pause stalled=480 20000 ordered rc=0 end=0" \
        "L3 download: the producer stopped at 480 items in libfmq's ring and finished after reading resumed"
    judge "$(run cancel 100 4100)" \
        "RESULT cancel 100 finished=1 error=-38" \
        "L4 download: libfmq's EventFlag::wake(CANCEL) released the parked producer with InvalidOperation"
    judge "$(run upload 50000 4100)" \
        "RESULT upload 50000 ordered finished=1 error=0 rc=0" \
        "L6 upload: 50000 items from a libfmq producer into a 4100-byte rsbinder ring, clean end"
    killed_case "orphan 65540" "RESULT orphan [1-9][0-9]* rc=-32" \
        "L5 download: the killed producer's death woke libfmq's NOT_EMPTY wait as -EPIPE"
fi
stop_service; sleep 1
if start_service; then
    killed_case "vanish 4100" "RESULT vanish [1-9][0-9]* rc=-32" \
        "L7 upload: the killed consumer's death woke libfmq's NOT_FULL wait as -EPIPE"
fi

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
