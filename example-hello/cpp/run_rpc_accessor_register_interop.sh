#!/usr/bin/env bash
# Subplan 2-14 D.9 STAGE3 — drive a real-libbinder *client* against the
# rsbinder IAccessor *register*-side bridge on the android-16 emulator.
# The role-inverse of subplan 2-13 D.8's `run_rpc_accessor_interop.sh` (there
# rsbinder was the client + libbinder served; here rsbinder serves +
# libbinder is the client).
#
# Prereqs:
#   * Android_16 AVD booted (`emulator -avd Android_16 -port 5556`).
#   * NDK r29+ at `$ANDROID_NDK_HOME` (default
#     `/opt/homebrew/share/android-ndk`).
#   * `cargo ndk` installed (`cargo install cargo-ndk`).
#   * the rustup target for the device's ABI installed
#     (`rustup target add aarch64-linux-android`).
#
# Usage:
#   ./run_rpc_accessor_register_interop.sh [-s emulator-5556] [-t <abi>]
#
# The ABI defaults to the device's own; -t overrides it.
# Exits 0 on STAGE3 PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5556
ABI=""
INSTANCE=rsbinder.test.acc.reg
SOCK=/data/local/tmp/rsacc-reg-rpc.sock
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
adb -s "$DEVICE" pull /system/lib64/libbinder_ndk.so /tmp/libbinder_ndk.so >/dev/null
adb -s "$DEVICE" pull /system/lib64/libbinder_rpc_unstable.so /tmp/libbinder_rpc_unstable.so >/dev/null

echo "==> building C++ launcher (NDK)"
"$CXX" \
    -O2 -Wall -std=c++17 -static-libstdc++ \
    -L /tmp \
    -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/rpc_accessor_register_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_accessor_register_interop_launcher"

echo "==> cross-compiling rsbinder server"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p example-hello \
        --features rpc,android_16 \
        --bin rpc_accessor_register_interop_server )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_accessor_register_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/rpc_accessor_register_interop_server" \
    /data/local/tmp/ >/dev/null

echo "==> killing any old server + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor_register 2>/dev/null; rm -f $SOCK /data/local/tmp/rsacc_reg.stdout /data/local/tmp/rsacc_reg.stderr; sleep 1" || true

echo "==> starting rsbinder server (background)"
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_accessor_register_interop_server $INSTANCE $SOCK 2 > /data/local/tmp/rsacc_reg.stdout 2> /data/local/tmp/rsacc_reg.stderr &" &
sleep 4
adb -s "$DEVICE" shell "cat /data/local/tmp/rsacc_reg.stderr"

echo "==> running C++ launcher (libbinder client)"
adb -s "$DEVICE" shell "/data/local/tmp/rpc_accessor_register_interop_launcher $INSTANCE; echo client-exit=\$?"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor_register_interop_server 2>/dev/null; rm -f $SOCK" || true
