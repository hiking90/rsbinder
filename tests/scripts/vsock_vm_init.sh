#!/usr/bin/ash
# The guest's first process for run_vsock_vm.sh, from the initramfs: mounts the host's root
# read-only over 9p and hands over to vsock_vm_guest.sh in the directory rsbvm_dir names.
PATH=/usr/bin:/bin:/sbin

mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev

for m in 9pnet_virtio 9p; do
    modprobe "$m" || echo "GUEST: modprobe $m failed"
done
mkdir -p /new_root
if ! mount -t 9p -o trans=virtio,version=9p2000.L,ro,msize=1048576,cache=loose hostroot /new_root; then
    echo "GUEST-EXIT=125"
    echo o >/proc/sysrq-trigger
    sleep 10
fi

mount --move /proc /new_root/proc
mount --move /sys /new_root/sys
mount --move /dev /new_root/dev
mount -t tmpfs tmp /new_root/tmp
mount -t tmpfs run /new_root/run
exec switch_root /new_root /usr/bin/bash "$rsbvm_dir/vsock_vm_guest.sh"
