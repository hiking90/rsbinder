#!/usr/bin/env bash
# Plan 10-7b P5 STAGE3 — the RPC-path stream between rsbinder and the
# device's own libbinder `RpcSession` (libbinder_rpc_unstable.so).
#
# rsbinder serves `streamdemo.IStreamDemo` as the root of an `RpcServer` on
# a Unix socket (`stream_probe serve-rpc`); the NDK client
# (stream_rpc_interop.cpp) connects with two incoming threads and runs the
# stream contract from the `.aidl` files rsbinder ships, compiled by the
# platform's `aidl --lang=ndk`. Over RPC the endpoint carries only the
# sink: items are `oneway onBatch` calls paced by `oneway request` grants,
# and a death is the session going away (plans/10-7b §13.2).
#
#   R1  download: 20000 items in 16-item batches, in order, clean end; the
#       grant totals 1, 1, 0 after the first batch buy one batch, not two
#   R2  download: the end carries a service-specific failure
#   R3  download: the C++ consumer cancels; the producer stops with
#       InvalidOperation
#   R4  download: the server is killed; the consumer's death link on the
#       source fires
#   R5  download: the C++ consumer is killed while the producer waits for
#       credit; the producer ends with DeadObject
#   R6  upload: the C++ producer opens the stream on the service's sink and
#       sends 20000 items on the credit rsbinder grants
#   R7  upload: the C++ producer is killed; the service's receiver ends
#       with DeadObject
#   R8a stream ping (plan 2-24 D9), server reply deadline 1 s: the producer
#       parked for credit pings the C++ sink every third of it, libbinder
#       answers `PING_TRANSACTION`, and the producer is still parked after
#       five seconds
#   R8b the same with the C++ consumer stopped (SIGSTOP): its socket still
#       takes the ping, nothing answers, the session ends on the deadline
#       and the producer ends with DeadObject. Without the ping it would
#       wait for good, so R8b is what shows R8a's pings were sent
#   R9  upload against `stream_probe serve-rpc-ring`, whose receiver opts in
#       to a ring (plan 10-7c AC-C10): the C++ producer asks for `Unix` fds,
#       gets a ring endpoint, refuses it and exits; the rsbinder receiver
#       ends with DeadObject
#
# Prereqs:
#   * a booted AVD (AOSP `default` or google_apis, rootable), SDK >= 34
#     (libbinder_rpc_unstable with the preconnected client)
#   * NDK at $ANDROID_NDK_HOME (default /opt/android-ndk), `cargo ndk`,
#     the rustup target for the device's ABI
#   * SDK at $ANDROID_HOME (default /opt/android-sdk) with a build-tools
#     `aidl` and a platform that has `optional/libbinder_ndk_cpp`
#   * AOSP checkout at $AOSP (default ~/Workspace/aosp): the frozen V1
#     `android.hardware.common{,.fmq}` AIDL (StreamEndpoint imports
#     MQDescriptor, absent over RPC but still in the type)
#   * `adb root` + `setenforce 0`, applied here and restored on exit: the
#     socket file is created under /data/local/tmp.
#
# Usage: ./run_stream_rpc_interop.sh [-s emulator-5554] [-t <abi>]
# Exit 0 when every case passes.

set -euo pipefail

DEVICE=emulator-5554
ABI=""
CPP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$CPP_DIR/../.." && pwd)"
NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
SDK="${ANDROID_HOME:-/opt/android-sdk}"
AOSP="${AOSP:-$HOME/Workspace/aosp}"
WORK="$REPO_ROOT/target/stream_rpc_interop"
DEV_DIR=/data/local/tmp
RS_LOG=$DEV_DIR/stream_probe_rpc.log
SOCK=$DEV_DIR/rsb_stream_rpc.sock

while [[ ${1:-} == -* ]]; do
    case "$1" in
        -s) DEVICE="$2"; shift 2 ;;
        -t) ABI="$2"; shift 2 ;;
        *) echo "usage: $0 [-s <device>] [-t <abi>]" >&2; exit 2 ;;
    esac
done
ADB=(adb -s "$DEVICE")

