#!/usr/bin/env bash
# STAGE3: the Android 15 service-manager split, against real AOSP binaries.
#
# `android-15.0.0_r6` inserted `getService2` at index 1 of `IServiceManager`
# and shifted every transaction code after it, keeping SDK 35. rsbinder
# therefore probes at startup and picks `android_14` or `android_15`. This
# script runs the same client against **both** numberings and requires both
# to work:
#
#   A15-1  the emulator's own servicemanager — an initial Android 15 image
#          (`AE3A.*`), i.e. the pre-r6 numbering.
#   A15-2  a servicemanager extracted from an Android 15 QPR2 GSI, run on a
#          *separate* binder node, i.e. the r6+ numbering.
#
# A15-2 exists because no emulator system image carries the QPR platform:
# API 35 images are all the initial release with SDK extensions on top
# (`ext14`/`ext15` are extension levels, not QPR platforms), and
# `servicemanager`/`libbinder` are platform binaries, not mainline. AOSP's
# `servicemanager` takes the driver path as `argv[1]`
# (`cmds/servicemanager/main.cpp`), so a second one can be run beside the
# platform's without touching it.
#
# The verdict is behavioural, and that is what makes it decisive: a wrong
# pick does not fail at the probe, it fails one method at a time
# (`addService` landing on `checkService` and being refused as
# BAD_PARCELABLE; `getServiceDebugInfo` past the end of the interface). See
# the header of `example-hello/src/bin/a15_qpr_interop.rs`.
#
# Each half also names the numbering it expects (`pre-r6` / `r6+`) and the
# binary fails if the probe picked the other one, so the two halves cannot
# silently test the same peer twice — e.g. a pre-r6 servicemanager
# mis-extracted into `gsi-dir` fails A15-2 up front instead of green-running
# the `android_14` module a second time.
#
#   ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk \
#     cargo ndk -t aarch64-linux-android build -p example-hello \
#       --bin a15_qpr_interop
#   ./example-hello/cpp/run_a15_qpr_stage3.sh [serial] [gsi-dir]
#
# Needs an Android 15 (API 35) userdebug/google_apis AVD: `addService` from
# the shell domain is an SELinux denial otherwise, so the script needs
# `adb root` + `setenforce 0`. It restores the previous mode on exit.
#
# `gsi-dir` (default `plans/android15-qpr-validation`) must hold
# `servicemanager` plus the libraries it links, pulled out of an Android 15
# QPR2 GSI. `plans/` is gitignored, so a fresh checkout has to make it:
#
#   curl -sSL -C - --retry 30 --retry-all-errors -o gsi.zip \
#     https://dl.google.com/developers/android/vic/images/gsi/aosp_arm64-exp-BP11.241210.004-12926906-ac28fd4c.zip
#   unzip -o gsi.zip system.img          # raw ext4
#   # debugfs reads it without mounting (brew install e2fsprogs)
#   for f in bin/servicemanager lib64/lib{base,binder,c++,cutils,log,perfetto_c,selinux,utils,vintf}.so; do
#       debugfs -R "dump /system/$f $(basename $f)" system.img
#   done
#   # Do NOT take libc/libm/libdl — the emulator's own bionic must be used.
#
# If A15-2 is skipped for a missing GSI, the script exits 2: half this gate
# passing is not this gate passing.
set -u

cd "$(dirname "$0")/../.." || exit 1

SERIAL=${1:-}
GSI_DIR=${2:-plans/android15-qpr-validation}
ADB=(adb)
[ -n "$SERIAL" ] && ADB=(adb -s "$SERIAL")

DEV=/data/local/tmp
BIN=target/aarch64-linux-android/debug
NODE=gsi_test
GSI_LIBS="libbase.so libbinder.so libc++.so libcutils.so liblog.so libperfetto_c.so libselinux.so libutils.so libvintf.so"
PASS=0; FAIL=0

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
sh_()  { "${ADB[@]}" shell "$@"; }

[ -x "$BIN/a15_qpr_interop" ] || { echo "missing $BIN/a15_qpr_interop — see the build line in this script's header"; exit 2; }
[ -x "$BIN/rsb_device" ] || { echo "missing $BIN/rsb_device — cargo ndk ... build -p rsbinder-tools --bin rsb_device"; exit 2; }

"${ADB[@]}" root >/dev/null 2>&1
sh_ 'true' >/dev/null 2>&1 || { echo "no device (serial: ${SERIAL:-default})"; exit 2; }

SDK=$(sh_ 'getprop ro.build.version.sdk' | tr -d '\r')
BUILD_ID=$(sh_ 'getprop ro.build.id' | tr -d '\r')
if [ "$SDK" != 35 ]; then
    echo "this gate needs an Android 15 (API 35) AVD; this one is SDK $SDK ($BUILD_ID)"
    exit 2
fi
echo "device: SDK $SDK, build $BUILD_ID"

