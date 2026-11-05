#!/usr/bin/bash
# Runs inside the guest of run_vsock_vm.sh, as PID 1 on the host's read-only root. The kernel
# command line names one scenario (rsbvm_scenario) and what it runs, through init's environment.
# GUEST-EXIT=<status> on the console is what the host reads, since the guest shares no writable
# directory with it.
finish() {
    echo "GUEST-EXIT=$1"
    sync
    echo o >/proc/sysrq-trigger
    sleep 10
}

for m in vsock vmw_vsock_virtio_transport; do
    modprobe "$m" || echo "GUEST: modprobe $m failed"
done
[ -e /dev/vsock ] || finish 124

cd "$rsbvm_repo" || finish 123
export RUST_BACKTRACE=1

case "$rsbvm_scenario" in
loopback)
    # Every #[ignore]d vsock test of the crate; VMADDR_CID_LOCAL takes vsock_loopback.
    modprobe vsock_loopback || finish 122
    "$rsbvm_vsock_test" --ignored || finish $?
    "$rsbvm_tls_test" --ignored --exact vsock_tls_loopback_e2e || finish $?
    finish 0
    ;;
role)
    "$rsbvm_test" --ignored --exact --nocapture "$rsbvm_role"
    finish $?
    ;;
*)
    echo "GUEST: unknown scenario '$rsbvm_scenario'"
    finish 121
    ;;
esac
