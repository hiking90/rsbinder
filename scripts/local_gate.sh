#!/usr/bin/env bash
# Run, before opening a PR, what CI runs and what it cannot.
#
#   hermetic  what .github/workflows/build.yml runs on every PR: fmt, clippy,
#             rustdoc, MSRV, every hermetic test target, big-endian through
#             `cross`, the public API goldens, cargo-ndk builds.
#   kernel    what integration-test.yml's Linux job runs against a live
#             binderfs, `rsb_hub` and `test_service`, plus the gates no
#             workflow runs: tests/scripts/run_*_ac.sh, run_d8b_register.sh,
#             vsock loopback and the libfmq host peer.
#   stage3    the interop scripts against a booted Android device or
#             emulator (real servicemanager, libbinder, libfmq): every
#             script the device can take, when the diff against --base
#             touches code a device run exercises.
#
# A tier or step whose environment is missing is reported SKIP with what it
# needs, never silently dropped. Exit status is 1 when any step failed.
# Logs: target/local-gate/<timestamp>/ (one file per step).
#
# Usage: scripts/local_gate.sh [options] [hermetic] [kernel] [stage3]
#   (no tier: all three)
#   -s SERIAL        device for stage3 (default $ANDROID_SERIAL, else the
#                    only attached device)
#   --base REF       diff base that decides whether stage3 runs
#                    (default origin/master)
#   --stage3-all     run stage3 regardless of the diff
#   --only A,B       run only these stage3 scripts (name without run_/.sh)
#   --iterations N   passes of the kernel-tier `tests` loop (default 1)
#
# Keep this in step with the workflows: a step added to build.yml or to
# integration-test.yml's Linux job belongs in the matching tier here.
set -u

cd "$(dirname "$0")/.." || exit 1

TIERS=()
SERIAL=${ANDROID_SERIAL:-}
BASE=origin/master
STAGE3_ALL=0
ONLY=
ITERATIONS=1
while [ $# -gt 0 ]; do
    case "$1" in
        hermetic|kernel|stage3) TIERS+=("$1") ;;
        -s) SERIAL=$2; shift ;;
        --base) BASE=$2; shift ;;
        --stage3-all) STAGE3_ALL=1 ;;
        --only) ONLY=$2; shift ;;
        --iterations) ITERATIONS=$2; shift ;;
        -h|--help) sed -n '2,34p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done
