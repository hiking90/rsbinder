#!/usr/bin/env bash
# Subplan 2-14 D.8.b — cross-process accessor discovery via rsb_hub on
# a Linux host. The Linux native sibling of `run_rpc_accessor_register_interop.sh`:
# instead of cross-compiling for an Android emulator and using real
# libbinder as the client, this drives the rsbinder consume-side
# (`hub::get_service` → `Service::Accessor(Some(_))` arm → bridged RPC
# root) against rsbinder server + rsb_hub all on the same Linux box.
#
# What gets exercised here that the D.9 STAGE3 (emulator) gate does
# not: rsb_hub's `addService` descriptor auto-detect (B.6) and
# `getService2`/`checkService2` accessor-arm vend (B.7). The D.9 path
# uses real Android servicemanager, which already has VINTF semantics
# rsb_hub doesn't — so D.8.b is the *only* gate where rsb_hub's
# Phase B code runs in earnest.
#
# Prereqs:
#   * Linux kernel with `CONFIG_ANDROID_BINDER_IPC` (or `_RUST`) and
#     `CONFIG_ANDROID_BINDERFS`, binderfs mounted, the default
#     `/dev/binderfs/binder` device world-`rw` (no `sudo` needed).
#   * cargo + rustc.
#
# Usage:
#   cd <workspace> && ./example-hello/cpp/run_d8b_register.sh
#
# Exits 0 on D.8.b PASS; non-zero otherwise. On failure, leaves
# /tmp/rsb_hub.log and /tmp/rsacc_d8b.log around for inspection.

set -euo pipefail

INSTANCE=rsbinder.test.d8b.accessor
SOCK=/tmp/rsacc-d8b.sock
HUB_LOG=/tmp/rsb_hub.log
SRV_LOG=/tmp/rsacc_d8b.log

# Locate the workspace root regardless of where the script is invoked
# from (so `./example-hello/cpp/run_d8b_register.sh` and `cd cpp &&
# ./run_d8b_register.sh` both work).
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

cleanup() {
    kill ${SRV_PID:-} ${HUB_PID:-} 2>/dev/null || true
    sleep 1
    pkill -f 'target/debug/rsb_hub' 2>/dev/null || true
    pkill -f 'rpc_accessor_register_interop_server' 2>/dev/null || true
    rm -f "$SOCK"
}
trap cleanup EXIT

echo "==> [1/4] cleaning any stale state"
pkill -f 'target/debug/rsb_hub' 2>/dev/null || true
pkill -f 'rpc_accessor_register_interop_server' 2>/dev/null || true
rm -f "$SOCK"
sleep 1

echo "==> [2/4] building rsb_hub + accessor server + test binary"
cargo build --bin rsb_hub
# `example-hello` needs its own `android_16` feature to pull in
# `rsbinder/android_16` (the `hub::android_16::create_accessor` and
# friends). The `tests/` crate transitively gets `android_16` via
# `rsbinder/android_10_plus` and only declares its own `rpc` feature
# (see `tests/Cargo.toml`), so its invocation passes `--features rpc`.
cargo build -p example-hello --features rpc,android_16 \
    --bin rpc_accessor_register_interop_server
cargo test --no-run -p tests --features rpc --lib 2>&1 | tail -3

echo "==> [3/4] starting rsb_hub + accessor server"
RUST_LOG=warn nohup ./target/debug/rsb_hub > "$HUB_LOG" 2>&1 &
HUB_PID=$!
disown $HUB_PID || true
sleep 2
kill -0 $HUB_PID 2>/dev/null || {
    echo "ERROR: rsb_hub (PID $HUB_PID) died. log:"; cat "$HUB_LOG"; exit 1
}
echo "  rsb_hub PID=$HUB_PID"

RUST_LOG=warn nohup ./target/debug/rpc_accessor_register_interop_server \
    "$INSTANCE" "$SOCK" 2 > "$SRV_LOG" 2>&1 &
SRV_PID=$!
disown $SRV_PID || true
sleep 2
kill -0 $SRV_PID 2>/dev/null || {
    echo "ERROR: accessor server (PID $SRV_PID) died. log:"; cat "$SRV_LOG"; exit 1
}
echo "  accessor server PID=$SRV_PID (waiting for READY)"
# Wait up to ~5s for the server's "READY" marker on stdout.
for _ in $(seq 1 50); do
    if grep -q READY "$SRV_LOG"; then break; fi
    sleep 0.1
done
grep -q READY "$SRV_LOG" || {
    echo "ERROR: accessor server never printed READY. log:"; cat "$SRV_LOG"; exit 1
}

echo "==> [4/4] running D.8.b ignored test"
cargo test --features rpc -p tests \
    --lib d8b_cross_process_accessor_via_rsb_hub \
    -- --ignored --nocapture
TEST_EXIT=$?

if [[ $TEST_EXIT -eq 0 ]]; then
    echo "==> D.8.b PASS"
else
    echo "==> D.8.b FAIL (exit $TEST_EXIT)"
    echo "--- rsb_hub log tail ---"
    tail -20 "$HUB_LOG" || true
    echo "--- accessor server log tail ---"
    tail -20 "$SRV_LOG" || true
fi

exit $TEST_EXIT
