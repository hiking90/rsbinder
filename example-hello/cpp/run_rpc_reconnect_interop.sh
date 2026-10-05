#!/usr/bin/env bash
# Plan 10-10b AC-10b.10 STAGE3 — `rsbinder::Reconnecting` across a restart
# of a real-libbinder RpcServer (`rpc_reconnect_interop_launcher.cpp`) that
# serves only its root object, the server a fragment-less URI reaches.
#
# For each mode the launcher starts as instance `a`, the rsbinder client
# (`rpc_reconnect_interop_client`) connects and prints PHASE1, the launcher
# is killed and started again as `b` on the same socket, and the client must
# get `b`'s answer through the same helper:
#
#   probe  no incoming connection — the first call after the restart finds
#          the closed socket before it is sent;
#   death  one incoming connection — the session ends with the server and
#          the helper reconnects with no call.
#
# Prereqs: as run_rpc_incoming_interop.sh (Android 16+ AVD, NDK, cargo ndk,
# the rustup target for the device's ABI), and `adb root`: the `shell`
# SELinux domain cannot create a socket file under /data/local/tmp.
#
# Usage: ./run_rpc_reconnect_interop.sh [-s emulator-5554] [-t <abi>]
# Exits 0 on PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
SOCK=/data/local/tmp/rsrc.sock
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done

adbs() { adb -s "$DEVICE" shell "$@"; }

sdk=$(adbs getprop ro.build.version.sdk | tr -d '\r')
# Verified on SDK 36; Android 17 (SDK 37) speaks the android-16 RPC wire byte for byte.
[[ "$sdk" -ge 36 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 36"; exit 1; }

case "${ABI:-$(adbs getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level.
CXX=""
API=""
for a in $(seq "$sdk" -1 29); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

echo "==> pulling libbinder_*.so so we can link against them"
SYS="$REPO_ROOT/target/rpc_reconnect_interop/sys"
mkdir -p "$SYS"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so "$SYS/libbinder_ndk.so" >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so "$SYS/libbinder_rpc_unstable.so" >/dev/null

echo "==> building C++ server launcher (NDK)"
OUT="$REPO_ROOT/target/rpc_reconnect_interop"
"$CXX" -O2 -Wall -std=c++17 -static-libstdc++ \
    -L "$SYS" -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_reconnect_interop_launcher.cpp" \
    -o "$OUT/rpc_reconnect_interop_launcher"

echo "==> cross-compiling rsbinder client"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" --platform "$API" build --release -p example-hello \
        --features rpc --bin rpc_reconnect_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$OUT/rpc_reconnect_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/rpc_reconnect_interop_client" \
    /data/local/tmp/ >/dev/null

# wait_for <device file> <pattern> <seconds>
wait_for() {
    local i
    for i in $(seq 1 $(($3 * 4))); do
        [ -n "$(adbs "grep -m1 '$2' $1 2>/dev/null")" ] && return 0
        sleep 0.25
    done
    return 1
}

# start_launcher <tag>
start_launcher() {
    adbs "rm -f /data/local/tmp/rsrc.$1.err"
    adbs "nohup /data/local/tmp/rpc_reconnect_interop_launcher $SOCK $1 \
        > /dev/null 2> /data/local/tmp/rsrc.$1.err &" &
    wait_for "/data/local/tmp/rsrc.$1.err" READY 10 || { echo "launcher $1 never got READY"; return 1; }
}

# By exact process name: this AVD's toybox `pkill -f` kills the calling `sh -c` even when
# nothing else matches, bracket pattern or not.
kill_named() {
    adbs "for p in \$(pidof $1); do kill -9 \$p; done; true"
}

stop_launchers() {
    kill_named rpc_reconnect_interop_launcher
}

FAILED=0
# run_mode <probe|death>
run_mode() {
    local out=/data/local/tmp/rsrc.client.out
    local marker=/data/local/tmp/rsrc.restarted
    echo
    echo "=== $1"
    stop_launchers
    kill_named rpc_reconnect_interop_client
    adbs "rm -f $SOCK $out $marker"

    start_launcher a || { FAILED=1; return; }
    adbs "nohup sh -c '/data/local/tmp/rpc_reconnect_interop_client $SOCK $1 $marker; \
        echo client-exit=\$?' > $out 2>&1 &" &
    if ! wait_for "$out" PHASE1 20; then
        echo "FAIL ($1): no PHASE1"; adbs "cat $out"; FAILED=1; stop_launchers; return
    fi

    echo "==> restarting the server as instance b"
    stop_launchers
    sleep 0.5
    start_launcher b || { FAILED=1; return; }
    # The client's next call goes out only now, with `b` already listening.
    adbs "touch $marker"

    if ! wait_for "$out" client-exit= 90; then
        echo "FAIL ($1): client did not finish"; FAILED=1
    fi
    local result
    result=$(adbs "cat $out" | tr -d '\r')
    printf '%s\n' "$result"
    echo "==> server b log"
    adbs "cat /data/local/tmp/rsrc.b.err"
    stop_launchers
    kill_named rpc_reconnect_interop_client
    adbs "rm -f $SOCK $marker"

    if grep -q '^client-exit=0$' <<<"$result" && grep -q '^PASS' <<<"$result"; then
        echo "PASS ($1)"
    else
        echo "FAIL ($1)"
        FAILED=1
    fi
}

run_mode probe
run_mode death

[ "$FAILED" -eq 0 ] || exit 1
echo "PASS: plan 10-10b AC-10b.10 Reconnecting across a real libbinder RpcServer restart (probe + death)"
