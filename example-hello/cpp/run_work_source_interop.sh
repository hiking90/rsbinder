#!/usr/bin/env bash
# Plan 10-9 AC-9.3 STAGE3 — the calling work source between rsbinder and the
# real AOSP binder stack.
#
#   (a) rsbinder client sets it     -> libbinder service reads it
#   (b) libbinder client sets it    -> rsbinder service reads it
#   (c) libbinder -> rsbinder -> libbinder: the received value is not
#       forwarded unless the rsbinder handler sets its own
#   (d) nothing a handler set leaks into the next call its thread serves
#
# The work source adds no bytes of its own: it is one field of the request
# header both stacks already write. With rsbinder on both ends a shared
# misplacement of that field would pass, hence a real libbinder peer.
#
# The NDK has no work-source API, so the C++ half resolves
# `IPCThreadState::{self,setCallingWorkSourceUid,getCallingWorkSourceUid,
# shouldPropagateWorkSource}` from the device's libbinder.so with dlsym.
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0` are applied by this script: both halves
#     register a service, which SELinux otherwise denies.
#
# Usage: ./run_work_source_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when every check passes.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
LIB_DIR="$REPO_ROOT/target/stage3-libs"
DEV_DIR=/data/local/tmp
RS_LOG=$DEV_DIR/work_source_probe.log
CPP_LOG=$DEV_DIR/work_source_cpp.log
RS_SVC=rsb109.rust
CPP_SVC=rsb109.cpp

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
ADB=(adb -s "$DEVICE")

echo "==> checking device $DEVICE"
sdk=$("${ADB[@]}" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 29 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 29"; exit 1; }

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

echo "==> pulling libbinder_ndk.so so we can link against it"
mkdir -p "$LIB_DIR"
"${ADB[@]}" pull /system/lib64/libbinder_ndk.so "$LIB_DIR/libbinder_ndk.so" >/dev/null

echo "==> building the C++ half (NDK)"
"$CXX" -O2 -Wall -std=c++17 -static-libstdc++ \
    -L "$LIB_DIR" -lbinder_ndk -ldl \
    "$CPP_DIR/work_source_interop.cpp" -o "$CPP_DIR/work_source_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin work_source_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/work_source_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/work_source_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/work_source_probe" "$DEV_DIR/work_source_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    # Bracket patterns so pkill does not match the device shell carrying
    # the pattern itself.
    "${ADB[@]}" shell 'pkill -f "[w]ork_source_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[w]ork_source_interop" 2>/dev/null' >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

wait_for_serving() {   # <log> <name>
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^SERVING $2' $1 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 60"; fi

# expect <description> <device command> <wanted line>
expect() {
    local out
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$2 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "$3" ] && ok "$1" || bad "$1: got '$out', want '$3'"
}

echo "==> starting both services"
"${ADB[@]}" shell "rm -f $RS_LOG $CPP_LOG" >/dev/null 2>&1 || true
# The trailing `&` backgrounds the host-side `adb shell`: adb holds the
# connection for as long as the remote process lives.
"${ADB[@]}" shell "$DEV_DIR/work_source_interop server $CPP_SVC > $CPP_LOG 2>&1" &
if ! wait_for_serving "$CPP_LOG" "$CPP_SVC"; then
    echo "the C++ service did not start:"; "${ADB[@]}" shell cat "$CPP_LOG"; exit 1
fi
# FORWARD on the rsbinder service calls GET on the C++ one.
"${ADB[@]}" shell "$DEV_DIR/work_source_probe serve $RS_SVC $CPP_SVC > $RS_LOG 2>&1" &
if ! wait_for_serving "$RS_LOG" "$RS_SVC"; then
    echo "the rsbinder service did not start:"; "${ADB[@]}" shell cat "$RS_LOG"; exit 1
fi

PROBE="$DEV_DIR/work_source_probe"
CPP="$DEV_DIR/work_source_interop"

echo
echo "=== (a) rsbinder client -> libbinder service"
expect "nothing set: libbinder sees unset" \
    "$PROBE get $CPP_SVC unset" "RESULT rs get unset SEEN -1 0"
# libbinder installs a received value without propagation.
expect "a value set on an rsbinder client thread reaches libbinder" \
    "$PROBE get $CPP_SVC 1234" "RESULT rs get 1234 SEEN 1234 0"

echo
echo "=== (b) libbinder client -> rsbinder service"
expect "nothing set: rsbinder sees unset" \
    "$CPP get $RS_SVC unset" "RESULT cpp get unset SEEN -1 0"
expect "a value set on a libbinder client thread reaches rsbinder" \
    "$CPP get $RS_SVC 5678" "RESULT cpp get 5678 SEEN 5678 0"

echo
echo "=== (c) libbinder -> rsbinder -> libbinder"
expect "a received value is not forwarded" \
    "$CPP forward $RS_SVC 5678 none" "RESULT cpp forward 5678 none HERE 5678 DOWN -1"
expect "a value the rsbinder handler sets is forwarded" \
    "$CPP forward $RS_SVC 5678 4321" "RESULT cpp forward 5678 4321 HERE 5678 DOWN 4321"

echo
echo "=== (d) no leak into later calls"
# Every rsbinder pool thread either served a call above or never ran;
# repeat so more than one of them answers.
for i in 1 2 3 4; do
    expect "rsbinder thread starts clean ($i)" \
        "$CPP get $RS_SVC unset" "RESULT cpp get unset SEEN -1 0"
done
expect "the handler's own value did not stay on the forwarding path" \
    "$PROBE get $CPP_SVC unset" "RESULT rs get unset SEEN -1 0"

printf '\n==== Plan 10-9 AC-9.3: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
