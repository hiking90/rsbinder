#!/usr/bin/env bash
# Run a command in a container of its own: a binder device of its own, several
# uids, or no binder at all (plan 6-7 §8, plan 15-pubsub-uds §4.9).
#
#   tests/container/run.sh [--no-binder] [--as-root | --uid N] [--rw DIR] -- COMMAND...
#
# --rw mounts DIR of the host, which has to exist, writable at its own path.
#
# Exit code: COMMAND's. Setup that fails is 125 and says so on stderr, so that
# it is never read as a test result. No docker is 125 under
# RSB_CONTAINER_REQUIRED=1 (CI) and 77 otherwise (a caller reports a skip).
#
# Default: a device from `rsb_device binder --mode 0666`, which needs
# target/debug/rsb_device, and COMMAND runs as the repository's owner: rsb_hub
# trusts a configuration file of root or of its own uid, and the owner has
# built what runs here.
#
# --no-binder grants nothing: no SYS_ADMIN, no device rule, and Docker's default
# seccomp and apparmor profiles, so binderfs cannot be mounted and binder-control
# cannot be opened. This is the environment the topic transport over a Unix
# socket is for, so a test here shows that it works where binder cannot, rather
# than that it happens not to open binder. A path that would need more than the
# default profile fails here instead of passing with the profile lifted.
#
# With a device the profiles are lifted: seccomp filters the mount binderfs needs,
# and apparmor's docker-default denies mount where it is loaded. Nothing else of
# --privileged is taken: it makes /sys writable and needs no device rule.
#
# The repository is mounted read-only at its own path (test binaries hold absolute
# paths into it; parallel runs cannot collide in target/), and /usr from the host
# (they link to the host's glibc). /tmp and everything else are the container's.
set -u

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
IMAGE=${RSB_CONTAINER_IMAGE:-debian:trixie-slim}
LABEL=rsbinder.test.container

binder=1
user=
writable=
while [ $# -gt 0 ]; do
    case "$1" in
        --no-binder) binder=0 ;;
        --as-root) user=0 ;;
        --uid) user=$2; shift ;;
        --rw) writable=$2; shift ;;
        --) shift; break ;;
        *) echo "run.sh: unknown argument: $1" >&2; exit 125 ;;
    esac
    shift
done
if [ $# -eq 0 ]; then
    echo "run.sh: no command" >&2
    exit 125
fi

if ! command -v docker >/dev/null || ! docker info >/dev/null 2>&1; then
    echo "run.sh: docker is not available" >&2
    [ "${RSB_CONTAINER_REQUIRED:-0}" = 1 ] && exit 125
    exit 77
fi

[ -n "$user" ] || user=$(stat -c %u "$ROOT")

options=(
    --rm
    --label "$LABEL"
    --label "$LABEL.run=${RSB_CONTAINER_RUN:-}"
    --tmpfs /tmp:mode=1777,exec
    -v "$ROOT:$ROOT:ro"
    -v /usr:/usr:ro
    -w "$ROOT"
    -e "RSB_CONTAINER_USER=$user"
    -e "RSB_CONTAINER_BINDER=$binder"
    -e "RUST_LOG=${RUST_LOG:-}"
    -e "RUST_BACKTRACE=${RUST_BACKTRACE:-}"
)

if [ -n "$writable" ]; then
    options+=(-v "$writable:$writable")
fi

if [ "$binder" = 1 ]; then
    major=$(awk '$2 == "binder" { print $1 }' /proc/devices)
    if [ -z "$major" ] && [ -e /dev/binderfs/binder ]; then
        major=$(stat -c %t /dev/binderfs/binder | xargs -I{} printf '%d' 0x{})
    fi
    if [ -z "$major" ]; then
        echo "run.sh: this kernel has no binder device major in /proc/devices" >&2
        exit 125
    fi
    options+=(
        --cap-add SYS_ADMIN
        --device-cgroup-rule "c $major:* rwm"
        --security-opt seccomp=unconfined
        --security-opt apparmor=unconfined
    )
fi

name=rsbinder-test-$$-$RANDOM
# The client can end without docker noticing; removal ends the container's processes too.
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# In the background: bash runs a trap only after a foreground command ends, and `wait` returns
# at once on a signal, so a run that hangs can still be ended from outside.
docker run --name "$name" "${options[@]}" "$IMAGE" \
    "$ROOT/tests/container/entry.sh" "$@" &
wait $!
exit $?
