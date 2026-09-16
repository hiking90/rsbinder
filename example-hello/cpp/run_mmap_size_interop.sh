#!/usr/bin/env bash
# Plan 10-1 AC-1.5 STAGE3 — a real AOSP binder client sends a payload
# larger than the default receive mapping to an rsbinder service that
# raised its own.
#
# Two runs of the same C++ client (cpp/mmap_size_interop.cpp, linked
# against the device's own libbinder_ndk.so) against two rsbinder
# services that differ only in `binder://?mmap=`:
#
#   (a) 4 MB service, 2 MB payload  -> OK
#   (b) default service, 2 MB payload -> FAILEDTXN
#
# (b) is what makes (a) mean something: the same client bytes, refused
# by the driver when the receiver kept the ~1 MB default. Nothing on the
# sender's side differs between the two, which is the claim — the
# mapping is the receiver's and is not negotiated.
#
# Prereqs:
#   * a booted AVD (AOSP/google_apis, rootable), SDK >= 29
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-sdk/ndk-bundle),
#     `cargo ndk`, the rustup target for the device's ABI
#   * `adb root` + `setenforce 0` are applied by this script:
#     registering an arbitrary service name is otherwise denied by
#     SELinux, as everywhere else in tests/.
#
# Usage: ./run_mmap_size_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when both halves pass.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-sdk/ndk-bundle}"
DEV_DIR=/data/local/tmp
LOG_DEV=/data/local/tmp/mmap_probe.log
PAYLOAD=$((2 * 1024 * 1024))
BIG=$((4 * 1024 * 1024))

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

echo "==> building the C++ client (NDK)"
"$CXX" -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp -lbinder_ndk \
    "$CPP_DIR/mmap_size_interop.cpp" -o "$CPP_DIR/mmap_size_interop"

echo "==> cross-compiling the rsbinder service (mmap_probe)"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p tests --bin mmap_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/mmap_probe" "$DEV_DIR/" >/dev/null
"${ADB[@]}" push "$CPP_DIR/mmap_size_interop" "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/mmap_probe" "$DEV_DIR/mmap_size_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

cleanup() {
    # A bracket in the pattern so pkill does not match the shell running
    # this very command (the trap fires inside an `adb shell`, too).
    "${ADB[@]}" shell 'pkill -f "[m]map_probe" 2>/dev/null' >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

# start_service <bytes|default> <name>
start_service() {
    "${ADB[@]}" shell "rm -f $LOG_DEV" >/dev/null 2>&1 || true
    # The trailing `&` backgrounds the *host-side* `adb shell`, which is
    # the only thing that works for a long-lived device process: adb
    # holds the connection open for as long as the remote process lives,
    # whatever the remote redirections say. Same shape as the other
    # STAGE3 launchers here.
    "${ADB[@]}" shell "$DEV_DIR/mmap_probe serve $1 $2 > $LOG_DEV 2>&1" &
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^SERVING $2 ' $LOG_DEV 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
run_client() {
    # `|| true`: the client exits non-zero on anything but OK, and the
    # refusal in (b) is the expected outcome there — the line it prints
    # is what this script judges, not its exit status.
    "${ADB[@]}" shell "$DEV_DIR/mmap_size_interop $1 $PAYLOAD || true" | tr -d '\r'
}

echo
echo "=== (a) 2 MB payload -> rsbinder service that mapped 4 MB"
if start_service "$BIG" rsb101.big; then
    mapped=$("${ADB[@]}" shell "sed -n 's/^SERVING rsb101.big mmap=//p' $LOG_DEV" | tr -d '\r')
    [ "$mapped" = "$BIG" ] && ok "service mapped $mapped" || bad "service mapped $mapped, want $BIG"
    out=$(run_client rsb101.big)
    [ "$out" = "RESULT call $PAYLOAD OK" ] && ok "real libbinder client sent 2 MB" || bad "got '$out'"
else
    bad "the 4 MB service did not start"
    "${ADB[@]}" shell cat "$LOG_DEV"
fi
cleanup; sleep 1

echo
echo "=== (b) the same client, same payload -> default-sized service"
if start_service default rsb101.small; then
    mapped=$("${ADB[@]}" shell "sed -n 's/^SERVING rsb101.small mmap=//p' $LOG_DEV" | tr -d '\r')
    [ "$mapped" -lt "$PAYLOAD" ] && ok "service mapped $mapped, below the payload" \
                                 || bad "service mapped $mapped, expected below $PAYLOAD"
    out=$(run_client rsb101.small)
    [ "$out" = "RESULT call $PAYLOAD FAILEDTXN" ] && ok "driver refused it, as FAILED_TRANSACTION" \
                                                  || bad "got '$out'"
else
    bad "the default-sized service did not start"
    "${ADB[@]}" shell cat "$LOG_DEV"
fi

printf '\n==== Plan 10-1 AC-1.5: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
