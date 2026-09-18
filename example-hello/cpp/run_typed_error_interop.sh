#!/usr/bin/env bash
# Plan 10-4 AC-4.5 STAGE3 — a typed service-specific error between
# rsbinder and the real AOSP binder stack, both directions.
#
#   (a) rsbinder service throws  -> libbinder client reads code + message
#   (b) libbinder service throws -> rsbinder client reads it as its own
#       enum value, and an undeclared code stays an i32
#
# 10-4 adds no bytes to the wire, which is the reason for (a) and (b)
# rather than a hermetic test alone: with rsbinder on both ends, a shared
# misreading of AOSP's Status header would pass.
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0` are applied by this script: registering
#     an arbitrary service name is otherwise denied by SELinux — and here
#     both halves register one.
#
# Usage: ./run_typed_error_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when both directions pass.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
DEV_DIR=/data/local/tmp
RS_LOG=/data/local/tmp/typed_error_probe.log
CPP_LOG=/data/local/tmp/typed_error_cpp.log
RS_SVC=rsb104.rust
CPP_SVC=rsb104.cpp

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
    "$CPP_DIR/typed_error_interop.cpp" -o "$CPP_DIR/typed_error_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin typed_error_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/typed_error_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/typed_error_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/typed_error_probe" "$DEV_DIR/typed_error_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    # Bracket patterns so pkill does not match the device shell carrying
    # the pattern itself.
    "${ADB[@]}" shell 'pkill -f "[t]yped_error_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[t]yped_error_interop" 2>/dev/null' >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

# start_service <binary> <args...> — the trailing `&` backgrounds the
# host-side `adb shell`, which is what works for a long-lived device
# process: adb holds the connection for as long as the remote process
# lives, whatever the remote redirections say.
wait_for_serving() {   # <log> <name>
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^SERVING $2' $1 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
# A hung transaction would otherwise stall the script instead of failing
# it. Unset on a host without coreutils `timeout` (word-split on purpose,
# so an empty value adds no word).
CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 60"; fi

echo
echo "=== (a) rsbinder service throws -> real libbinder client reads it"
"${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/typed_error_probe serve $RS_SVC > $RS_LOG 2>&1" &
if wait_for_serving "$RS_LOG" "$RS_SVC"; then
    # `|| true`: the C++ client exits non-zero on anything but a
    # service-specific answer, and the printed line is what is judged.
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/typed_error_interop client $RS_SVC 3 || true" | tr -d '\r')
    [ "$out" = "RESULT cpp 3 SERVICE_SPECIFIC 3 typed from rsbinder" ] \
        && ok "libbinder read code 3 and the message" || bad "got '$out'"
else
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
fi
cleanup; sleep 1

echo
echo "=== (b) real libbinder service throws -> rsbinder client reads it as a type"
"${ADB[@]}" shell "rm -f $CPP_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/typed_error_interop server $CPP_SVC > $CPP_LOG 2>&1" &
if wait_for_serving "$CPP_LOG" "$CPP_SVC"; then
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/typed_error_probe call $CPP_SVC 3 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "RESULT typed 3 TYPED LOCKED" ] \
        && ok "rsbinder read libbinder's code 3 as LOCKED" || bad "got '$out'"
    # A code the rsbinder enum does not declare is still a
    # service-specific failure, just not one of its values.
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/typed_error_probe call $CPP_SVC 41 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "RESULT typed 41 UNTYPED 41" ] \
        && ok "an undeclared code stays an i32" || bad "got '$out'"
else
    bad "the C++ service did not start"
    "${ADB[@]}" shell cat "$CPP_LOG"
fi

printf '\n==== Plan 10-4 AC-4.5: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