FMQ_API="$AOSP/hardware/interfaces/common/fmq/aidl/aidl_api/android.hardware.common.fmq/1"
COMMON_API="$AOSP/hardware/interfaces/common/aidl/aidl_api/android.hardware.common/1"
for d in "$FMQ_API" "$COMMON_API"; do
    [ -d "$d" ] || { echo "missing $d (see CLAUDE.md, Local AOSP Reference Checkout)" >&2; exit 2; }
done

echo "==> checking device $DEVICE"
sdk=$("${ADB[@]}" shell getprop ro.build.version.sdk | tr -d '\r')
[[ "$sdk" -ge 34 ]] || { echo "device $DEVICE is SDK $sdk, expected >= 34"; exit 1; }

case "${ABI:-$("${ADB[@]}" shell getprop ro.product.cpu.abi | tr -d '\r')}" in
    arm64-v8a|aarch64) TRIPLE=aarch64-linux-android ;;
    x86_64)            TRIPLE=x86_64-linux-android ;;
    *) echo "unsupported ABI; pass -t arm64-v8a or -t x86_64" >&2; exit 2 ;;
esac

# The binary's minSdk must not exceed the device's API level.
CXX=""
API=""
for a in $(seq "$sdk" -1 34); do
    for c in "$NDK"/toolchains/llvm/prebuilt/*/bin/"${TRIPLE}${a}"-clang++; do
        [ -x "$c" ] && { CXX="$c"; API="$a"; break 2; }
    done
done
[ -n "$CXX" ] || { echo "no NDK clang++ for $TRIPLE at API <= $sdk under $NDK" >&2; exit 2; }
echo "==> target $TRIPLE, API $API (device SDK $sdk)"

AIDL=$(ls "$SDK"/build-tools/*/aidl 2>/dev/null | sort -V | tail -1 || true)
[ -x "$AIDL" ] || { echo "no build-tools aidl under $SDK" >&2; exit 2; }
NDK_CPP=$(ls -d "$SDK"/platforms/android-*/optional/libbinder_ndk_cpp 2>/dev/null | sort -V | tail -1 || true)
[ -d "$NDK_CPP" ] || { echo "no optional/libbinder_ndk_cpp under $SDK/platforms" >&2; exit 2; }

