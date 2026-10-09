#!/usr/bin/env bash
# Plan 2-25 Phase 7 STAGE3 — rsbinder's r34 RPC wire against real
# android-12 libbinder, both directions, on an SDK 31 emulator.
#
#   gate A: android-12 client (`rpc_r34_interop client`) → rsbinder
#           `RpcServer` (`rpc_r34_interop_server`, `set_max_threads(3)`):
#           (A1) 3 connections + echo, (A2) parallel twoway, (A3) oneway
#           order, (A4) nested callback, (A5) null binder then int, (A6)
#           refcounts (own binder echoed back, a callback held twice and
#           released, server node count after a child binder is dropped),
#           (A7) DEC_STRONG flood, (A8) 90 KB / 110 KB request, (A9)
#           GET_ROOT without a root, (A10) 110 KB reply.
#   gate B: rsbinder client (`rpc_r34_interop_client`) → android-12
#           `RpcServer` (`rpc_r34_interop server`, `setMaxThreads(3)`):
#           (B1) root + echo, (B2) negotiate, (B3) 4-byte session id,
#           (B4) fd mode None, (B5) nested callback, (B6) null binder,
#           (B7) own binder echoed back, (B8) oneway order, (B9) node count
#           after hold/release, (B10) 90 KB, (B11) 110 KB request ends the
#           session, (B12) server kill → DeadObject.
#
# Android 12 has no RPC C API, so the C++ half calls libbinder's platform
# C++ `RpcServer`/`RpcSession` with android-12.0.0_r34 headers taken from
# the AOSP checkout (`git archive`, no checkout of the tag needed) and links
# against libraries pulled from the device. See rpc_r34_interop.cpp.
#
# Prereqs:
#   * an SDK 31 or 32 AVD, rootable (google_apis), e.g.
#     `emulator -avd Android_12 -port 5560 -no-window -no-audio`.
#   * $AOSP (default ~/Workspace/aosp) with the android-12.0.0_r34 tag in
#     frameworks/native, system/core, system/libbase and system/logging.
#   * NDK at $ANDROID_NDK_HOME, `cargo ndk`, the rustup target for the ABI.
#
# Usage: ./run_rpc_r34_interop.sh [-s emulator-5560] [-t <abi>]
# Exits 0 when both gates pass; logcat (RpcState RpcSession RpcServer
# Stability) is saved to target/rpc_r34_interop/logcat.txt.

set -euo pipefail

DEVICE=emulator-5560
ABI=""
TMP=/data/local/tmp
SOCK_A=$TMP/rsr34.sock
NOROOT_A=$TMP/rsr34_noroot.sock
SOCK_B=$TMP/a12r34.sock
TAG=android-12.0.0_r34
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"
AOSP="${AOSP:-$HOME/Workspace/aosp}"
OUT="$REPO_ROOT/target/rpc_r34_interop"

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done

adbs() { adb -s "$DEVICE" shell "$@"; }

sdk=$(adbs getprop ro.build.version.sdk | tr -d '\r')
echo "==> verifying device $DEVICE is Android 12 or 12L"
[[ "$sdk" == 31 || "$sdk" == 32 ]] || { echo "device $DEVICE is SDK $sdk, expected 31 or 32"; exit 1; }

case "${ABI:-$(adbs getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

CXX=""
API=""
for a in $(seq "$sdk" -1 29); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

echo "==> extracting $TAG headers from $AOSP"
INC="$OUT/include"
rm -rf "$INC"
mkdir -p "$INC"
# extract <project> <path>...
extract() {
    local project=$1
    shift
    git -C "$AOSP/$project" rev-parse -q --verify "refs/tags/$TAG" >/dev/null ||
        { echo "$AOSP/$project has no tag $TAG" >&2; exit 2; }
    mkdir -p "$INC/$project"
    git -C "$AOSP/$project" archive "$TAG" "$@" | tar -x -C "$INC/$project"
}
extract frameworks/native libs/binder/include libs/binder/ndk/include_platform \
    libs/binder/ndk/include_ndk
extract system/core libutils/include libcutils/include libsystem/include
extract system/libbase include
extract system/logging liblog/include

echo "==> pulling the device's libbinder and its dependencies to link against"
SYS="$OUT/sys"
mkdir -p "$SYS"
for l in libbinder libbinder_ndk libutils libcutils libbase liblog; do
    adb -s "$DEVICE" pull "/system/lib64/$l.so" "$SYS/$l.so" >/dev/null
done

echo "==> building the android-12 C++ half (NDK)"
"$CXX" -O2 -Wall -std=c++17 -static-libstdc++ \
    -I "$INC/frameworks/native/libs/binder/include" \
    -I "$INC/frameworks/native/libs/binder/ndk/include_platform" \
    -I "$INC/frameworks/native/libs/binder/ndk/include_ndk" \
    -I "$INC/system/core/libutils/include" \
    -I "$INC/system/core/libcutils/include" \
    -I "$INC/system/core/libsystem/include" \
    -I "$INC/system/libbase/include" \
    -I "$INC/system/logging/liblog/include" \
    "$CPP_DIR/rpc_r34_interop.cpp" \
    -L "$SYS" -lbinder -lbinder_ndk -lutils -llog \
    -o "$OUT/rpc_r34_interop"
