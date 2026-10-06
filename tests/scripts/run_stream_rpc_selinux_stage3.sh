#!/usr/bin/env bash
# Plan 10-7c B6: a ring over an RPC Unix session whose two ends are in
# different SELinux domains, on an enforcing Android device.
#
# The kernel-path counterpart is AC-7b.8 in `run_stream_ac.sh`; this one
# runs `tests/tests/stream_rpc.rs`'s two-process test with the client in
# `shell` (enforcing) and its `stream_probe serve-rpc-ring` child in `su`.
# The download maps a `shell` memfd in `su`; the upload maps a `su` memfd
# in `shell`, which is the enforced check. Besides what AC-7b.8 rests on
# (`allow domain su:fd use`, `allow domain su:memfd_file { getattr read
# write map }`, userdebug rules in domain.te), the session needs
# `allow domain su:unix_stream_socket connectto`. The socket is abstract:
# a path under /data/local/tmp is `shell_data_file`, whose `sock_file
# write` `shell` is denied.
#
#   cargo ndk -t arm64-v8a --platform 35 test -p tests --features rpc --test stream_rpc --no-run
#   cargo ndk -t arm64-v8a --platform 35 build -p tests --features rpc --bin stream_probe
#   ./tests/scripts/run_stream_rpc_selinux_stage3.sh [-s <serial>] [--target <triple>]
#
# Needs a userdebug image (`adb root`, `su`). Leaves the SELinux mode as
# it found it. Device-only, so not named `run_*_ac.sh`: that glob is the
# host kernel gate in integration-test.yml and scripts/local_gate.sh.
set -u

cd "$(dirname "$0")/../.." || exit 1

SERIAL=
TARGET=aarch64-linux-android
while [ $# -gt 0 ]; do
    case "$1" in
        -s) SERIAL=$2; shift ;;
        --target) TARGET=$2; shift ;;
        *) echo "usage: $0 [-s <serial>] [--target <triple>]"; exit 2 ;;
    esac
    shift
done

PROBE=target/$TARGET/debug/stream_probe
TEST=
for f in target/"$TARGET"/debug/deps/stream_rpc-*; do
    case $f in *.d) continue ;; esac
    { [ -z "$TEST" ] || [ "$f" -nt "$TEST" ]; } && TEST=$f
done
[ -f "$PROBE" ] && [ -f "$TEST" ] || { echo "missing $PROBE or the stream_rpc test — see the header"; exit 1; }

ADB=(adb)
[ -n "$SERIAL" ] && ADB=(adb -s "$SERIAL")
DEV=/data/local/tmp/rsb107c-b6
CASE=two_process::a_ring_over_rpc_crosses_two_processes

dev() { "${ADB[@]}" shell "$@" </dev/null | tr -d '\r'; }
as_root()  { "${ADB[@]}" root   >/dev/null 2>&1; sleep 3; "${ADB[@]}" wait-for-device; }
as_shell() { "${ADB[@]}" unroot >/dev/null 2>&1; sleep 3; "${ADB[@]}" wait-for-device; }

as_root
[ "$(dev id -u)" = 0 ] || { echo "adb root did not take; use a userdebug image"; exit 1; }
[ -n "$(dev su root id -u 2>/dev/null)" ] || { echo "no usable su binary on this image"; exit 1; }
SELINUX_WAS=$(dev getenforce)

cleanup() {
    as_root
    # The client cannot signal its `su` child, so the server outlives it.
    dev "pkill -9 -x stream_probe; rm -rf $DEV" >/dev/null 2>&1
    [ "$SELINUX_WAS" = Permissive ] && dev setenforce 0
}
trap cleanup EXIT

dev "pkill -9 -x stream_probe; rm -rf $DEV; mkdir -p $DEV; chmod 777 $DEV" >/dev/null 2>&1
"${ADB[@]}" push -q "$PROBE" "$DEV/stream_probe" || { echo "push failed"; exit 1; }
"${ADB[@]}" push -q "$TEST" "$DEV/stream_rpc" || { echo "push failed"; exit 1; }
# `STREAM_PROBE_BIN` names one executable; this one moves the probe into `su`.
dev "echo '#!/system/bin/sh' > $DEV/probe-su.sh; \
     echo 'exec su root $DEV/stream_probe \"\$@\"' >> $DEV/probe-su.sh; \
     chmod 755 $DEV/stream_probe $DEV/stream_rpc $DEV/probe-su.sh"

dev setenforce 1
as_shell
[ "$(dev id -u)" != 0 ] || { echo "adb unroot did not take"; exit 1; }
T0=$(dev "date +'%m-%d %H:%M:%S.000'")
printf 'client domain %s, %s\n' "$(dev id -Z)" "$(dev getenforce)"

# Every stdio stream off adb's: the `su` server outlives the test and would hold them open.
dev "cd $DEV && STREAM_PROBE_BIN=$DEV/probe-su.sh timeout 120 ./stream_rpc --exact $CASE \
     --test-threads=1 < /dev/null > out.txt 2>&1; echo \$? > rc"
SVC_LABEL=$(dev "ps -A -o LABEL,NAME | grep stream_probe" | awk '{print $1}' | head -1)
printf 'service domain %s\n' "${SVC_LABEL:-<gone>}"
dev cat "$DEV/out.txt" | grep -E '^test |test result|panicked'
DENIALS=$(dev su root logcat -d -b all -T "'$T0'" 2>/dev/null | grep 'avc:  denied' | grep -v adbd)

FAIL=0
[ "$(dev cat $DEV/rc)" = 0 ] || { echo "FAIL: the test failed"; FAIL=1; }
[ "$SVC_LABEL" = u:r:su:s0 ] || { echo "FAIL: the service did not run in su"; FAIL=1; }
if [ -n "$DENIALS" ]; then
    echo "FAIL: AVC denials during the run:"; echo "$DENIALS" | tail -5 | sed 's/^/    /'
    FAIL=1
fi
[ "$FAIL" = 0 ] && echo "PASS: a ring over RPC crossed shell <-> su under enforcing"
exit "$FAIL"
