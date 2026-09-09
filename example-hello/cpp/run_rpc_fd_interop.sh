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
#   * a booted AVD, SDK >= 34; NDK at $ANDROID_NDK_HOME (default
#     /opt/homebrew/share/android-ndk); `cargo ndk`; the rustup target
#     for the device's ABI.
#
# Usage:
#   ./run_rpc_fd_interop.sh [-s emulator-5554] [-t <abi>] [max-wire-version]
#
# The ABI defaults to the device's own; -t overrides it.
# Exits 0 on FD_PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
SOCK=/data/local/tmp/rsfd.sock
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}"

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>] [max-wire-version]" >&2; exit 2 ;;
    esac
done
MAX_VERSION="${1:-2}"

# libbinder's own RPC max is v1 on Android 15 and v2 on 16; the version
# is negotiated, so an older libbinder just settles lower. SDK 34 is the
# oldest measured to carry libbinder_rpc_unstable with fd transport.
echo "==> checking device $DEVICE"
sdk=$(adb -s "$DEVICE" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 34 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 34"; exit 1; }

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

echo "==> pulling libbinder_*.so so we can link against them"
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so /tmp/libbinder_rpc_unstable.so >/dev/null

echo "==> building C++ launcher (NDK)"
"$CXX" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_fd_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_fd_interop_launcher"

echo "==> cross-compiling rsbinder server"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p example-hello \
        --features rpc \
        --bin rpc_fd_interop_server )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_fd_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/rpc_fd_interop_server" \
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
