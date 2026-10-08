#!/usr/bin/env bash
# Build `libfmq_peer` for the Linux host against AOSP's libfmq sources
# (plan 12-fmq §6.1, the F2 spike), then print how to run the tests that
# drive it.
#
#   AOSP=~/Workspace/aosp tests/cpp/build_host_peer.sh
#
# Needs system/libfmq (full), system/core (libcutils, libutils), system/libbase and
# system/logging (headers) under $AOSP: a full tree, or one clone per project.
# Output goes under the workspace `target/` directory.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
AOSP="${AOSP:-$HOME/Workspace/aosp}"
OUT="${OUT:-$REPO_ROOT/target/libfmq-host}"
CXX="${CXX:-clang++}"

FMQ="$AOSP/system/libfmq"
for d in "$FMQ/include" "$FMQ/base" "$AOSP/system/core/libcutils/include" \
         "$AOSP/system/core/libutils/include" "$AOSP/system/libbase/include" \
         "$AOSP/system/logging/liblog/include"; do
    [ -d "$d" ] || { echo "missing $d (see the header of tests/cpp/build_host_peer.sh)" >&2; exit 2; }
done

mkdir -p "$OUT"
"$CXX" -std=c++17 -O1 -g -Wall -Wno-unused-parameter \
    -I "$FMQ/include" -I "$FMQ/base" \
    -I "$AOSP/system/core/libcutils/include" \
    -I "$AOSP/system/core/libutils/include" \
    -I "$AOSP/system/libbase/include" \
    -I "$AOSP/system/logging/liblog/include" \
    "$HERE/libfmq_peer.cpp" "$HERE/host_shims.cpp" \
    "$FMQ/EventFlag.cpp" \
    "$AOSP/system/core/libcutils/native_handle.cpp" \
    -o "$OUT/libfmq_peer"

echo "built $OUT/libfmq_peer (libfmq @ $(git -C "$FMQ" log -1 --format=%h))"
echo "run:  RSBINDER_FMQ_LIBFMQ_PEER=$OUT/libfmq_peer cargo test -p rsbinder-fmq --test libfmq_host -- --ignored"
