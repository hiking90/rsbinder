#!/usr/bin/env bash
# Plan 10-3 AC-3.2 STAGE3 — a pipe fd in a parcel between rsbinder and a
# real AOSP binder peer, both directions.
#
#   (a) rsbinder streams  -> a libbinder-side client drains the pipe
#   (b) the C++ side streams -> rsbinder drains it
#
# What the real peer proves: the `ParcelFileDescriptor` wire (not-null
# marker, hasComm, descriptor object) and the driver's fd translation.
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0`, applied here: both halves register a
#     service under a name SELinux does not know.
#
# Usage: ./run_pipe_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when both directions pass.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
DEV_DIR=/data/local/tmp
RS_LOG=/data/local/tmp/pipe_probe.log
CPP_LOG=/data/local/tmp/pipe_cpp.log
RS_SVC=rsb103.rust
CPP_SVC=rsb103.cpp
FOUR_MB=$((4 * 1024 * 1024))

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
    "$CPP_DIR/pipe_interop.cpp" -o "$CPP_DIR/pipe_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin pipe_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/pipe_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/pipe_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/pipe_probe" "$DEV_DIR/pipe_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    "${ADB[@]}" shell 'pkill -f "[p]ipe_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[p]ipe_interop" 2>/dev/null' >/dev/null 2>&1 || true
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
# A stalled reader or writer would hang the script instead of failing it
# — the pipe buffer is ~64 KB, so both sides must keep moving. Unset on a
# host without coreutils `timeout` (word-split on purpose).
CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 180"; fi

echo
echo "=== (a) rsbinder streams -> the C++ side drains the pipe"
"${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/pipe_probe serve $RS_SVC > $RS_LOG 2>&1" &
if wait_for_serving "$RS_LOG" "$RS_SVC"; then
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/pipe_interop client $RS_SVC $FOUR_MB 2>/dev/null || true" | tr -d '\r')
    case "$out" in
        "RESULT cpp $FOUR_MB OK "*) ok "libbinder drained 4 MB from an rsbinder pipe ($out)" ;;
        *) bad "got '$out'" ;;
    esac
else
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
fi
cleanup; sleep 1

echo
echo "=== (b) the C++ side streams -> rsbinder drains the pipe"
"${ADB[@]}" shell "rm -f $CPP_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/pipe_interop server $CPP_SVC > $CPP_LOG 2>&1" &
if wait_for_serving "$CPP_LOG" "$CPP_SVC"; then
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/pipe_probe read $CPP_SVC $FOUR_MB 2>/dev/null || true" | tr -d '\r')
    case "$out" in
        "RESULT pipe $FOUR_MB OK "*) ok "rsbinder drained 4 MB from a libbinder pipe ($out)" ;;
        *) bad "got '$out'" ;;
    esac
else
    bad "the C++ service did not start"
    "${ADB[@]}" shell cat "$CPP_LOG"
fi

printf '\n==== Plan 10-3 AC-3.2: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
