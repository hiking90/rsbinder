#!/usr/bin/env bash
# Plan 10-2 AC-2.2 STAGE3 — the blob wire between rsbinder and a real
# AOSP binder peer.
#
#   (a) rsbinder writes  -> libbinder-side client reads it
#   (b) the C++ side writes -> rsbinder reads it
#
# Scope: the **inline** form in both directions, plus the tag rsbinder
# chooses for a payload over the limit. The fd form is out of reach here
# — the NDK's AParcel has no bare file-descriptor API, and AOSP's blob
# uses one (see the header of cpp/blob_interop.cpp for what covers it
# instead).
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0`, applied here: both halves register a
#     service under a name SELinux does not know.
#
# Usage: ./run_blob_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when every case passes.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
DEV_DIR=/data/local/tmp
RS_LOG=/data/local/tmp/blob_probe.log
CPP_LOG=/data/local/tmp/blob_cpp.log
RS_SVC=rsb102.rust
CPP_SVC=rsb102.cpp
SMALL=4096
LARGE=$((16 * 1024 + 4096))

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
    "$CPP_DIR/blob_interop.cpp" -o "$CPP_DIR/blob_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin blob_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/blob_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/blob_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/blob_probe" "$DEV_DIR/blob_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    "${ADB[@]}" shell 'pkill -f "[b]lob_probe" 2>/dev/null' >/dev/null 2>&1 || true
    "${ADB[@]}" shell 'pkill -f "[b]lob_interop" 2>/dev/null' >/dev/null 2>&1 || true
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
echo "=== (a) rsbinder writes the blob -> the C++ side reads it"
"${ADB[@]}" shell "rm -f $RS_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/blob_probe serve $RS_SVC > $RS_LOG 2>&1" &
if wait_for_serving "$RS_LOG" "$RS_SVC"; then
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/blob_interop client $RS_SVC $SMALL 2>/dev/null || true" | tr -d '\r')
    case "$out" in
        "RESULT cpp $SMALL INLINE "*) ok "a small blob arrived inline, payload verified ($out)" ;;
        *) bad "got '$out'" ;;
    esac
    # Over the limit on a kernel parcel, rsbinder must pick the shared
    # form. The C++ half can read the tag but not the fd — the scope
    # note in blob_interop.cpp says why.
    out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/blob_interop client $RS_SVC $LARGE 2>/dev/null || true" | tr -d '\r')
    [ "$out" = "RESULT cpp $LARGE SHARED 1" ] \
        && ok "a large blob was tagged BLOB_ASHMEM_IMMUTABLE" || bad "got '$out'"
else
    bad "the rsbinder service did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
fi
cleanup; sleep 1

echo
echo "=== (b) the C++ side writes the blob -> rsbinder reads it"
"${ADB[@]}" shell "rm -f $CPP_LOG" >/dev/null 2>&1 || true
"${ADB[@]}" shell "$DEV_DIR/blob_interop server $CPP_SVC > $CPP_LOG 2>&1" &
if wait_for_serving "$CPP_LOG" "$CPP_SVC"; then
    for n in 0 1 "$SMALL"; do
        out=$($CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/blob_probe read $CPP_SVC $n 2>/dev/null || true" | tr -d '\r')
        case "$out" in
            "RESULT blob $n INLINE "*) ok "rsbinder read a $n-byte inline blob, payload verified" ;;
            *) bad "n=$n: got '$out'" ;;
        esac
    done
else
    bad "the C++ service did not start"
    "${ADB[@]}" shell cat "$CPP_LOG"
fi

printf '\n==== Plan 10-2 AC-2.2: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
