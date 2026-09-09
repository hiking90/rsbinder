#!/usr/bin/env bash
# Subplan 2-12 Phase D / AC-12.6 STAGE3 — drive a real-libbinder *client*
# (`ARpcSession_setMaxOutgoingConnections(2) + setMaxIncomingThreads(1)`)
# against an rsbinder *server* (`RpcServer::set_max_threads(3)`) on the
# android-16 emulator. The non-negotiable multi-connection-per-session
# interop gate (Plan 2-12 §3 AC-12.6).
#
# STATUS (2026-05-28): AC-12.6 PASS — all three gates green, multi-
# conn is out of EXPERIMENTAL.
#   * (a) PASS: concurrent twoway across 2 outgoing slots (~80 ms,
#     well under 250 ms parallel budget). Validates the plan 2-12
#     Phase D wire fix (`server_accept` INCOMING bit parse +
#     NewSessionResponse only for new session + direction-aware
#     "cci" exchange).
#   * (b) PASS: 20 oneway calls + polled TX_GET_LOG drain. Validates
#     Phase C (per-`mNodeForAddress` `asyncNumber` send-side +
#     receive-side `asyncTodo` priority replay): libbinder's
#     `ExclusiveConnection::find(CLIENT_ASYNC)` round-robins oneway
#     across the client's `mOutgoing` pool, so the rsbinder server
#     receives them split across slots whose `serve_blocking_on`
#     workers dispatch independently. The per-node `asyncTodo` parks
#     out-of-order arrivals and the priority replay drains them when
#     the matching expected `async_number` arrives — eventual per-
#     node monotonic order, bounded poll window in the launcher.
#   * (d) PASS (2026-08-30, plan 2-20): TX_SCHEDULE_CALLBACK parks the
#     callback and the rsbinder server drives it ~150 ms later from a
#     thread inside no handler (twoway echo + oneway notify). The send
#     rides the client's `setMaxIncomingThreads(1)` connection, admitted
#     by the server as a callback (`Outgoing`) slot; libbinder dispatches
#     both on that connection's thread.
#   * (c) PASS: 2 parallel TX_INVOKE_CALLBACK from 2 client threads
#     each land on their own server incoming-slot worker; the nested
#     server→client `cb.transact` rides the **same** slot via the
#     plan 2-12 Phase A `find_conn` DRIVING `(sess, slot)` re-entry
#     pin (bidirectional wire on one TCP socket). The libbinder
#     client's `waitForReply` loop on that slot dispatches the
#     inbound nested TRANSACT in re-entrant context and writes the
#     reply on the same slot. F8.B (split `mOutgoing`/`mIncoming`
#     pools) turned out to be **not required** — the AOSP-divergent
#     "1 slot pool + DRIVING `(sess, slot)` reentrant pin" model is
#     functionally equivalent at the wire level.
#
# Prereqs (same as run_rpc_accessor_interop.sh / run_rpc_accessor_register_interop.sh):
#   * Android_16 AVD booted (`emulator -avd Android_16 -port 5556`).
#   * NDK r29+ at `$ANDROID_NDK_HOME` (default
#     `/opt/homebrew/share/android-ndk`).
#   * `cargo ndk` installed.
#   * the rustup target for the device's ABI installed.
#
# Usage:
#   ./run_rpc_multiconn_interop.sh [-s emulator-5556] [-t <abi>]
#
# The ABI defaults to the device's own; -t overrides it.
# Exits 0 on AC-12.6 PASS; non-zero otherwise.

set -euo pipefail

DEVICE=emulator-5556
ABI=""
SOCK=/data/local/tmp/rsmc.sock
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
    "$CPP_DIR/rpc_multiconn_interop_launcher.cpp" \
    -o "$CPP_DIR/rpc_multiconn_interop_launcher"

echo "==> cross-compiling rsbinder server"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" -p "$API" build --release -p example-hello \
        --features rpc \
        --bin rpc_multiconn_interop_server )

echo "==> pushing binaries"
adb -s "$DEVICE" push "$CPP_DIR/rpc_multiconn_interop_launcher" /data/local/tmp/ >/dev/null
adb -s "$DEVICE" push "$REPO_ROOT/target/$TRIPLE/release/rpc_multiconn_interop_server" \
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
launcher_out=$(adb -s "$DEVICE" shell "/data/local/tmp/rpc_multiconn_interop_launcher $SOCK 2>&1; echo client-exit=\$?" | tr -d '\r')
printf '%s\n' "$launcher_out"

echo "==> stopping server"
adb -s "$DEVICE" shell "pkill -9 -f rpc_multiconn_interop_server 2>/dev/null; rm -f $SOCK" || true

# Gate: the launcher's exit status (0 only after (a)–(d) all passed) and
# the (d) marker — the plan 2-20 out-of-handler callback gate.
if ! grep -q '^client-exit=0$' <<<"$launcher_out"; then
    echo "FAIL: launcher exited non-zero"
    exit 1
fi
if ! grep -q '(d) PASS' <<<"$launcher_out"; then
    echo "FAIL: gate (d) (callback outside a handler) did not pass"
    exit 1
fi
echo "PASS: AC-12.6 (a)(b)(c) + plan 2-20 (d)"
