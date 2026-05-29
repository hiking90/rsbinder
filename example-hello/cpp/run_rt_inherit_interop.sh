#!/usr/bin/env bash
# Plan 4-5 Phase D STAGE3 — RT scheduler inheritance interop.
#
# This script targets **REMOTE_LINUX** (king@archlinux,
# kernel ≥ 7.0.3-zen with Rust binder driver — see
# `project_remote_linux_box`) because:
#
#   * The Android emulator boots with a SELinux policy that
#     forbids `sched_setscheduler` for non-system processes, so the
#     hard RT inheritance check (Call B) cannot run.
#   * REMOTE_LINUX accepts `CAP_SYS_NICE` via `setcap`/`sudo`, so the
#     RT escalation actually fires and the kernel binder driver lifts
#     the worker thread's scheduler class for the duration of the
#     transaction.
#
# Pair:
#   * rt_inherit_interop_service — `BinderFeatures::min_sched_policy = SCHED_FIFO`,
#     `min_priority = 5`, `inherit_rt = true`.
#   * rt_inherit_interop_client  — Call A (caller = SCHED_NORMAL/OTHER) +
#     Call B (caller = SCHED_FIFO via `sched_setscheduler`). Call B
#     SKIPs cleanly when the client lacks CAP_SYS_NICE; PASS still
#     covers AC-4.5.6 (Call A) + helper plumbing (AC-4.5.7).
#
# Usage:
#   ./run_rt_inherit_interop.sh                       # default ssh host
#   REMOTE=archlinux ./run_rt_inherit_interop.sh      # override host
#   AS_ROOT=1 ./run_rt_inherit_interop.sh             # use sudo for Call B
#
# Exit 0 on PASS.

set -euo pipefail

REMOTE="${REMOTE:-archlinux}"
REPO_REMOTE="${REPO_REMOTE:-workspace/rsbinder}"

echo "==> syncing source to $REMOTE:$REPO_REMOTE"
rsync -avz \
    --include="rsbinder/" --include="rsbinder/**" \
    --include="example-hello/" --include="example-hello/**" \
    --exclude="*" \
    "$(cd "$(dirname "$0")/../.." && pwd)/" \
    "$REMOTE:$REPO_REMOTE/" >/dev/null

echo "==> building service + client"
ssh "$REMOTE" "cd $REPO_REMOTE && cargo build -p example-hello \
    --bin rt_inherit_interop_service --bin rt_inherit_interop_client"

echo "==> ensuring rsb_hub is running"
ssh "$REMOTE" "cd $REPO_REMOTE && pgrep -af rsb_hub >/dev/null || \
    (nohup ./target/debug/rsb_hub > /tmp/rsb_hub.log 2>&1 < /dev/null &) ; sleep 1"

echo "==> restarting service"
ssh "$REMOTE" "cd $REPO_REMOTE && \
    pkill -9 -f rt_inherit_interop_service 2>/dev/null || true ; \
    nohup ./target/debug/rt_inherit_interop_service > /tmp/svc45.stdout 2> /tmp/svc45.stderr < /dev/null &"
sleep 3

echo "==> running client"
if [[ "${AS_ROOT:-0}" == "1" ]]; then
    out=$(ssh "$REMOTE" "cd $REPO_REMOTE && sudo ./target/debug/rt_inherit_interop_client; echo client-exit=\$?" 2>&1)
else
    out=$(ssh "$REMOTE" "cd $REPO_REMOTE && ./target/debug/rt_inherit_interop_client; echo client-exit=\$?" 2>&1)
fi
echo "$out"

echo "==> stopping service"
ssh "$REMOTE" "pkill -9 -f rt_inherit_interop_service 2>/dev/null" || true

echo "$out" | grep -q "STAGE3_4_5_A caller=SCHED_NORMAL"  || { echo "FAIL: Call A did not run"; exit 1; }
echo "$out" | grep -q "STAGE3_4_5_PASS"                   || { echo "FAIL: missing STAGE3_4_5_PASS"; exit 1; }
echo "$out" | grep -q "client-exit=0"                     || { echo "FAIL: client-exit != 0"; exit 1; }
echo "==> PASS"
