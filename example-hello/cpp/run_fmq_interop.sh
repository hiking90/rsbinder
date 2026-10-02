#!/usr/bin/env bash
# Plan 12-fmq F2 STAGE3 — a Fast Message Queue whose descriptor crosses
# binder, between rsbinder and AOSP's own libfmq + libbinder_ndk on an
# Android emulator, with either side as the service:
#
#   (a) rsbinder serves `fmqinterop.IFmqPeer`; the C++ client takes the
#       descriptor of a queue rsbinder made (memfd), hands rsbinder one
#       libfmq made (libcutils' region), moves 5000 items each way over
#       each, and damages the counters of one to record rsbinder's answer.
#   (b) the C++ peer serves; `fmq_probe client` does the same from the
#       rsbinder side — the queue it receives is libcutils' region (ashmem:
#       plan §6.2 S2, no seals), the one it makes is a sealed memfd, and
#       the three damages record libfmq's answers (S4).
#   (c) (b)'s first case again with `sys.use_memfd=true`, so libcutils
#       makes a memfd sealed GROW|SHRINK and rsbinder attaches to that.
#
# Which region libcutils makes depends on the device (see LIBCUTILS_KIND
# below): ashmem up to Android 16, a sealed memfd on an Android 17 device
# that meets the VSR memfd requirements. There no native binary can get
# ashmem from libcutils, so the ashmem path of (a2')/(b1) is covered only
# on an SDK <= 36 device, and (c) is skipped as identical to (b1).
#
# The C++ half is built from the two `.aidl` files in tests/aidl/fmqinterop
# plus the frozen V1 `android.hardware.common{,.fmq}` API from the AOSP
# checkout (the platform `aidl` in build-tools 34 does not parse the
# `@FixedSize T` parameter annotation the current files carry; the wire is
# the same), compiled by the platform's own `aidl`, and linked against the
# device's libbinder_ndk.so / libfmq.so / libcutils.so.
#
# Prereqs:
#   * a booted AVD (AOSP `default` or google_apis, rootable), SDK >= 33
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-ndk), `cargo ndk`,
#     the rustup target for the device's ABI
#   * SDK at $ANDROID_HOME (default /opt/android-sdk) with a build-tools
#     `aidl` and a platform that has `optional/libbinder_ndk_cpp`
#   * AOSP checkout at $AOSP (default ~/Workspace/aosp): system/libfmq,
#     hardware/interfaces/common (fmq + aidl), system/core (libcutils,
#     libutils), system/libbase, system/logging — see CLAUDE.md
#   * `adb root` + `setenforce 0`, applied here: both halves register a
#     service under a name SELinux does not know.
#
# Usage: ./run_fmq_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when every case passes.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
SDK="${ANDROID_HOME:-/opt/android-sdk}"
AOSP="${AOSP:-$HOME/Workspace/aosp}"
WORK="$REPO_ROOT/target/fmq_interop"
DEV_DIR=/data/local/tmp
RS_LOG=$DEV_DIR/fmq_probe.log
CPP_LOG=$DEV_DIR/fmq_cpp.log
RS_SVC=rsb12.rust
CPP_SVC=rsb12.cpp
COUNT=5000
CAPACITY=64

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
ADB=(adb -s "$DEVICE")

FMQ="$AOSP/system/libfmq"
FMQ_API="$AOSP/hardware/interfaces/common/fmq/aidl/aidl_api/android.hardware.common.fmq/1"
COMMON_API="$AOSP/hardware/interfaces/common/aidl/aidl_api/android.hardware.common/1"
for d in "$FMQ/include" "$FMQ/base" "$FMQ_API" "$COMMON_API" \
         "$AOSP/system/core/libcutils/include" "$AOSP/system/core/libutils/include" \
         "$AOSP/system/libbase/include" "$AOSP/system/logging/liblog/include"; do
    [ -d "$d" ] || { echo "missing $d (see CLAUDE.md, Local AOSP Reference Checkout)" >&2; exit 2; }
done

