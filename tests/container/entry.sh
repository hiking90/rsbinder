#!/bin/sh
# Inside the container of run.sh: prepare what the run asks for, then run COMMAND and end with its
# exit code. A failure in the preparing is 125, which run.sh documents as a setup failure.
set -u

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
setup_failed() {
    echo "run.sh: setup failed: $*" >&2
    exit 125
}

# Names for the test uids: a hub looks groups up by name through NSS.
for uid in 1000 1001 1002 1003 "$RSB_CONTAINER_USER"; do
    [ "$uid" = 0 ] && continue
    grep -q "^[^:]*:[^:]*:$uid:" /etc/passwd && continue
    echo "rsbtest$uid:x:$uid:$uid::/tmp:/bin/sh" >>/etc/passwd
    echo "rsbtest$uid:x:$uid:" >>/etc/group
done

if [ "$RSB_CONTAINER_BINDER" = 1 ]; then
    device=$ROOT/target/debug/rsb_device
    [ -x "$device" ] || setup_failed "no $device: cargo build -p rsbinder-tools first"
    "$device" binder --mode 0666 || setup_failed "rsb_device binder"
    [ -w /dev/binderfs/binder ] || setup_failed "no writable /dev/binderfs/binder"
fi

# No `exec`: PID 1 reaps orphans, and `--init`'s /sbin/docker-init is under the host's /usr here.
if [ "$RSB_CONTAINER_USER" = 0 ]; then
    "$@"
else
    setpriv --reuid="$RSB_CONTAINER_USER" --regid="$RSB_CONTAINER_USER" --clear-groups -- "$@"
fi
exit $?
