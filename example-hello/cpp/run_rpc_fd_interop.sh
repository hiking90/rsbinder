#!/usr/bin/env bash
# Plan 2-11 AC-11.3 STAGE3 — FD-over-RPC against a real libbinder client.
#
# A real-libbinder `ARpcSession` client (`setFileDescriptorTransportMode
# (Unix)`, cpp/rpc_fd_interop_launcher.cpp) drives a `ParcelFileDescriptor`
# in both directions against the rsbinder `rpc_fd_interop_server` over a
# Unix-domain socket: the AOSP v1+ fd body + SCM_RIGHTS on the wire
# libbinder actually speaks. Works on Android 15 (libbinder's own RPC max
# is v1) and Android 16 (v2) emulators.
#
# Prereqs (as run_rpc_multiconn_interop.sh):
#   * an Android 15/16 AVD booted; NDK at $ANDROID_NDK_HOME (default
#     /opt/homebrew/share/android-ndk); `cargo ndk`; the
#     aarch64-linux-android rustup target.
#
# Usage:
#   ./run_rpc_fd_interop.sh [-s emulator-5554] [max-wire-version]
#
# Exits 0 on FD_PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5554
SOCK=/data/local/tmp/rsfd.sock
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK_BIN="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}/toolchains/llvm/prebuilt/darwin-x86_64/bin"

if [[ ${1:-} == "-s" ]]; then
    DEVICE="$2"
    shift 2
fi
MAX_VERSION="${1:-2}"

echo "==> verifying device $DEVICE is Android 15 or 16"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" == "35" || "$sdk" == "36" ]] || { echo "device $DEVICE is SDK $sdk, expected 35/36"; exit 1; }

echo "==> pulling libbinder_*.so so we can link against them"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so /tmp/libbinder_rpc_unstable.so >/dev/null

echo "==> building C++ launcher (NDK)"
"$NDK_BIN/aarch64-linux-android35-clang++" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_fd_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_fd_interop_launcher"

echo "==> cross-compiling rsbinder server"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --features rpc \
        --bin rpc_fd_interop_server )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_fd_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/aarch64-linux-android/release/rpc_fd_interop_server" \
    /data/local/tmp/ >/dev/null

cleanup() {
    adb -s "$DEVICE" shell "pkill -9 -f rpc_fd_interop_server 2>/dev/null; rm -f $SOCK" || true
}
trap cleanup EXIT

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f rpc_fd_interop 2>/dev/null; rm -f $SOCK /data/local/tmp/rsfd.stderr; sleep 1" || true

echo "==> starting rsbinder server (background, max wire version $MAX_VERSION)"
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_fd_interop_server $SOCK $MAX_VERSION > /data/local/tmp/rsfd.stdout 2> /data/local/tmp/rsfd.stderr &" &
sleep 3

echo "==> running C++ launcher (libbinder client)"
out=$(adb -s "$DEVICE" shell "/data/local/tmp/rpc_fd_interop_launcher $SOCK 2>&1; echo client-exit=\$?" | tr -d '\r')
echo "$out"
echo "==> server log"
adb -s "$DEVICE" shell "cat /data/local/tmp/rsfd.stderr" | grep -v '^\[.*DEBUG' | tail -20 || true

if grep -q '^FD_PASS' <<<"$out" && grep -q 'client-exit=0' <<<"$out"; then
    echo "==> AC-11.3 PASS (device $DEVICE, SDK $sdk)"
    exit 0
fi
echo "==> AC-11.3 FAIL"
exit 1