[ ${#TIERS[@]} -gt 0 ] || TIERS=(hermetic kernel stage3)

LOG_DIR=${LOG_DIR:-target/local-gate/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$LOG_DIR"
SUMMARY=()
FAILED=0
STEP=0

section() { printf '\n== %s\n' "$*"; }

record() { # status name seconds detail
    printf '  %-4s %5ss  %s%s\n' "$1" "$3" "$2" "${4:+  ($4)}"
    SUMMARY+=("$(printf '%-4s  %s%s' "$1" "$2" "${4:+  ($4)}")")
    [ "$1" = FAIL ] && FAILED=1
    return 0
}

# Called as $(step_log) — a subshell — so the caller advances STEP.
step_log() {
    printf '%s/%03d-%s.log' "$LOG_DIR" "$STEP" "$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')"
}

# run NAME CMD...: one step in a subshell, its output in its own log; PASS
# when CMD exits 0. `set -e` is inert here (the step is an `if` condition),
# so a multi-command step chains with `&&`.
run() {
    local name=$1; shift
    local log t0=$SECONDS
    STEP=$((STEP + 1))
    log=$(step_log "$name")
    if ( "$@" ) >"$log" 2>&1; then
        record PASS "$name" $((SECONDS - t0))
    else
        record FAIL "$name" $((SECONDS - t0)) "$log"
        return 1
    fi
}

# run_here NAME CMD...: as `run`, in this shell, for steps that set state (PIDs).
run_here() {
    local name=$1; shift
    local log t0=$SECONDS
    STEP=$((STEP + 1))
    log=$(step_log "$name")
    if "$@" >"$log" 2>&1; then
        record PASS "$name" $((SECONDS - t0))
    else
        record FAIL "$name" $((SECONDS - t0)) "$log"
        return 1
    fi
}

skip() { record SKIP "$1" 0 "$2"; }

# libtest exits 0 when a filter or a `cfg` leaves nothing to run, so a test
# command also has to report at least one passing test.
tpass() {
    local out rc
    out=$("$@" 2>&1); rc=$?
    printf '%s\n' "$out"
    [ $rc -eq 0 ] && grep -qE 'test result: ok\. [1-9][0-9]* passed' <<<"$out"
}

have() { command -v "$1" >/dev/null 2>&1; }
toolchain() { rustup toolchain list 2>/dev/null | grep -q "^$1"; }

# ---------------------------------------------------------------- hermetic

rpc_build_matrix() {
    cargo build -p rsbinder &&
        cargo build -p rsbinder --features rpc &&
        cargo build -p rsbinder --features rpc-tcp-debug &&
        cargo build -p rsbinder --features rpc-tls &&
        cargo build -p rsbinder --features rpc-vsock
}

rpc_suite_soak() {
    local i
    for i in 1 2 3 4 5; do
        cargo test -p rsbinder --features rpc --test rpc_server --test rpc_e2e || return 1
    done
}

big_endian() {
    local s390=(cross test -p rsbinder --target s390x-unknown-linux-gnu)
    tpass cargo test -p rsbinder --no-default-features --lib parcel -- --skip shared_memory --skip fmq:: &&
        tpass cargo test -p rsbinder --no-default-features --lib blob:: &&
        "${s390[@]}" --no-default-features --lib parcel --no-run &&
        file target/s390x-unknown-linux-gnu/debug/deps/rsbinder-* | grep 'MSB.*IBM S/390' &&
        tpass "${s390[@]}" --no-default-features --lib parcel -- --skip shared_memory --skip fmq:: &&
        tpass "${s390[@]}" --features rpc,rpc-tcp-debug --lib file_descriptor:: &&
        tpass "${s390[@]}" --features rpc,rpc-tcp-debug --lib parcel:: -- --skip shared_memory &&
        tpass "${s390[@]}" --features rpc,rpc-tcp-debug --lib rpc::
}

public_api() {
    local out=$LOG_DIR/public-api
    cargo +nightly-2026-05-25 public-api -p rsbinder --simplified > "$out.txt" &&
        diff -u api/rsbinder.txt "$out.txt" &&
        cargo +nightly-2026-05-25 public-api -p rsbinder --simplified --features rpc > "$out-rpc.txt" &&
        diff -u api/rsbinder-rpc.txt "$out-rpc.txt"
}

semver() {
    cargo semver-checks check-release -p rsbinder -p rsbinder-aidl --default-features &&
        cargo semver-checks check-release -p rsbinder --only-explicit-features \
            --features rpc,rpc-tcp-debug,rpc-tls,rpc-vsock,async
}

tier_hermetic() {
    section "hermetic — .github/workflows/build.yml"
    run "fmt" cargo fmt --all -- --check
    run "clippy (workspace, all features)" cargo clippy --all-targets --all-features -- -D warnings
    run "rustdoc (all features)" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    run "rustdoc (default features)" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    if toolchain 1.85; then
        run "MSRV 1.85 check (all features)" env RUSTFLAGS= cargo +1.85 check --workspace --all-features
    else
        skip "MSRV 1.85 check" "rustup toolchain install 1.85"
    fi
    run "test builds (workspace)" cargo test --workspace --no-run
    run "rsbinder-aidl" tpass cargo test -p rsbinder-aidl
    run "tests: async_runtime" tpass cargo test -p tests --test async_runtime
    run "tests: fmq_parcel" tpass cargo test -p tests --test fmq_parcel
    run "tests: c_stream_header" tpass cargo test -p tests --test c_stream_header
    run "rsbinder-fmq" tpass cargo test -p rsbinder-fmq
    run "rsbinder-tools" tpass cargo test -p rsbinder-tools

    run "rpc build matrix" rpc_build_matrix
    run "rsbinder lib (no binder device)" bash scripts/ci_rsbinder_lib_tests.sh
    run "rsbinder doctests" tpass cargo test -p rsbinder --features rpc --doc
    run "rsbinder rpc targets" tpass cargo test -p rsbinder --features rpc --test rpc_e2e \
        --test rpc_scenarios --test rpc_server --test rpc_fd --test rpc_shared_memory \
        --test rpc_cross_stack --test rpc_transport_conformance --test source_invariants \
        --test blob_rpc --test pipe_rpc --test parcel_alloc
    run "rsbinder tls targets" tpass cargo test -p rsbinder --features rpc-tls,rpc-tcp-debug \
        --test rpc_tls --test rpc_transport_conformance
    # Skips (and still passes) where unprivileged user namespaces are off; CI sets REQUIRED.
    run "rpc_link_break (network namespace)" tpass cargo test -p rsbinder \
        --features rpc-tls,rpc-tcp-debug --test rpc_link_break
    run "rpc_server: tls nested callback" tpass cargo test -p rsbinder --features rpc-tls,rpc-tcp-debug \
        --test rpc_server -- --exact tls_android13plus_nested_callback_e2e
    run "rpc_server: preconnected inet fd" tpass cargo test -p rsbinder --features rpc-tls,rpc-tcp-debug \
        --test rpc_server -- --exact preconnected_inet_fd_handshakes_over_tcp_debug
    run "rpc_accessor (android_16)" tpass cargo test -p rsbinder --features rpc,android_16 --test rpc_accessor
    run "tests: rpc targets" tpass cargo test -p tests --features rpc --test rpc_generated_stub \
        --test entry_rpc --test gateway_rpc --test codegen_shapes --test rpc_async \
        --test rpc_calling_identity --test rpc_enforce_permission_deny --test work_source_rpc \
        --test observer_rpc --test transaction_names --test transport_caps --test cancel_rpc \
        --test stream_rpc
    run "tests: rpc_generated_stub (tcp-debug)" tpass cargo test -p tests --features rpc-tcp-debug --test rpc_generated_stub
    run "tests: aidl_spans" tpass cargo test -p tests --features rpc,tracing --test aidl_spans
    run "rsbinder-macros" tpass cargo test -p rsbinder-macros
    run "macro_interface" tpass cargo test -p rsbinder --features macros,rpc --test macro_interface
    run "tests: macro_cross" tpass cargo test -p tests --features rpc,macros --test macro_cross
    run "tests: data_serde" tpass cargo test -p tests --features rpc,macros --test data_serde
    run "serde_demo" cargo run -p example-hello --features rpc --bin serde_demo
    run "rpc soak (5x rpc_server + rpc_e2e)" rpc_suite_soak

    if have cross && docker info >/dev/null 2>&1; then
        run "big-endian wire (s390x via cross)" big_endian
    else
        skip "big-endian wire (s390x)" "needs cross (cargo install cross) and a running docker"
    fi
    if toolchain nightly-2026-05-25 && have cargo-public-api; then
        run "public API goldens" public_api
    else
        skip "public API goldens" "rustup toolchain install nightly-2026-05-25; cargo install cargo-public-api --version 0.52.0"
    fi
    if have cargo-semver-checks; then
        run "semver checks" semver
    else
        skip "semver checks" "cargo install cargo-semver-checks (CI runs it on the PR)"
    fi
    if have cargo-audit; then
        run "cargo audit" cargo audit
    else
        skip "cargo audit" "cargo install cargo-audit"
    fi
    if have cargo-ndk && [ -n "${ANDROID_NDK_HOME:-}" ]; then
        local t
        for t in x86_64-linux-android aarch64-linux-android; do
            run "android build $t" cargo ndk -t "$t" build --release
            run "android test build $t" cargo ndk -t "$t" test --no-run
        done
    else
        skip "android builds" "needs cargo-ndk and ANDROID_NDK_HOME"
    fi
}

# ------------------------------------------------------------------ kernel

HUB_PID=
SVC_PID=
TESTS_SKIP=(--skip test_vintf_parcelable_holder_cannot_contain_unstable_parcelable
            --skip test_vintf_parcelable_holder_cannot_contain_not_vintf_parcelable
            --skip test_versioned_unknown_union_field_triggers_error)

stop_service() {
    [ -n "$SVC_PID" ] && kill "$SVC_PID" 2>/dev/null && wait "$SVC_PID" 2>/dev/null
    SVC_PID=
}

stop_hub() {
    [ -n "$HUB_PID" ] && kill "$HUB_PID" 2>/dev/null && wait "$HUB_PID" 2>/dev/null
    HUB_PID=
}

start_hub() {
    RUST_LOG=info target/debug/rsb_hub --config tests/policy/permissive.toml \
        > "$LOG_DIR/rsb_hub.log" 2>&1 &
    HUB_PID=$!
    sleep 1
    kill -0 "$HUB_PID" 2>/dev/null
}

# start_service BIN: (re)start the shared test service and wait for its marker.
start_service() {
    stop_service
    RUST_LOG=info "target/debug/$1" > "$LOG_DIR/$1.log" 2>&1 &
    SVC_PID=$!
    echo "$SVC_PID" > "$LOG_DIR/test_service.pid"
    bash scripts/ci_wait_test_service_ready.sh "$LOG_DIR/$1.log" "$LOG_DIR/test_service.pid"
}

# The issue #97 signature integration-test.yml checks after its run.
dmesg_since() {
    local new
    new=$(dmesg 2>/dev/null | tail -n +"$(($1 + 1))")
    ! grep -E 'BC_FREE_BUFFER|binder_alloc.*no match' <<<"$new"
}

tier_kernel() {
    section "kernel — integration-test.yml Linux job + host-only gates"
    if [ "$(uname -s)" != Linux ] || [ ! -w /dev/binderfs/binder ]; then
        skip "kernel tier" "needs a writable /dev/binderfs/binder: sudo target/debug/rsb_device binder"
        return
    fi
    local running
    running=$(pgrep -x rsb_hub | tr '\n' ' ')
    if [ -n "$running" ]; then
        record FAIL "kernel tier" 0 "rsb_hub already running (pid $running); stop it — this tier and the AC scripts start their own"
        return
    fi
    run "build workspace (bins + tests)" cargo build --workspace --bins --tests || return

    local dmesg0=
    dmesg >/dev/null 2>&1 && dmesg0=$(dmesg | wc -l)

    if ! run_here "start rsb_hub + test_service" eval 'start_hub && start_service test_service'; then
        stop_service; stop_hub
        return
    fi
    run "rsbinder (unit + doc + default targets)" tpass cargo test -p rsbinder
    run "rsbinder parcel_alloc (ignored)" tpass cargo test -p rsbinder --test parcel_alloc -- --ignored
    local i
    for ((i = 1; i <= ITERATIONS; i++)); do
        run "tests (sync) $i/$ITERATIONS" tpass cargo test -p tests -- "${TESTS_SKIP[@]}"
    done
    run_here "restart as test_service_async" start_service test_service_async
    run "tests (async)" tpass cargo test -p tests -- "${TESTS_SKIP[@]}"
    run "mesh_rpc" tpass cargo test -p tests --features rpc --test mesh_rpc
    run "mesh_kernel" tpass cargo test -p tests --features rpc --test mesh_kernel -- --ignored
    run "gateway_kernel" tpass cargo test -p tests --features rpc --test gateway_kernel -- --ignored
    run "async_wait + entry_kernel_options" tpass cargo test -p tests --features rpc \
        --test async_wait --test entry_kernel_options -- --ignored --test-threads=1
    run "fmq_binder" tpass cargo test -p tests --test fmq_binder -- --ignored
    local t
    for t in test_death_recipient test_wibinder_upgrade_after_obituary \
        test_death_recipient_panic_does_not_starve_others \
        test_unlink_to_death_single_remove_via_obituary \
        test_dump_fast_fails_on_dead_proxy_closes_fd \
        test_weak_upgrade_after_sole_strong_drop_is_dead_then_reresolve_works; do
        # Each consumes the service or must be its only strong holder: a fresh one each.
        run_here "restart test_service for $t" start_service test_service &&
            run "$t" tpass cargo test -p tests -- --ignored --exact "test_client::$t"
    done
    stop_service; stop_hub

    local s
    for s in tests/scripts/run_*_ac.sh; do
        run "$(basename "$s" .sh)" bash "$s"
    done
    run "run_d8b_register" bash example-hello/cpp/run_d8b_register.sh

    if [ -e /dev/vsock ] && grep -q '^vsock_loopback ' /proc/modules 2>/dev/null; then
        run "rpc_vsock (loopback)" tpass cargo test -p rsbinder --features rpc-vsock --test rpc_vsock -- --ignored
        run "rpc_tls over vsock" tpass cargo test -p rsbinder --features rpc-tls,rpc-vsock \
            --test rpc_tls -- --ignored --exact vsock_tls_loopback_e2e
    else
        skip "vsock tests" "sudo modprobe vsock_loopback"
    fi

    local aosp=${AOSP:-$HOME/Workspace/aosp}
    if [ -d "$aosp/system/libfmq" ]; then
        run "libfmq host peer (build)" env AOSP="$aosp" bash rsbinder-fmq/tests/cpp/build_host_peer.sh &&
            run "libfmq_host" tpass env RSBINDER_FMQ_LIBFMQ_PEER="$PWD/target/libfmq-host/libfmq_peer" \
                cargo test -p rsbinder-fmq --test libfmq_host -- --ignored
    else
        skip "libfmq_host" "needs the AOSP checkout at \$AOSP (CLAUDE.md, system/libfmq)"
    fi

    if [ -n "$dmesg0" ]; then
        run "dmesg: no binder buffer errors" dmesg_since "$dmesg0"
    else
        skip "dmesg check" "dmesg is not readable (kernel.dmesg_restrict=1)"
    fi
}

# ------------------------------------------------------------------ stage3

# name | min SDK | max SDK (- = none) | needs
#   needs: aosp = headers from $AOSP; enforcing = SELinux enforcing at start
STAGE3=(
    "blob_interop|29|-|"
    "cancel_interop|29|-|"
    "codegen_shapes_interop|29|-|"
    "calling_identity_interop|36|36|enforcing"
    "enforce_permission_interop|36|36|enforcing"
    "fmq_interop|33|-|aosp"
    "imemory_interop|29|-|aosp"
    "lazy_service_stage3|30|-|"
    "mmap_size_interop|29|-|"
    "pipe_interop|29|-|"
    "rpc_accessor_interop|36|36|"
    "rpc_accessor_register_interop|36|36|"
    "rpc_fd_interop|34|-|"
    "rpc_incoming_interop|36|-|"
    "rpc_multiconn_interop|36|-|"
    "stream_interop|33|-|aosp"
    "stream_rpc_interop|34|-|aosp"
    "stream_ac|33|-|"
    "typed_error_interop|29|-|"
    "update_txn_interop|31|-|"
    "work_source_interop|29|-|"
)
STAGE3_MANUAL=(
    "a15_qpr_stage3|an arm64 Android 15 AVD and servicemanager extracted from a QPR2 GSI (see its header)"
    "phasec_vintf|remounts /system on an Android 16 AVD booted with -writable-system"
    "rt_inherit_interop|runs on the REMOTE_LINUX host over ssh"
)

DEV_ENFORCE=
DEV_UID=

adb_() { adb -s "$SERIAL" "$@"; }
getprop() { adb_ shell getprop "$1" 2>/dev/null | tr -d '\r'; }

# Put SELinux back to the mode the device started in; scripts leave it permissive.
restore_enforce() {
    local now
    now=$(adb_ shell getenforce 2>/dev/null | tr -d '\r')
    [ "$now" = "$DEV_ENFORCE" ] && return 0
    adb_ root >/dev/null 2>&1; adb_ wait-for-device
    adb_ shell setenforce "$([ "$DEV_ENFORCE" = Enforcing ] && echo 1 || echo 0)" >/dev/null 2>&1
}

stage3_restore() {
    [ -n "$DEV_ENFORCE" ] || return 0
    restore_enforce
    [ "$DEV_UID" = 0 ] || { adb_ unroot >/dev/null 2>&1; adb_ wait-for-device; }
}

stage3_selected() {
    [ -n "$ONLY" ] && return 0
    [ "$STAGE3_ALL" = 1 ] && return 0
    local changed
    changed=$({ git diff --name-only "$BASE"...HEAD 2>/dev/null
                git diff --name-only HEAD
                git ls-files --others --exclude-standard; } |
              grep -E '^(rsbinder|rsbinder-aidl|rsbinder-fmq|rsbinder-macros|contrib|example-hello|tests)/' |
              grep -vE '\.md$')
    [ -n "$changed" ]
}

stage3_script() {
    case "$1" in
        stream_ac) echo tests/scripts/run_stream_ac.sh ;;
        *) echo "example-hello/cpp/run_$1.sh" ;;
    esac
}

tier_stage3() {
    section "stage3 — interop against a real Android device"
    local m
    for m in "${STAGE3_MANUAL[@]}"; do
        skip "run_${m%%|*}" "manual: ${m#*|}"
    done
    if ! stage3_selected; then
        skip "stage3 tier" "no change against $BASE under a crate a device run exercises (--stage3-all to force)"
        return
    fi
    if ! have adb; then
        skip "stage3 tier" "adb not on PATH"
        return
    fi
    if [ -z "$SERIAL" ]; then
        local devices
        devices=$(adb devices | awk 'NR > 1 && $2 == "device" { print $1 }')
        if [ "$(printf '%s' "$devices" | grep -c .)" -ne 1 ]; then
            skip "stage3 tier" "needs exactly one device, or -s SERIAL (attached: ${devices:-none}); e.g. emulator -avd rsbinder-a16 -no-window"
            return
        fi
        SERIAL=$devices
    fi
    if [ "$(adb_ get-state 2>/dev/null)" != device ]; then
        skip "stage3 tier" "device $SERIAL is not online"
        return
    fi
    if [ -z "${ANDROID_NDK_HOME:-}" ] || ! have cargo-ndk; then
        skip "stage3 tier" "needs cargo-ndk and ANDROID_NDK_HOME"
        return
    fi
    local sdk abi triple build_type aosp=${AOSP:-$HOME/Workspace/aosp}
    sdk=$(getprop ro.build.version.sdk)
    abi=$(getprop ro.product.cpu.abi)
    build_type=$(getprop ro.build.type)
    case "$abi" in
        x86_64) triple=x86_64-linux-android ;;
        arm64-v8a) triple=aarch64-linux-android ;;
        *) skip "stage3 tier" "unsupported ABI $abi"; return ;;
    esac
    if [ "$build_type" = user ]; then
        skip "stage3 tier" "$SERIAL is a user build; the scripts need adb root (google_apis or AOSP default image)"
        return
    fi
    DEV_ENFORCE=$(adb_ shell getenforce 2>/dev/null | tr -d '\r')
    DEV_UID=$(adb_ shell id -u 2>/dev/null | tr -d '\r')
    printf '  device %s: SDK %s, %s, %s, SELinux %s\n' "$SERIAL" "$sdk" "$abi" "$build_type" "$DEV_ENFORCE"
    export ANDROID_NDK_HOME AOSP="$aosp"

    local entry name min max needs
    local -a queue=()
    for entry in "${STAGE3[@]}"; do
        IFS='|' read -r name min max needs <<<"$entry"
        if [ -n "$ONLY" ] && ! grep -qx "$name" <<<"${ONLY//,/$'\n'}"; then
            continue
        fi
        if [ "$sdk" -lt "$min" ] || { [ "$max" != - ] && [ "$sdk" -gt "$max" ]; }; then
            local want="SDK $min+"
            [ "$max" = "$min" ] && want="SDK $min"
            skip "run_$name" "device SDK $sdk, needs $want"
            continue
        fi
        if [ "$needs" = aosp ] && [ ! -d "$aosp/frameworks/native" ]; then
            skip "run_$name" "needs the AOSP checkout at \$AOSP"
            continue
        fi
        if [ "$needs" = enforcing ] && [ "$DEV_ENFORCE" != Enforcing ]; then
            skip "run_$name" "needs SELinux enforcing at start; device is $DEV_ENFORCE"
            continue
        fi
        queue+=("$name")
    done

    if grep -qx lazy_service_stage3 <<<"$(printf '%s\n' "${queue[@]}")"; then
        run "cargo ndk build example-hello ($triple)" cargo ndk -t "$triple" build -p example-hello --bins
    fi
    if grep -qx stream_ac <<<"$(printf '%s\n' "${queue[@]}")"; then
        run "cargo ndk build stream_probe ($triple)" cargo ndk -t "$triple" -p "$((sdk < 35 ? sdk : 35))" \
            build -p tests --bin stream_probe
    fi

    trap 'stage3_restore; stop_service; stop_hub' EXIT
    for name in "${queue[@]}"; do
        restore_enforce
        local script
        script=$(stage3_script "$name")
        case "$name" in
            lazy_service_stage3) run "run_$name" timeout 1800 bash "$script" "$SERIAL" ;;
            stream_ac) run "run_stream_ac --adb" timeout 1800 bash "$script" --adb -s "$SERIAL" --target "$triple" ;;
            *) run "run_$name" timeout 1800 bash "$script" -s "$SERIAL" ;;
        esac
    done
    stage3_restore
}

# -------------------------------------------------------------------- main

trap 'stop_service; stop_hub' EXIT
trap 'exit 130' INT TERM

printf 'logs: %s\n' "$LOG_DIR"
for tier in "${TIERS[@]}"; do
    "tier_$tier"
done

section "summary"
printf '  %s\n' "${SUMMARY[@]}"
printf '\nlogs: %s\n' "$LOG_DIR"
exit "$FAILED"
