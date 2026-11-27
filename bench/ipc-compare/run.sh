#!/usr/bin/env bash
# Plan 15 §8 measurement sweep.
#
#   bench/ipc-compare/run.sh all|b1|b2|b3|b4|stream|ix-block [repeats]
#
# `stream` is B2 for rsbinder's stream alone, the comparison AC-15.1.10 makes before and after
# P1a-3; run it with 5 repeats. `ix-block` is the iceoryx2 blocking rows of B2 and B3. `topic`
# is B2 and B3 for rsbinder-topic (futex, fd and poll waits) next to iceoryx2's. `b5` and `b6`
# time one publish in one process (waiting subscribers by wake; encode by codec); `p0b` is both,
# and B3's large payloads with iceoryx2 subscribers that copy them out. `p2e5` is `b6` and
# rsbinder-topic's B3 rows, to set against the `topic` set's. `p3` is B3's large payloads with
# rsbinder-topic's loans, read in place and copied out (by a subscriber that does not vouch for
# the publisher, and by one that does), next to its ring copies and iceoryx2's two kinds of
# subscriber. `p36` is rsbinder-topic's large payloads, ring and pool, without and with
# backpressure; `p36f` is the large payloads with backpressure, without and with a window. Each
# repeat writes its own CSV.
#
# `dbus` is the comparison with D-Bus (plan 20), rsbinder next to iceoryx2 and D-Bus: its parts
# are `dbus-calls` (round trips by size, a struct argument, a name lookup, a value read,
# callers at once), `dbus-large` (large payloads in the message, as an fd, lent) and
# `dbus-pubsub`. They start a dbus-broker of their own (bus.sh) and need systemd's user
# manager. `region` is rsbinder's ways to carry a payload at the same sizes (in the transaction,
# a blob, a SharedMemory kept, a MemoryDealer). `soak` is rsbinder-topic under churn, checked
# rather than timed; SOAK_SECONDS sets each of its four runs' length (600).
#
# Results go to plans/15-pubsub-bench/<time>-<set>/ (plans/20-dbus-bench/ for the `dbus` sets
# and `soak`): conditions.txt and results-<repeat>.csv.
#
# Needs a binder device, this tree's release rsb_hub running
# (target/release/rsb_hub --config tests/policy/permissive.toml; ALLOW_HUB=1 takes another and
# records it), and every CPU on the `performance` governor (plan 15 §8.1); ALLOW_GOVERNOR=1
# measures anyway and records it.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
set_name=${1:?usage: run.sh all|b1|b2|b3|b4|stream [repeats]}
repeats=${2:-1}
bin=$here/target/release/ipc-compare

hub_pid=$(pgrep -x rsb_hub | head -n 1 || true)
if [ -z "$hub_pid" ]; then
    echo "run.sh: rsb_hub is not running; start it with" >&2
    echo "  target/release/rsb_hub --config tests/policy/permissive.toml" >&2
    exit 1
fi
# A name lookup is a call to the hub, so a debug hub is in B9's rows: 301 us against 11.7 us.
hub_exe=$(readlink "/proc/$hub_pid/exe")
if [ "$hub_exe" != "$root/target/release/rsb_hub" ] && [ "${ALLOW_HUB:-0}" != 1 ]; then
    echo "run.sh: the running rsb_hub is $hub_exe, not this tree's release build" >&2
    echo "  cargo build --release -p rsbinder-tools --bin rsb_hub, and start that one" >&2
    echo "  or ALLOW_HUB=1 to measure anyway" >&2
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

case "$set_name" in
    dbus*|soak|region) results=20-dbus-bench ;;
    *) results=15-pubsub-bench ;;
esac
out=$root/plans/$results/$(date +%Y%m%d-%H%M%S)-$set_name
mkdir -p "$out"