# No NDK libc++ type (`std::__ndk1`) may appear in a platform symbol.
NM="$(dirname "$CXX")/llvm-nm"
if "$NM" -u "$OUT/rpc_r34_interop" | grep '7android' | grep -q '__ndk1'; then
    echo "FAIL: a libbinder symbol takes an NDK libc++ type"; exit 1
fi

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" --platform "$API" build --release -p example-hello \
        --features rpc --bin rpc_r34_interop_server --bin rpc_r34_interop_client )

adb -s "$DEVICE" root >/dev/null
adb -s "$DEVICE" wait-for-device

echo "==> pushing binaries"
adb -s "$DEVICE" push "$OUT/rpc_r34_interop" $TMP/ >/dev/null
for b in rpc_r34_interop_server rpc_r34_interop_client; do
    adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/$b" $TMP/ >/dev/null
done

# wait_for <device file> <pattern> <seconds>
wait_for() {
    local i
    for i in $(seq 1 $(($3 * 4))); do
        [ -n "$(adbs "grep -m1 '$2' $1 2>/dev/null")" ] && return 0
        sleep 0.25
    done
    return 1
}

# By exact process name: toybox `pkill -f` can kill the calling `sh -c`.
kill_named() {
    adbs "for p in \$(pidof $1); do kill -9 \$p; done; true"
}

stop_all() {
    kill_named rpc_r34_interop_server
    kill_named rpc_r34_interop_client
    kill_named rpc_r34_interop
    adbs "rm -f $SOCK_A $NOROOT_A $SOCK_B $TMP/rsr34.*" || true
}

stop_all
adbs "logcat -c"
FAILED=0

echo
echo "=== gate A: android-12 client -> rsbinder r34 server"
adbs "nohup $TMP/rpc_r34_interop_server $SOCK_A $NOROOT_A 3 \
    > $TMP/rsr34.server.out 2> $TMP/rsr34.server.err &" &
if ! wait_for $TMP/rsr34.server.out READY 15; then
    echo "FAIL: rsbinder server never got READY"; adbs "cat $TMP/rsr34.server.err"; exit 1
fi
a_out=$(adbs "timeout 300 $TMP/rpc_r34_interop client $SOCK_A $NOROOT_A 2>&1; \
    echo client-exit=\$?" | tr -d '\r')
printf '%s\n' "$a_out"
grep -q '^client-exit=0$' <<<"$a_out" && grep -q '^GATE A PASS$' <<<"$a_out" || FAILED=1

echo "--- (A10) a reply past android-12's 100,000-byte limit"
big_out=$(adbs "timeout 60 $TMP/rpc_r34_interop client-bigreply $SOCK_A 2>&1; \
    echo client-exit=\$?" | tr -d '\r')
printf '%s\n' "$big_out"
grep -q '^client-exit=0$' <<<"$big_out" || FAILED=1
sleep 0.5
echo "--- rsbinder server stderr"
adbs "cat $TMP/rsr34.server.err" | tr -d '\r' | tail -40
# The server must still be up after both clients.
[ -n "$(adbs "pidof rpc_r34_interop_server")" ] || { echo "FAIL: rsbinder server died"; FAILED=1; }
kill_named rpc_r34_interop_server

echo
echo "=== gate B: rsbinder r34 client -> android-12 server"
adbs "nohup sh -c 'echo \$\$ > $TMP/rsr34.a12.pid; \
    exec $TMP/rpc_r34_interop server $SOCK_B 3' > $TMP/rsr34.a12.out 2>&1 &" &
if ! wait_for $TMP/rsr34.a12.out READY 15; then
    echo "FAIL: android-12 server never got READY"; adbs "cat $TMP/rsr34.a12.out"; exit 1
fi
a12_pid=$(adbs "cat $TMP/rsr34.a12.pid" | tr -d '\r')
b_out=$(adbs "RUST_LOG=rsbinder::rpc=info timeout 120 $TMP/rpc_r34_interop_client $SOCK_B $a12_pid 2>&1; \
    echo client-exit=\$?" | tr -d '\r')
printf '%s\n' "$b_out"
grep -q '^client-exit=0$' <<<"$b_out" && grep -q '^GATE B PASS$' <<<"$b_out" || FAILED=1
echo "--- android-12 server output"
adbs "cat $TMP/rsr34.a12.out" | tr -d '\r' | tail -20

stop_all

echo
echo "==> logcat (RpcState RpcSession RpcServer Stability) -> $OUT/logcat.txt"
adbs "logcat -d -s RpcState RpcSession RpcServer Stability" | tr -d '\r' > "$OUT/logcat.txt"
tail -40 "$OUT/logcat.txt"

if [ "$FAILED" != 0 ]; then
    echo "FAIL: r34 interop against android-12 libbinder"
    exit 1
fi
echo "PASS: r34 interop against android-12 libbinder (gates A and B)"
