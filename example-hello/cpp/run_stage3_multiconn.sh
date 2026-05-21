#!/usr/bin/env bash
# Subplan 2-12 Phase D / AC-12.6 STAGE3 — drive a real-libbinder *client*
# (`ARpcSession_setMaxOutgoingConnections(2) + setMaxIncomingThreads(1)`)
# against an rsbinder *server* (`RpcServer::set_max_threads(3)`) on the
# android-16 emulator. The non-negotiable multi-connection-per-session
# interop gate (Plan 2-12 §3 AC-12.6).
#
# ⚠ STATUS (2026-05-21): GATE NOT YET PASSING — kept in tree as the
# future-gate harness. See launcher source (cpp/rpc_multiconn_interop_
# launcher.cpp) for the 2026-05-21 diagnostic summary (rsbinder wire
# bytes are byte-correct; even a process-wide server send mutex
# doesn't fix it; root cause still undiagnosed — multi-conn is
# experimental in `RpcServer::set_max_threads(N >= 2)`'s doc).
#
# Prereqs (same as run_stage3.sh / run_stage3_register.sh):
#   * Android_16 AVD booted (`emulator -avd Android_16 -port 5556`).
#   * NDK r29+ at `$ANDROID_NDK_HOME` (default
#     `/opt/homebrew/share/android-ndk`).
#   * `cargo ndk` installed.
#   * `aarch64-linux-android` rustup target installed.
#
# Usage:
#   ./run_stage3_multiconn.sh [-s emulator-5556]
#
# Exits 0 on AC-12.6 PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5556
SOCK=/data/local/tmp/rsmc.sock
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

echo "==> building C++ launcher (NDK)"
"$NDK_BIN/aarch64-linux-android35-clang++" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_multiconn_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_multiconn_interop_launcher"

echo "==> cross-compiling rsbinder server"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --features rpc \
        --bin rpc_multiconn_interop_server )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_multiconn_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/aarch64-linux-android/release/rpc_multiconn_interop_server" \
    /data/local/tmp/ >/dev/null

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f rpc_multiconn 2>/dev/null; rm -f $SOCK /data/local/tmp/rsmc.stdout /data/local/tmp/rsmc.stderr; sleep 1" || true

# Server slot cap = 3: 2 outgoing + 1 incoming. The launcher requests
# setMaxIncomingThreads(1) to keep libbinder on its in-tree config path.
echo "==> starting rsbinder server (background, max_threads=3)"
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_multiconn_interop_server $SOCK 3 > /data/local/tmp/rsmc.stdout 2> /data/local/tmp/rsmc.stderr &" &
sleep 3
adb -s "$DEVICE" shell "cat /data/local/tmp/rsmc.stderr"

echo "==> running C++ launcher (libbinder client)"
adb -s "$DEVICE" shell "/data/local/tmp/rpc_multiconn_interop_launcher $SOCK; echo client-exit=\$?"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f rpc_multiconn_interop_server 2>/dev/null; rm -f $SOCK" || true