uniq_of() { sort | uniq -c | sed 's/^ *//' | tr '\n' ' '; }
{
    echo "date: $(date -Is)"
    echo "set: $set_name, repeats: $repeats"
    echo "git: $(git -C "$root" rev-parse --short HEAD) on $(git -C "$root" branch --show-current)," \
        "$(git -C "$root" status --short | wc -l) changed paths"
    echo "rsb_hub: $hub_exe $(tr '\0' ' ' < "/proc/$hub_pid/cmdline" | cut -d' ' -f2-)," \
        "built $(date -Is -r "$hub_exe")"
    echo "kernel: $(uname -r)"
    echo "binder driver: $([ -d /sys/module/rust_binder ] && echo rust_binder || echo C binder)"
    echo "cpu: $(lscpu | sed -n 's/^Model name: *//p')"
    echo "cpufreq driver: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver)"
    echo "amd_pstate: $(cat /sys/devices/system/cpu/amd_pstate/status 2>/dev/null || echo -)"
    echo "governor: $(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | uniq_of)"
    echo "epp: $(cat /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference 2>/dev/null | uniq_of)"
    echo "boost: $(cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo -)"
    echo "cpuidle: $(cat /sys/devices/system/cpu/cpuidle/current_governor 2>/dev/null || echo -);" \
        "cpus with each state disabled: $(for s in /sys/devices/system/cpu/cpu0/cpuidle/state*; do
            printf '%s=%s ' "$(cat "$s/name")" \
                "$(cat /sys/devices/system/cpu/cpu*/cpuidle/"${s##*/}"/disable | grep -c 1)"; done)"
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

b5() {
    for wake in futex fd; do
        for n in 0 1 8 64; do
            measure b5 --wake "$wake" --n "$n"
        done
    done
}

b6() {
    for side in publish receive; do
        for size in 64 1024 16384 65536 1048576; do
            for codec in alloc parcel bytes; do
                measure b6 --codec "$codec" --side "$side" --size "$size"
            done
        done
    done
}

# B3's large payloads with iceoryx2 subscribers that copy each payload out, as a reader that
# keeps the value does; rsbinder-topic's rows are the `topic` set's.
b3_touch() {
    for n in 1 8; do
        for size in 65536 1048576 4194304; do
            for rate in 1000 10000; do
                for wait in block poll; do
                    measure b3 --impl ix --wait "$wait" --touch yes --n "$n" --size "$size" --rate "$rate"
                done
            done
        done
    done
}

# B3's large payloads lent from rsbinder-topic's pool (plan 15 P3-4), each next to the same
# payload copied through its ring and through iceoryx2, read in place and copied out.
b3_loan() {
    local touch
    for n in 1 8; do
        for size in 65536 1048576 4194304; do
            for rate in 1000 10000; do
                for wait in block poll; do
                    for touch in no yes; do
                        measure b3 --impl rsb-topic-loan --wait "$wait" --touch "$touch" \
                            --n "$n" --size "$size" --rate "$rate"
                        # A subscriber that vouches for the publisher copies as iceoryx2's does.
                        if [ "$touch" = yes ]; then
                            measure b3 --impl rsb-topic-loan --wait "$wait" --touch yes \
                                --trust yes --n "$n" --size "$size" --rate "$rate"
                        fi
                        measure b3 --impl ix --wait "$wait" --touch "$touch" \
                            --n "$n" --size "$size" --rate "$rate"
                    done
                    measure b3 --impl rsb-topic --wait "$wait" --n "$n" --size "$size" --rate "$rate"
                done
            done
        done
    done
}

# B3's large payloads through rsbinder-topic's ring and its pool, each without and with a
# backpressure of 1 s (plan 15 P3-6, AC-15.3.14): where the subscribers keep up the two rows
# differ by the cost of the wait's check; where they cannot, the row with backpressure goes at
# the slowest subscriber's pace and loses nothing.
b3_backpressure() {
    local bp
    for n in 1 8; do
        for size in 65536 1048576 4194304; do
            for rate in 1000 10000 0; do
                for wait in block poll; do
                    for bp in no yes; do
                        local flags=()
                        [ "$bp" = yes ] && flags=(--backpressure 1000)
                        measure b3 --impl rsb-topic --wait "$wait" --n "$n" --size "$size" \
                            --rate "$rate" "${flags[@]}"
                        measure b3 --impl rsb-topic-loan --wait "$wait" --touch yes --n "$n" \
                            --size "$size" --rate "$rate" "${flags[@]}"
                    done
                done
            done
        done
    done
}

# B3's large payloads through rsbinder-topic's ring and its pool with a backpressure of 1 s,
# without and with a window of 2 records (plan 15 P3-6f): where the subscribers cannot keep up
# the window bounds the latency by two records and the bytes behind the slowest subscriber;
# where they keep up the two rows should not differ.
b3_window() {
    local window
    for n in 1 8; do
        for size in 1048576 4194304; do
            for rate in 1000 0; do
                for window in 0 2; do
                    local flags=(--backpressure 1000)
                    [ "$window" -gt 0 ] && flags+=(--window "$window")
                    measure b3 --impl rsb-topic --wait block --n "$n" --size "$size" \
                        --rate "$rate" "${flags[@]}"
                    measure b3 --impl rsb-topic-loan --wait block --touch yes --n "$n" \
                        --size "$size" --rate "$rate" "${flags[@]}"
                done
            done
        done
    done
}

# The D-Bus comparison (plan 20): every kind of call and publish-subscribe, rsbinder next to
# iceoryx2 and D-Bus on a dbus-broker of its own.
dbus_calls() {
    local imp size n
    for size in 8 64 1024 16384 65536 1048576; do
        for imp in rsb ix dbus; do measure b1 --impl "$imp" --size "$size"; done
    done
    for imp in rsb ix dbus; do measure b8 --impl "$imp"; done
    for imp in rsb ix dbus; do measure b9 --impl "$imp"; done
    for imp in rsb rsb-state ix dbus dbus-cached; do measure b10 --impl "$imp"; done
    for n in 1 2 4 8 16; do
        for imp in rsb ix dbus; do measure b7 --impl "$imp" --n "$n" --size 64; done
    done
    # The same callers of a server whose threads the scheduler places.
    for n in 4 8 16; do
        for imp in rsb dbus; do measure b7 --impl "$imp" --n "$n" --size 64 --server-pin no; done
    done
}

# Large payloads three ways: in the message (rsbinder's transaction buffer ends below 1 MiB;
# `rsb` moves a blob to a memfd by itself), as an fd the receiver maps, and lent from shared
# memory (iceoryx2's rows of B1, and rsb-topic-loan's of B3).
dbus_large() {
    local imp size
    for size in 262144 524288 1048576; do measure b1 --impl rsb-bytes --size "$size"; done
    for size in 4194304 16777216 67108864; do
        for imp in rsb ix dbus; do measure b1 --impl "$imp" --size "$size"; done
    done
    # A blob read into a Vec each side keeps, at the sizes where a new Vec is new pages.
    for size in 65536 1048576 4194304 16777216 67108864; do
        measure b1 --impl rsb-reuse --size "$size"
    done
    for size in 65536 1048576 16777216 67108864; do
        for imp in rsb dbus; do measure b11 --impl "$imp" --size "$size"; done
    done
}

# A payload round trip four ways at the same sizes: in the transaction, a blob (a memfd a
# call), one SharedMemory shared before, and an IMemory of a MemoryDealer each way.
region() {
    local imp size
    for size in 16384 32768 65536 131072 262144 524288; do
        measure b1 --impl rsb-bytes --size "$size"
    done
    for size in 16384 32768 65536 131072 262144 524288 1048576 4194304 16777216; do
        measure b1 --impl rsb --size "$size"
        measure b1 --impl rsb-reuse --size "$size"
        for imp in rsb-shm rsb-dealer; do measure b12 --impl "$imp" --size "$size"; done
    done
}

dbus_pubsub() {
    local imp n size rate
    for n in 1 4 8; do
        for size in 64 1024 65536 1048576; do
            for rate in 1000 10000 0; do
                for imp in rsb-topic rsb-oneway ix dbus; do
                    measure b3 --impl "$imp" --wait block --n "$n" --size "$size" --rate "$rate"
                done
                if [ "$size" -ge 65536 ]; then
                    measure b3 --impl rsb-topic-loan --wait block --touch yes --n "$n" \
                        --size "$size" --rate "$rate"
                fi
            done
        done
    done
}

# rsbinder-topic under churn, checked (src/soak.rs). SOAK_SECONDS is each run's length.
soak() {
    local seconds=${SOAK_SECONDS:-600}
    measure soak --duration "$seconds" --size 4096
    measure soak --duration "$seconds" --size 4096 --rate 10000
    measure soak --duration "$seconds" --size 1048576 --rate 1000 --loan yes
    measure soak --duration "$seconds" --size 65536 --backpressure 1000
}

case "$set_name" in
    dbus|dbus-calls|dbus-large|dbus-pubsub)
        address=$("$here/bus.sh" start)
        export DBUS_SESSION_BUS_ADDRESS=$address
        trap '"$here/bus.sh" stop' EXIT
        "$here/bus.sh" info | tee -a "$out/conditions.txt"
        echo "zbus: $(sed -n '/^name = "zbus"$/{n;p}' "$here/Cargo.lock"), blocking API" \
            | tee -a "$out/conditions.txt"
        ;;
esac

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
        b5) b5 ;;
        b6) b6 ;;
        p0b) b5; b6; b3_touch ;;
        p2e5) b6; b3 "rsb-topic block" "rsb-topic poll" ;;
        p3) b3_loan ;;
        p36) b3_backpressure ;;
        p36f) b3_window ;;
        dbus) dbus_calls; dbus_large; dbus_pubsub ;;
        dbus-calls) dbus_calls ;;
        dbus-large) dbus_large ;;
        dbus-pubsub) dbus_pubsub ;;
        region) region ;;
        soak) soak ;;
        *) echo "run.sh: unknown set $set_name" >&2; exit 2 ;;
    esac
done
echo "results: $out"