# `pkill -x` matches the 15-char-truncated comm, and the platform's
# servicemanager has the same comm — so find ours by its cmdline, which is
# `./servicemanager /dev/binderfs/<node>` (argv[0] is relative: the launch
# below `cd`s first, so a path-prefixed pattern matches nothing). The
# bracket keeps the pattern from matching any adb wrapper shell whose own
# command line carries it. Kill by pid, not pkill -f, for the same reason.
gsi_pids() { sh_ "pgrep -f '[s]ervicemanager /dev/binderfs/$NODE'" | tr -d '\r'; }

PREV_ENFORCE=$(sh_ 'getenforce' | tr -d '\r')
cleanup() {
    for pid in $(gsi_pids); do sh_ "kill $pid" >/dev/null 2>&1; done
    [ "$PREV_ENFORCE" = "Enforcing" ] && sh_ 'setenforce 1' >/dev/null 2>&1
    return 0
}
trap cleanup EXIT
sh_ 'setenforce 0' >/dev/null 2>&1

"${ADB[@]}" push "$BIN/a15_qpr_interop" "$DEV/a15_qpr_interop" >/dev/null 2>&1 || { echo "push failed"; exit 2; }
sh_ "chmod 755 $DEV/a15_qpr_interop" >/dev/null 2>&1

######################################################################
note "A15-1  the pre-r6 numbering, against the platform servicemanager"

# `AE3A.*` is the initial Android 15 platform. A QPR image here would make
# this half test the same numbering as A15-2 and say nothing.
case "$BUILD_ID" in
    AE3A.*) ok "the platform image is the initial Android 15 release ($BUILD_ID)" ;;
    *) bad "build $BUILD_ID may not be the pre-r6 platform; A15-1 would not test what it claims" ;;
esac

OUT=$(sh_ "cd $DEV && TMPDIR=$DEV ./a15_qpr_interop '' pre-r6 2>/dev/null; echo rc=\$?")
printf '%s\n' "$OUT" | sed 's/^/    /'
if grep -q '^rc=0' <<<"$OUT"; then
    ok "the whole hub surface round-trips on the pre-r6 numbering"
else
    bad "exit $(grep '^rc=' <<<"$OUT" | tr -d 'rc=\r')"
fi

######################################################################
note "A15-2  the r6+ numbering, against an Android 15 QPR2 servicemanager"

missing=""
for f in servicemanager $GSI_LIBS; do
    [ -f "$GSI_DIR/$f" ] || missing="$missing $f"
done
if [ -n "$missing" ]; then
    bad "$GSI_DIR is missing:$missing — see this script's header for how to extract them"
    printf '\n===== %d passed, %d failed =====\n' "$PASS" "$FAIL"
    exit 2
fi

for pid in $(gsi_pids); do sh_ "kill $pid" >/dev/null 2>&1; done
sh_ "mkdir -p $DEV/gsi" >/dev/null 2>&1
for f in servicemanager $GSI_LIBS; do
    "${ADB[@]}" push "$GSI_DIR/$f" "$DEV/gsi/$f" >/dev/null 2>&1 || { bad "push $f failed"; break; }
done
sh_ "chmod 755 $DEV/gsi/servicemanager" >/dev/null 2>&1

# A node of its own: the GSI servicemanager must become context 0 somewhere,
# and that somewhere may not be the one the platform is running.
if ! sh_ "ls /dev/binderfs/$NODE" >/dev/null 2>&1; then
    "${ADB[@]}" push "$BIN/rsb_device" "$DEV/rsb_device" >/dev/null 2>&1
    sh_ "chmod 755 $DEV/rsb_device && $DEV/rsb_device $NODE -m 0666" >/dev/null 2>&1
fi
if sh_ "ls /dev/binderfs/$NODE" >/dev/null 2>&1; then
    ok "binder node /dev/binderfs/$NODE"
else
    bad "could not create /dev/binderfs/$NODE"
fi

sh_ "cd $DEV/gsi && (LD_LIBRARY_PATH=$DEV/gsi nohup ./servicemanager /dev/binderfs/$NODE > $DEV/gsi_sm.log 2>&1 &)" >/dev/null 2>&1
waited=0
while [ -z "$(gsi_pids)" ] && [ "$waited" -lt 10 ]; do sleep 1; waited=$((waited + 1)); done
if [ -n "$(gsi_pids)" ]; then
    ok "the QPR2 servicemanager is running on /dev/binderfs/$NODE"
else
    bad "it did not start: $(sh_ "cat $DEV/gsi_sm.log" | tail -5)"
fi

# The `r6+` argument is what keeps the halves apart: the binary fails here
# unless the probe picked the r6+ module, so a mis-extracted pre-r6
# servicemanager cannot pass this half on the other numbering (and the
# `pre-r6` argument above covers the mirror case, a QPR platform image).
OUT=$(sh_ "cd $DEV && TMPDIR=$DEV ./a15_qpr_interop /dev/binderfs/$NODE r6+ 2>/dev/null; echo rc=\$?")
printf '%s\n' "$OUT" | sed 's/^/    /'
if grep -q '^rc=0' <<<"$OUT"; then
    ok "the whole hub surface round-trips on the r6+ numbering"
else
    bad "exit $(grep '^rc=' <<<"$OUT" | tr -d 'rc=\r')"
fi

######################################################################
printf '\n===== %d passed, %d failed =====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