echo "==> compiling the .aidl files with $AIDL"
rm -rf "$WORK"
mkdir -p "$WORK/sys"
"$AIDL" --lang=ndk --structured --stability vintf -I "$FMQ_API" -I "$COMMON_API" \
    -o "$WORK/src" -h "$WORK/include" \
    "$FMQ_API"/android/hardware/common/fmq/*.aidl "$COMMON_API"/android/hardware/common/*.aidl
"$AIDL" --lang=ndk --structured -I "$FMQ_API" -I "$COMMON_API" \
    -I "$REPO_ROOT/rsbinder/aidl/stream" -I "$REPO_ROOT/tests/aidl" \
    -o "$WORK/src" -h "$WORK/include" \
    "$REPO_ROOT"/rsbinder/aidl/stream/rsbinder/stream/*.aidl \
    "$REPO_ROOT/tests/aidl/streamdemo/IStreamDemo.aidl"

echo "==> pulling the device's libbinder_ndk.so and libbinder_rpc_unstable.so to link against"
for l in libbinder_ndk.so libbinder_rpc_unstable.so; do
    "${ADB[@]}" pull "/system/lib64/$l" "$WORK/sys/$l" >/dev/null
done

echo "==> building the C++ half (NDK clang, generated NDK stubs, device libbinder)"
"$CXX" -O2 -Wall -Wextra -Wno-unused-parameter -std=c++17 -DANDROID -static-libstdc++ \
    -I "$WORK/include" -I "$NDK_CPP" \
    -L "$WORK/sys" -lbinder_ndk -lbinder_rpc_unstable -llog \
    "$CPP_DIR/stream_rpc_interop.cpp" \
    "$WORK"/src/android/hardware/common/fmq/*.cpp \
    "$WORK"/src/android/hardware/common/*.cpp \
    "$WORK"/src/rsbinder/stream/*.cpp \
    "$WORK"/src/streamdemo/*.cpp \
    -o "$WORK/stream_rpc_interop"

echo "==> cross-compiling the rsbinder half"
( cd "$REPO_ROOT" && ANDROID_NDK_HOME="$NDK" \
    cargo ndk -t "$TRIPLE" --platform "$API" build --release -p tests --features rpc --bin stream_probe )

echo "==> pushing artifacts"
"${ADB[@]}" root >/dev/null 2>&1 || true
"${ADB[@]}" wait-for-device
ENFORCE=$("${ADB[@]}" shell getenforce | tr -d '\r')
"${ADB[@]}" shell setenforce 0 >/dev/null 2>&1 || true
"${ADB[@]}" push "$REPO_ROOT/target/$TRIPLE/release/stream_probe" "$WORK/stream_rpc_interop" \
    "$DEV_DIR/" >/dev/null
"${ADB[@]}" shell chmod 755 "$DEV_DIR/stream_probe" "$DEV_DIR/stream_rpc_interop"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

# `pidof` matches the process name, so the shell running it is never a match.
stop_all() {
    "${ADB[@]}" shell 'for p in $(pidof stream_probe) $(pidof stream_rpc_interop); do kill $p; done' \
        >/dev/null 2>&1 || true
}
cleanup() {
    stop_all
    "${ADB[@]}" shell "rm -f $SOCK" >/dev/null 2>&1 || true
    [ "$ENFORCE" = "Enforcing" ] && "${ADB[@]}" shell setenforce 1 >/dev/null 2>&1 || true
}
trap cleanup EXIT
stop_all

SERVE_MODE=serve-rpc
start_server() {   # [replyTimeoutMs]; `SERVE_MODE=serve-rpc-ring` takes none
    "${ADB[@]}" shell "rm -f $RS_LOG $SOCK" >/dev/null 2>&1 || true
    "${ADB[@]}" shell "$DEV_DIR/stream_probe $SERVE_MODE $SOCK ${1:-} > $RS_LOG 2>&1" &
    for _ in $(seq 1 20); do
        if "${ADB[@]}" shell "grep -c '^SERVING' $RS_LOG 2>/dev/null" | tr -d '\r' | grep -qv '^0$'; then
            return 0
        fi
        sleep 0.5
    done
    bad "the rsbinder RPC server did not start"
    "${ADB[@]}" shell cat "$RS_LOG"
    return 1
}

CALL_TIMEOUT=""
if command -v timeout >/dev/null 2>&1; then CALL_TIMEOUT="timeout 120"; fi
# `|| true`: the printed lines are what is judged.
run() { { $CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/stream_rpc_interop $SOCK $* 2>&1 || true" || true; } | tr -d '\r'; }

judge() {   # <output> <expected RESULT regex> <label>
    echo "$1" | sed 's/^/      /'
    if echo "$1" | grep -qE "^$2\$"; then ok "$3"; else bad "$3"; fi
}

# The client in the background until READY and two seconds more, then
# `kill -9 $(pidof <victim>)`; returns the client's output.
killed_run() {   # <case args> <victim>
    local out="$WORK/killed.out"
    rm -f "$out"
    # Straight to the file: a `tr` in between would hold READY until the client exits.
    { $CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/stream_rpc_interop $SOCK $1 2>&1" || true; } > "$out" &
    local client=$!
    for _ in $(seq 1 20); do grep -q '^READY' "$out" 2>/dev/null && break; sleep 0.5; done
    sleep 2
    "${ADB[@]}" shell "kill -9 \$(pidof $2)" >/dev/null 2>&1 || true
    wait "$client" || true
    tr -d '\r' < "$out"
}

# The service's counters on a fresh session, polled until `field` reads `want`.
status_until() {   # <field> <want>
    local s=""
    for _ in $(seq 1 20); do
        s=$(run status)
        echo "$s" | grep -q " $1=$2\( \|\$\)" && break
        sleep 0.5
    done
    echo "$s"
}

echo
echo "=== libbinder RpcSession client against rsbinder's RpcServer"
if start_server; then
    judge "$(run download 20000 64 4)" \
        "RESULT download 20000 ordered end=0 held=5 overrun=0 decoded=1 stalled=0" \
        "R1 download: 20000 items in 16-item batches over libbinder's incoming connections; totals 1,1,0 bought one batch"
    judge "$(run failing 3 77 quota-exhausted)" \
        "RESULT failing 3 ended=1 exception=-8 code=77 message=quota-exhausted" \
        "R2 download: onEnd carries EX_SERVICE_SPECIFIC, the code and the message"
    judge "$(run cancel 10)" \
        "RESULT cancel 10 finished=1 error=-38" \
        "R3 download: the C++ cancel stopped the rsbinder producer with InvalidOperation"
    judge "$(run upload 20000 4 16)" \
        "RESULT upload 20000 ordered finished=1 error=0 sent=20000" \
        "R6 upload: 20000 items from the C++ producer on credit rsbinder granted, clean end"
    judge "$(killed_run hang\ 4 stream_rpc_interop | grep -v '^READY'; status_until finished 1)" \
        "RESULT status sent=[0-9]+ finished=1 error=-32 .*" \
        "R5 download: the C++ consumer killed with the producer parked for credit; the producer ended with DeadObject"
    judge "$(killed_run vanish stream_rpc_interop | grep -v '^READY'; status_until up_finished 1)" \
        "RESULT status .* up_ordered=1 up_finished=1 up_error=-32" \
        "R7 upload: the C++ producer killed mid-stream; the rsbinder receiver ended with DeadObject"
    judge "$(killed_run orphan stream_probe | grep -v '^READY')" \
        "RESULT orphan [1-9][0-9]* linked=1 died=1 ended=0 stalled=0" \
        "R4 download: the rsbinder server killed; the C++ consumer's death link on the source fired"
fi

# `hang 4` in the background until READY: the producer is then parked for credit.
start_hang() {
    local out="$WORK/hang.out"
    rm -f "$out"
    { $CALL_TIMEOUT "${ADB[@]}" shell "$DEV_DIR/stream_rpc_interop $SOCK hang 4 2>&1" || true; } > "$out" &
    HANG_PID=$!
    for _ in $(seq 1 20); do grep -q '^READY' "$out" 2>/dev/null && break; sleep 0.5; done
}
end_hang() {
    "${ADB[@]}" shell "kill -9 \$(pidof stream_rpc_interop)" >/dev/null 2>&1 || true
    wait "$HANG_PID" || true
}

# The service's counters are per process, so each case gets a server of its own.
PING_MS=1000
echo
echo "=== the stream ping against a libbinder peer (reply deadline $PING_MS ms)"
stop_all
if start_server "$PING_MS"; then
    start_hang
    # Past four thirds of the deadline three times over: an unanswered ping would have ended it.
    sleep 5
    judge "$(run status)" \
        "RESULT status sent=[0-9]+ finished=0 error=0 .*" \
        "R8a ping: libbinder answers the parked producer's pings; the stream is alive after 5 s"
    end_hang
fi
stop_all
if start_server "$PING_MS"; then
    start_hang
    "${ADB[@]}" shell "kill -STOP \$(pidof stream_rpc_interop)" >/dev/null 2>&1 || true
    judge "$(status_until finished 1)" \
        "RESULT status sent=[0-9]+ finished=1 error=-32 .*" \
        "R8b ping: a stopped C++ consumer leaves the ping unanswered; the producer ended with DeadObject"
    end_hang
fi

# Plan 10-7c AC-C10: the rsbinder consumer opts in to a ring and the C++ producer, which
# implements only calls, asks for `Unix` fds. The opening call has already succeeded when the
# producer refuses, so the consumer learns of it from the producer's process going away.
stop_all
SERVE_MODE=serve-rpc-ring
if start_server; then
    judge "$(run upload-fds 100 4 16)" \
        "RESULT ERROR ring-over-rpc" \
        "R9a upload: an opted-in rsbinder consumer handed the C++ producer a ring, which it refused"
    judge "$(status_until up_finished 1)" \
        "RESULT status .* up_finished=1 up_error=-32" \
        "R9b upload: the refusing producer exited; the rsbinder receiver ended with DeadObject"
fi
SERVE_MODE=serve-rpc

echo
echo "PASS $PASS  FAIL $FAIL"
[ "$FAIL" -eq 0 ]
