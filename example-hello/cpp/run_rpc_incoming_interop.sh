#!/usr/bin/env bash
# Plan 2-20 STAGE3 gate (e) — the reverse of `run_rpc_multiconn_interop.sh`
# gate (d): a real-libbinder **RPC server** (`rpc_incoming_interop_launcher.cpp`)
# drives an rsbinder **client**'s callback from a plain `std::thread`; the
# client opened one incoming connection (`incoming_connections(1)`).
#
# STATUS: see the bottom of this file / the plan (plans/2-20-*.md).
#
# Prereqs: as run_rpc_multiconn_interop.sh (Android 16 AVD, NDK, cargo ndk,
# aarch64-linux-android target).
#
# Usage: ./run_rpc_incoming_interop.sh [-s emulator-5554]
# Exits 0 on PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5554
SOCK=/data/local/tmp/rsinc.sock
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK_BIN="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}/toolchains/llvm/prebuilt/darwin-x86_64/bin"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi

echo "==> verifying device $DEVICE is Android 16"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 36"; exit 1; }

echo "==> pulling libbinder_*.so so we can link against them"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so /tmp/libbinder_rpc_unstable.so >/dev/null

echo "==> building C++ server launcher (NDK)"
"$NDK_BIN/aarch64-linux-android35-clang++" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_incoming_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_incoming_interop_launcher"

echo "==> cross-compiling rsbinder client"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --features rpc \
        --bin rpc_incoming_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_incoming_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/aarch64-linux-android/release/rpc_incoming_interop_client" \
    /data/local/tmp/ >/dev/null

echo "==> killing any old launcher + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f rpc_incoming_interop 2>/dev/null; rm -f $SOCK /data/local/tmp/rsinc.stdout /data/local/tmp/rsinc.stderr; sleep 1" || true

echo "==> starting libbinder server launcher (background)"
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_incoming_interop_launcher $SOCK > /data/local/tmp/rsinc.stdout 2> /data/local/tmp/rsinc.stderr &" &
sleep 3
adb -s "$DEVICE" shell "cat /data/local/tmp/rsinc.stderr"

echo "==> running rsbinder client"
# `timeout` so a wedged client fails the gate instead of hanging it; the
# client carries its own 60 s watchdog, this is the backstop for a stall
# before that thread starts (adb itself, a dead device).
client_out=$(timeout 180 adb -s "$DEVICE" shell "/data/local/tmp/rpc_incoming_interop_client $SOCK 2>&1; echo client-exit=\$?" | tr -d '\r') || true
printf '%s\n' "$client_out"

echo "==> server log"
adb -s "$DEVICE" shell "cat /data/local/tmp/rsinc.stderr"

echo "==> stopping launcher"
adb -s "$DEVICE" shell "pkill -9 -f rpc_incoming_interop_launcher 2>/dev/null; rm -f $SOCK" || true

if ! grep -q '^client-exit=0$' <<<"$client_out"; then
    echo "FAIL: rsbinder client exited non-zero"
    exit 1
fi
if ! grep -q '^PASS' <<<"$client_out"; then
    echo "FAIL: no PASS marker"
    exit 1
fi
echo "PASS: plan 2-20 (e) rsbinder incoming connection ↔ real libbinder RpcServer"
