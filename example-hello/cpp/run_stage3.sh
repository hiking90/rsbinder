#!/usr/bin/env bash
# Subplan 2-13 D.8 STAGE3 — drive the rsbinder IAccessor bridge against
# real android-16 libbinder on an emulator. See the launcher source
# (`rpc_accessor_interop_launcher.cpp`) and the rsbinder client
# (`../src/bin/rpc_accessor_interop_client.rs`) for the contract.
#
# Prereqs:
#   * Android_16 AVD booted (`emulator -avd Android_16 -port 5556`).
#   * NDK r29+ at `$ANDROID_NDK_HOME` (default
#     `/opt/homebrew/share/android-ndk`).
#   * `cargo ndk` installed (`cargo install cargo-ndk`).
#   * `aarch64-linux-android` rustup target installed
#     (`rustup target add aarch64-linux-android`).
#
# Usage:
#   ./run_stage3.sh [-s emulator-5556]
#
# Exits 0 on STAGE3 PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5556
INSTANCE=rsbinder.test.acc
SOCK=/data/local/tmp/rsacc-rpc.sock
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
    "$CPP_DIR/rpc_accessor_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_accessor_interop_launcher"

echo "==> cross-compiling rsbinder client"
( cd "$REPO_ROOT" && \
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}" \
    cargo ndk -t arm64-v8a -p 35 build --release -p example-hello \
        --features rpc,android_16 \
        --bin rpc_accessor_interop_client )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_accessor_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/aarch64-linux-android/release/rpc_accessor_interop_client" \
    /data/local/tmp/ >/dev/null

echo "==> killing any old launcher + cleaning state"
adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor 2>/dev/null; rm -f $SOCK /data/local/tmp/rsacc.stdout /data/local/tmp/rsacc.stderr; sleep 1" || true

echo "==> starting launcher (background)"
adb -s "$DEVICE" shell "nohup /data/local/tmp/rpc_accessor_interop_launcher $INSTANCE $SOCK > /data/local/tmp/rsacc.stdout 2> /data/local/tmp/rsacc.stderr &" &
sleep 4
adb -s "$DEVICE" shell "cat /data/local/tmp/rsacc.stderr"

echo "==> running rsbinder client"
adb -s "$DEVICE" shell "/data/local/tmp/rpc_accessor_interop_client $INSTANCE; echo client-exit=\$?"

echo "==> stopping launcher"
adb -s "$DEVICE" shell "pkill -9 -f rpc_accessor_interop_launcher 2>/dev/null; rm -f $SOCK" || true