echo "==> checking device $DEVICE"
sdk=$("${ADB[@]}" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 33 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 33"; exit 1; }

# The region libcutils makes for a queue, predicted from the device rather
# than read back from either peer. Android 17 `ashmem-dev.cpp` (`__use_memfd`)
# picks memfd when the kernel has the `memfd_class` sepolicy capability and
# `ro.vendor.api_level` >= 202604; its third condition, target SDK >= 37,
# holds for any executable on such a device, because the linker gives an
# executable the platform's API level (bionic `linker_config.cpp`), not the
# one it was built for. Before 17, memfd needs `sys.use_memfd=true` — (c).
LIBCUTILS_KIND=ashmem
if [[ "$sdk" -ge 37 ]]; then
    vendor_api=$("${ADB[@]}" shell getprop ro.vendor.api_level | tr -d '\r')
    memfd_class=$("${ADB[@]}" shell \
        'test -e /sys/fs/selinux/policy_capabilities/memfd_class && echo yes' | tr -d '\r')
    if [[ "$memfd_class" == yes && "${vendor_api:-0}" -ge 202604 ]]; then
        LIBCUTILS_KIND=memfd:sealed
    fi
fi
echo "==> libcutils makes $LIBCUTILS_KIND regions on this device"

case "${ABI:-$("${ADB[@]}" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level, so walk
# down to the newest clang the NDK actually ships at or below it.
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
# The VINTF types first, as their own module (`--stability vintf`), then
# the interop interface, which is not VINTF-stable but uses them.
"$AIDL" --lang=ndk --structured --stability vintf -I "$FMQ_API" -I "$COMMON_API" \
    -o "$WORK/src" -h "$WORK/include" \
    "$FMQ_API"/android/hardware/common/fmq/*.aidl "$COMMON_API"/android/hardware/common/*.aidl
"$AIDL" --lang=ndk --structured -I "$FMQ_API" -I "$COMMON_API" -I "$REPO_ROOT/tests/aidl" \
    -o "$WORK/src" -h "$WORK/include" \
    "$REPO_ROOT"/tests/aidl/fmqinterop/*.aidl

echo "==> pulling the device's libraries to link against"
# Only C-ABI libraries: the device's libc++ is `std::__1`, the NDK's is
# `std::__ndk1`, so libfmq.so (EventFlag, std::string in its ABI) cannot
# be linked from an NDK build; EventFlag.cpp is compiled in below and the
# ring/counter code is header-only anyway. libcutils is the real ashmem.
for l in libbinder_ndk.so libcutils.so; do
    "${ADB[@]}" pull "/system/lib64/$l" "$WORK/sys/$l" >/dev/null
done

echo "==> building the C++ half (NDK clang, AOSP libfmq sources, device libbinder_ndk + libcutils)"
# `AServiceManager_*` / `ABinderProcess_*` are platform API, absent from
# the NDK sysroot: their headers come from the checkout, the symbols from
# the device's libbinder_ndk.so.
"$CXX" -O2 -Wall -Wno-unused-parameter -std=c++17 -DANDROID -static-libstdc++ \
    -I "$WORK/include" -I "$NDK_CPP" \
    -I "$AOSP/frameworks/native/libs/binder/ndk/include_platform" \
    -I "$FMQ/include" -I "$FMQ/base" \
    -I "$AOSP/system/core/libcutils/include" \
    -I "$AOSP/system/core/libutils/include" \
    -I "$AOSP/system/libbase/include" \
    -I "$AOSP/system/logging/liblog/include" \
    -L "$WORK/sys" -lbinder_ndk -lcutils -llog \
    "$CPP_DIR/fmq_interop.cpp" "$FMQ/EventFlag.cpp" \
    "$WORK"/src/android/hardware/common/fmq/*.cpp \
    "$WORK"/src/android/hardware/common/*.cpp \
    "$WORK"/src/fmqinterop/*.cpp \
    -o "$WORK/fmq_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin fmq_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/fmq_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$WORK/fmq_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/fmq_probe" "$DEV_DIR/fmq_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    "${ADB[@]}" shell 'pkill -f "[f]mq_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[f]mq_interop" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell setprop sys.use_memfd false >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

wait_for_line() {   # <log> <line-prefix>
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^$2' $1 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 120"; fi
# `|| true`: the printed lines are what is judged.
run() { { $CALL_TIMEOUT "${ADB[@]}" shell "$* 2>/dev/null || true" || true; } | tr -d '\r'; }

start_cpp_server() {
    "${ADB[@]}" shell "rm -f $CPP_LOG" >/dev/null 2>&1 || true
    "${ADB[@]}" shell "$DEV_DIR/fmq_interop serve $CPP_SVC > $CPP_LOG 2>&1" &
    wait_for_line "$CPP_LOG" "SERVING $CPP_SVC" && return 0
    bad "the C++ service did not start"
    "${ADB[@]}" shell cat "$CPP_LOG"
    return 1
}

# A damage line is judged on shape only: `avail=<n>/<n> read=<n> sum=<n>`
# after the name. What the numbers are is the record (see the plan).
damage_line() {   # <output> <what>
    echo "$1" | grep -qE "^RESULT corrupt $2 avail=-?[0-9]+/-?[0-9]+ read=-?[0-9]+ sum=-?[0-9]+$"
}

echo
echo "=== (a) rsbinder serves, libfmq + libbinder_ndk is the client"
"${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/fmq_probe serve $RS_SVC > $RS_LOG 2>&1" &
if wait_for_line "$RS_LOG" "SERVING $RS_SVC"; then
    out=$(run "$DEV_DIR/fmq_interop client $RS_SVC all $COUNT $CAPACITY")
    echo "$out" | sed 's/^/      /'
    echo "$out" | grep -qx "RESULT server-queue $COUNT kind=memfd:sealed capacity=$CAPACITY in=ok out=ok" \
        && ok "(a1) libfmq attached to rsbinder's memfd queue; $COUNT items each way" \
        || bad "(a1) server-queue"
    echo "$out" | grep -qx "RESULT client-queue $COUNT adopted=$CAPACITY in=ok out=ok" \
        && ok "(a2) rsbinder adopted libfmq's $LIBCUTILS_KIND queue; $COUNT items each way" \
        || bad "(a2) client-queue"
    # rsbinder's contract: `Corrupted` on every operation over a damaged ring.
    for what in read-ahead over-capacity unaligned; do
        echo "$out" | grep -qx "RESULT corrupt $what avail=-1/-1 read=-1 sum=0" \
            && ok "(a3) rsbinder refused the ring libfmq's side damaged ($what)" \
            || bad "(a3) $what"
    done
    "${ADB[@]}" shell "grep -q 'adopted a $CAPACITY-item queue ($LIBCUTILS_KIND)' $RS_LOG" \
        && ok "(a2') the queue libfmq made reached rsbinder as $LIBCUTILS_KIND" \
        || { bad "(a2') rsbinder did not see $LIBCUTILS_KIND"; "${ADB[@]}" shell cat "$RS_LOG"; }
else
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
fi
cleanup; sleep 1

echo
echo "=== (b) libfmq + libbinder_ndk serves, rsbinder is the client"
if start_cpp_server; then
    out=$(run "$DEV_DIR/fmq_probe client $CPP_SVC all $COUNT $CAPACITY")
    echo "$out" | sed 's/^/      /'
    echo "$out" | grep -qx "RESULT server-queue $COUNT kind=$LIBCUTILS_KIND capacity=$CAPACITY in=ok out=ok" \
        && ok "(b1) rsbinder attached to libfmq's $LIBCUTILS_KIND queue; $COUNT items each way" \
        || bad "(b1) server-queue (expected kind=$LIBCUTILS_KIND)"
    echo "$out" | grep -qx "RESULT client-queue $COUNT adopted=$CAPACITY in=ok out=ok" \
        && ok "(b2) libfmq adopted rsbinder's memfd queue; $COUNT items each way" \
        || bad "(b2) client-queue"
    for what in read-ahead over-capacity unaligned; do
        damage_line "$out" "$what" \
            && ok "(b3) libfmq answered a damaged ring ($what) — recorded above" \
            || bad "(b3) $what"
    done
    "${ADB[@]}" shell "grep -q 'adopted a $CAPACITY-item queue (memfd:sealed)' $CPP_LOG" \
        && ok "(b2') the queue rsbinder made reached libfmq as a sealed memfd" \
        || { bad "(b2') libfmq did not see a sealed memfd"; "${ADB[@]}" shell cat "$CPP_LOG"; }
fi
cleanup; sleep 1

echo
echo "=== (c) as (b1), libcutils making memfd instead of ashmem"
# libcutils takes the property only on a kernel whose memfd answers
# `ASHMEM_GET_SIZE` (Android 16 `ashmem-dev.cpp`, `__has_memfd_support`);
# the GKI the API 36 emulator boots does not, and logs
# "no ashmem-memfd compat support". Then the case is not reachable here.
if [[ "$LIBCUTILS_KIND" == memfd:sealed ]]; then
    printf '  SKIP  %s\n' "(c) libcutils already makes memfd here without the property; (b1) covered it"
elif "${ADB[@]}" shell setprop sys.use_memfd true && start_cpp_server; then
    out=$(run "$DEV_DIR/fmq_probe client $CPP_SVC server-queue $COUNT $CAPACITY")
    echo "$out" | sed 's/^/      /'
    if echo "$out" | grep -qx "RESULT server-queue $COUNT kind=memfd:sealed capacity=$CAPACITY in=ok out=ok"; then
        ok "(c) rsbinder attached to libcutils' memfd (sealed GROW|SHRINK) queue"
    elif echo "$out" | grep -qx "RESULT server-queue $COUNT kind=ashmem capacity=$CAPACITY in=ok out=ok" \
         && "${ADB[@]}" logcat -d -s ashmem 2>/dev/null | grep -q "no ashmem-memfd compat support"; then
        printf '  SKIP  %s\n' "(c) this kernel has no ashmem-memfd compat, so libcutils stays on ashmem"
    else
        bad "(c) server-queue over memfd"
    fi
fi
cleanup

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
