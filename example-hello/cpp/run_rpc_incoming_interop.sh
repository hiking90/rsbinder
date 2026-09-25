#!/usr/bin/env bash
# Plan 2-20 STAGE3 gate (e) — the reverse of `run_rpc_multiconn_interop.sh`
# gate (d): a real-libbinder **RPC server** (`rpc_incoming_interop_launcher.cpp`)
# drives an rsbinder **client**'s callback from a plain `std::thread`; the
# client opened one incoming connection (`incoming_connections(1)`).
#
# Two runs of the same pair: over a Unix socket, and over loopback inet
# (plan 10-7 Phase 0 — incoming connections on every RPC transport; the
# rsbinder client builds them with `RpcClientConfig` over plaintext TCP,
# as libbinder's inet server speaks no TLS without certificates on both
# sides).
#
# STATUS: see the bottom of this file / the plan (plans/2-20-*.md).
#
# Prereqs: as run_rpc_multiconn_interop.sh (Android 16 AVD, NDK, cargo ndk,
# the rustup target for the device's ABI). The Unix-socket half needs
# `adb root` first: the `shell` SELinux domain cannot create a socket file
# under /data/local/tmp (EACCES); the inet half runs as `shell`.
#
# Usage: ./run_rpc_incoming_interop.sh [-s emulator-5554] [-t <abi>]
# The ABI defaults to the device's own; -t overrides it.
# Exits 0 on PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
SOCK=/data/local/tmp/rsinc.sock
INET_PORT=5711
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

sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')

# For the two 64-bit ABIs the NDK clang prefix and the cargo target
# directory are both the Rust triple; the 32-bit ones differ and are not
# covered.
case "${ABI:-$(adb -s "$DEVICE" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
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

echo "==> verifying device $DEVICE is Android 16"
[[ "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 36"; exit 1; }

echo "==> pulling libbinder_*.so so we can link against them"
SYS="$REPO_ROOT/target/rpc_incoming_interop/sys"
mkdir -p "$SYS"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so "$SYS/libbinder_ndk.so" >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so "$SYS/libbinder_rpc_unstable.so" >/dev/null

echo "==> building C++ server launcher (NDK)"
"$CXX" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L "$SYS" \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_incoming_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_incoming_interop_launcher"

echo "==> cross-compiling rsbinder client"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p example-hello \
        --features rpc,test-util,tcp-debug \
        --bin rpc_incoming_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_incoming_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/rpc_incoming_interop_client" \
    /data/local/tmp/ >/dev/null

FAILED=0
# run_pair <label> <launcher arg> <client arg>
run_pair() {
    echo
    echo "=== $1"
    echo "==> killing any old launcher + cleaning state"
    # Bracket pattern: the plain one matches this `sh -c` itself, and
    # `pkill -f` kills the shell before it reaches the `rm` and the sleep.
    adb -s "$DEVICE" shell "pkill -9 -f '[r]pc_incoming_interop' 2>/dev/null; rm -f $SOCK /data/local/tmp/rsinc.stdout /data/local/tmp/rsinc.stderr; sleep 1" || true

    echo "==> starting libbinder server launcher (background)"
    adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_incoming_interop_launcher $2 > /data/local/tmp/rsinc.stdout 2> /data/local/tmp/rsinc.stderr &" &
    # The launcher prints READY once it has bound, and the client's session
    # setup has no retry: a fixed sleep decides this gate on a cold device.
    for _ in $(seq 1 40); do
        if [ -n "$(adb -s "$DEVICE" shell "grep READY /data/local/tmp/rsinc.stderr 2>/dev/null")" ]; then
            break
        fi
        sleep 0.5
    done
    adb -s "$DEVICE" shell "cat /data/local/tmp/rsinc.stderr"

    echo "==> running rsbinder client"
    # `timeout` so a wedged client fails the gate instead of hanging it; the
    # client carries its own 60 s watchdog, this is the backstop for a stall
    # before that thread starts (adb itself, a dead device).
    local client_out
    client_out=$(timeout 180 adb -s "$DEVICE" shell "/data/local/tmp/rpc_incoming_interop_client $3 2>&1; echo client-exit=\$?" | tr -d '\r') || true
    printf '%s\n' "$client_out"

    echo "==> server log"
    adb -s "$DEVICE" shell "cat /data/local/tmp/rsinc.stderr"

    echo "==> stopping launcher"
    adb -s "$DEVICE" shell "pkill -9 -f '[r]pc_incoming_interop_launcher' 2>/dev/null; rm -f $SOCK" || true

    if ! grep -q '^client-exit=0$' <<<"$client_out"; then
        echo "FAIL ($1): rsbinder client exited non-zero"
        FAILED=1
    elif ! grep -q '^PASS' <<<"$client_out"; then
        echo "FAIL ($1): no PASS marker"
        FAILED=1
    else
        echo "PASS ($1)"
    fi
}

run_pair "unix" "$SOCK" "$SOCK"
run_pair "inet" "tcp:$INET_PORT" "tcp:127.0.0.1:$INET_PORT"

[ "$FAILED" -eq 0 ] || exit 1
echo "PASS: plan 2-20 (e) rsbinder incoming connection ↔ real libbinder RpcServer (unix + inet)"
