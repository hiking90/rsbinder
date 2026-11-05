#!/usr/bin/env bash
# vsock between this host and a real VM, which VMADDR_CID_LOCAL cannot show: both cids, the
# virtqueue path, and what a session hears when the VM is killed. The test bodies are in
# rsbinder/tests/rpc_vsock_vm.rs; this script says which side runs where.
#
#   ./tests/scripts/run_vsock_vm.sh
#   ./tests/scripts/run_vsock_vm.sh --check    # exit 77 and say what is missing, or exit 0
#
# Not named run_*_ac.sh: the gate's kernel tier runs those by glob, without asking --check first.
#
# The guest boots this host's kernel with this host's root mounted read-only over 9p, so a test
# binary runs there as built here. Needs qemu-system-x86_64, /dev/kvm and /dev/vhost-vsock
# (opening it loads vhost_vsock), mkinitcpio (Arch), and a /boot/vmlinuz-* of the running kernel,
# or VSOCK_VM_KERNEL. No root and no host module of its own: the loopback half loads
# vsock_loopback in the guest.
set -u

cd "$(dirname "$0")/../.." || exit 1
REPO=$PWD
WORK=$REPO/target/vsock-vm
KVER=$(uname -r)
GUEST_CID=${VSOCK_VM_CID:-4242}
HOST_CID=2
PORT=21248
PASS=0; FAIL=0
QEMU_PID=
HOST_PID=

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
need() { echo "$*"; exit 77; }

kernel=${VSOCK_VM_KERNEL:-}
if [ -z "$kernel" ]; then
    for k in /boot/vmlinuz-*; do
        if [ -r "$k" ] && file -L "$k" | grep -qF "version $KVER"; then kernel=$k; fi
    done
fi
[ -n "$kernel" ] || need "no readable /boot/vmlinuz-* of $KVER; set VSOCK_VM_KERNEL"
command -v qemu-system-x86_64 >/dev/null || need "qemu-system-x86_64"
command -v mkinitcpio >/dev/null || need "mkinitcpio"
[ -w /dev/kvm ] || need "write access to /dev/kvm"
[ -w /dev/vhost-vsock ] || need "write access to /dev/vhost-vsock"
[ "${1:-}" = --check ] && exit 0

cleanup() {
    [ -n "$QEMU_PID" ] && kill -KILL "$QEMU_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && kill -KILL "$HOST_PID" 2>/dev/null
}
trap cleanup EXIT

# The path cargo prints for the one test target of a build.
test_binary() {
    local out
    out=$(cargo test -p rsbinder "$@" --no-run 2>&1) || { printf '%s\n' "$out" >&2; return 1; }
    printf '%s\n' "$out" | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -n 1
}
note "build"
VSOCK_TEST=$REPO/$(test_binary --features rpc-vsock --test rpc_vsock) || exit 1
TLS_TEST=$REPO/$(test_binary --features rpc-tls,rpc-vsock --test rpc_tls) || exit 1
VM_TEST=$REPO/$(test_binary --features rpc-tls,rpc-vsock --test rpc_vsock_vm) || exit 1

mkdir -p "$WORK"
cat >"$WORK/mkinitcpio.conf" <<EOF
MODULES=(9pnet_virtio 9p)
BINARIES=()
FILES=($REPO/tests/scripts/vsock_vm_init.sh)
HOOKS=(base)
COMPRESSION=cat
EOF
mkinitcpio -c "$WORK/mkinitcpio.conf" -k "$KVER" -g "$WORK/initramfs.img" >"$WORK/mkinitcpio.log" 2>&1 ||
    { cat "$WORK/mkinitcpio.log"; exit 1; }
echo "kernel $kernel, guest cid $GUEST_CID"

