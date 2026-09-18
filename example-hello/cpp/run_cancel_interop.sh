#!/usr/bin/env bash
# Plan 10-5 AC-5.4 STAGE3 — `android.os.ICancellationSignal` between
# rsbinder and the real AOSP binder stack, both directions.
#
#   (a) rsbinder service hands out a transport -> libbinder client cancels it
#   (b) libbinder service hands out a transport -> rsbinder client cancels it
#
# The transport rides inside a reply as a binder object, so what needs a
# real peer is the object marshalling and the `oneway` dispatch — neither
# of which is visible with rsbinder on both ends.
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0`, applied here: both halves register a
#     service under a name SELinux does not know.
#
# Usage: ./run_cancel_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when both directions pass.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
DEV_DIR=/data/local/tmp
RS_LOG=/data/local/tmp/cancel_probe.log
CPP_LOG=/data/local/tmp/cancel_cpp.log
RS_SVC=rsb105.rust
CPP_SVC=rsb105.cpp

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
"${ADB[@]}" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null

echo "==> building the C++ half (NDK)"
"$CXX" -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp -lbinder_ndk \
    "$CPP_DIR/cancel_interop.cpp" -o "$CPP_DIR/cancel_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin cancel_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/cancel_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/cancel_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/cancel_probe" "$DEV_DIR/cancel_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    "${ADB[@]}" shell 'pkill -f "[c]ancel_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[c]ancel_interop" 2>/dev/null' >/dev/null 2>&1 || true
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
# A hung transaction would stall the script instead of failing it. Unset
# on a host without coreutils `timeout` (word-split on purpose, so an
# empty value adds no word).
CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 120"; fi

echo
echo "=== (a) rsbinder hands out the transport -> libbinder cancels it"
"${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/cancel_probe serve $RS_SVC > $RS_LOG 2>&1" &
if wait_for_serving "$RS_LOG" "$RS_SVC"; then
    # `|| true`: the client exits non-zero unless it saw false→true, and
    # the printed line is what is judged.
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/cancel_interop client $RS_SVC 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "RESULT cpp false true" ] \
        && ok "libbinder cancelled an rsbinder transport" || bad "got '$out'"
else
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
fi
cleanup; sleep 1

echo
echo "=== (b) libbinder hands out the transport -> rsbinder cancels it"
"${ADB[@]}" shell "rm -f $CPP_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/cancel_interop server $CPP_SVC > $CPP_LOG 2>&1" &
if wait_for_serving "$CPP_LOG" "$CPP_SVC"; then
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/cancel_probe cancel $CPP_SVC 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "RESULT cancel false true" ] \
        && ok "rsbinder cancelled a libbinder transport" || bad "got '$out'"
else
    bad "the C++ service did not start"
    "${ADB[@]}" shell cat "$CPP_LOG"
fi

printf '\n==== Plan 10-5 AC-5.4: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
