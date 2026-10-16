#!/usr/bin/env bash
# Plan 15 §8 measurement sweep.
#
#   bench/ipc-compare/run.sh all|b1|b2|b3|b4|stream|ix-block [repeats]
#
# `stream` is B2 for rsbinder's stream alone, the comparison AC-15.1.10 makes before and after
# P1a-3; run it with 5 repeats. `ix-block` is the iceoryx2 blocking rows of B2 and B3. `topic`
# is B2 and B3 for rsbinder-topic (futex, fd and poll waits) next to iceoryx2's. Each repeat
# writes its own CSV.
#
# Results go to plans/15-pubsub-bench/<time>-<set>/: conditions.txt and results-<repeat>.csv.
#
# Needs a binder device and a running rsb_hub
# (target/debug/rsb_hub --config tests/policy/permissive.toml), and every CPU on the
# `performance` governor (plan 15 §8.1); ALLOW_GOVERNOR=1 measures anyway and records it.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
set_name=${1:?usage: run.sh all|b1|b2|b3|b4|stream [repeats]}
repeats=${2:-1}
bin=$here/target/release/ipc-compare

if ! pgrep -x rsb_hub >/dev/null; then
    echo "run.sh: rsb_hub is not running; start it with" >&2
    echo "  target/debug/rsb_hub --config tests/policy/permissive.toml" >&2
    exit 1
fi
governors=$(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | sort -u | tr '\n' ' ')
if [ "$governors" != "performance " ] && [ "${ALLOW_GOVERNOR:-0}" != 1 ]; then
    echo "run.sh: governor is '$governors', not performance (plan 15 §8.1)" >&2
    echo "  echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor" >&2
    echo "  or ALLOW_GOVERNOR=1 to measure anyway" >&2
    exit 1
fi

(cd "$here" && cargo build --release --quiet)

out=$root/plans/15-pubsub-bench/$(date +%Y%m%d-%H%M%S)-$set_name
mkdir -p "$out"

uniq_of() { sort | uniq -c | sed 's/^ *//' | tr '\n' ' '; }
{
    echo "date: $(date -Is)"
    echo "set: $set_name, repeats: $repeats"
    echo "git: $(git -C "$root" rev-parse --short HEAD), $(git -C "$root" status --short | wc -l) changed paths"
    echo "kernel: $(uname -r)"
    echo "binder driver: $([ -d /sys/module/rust_binder ] && echo rust_binder || echo C binder)"
    echo "cpu: $(lscpu | sed -n 's/^Model name: *//p')"
    echo "cpufreq driver: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver)"
    echo "amd_pstate: $(cat /sys/devices/system/cpu/amd_pstate/status 2>/dev/null || echo -)"
    echo "governor: $(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | uniq_of)"
    echo "epp: $(cat /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference 2>/dev/null | uniq_of)"
    echo "boost: $(cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo -)"
    echo "smt siblings: $(cat /sys/devices/system/cpu/cpu*/topology/thread_siblings_list | sort -un | tr '\n' ' ')"
    echo "pinning: producer/server cpu 1; consumers/client cpus 2,3,4,5,6,7,0,8 in that order;" \
        "B4 threads cpus 2,3; the driver is not pinned"
    echo "rustc: $(rustc -V)"
    echo "iceoryx2: $(sed -n '/^name = "iceoryx2"$/{n;p}' "$here/Cargo.lock")"
    echo "loadavg: $(cat /proc/loadavg)"
} > "$out/conditions.txt"
cat "$out/conditions.txt"

"$bin" child ix-cleanup >/dev/null 2>&1 || true

csv=
measure() { "$bin" "$@" | tee -a "$csv"; }

b1() {
    for size in 8 64 1024 16384 32768 65536 1048576; do
        measure b1 --impl rsb --size "$size"
        measure b1 --impl ix --wait block --size "$size"
        measure b1 --impl ix --wait poll --size "$size"
    done
}

# Arguments: the "impl wait" pairs to run.
b2() {
    local pairs=("$@") pair imp wait
    for size in 64 1024 16384 65536 1048576; do
        for rate in 1000 10000 0; do
            for pair in "${pairs[@]}"; do
                read -r imp wait <<< "$pair"
                measure b2 --impl "$imp" --wait "$wait" --size "$size" --rate "$rate"
            done
        done
    done
}

# Arguments: the "impl wait" pairs to run.
b3() {
    local pairs=("$@") pair imp wait
    for n in 1 2 4 8; do
        for size in 64 4096 65536 1048576 4194304; do
            for rate in 1000 10000 0; do
                for pair in "${pairs[@]}"; do
                    read -r imp wait <<< "$pair"
                    measure b3 --impl "$imp" --wait "$wait" --n "$n" --size "$size" --rate "$rate"
                done
            done
        done
    done
}

b4() {
    for _ in 1 2 3; do
        measure b4 --impl fmq
        measure b4 --impl ix
    done
}

for repeat in $(seq "$repeats"); do
    csv=$out/results-$repeat.csv
    "$bin" header > "$csv"
    all_b3=("rsb-oneway block" "rsb-stream block" "rsb-stream poll" "ix block" "ix poll")
    case "$set_name" in
        all) b1; b2 "rsb-stream block" "rsb-stream poll" "ix block" "ix poll"; b3 "${all_b3[@]}"; b4 ;;
        b1) b1 ;;
        b2) b2 "rsb-stream block" "rsb-stream poll" "ix block" "ix poll" ;;
        b3) b3 "${all_b3[@]}" ;;
        b4) b4 ;;
        stream) b2 "rsb-stream block" "rsb-stream poll" ;;
        ix-block) b2 "ix block"; b3 "ix block" ;;
        topic)
            topic_pairs=("rsb-topic block" "rsb-topic fd" "rsb-topic poll" "ix block" "ix poll")
            b2 "${topic_pairs[@]}"; b3 "${topic_pairs[@]}" ;;
        *) echo "run.sh: unknown set $set_name" >&2; exit 2 ;;
    esac
done
echo "results: $out"