# Boots a guest in the background: boot_vm LOG KERNEL_ARGS...; the pid lands in QEMU_PID.
boot_vm() {
    local log=$1; shift
    rm -f "$WORK/qemu.pid"
    timeout -s KILL 180 qemu-system-x86_64 -enable-kvm -cpu host -m 2G -smp 4 \
        -nographic -monitor none -no-reboot -pidfile "$WORK/qemu.pid" \
        -kernel "$kernel" -initrd "$WORK/initramfs.img" \
        -virtfs local,path=/,mount_tag=hostroot,security_model=none,readonly=on \
        -device "vhost-vsock-pci,guest-cid=$GUEST_CID" \
        -append "console=ttyS0 quiet panic=-1 rdinit=$REPO/tests/scripts/vsock_vm_init.sh rsbvm_dir=$REPO/tests/scripts rsbvm_repo=$REPO rsbvm_port=$PORT rsbvm_peer_cid=$HOST_CID $*" \
        </dev/null >"$log" 2>&1 &
    QEMU_PID=$!
}
boot_role() { boot_vm "$1" rsbvm_scenario=role "rsbvm_test=$VM_TEST" "rsbvm_role=$2"; }

# The status the guest reported on its console; empty if it reported none. `--exact` on a name
# that matches nothing exits 0, so a status of 0 counts only with a test that ran.
guest_exit() {
    local line rc
    line=$(tr -d '\r' <"$1" | grep -oE 'GUEST-EXIT=[0-9]+' | tail -n 1)
    rc=${line#GUEST-EXIT=}
    if [ "$rc" = 0 ] && ! grep -qE ' [1-9][0-9]* passed' "$1"; then rc=no-test; fi
    echo "$rc"
}
# The host's half of the same rule, for a role that returned 0.
host_ran() { grep -qE ' [1-9][0-9]* passed' "$1"; }

host_role() {
    local role=$1; shift
    env rsbvm_peer_cid="$GUEST_CID" rsbvm_port="$PORT" rsbvm_qemu_pidfile="$WORK/qemu.pid" \
        RUST_BACKTRACE=1 timeout -s KILL 120 "$VM_TEST" --ignored --exact --nocapture "$role"
}

# The pid file exists only once qemu has got past its setup; killing before that kills nothing.
await_pidfile() {
    local i
    for i in $(seq 1 100); do
        [ -s "$WORK/qemu.pid" ] && return 0
        sleep 0.1
    done
    return 1
}

# Guest runs `$2`, host runs `$3`, both must end well: NAME GUEST_ROLE HOST_ROLE
guest_serves() {
    local name=$1 grole=$2 hrole=$3 log=$WORK/$1
    note "$name"
    boot_role "$log.guest.log" "$grole"
    if host_role "$hrole" >"$log.host.log" 2>&1; then hrc=0; else hrc=$?; fi
    wait "$QEMU_PID"; QEMU_PID=
    local grc; grc=$(guest_exit "$log.guest.log")
    if [ "$hrc" = 0 ] && host_ran "$log.host.log" && [ "$grc" = 0 ]; then
        ok "$name"
    else
        bad "$name (host rc $hrc, guest exit '${grc}'; $log.*.log)"
    fi
}

# Host runs `$3` first, then the guest `$2`; the host's status must be HOST_RC (default 0):
# NAME GUEST_ROLE HOST_ROLE [HOST_RC]
host_serves() {
    local name=$1 grole=$2 hrole=$3 want_hrc=${4:-0} log=$WORK/$1
    note "$name"
    host_role "$hrole" >"$log.host.log" 2>&1 &
    HOST_PID=$!
    sleep 0.3
    boot_role "$log.guest.log" "$grole"
    wait "$QEMU_PID"; QEMU_PID=
    local hrc=0; wait "$HOST_PID" || hrc=$?; HOST_PID=
    local grc; grc=$(guest_exit "$log.guest.log")
    if [ "$hrc" = "$want_hrc" ] && { [ "$hrc" != 0 ] || host_ran "$log.host.log"; } && [ "$grc" = 0 ]; then
        ok "$name"
    else
        bad "$name (host rc $hrc, guest exit '${grc}'; $log.*.log)"
    fi
}

# Guest serves; the host test kills the VM itself: NAME HOST_ROLE
guest_killed() {
    local name=$1 hrole=$1 log=$WORK/$1
    note "$name"
    boot_role "$log.guest.log" serve
    await_pidfile || { bad "$name (no qemu pid file)"; return; }
    if host_role "$hrole" >"$log.host.log" 2>&1 && host_ran "$log.host.log"; then
        ok "$name"
        grep -E 'after the kill|ended with|frames went out' "$log.host.log" | sed 's/^/        /'
    else
        bad "$name ($log.*.log)"
    fi
    kill -KILL "$QEMU_PID" 2>/dev/null
    wait "$QEMU_PID" 2>/dev/null; QEMU_PID=
}

note "guest loopback: every #[ignore]d vsock test the gate runs"
boot_vm "$WORK/loopback.log" rsbvm_scenario=loopback "rsbvm_vsock_test=$VSOCK_TEST" "rsbvm_tls_test=$TLS_TEST"
wait "$QEMU_PID"; QEMU_PID=
rc=$(guest_exit "$WORK/loopback.log")
if [ "$rc" = 0 ]; then ok "loopback in the guest"; else bad "loopback in the guest (exit '$rc'; $WORK/loopback.log)"; fi

guest_serves call_guest_serves serve call_all
host_serves call_host_serves call_all serve
guest_serves tls_guest_serves tls_serve tls_call
host_serves tls_host_serves tls_call tls_serve
guest_serves flood_guest_serves serve flood
host_serves flood_host_serves flood serve

# The serving side's process exits in a handler, on a machine that stays up.
name=peer_exit_guest_serves
note "$name"
boot_role "$WORK/$name.guest.log" serve
if host_role peer_exits_during_call >"$WORK/$name.host.log" 2>&1; then hrc=0; else hrc=$?; fi
wait "$QEMU_PID"; QEMU_PID=
grc=$(guest_exit "$WORK/$name.guest.log")
if [ "$hrc" = 0 ] && host_ran "$WORK/$name.host.log" && [ "$grc" = 42 ]; then
    ok "$name"
    grep 'reply wait ended' "$WORK/$name.host.log" | sed 's/^/        /'
else
    bad "$name (host rc $hrc, guest exit '$grc', want 42; $WORK/$name.*.log)"
fi
host_serves peer_exit_host_serves peer_exits_during_call serve 42

guest_killed guest_killed_during_call
guest_killed guest_killed_idle_session
guest_killed guest_killed_serve_loop
guest_killed guest_killed_incoming_connections
guest_killed guest_killed_while_send_blocks

# A host server outlives a VM its client ran in: the same cid connects again from a new boot, and
# the server still stops, so the dead VM's worker has ended.
name=host_server_outlives_guest
note "$name"
host_role serve >"$WORK/$name.host.log" 2>&1 &
HOST_PID=$!
sleep 0.3
boot_role "$WORK/$name.guest1.log" request_kill
wait "$QEMU_PID" 2>/dev/null; rc1=$?; QEMU_PID=
boot_role "$WORK/$name.guest2.log" call_all
wait "$QEMU_PID"; QEMU_PID=
hrc=0; wait "$HOST_PID" || hrc=$?; HOST_PID=
grc=$(guest_exit "$WORK/$name.guest2.log")
g1=$(guest_exit "$WORK/$name.guest1.log")
if [ -z "$g1" ] && [ "$grc" = 0 ] && [ "$hrc" = 0 ] && host_ran "$WORK/$name.host.log"; then
    ok "$name"
else
    bad "$name (first qemu rc $rc1, first guest exit '$g1', second '$grc', host rc $hrc; $WORK/$name.*.log)"
fi

note "summary"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
