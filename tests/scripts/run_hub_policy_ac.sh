#!/usr/bin/env bash
# rsb_hub access-control acceptance gates (Plan 6-1, AC-6.1.1 .. AC-6.1.9).
#
# Needs a Linux host with a working binder device: the point of these gates is
# that a denial crosses a real kernel transaction, because the caller uid the
# policy matches on is filled in by the binder driver. Nothing in-process can
# stand in for it, which is why none of this can run on macOS.
#
#   cargo build --bin rsb_hub -p rsbinder-tools
#   cargo build -p tests --bin ac61_probe
#   ./tests/scripts/run_hub_policy_ac.sh
#
# Runs entirely as the invoking unprivileged user. The "two different uids"
# axis is therefore covered by allow/deny *by name* for the real caller uid,
# plus real NSS group matching against a group the user is in and one it is
# not; the uid-set membership logic itself is covered by the unit tests in
# `rsbinder-tools`. Override IN_GROUP / OUT_GROUP if the defaults do not fit
# the host.
set -u

# Repo root, so the script works from anywhere.
cd "$(dirname "$0")/../.." || exit 1

HUB=./target/debug/rsb_hub
PROBE=./target/debug/ac61_probe
POLDIR=/tmp/rsb61-policy
LOG=/tmp/rsb61-hub.log
OUT=/tmp/rsb61-out
PASS=0; FAIL=0
MY_UID=$(id -u)
IN_GROUP=${IN_GROUP:-kvm}    # a supplementary group this user really is in
OUT_GROUP=${OUT_GROUP:-root}  # a real group this user really is not in

note() { printf '\n=== %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
# want_result <op> <arg> <outcome> <label>
want_result() {
    if grep -qx "RESULT $1 $2 $3" "$OUT"; then ok "$4"
    else bad "$4 (want '$3', got '$(grep -E "^RESULT $1 $2 " "$OUT" | head -1)')"; fi
}
want_line()   { if grep -qx "$1" "$OUT"; then ok "$2"; else bad "$2 (missing '$1')"; fi; }
reject_line() { if grep -qx "$1" "$OUT"; then bad "$2 (leaked '$1')"; else ok "$2"; fi; }

cleanup() { pkill -f 'target/debug/rsb_hub' 2>/dev/null; pkill -f 'target/debug/ac61_probe' 2>/dev/null; rm -rf "$POLDIR"; }
trap cleanup EXIT
cleanup; sleep 1

HUB_PID=""
start_hub() {
    RUST_LOG=info nohup $HUB "$@" > "$LOG" 2>&1 &
    HUB_PID=$!; disown 2>/dev/null; sleep 2
    kill -0 "$HUB_PID" 2>/dev/null
}
stop_hub() { pkill -f 'target/debug/rsb_hub' 2>/dev/null; sleep 1; HUB_PID=""; }
run_probe() { $PROBE "$@" >"$OUT" 2>>/tmp/rsb61-probe.err; }

######################################################################
note "AC-6.1.1  no policy -> refuse to start"
out=$($HUB --policy /tmp/rsb61-nonexistent 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "exits non-zero" || bad "exit $rc"
echo "$out" | grep -q "cannot load access-control policy" && ok "names the failure" || bad "message: $out"
echo "$out" | grep -q '\[\[rule\]\]'                      && ok "shows a minimal example" || bad "no example"

note "AC-6.1.1b  malformed policy -> refuse to start, point at the file"
mkdir -p "$POLDIR"; echo 'rule = "oops"' > "$POLDIR/10.toml"
out=$($HUB --policy "$POLDIR" 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "exits non-zero" || bad "exit $rc"
echo "$out" | grep -q "TOML parse error" && ok "reports the parse error with a location" || bad "$out"
rm -rf "$POLDIR"

######################################################################
note "AC-6.1.2  --insecure-allow-all -> starts, everything allowed"
if start_hub --insecure-allow-all; then
    ok "hub started (pid $HUB_PID)"
    grep -q "no access control is applied" "$LOG" && ok "warns in the log" || bad "no warning logged"
    run_probe add:ac61.anything find:ac61.anything debuginfo
    want_result add       ac61.anything OK "addService allowed"
    want_result find      ac61.anything OK "checkService allowed"
    want_result debuginfo ""            OK "getServiceDebugInfo allowed"
else
    bad "hub did not start"; cat "$LOG"
fi
stop_hub

######################################################################
note "AC-6.1.3/4/5/6/8  enforcing policy"
mkdir -p "$POLDIR"
cat > "$POLDIR/10-rules.toml" <<EOF
[global]
list = { uid = [$MY_UID] }

[[rule]]
name = "ac61.allowed.*"
add  = { uid = [$MY_UID] }
find = { uid = [$MY_UID] }

# registered by us, but never findable -> must vanish from listServices
[[rule]]
name = "ac61.hidden.*"
add  = { uid = [$MY_UID] }
find = "none"

[[rule]]
name = "ac61.ingroup.*"
add  = { group = ["$IN_GROUP"] }
find = { group = ["$IN_GROUP"] }

[[rule]]
name = "ac61.outgroup.*"
add  = { group = ["$OUT_GROUP"] }
find = { group = ["$OUT_GROUP"] }

[[rule]]
name = "ac61.shared.*"
add  = { uid = [$MY_UID] }
find = { uid = [$MY_UID] }
EOF
cat > "$POLDIR/99-catchall.toml" <<'EOF'
[[rule]]
name = "*"
add  = "none"
find = "none"
EOF

if ! start_hub --policy "$POLDIR"; then bad "hub did not start under an enforcing policy"; cat "$LOG"; exit 1; fi
ok "AC-6.1.8  hub self-registered under a policy that grants it nothing"

run_probe add:ac61.allowed.svc add:ac61.denied.svc add:ac61.ingroup.svc \
          add:ac61.outgroup.svc add:ac61.hidden.svc \
          find:ac61.allowed.svc find:ac61.hidden.svc notify:ac61.denied.svc list
want_result add    ac61.allowed.svc  OK        "AC-6.1.4  add allowed by a uid rule"
want_result add    ac61.denied.svc   SECURITY  "AC-6.1.3  add denied by the catch-all (EX_SECURITY)"
want_result add    ac61.ingroup.svc  OK        "AC-6.1.6a add allowed via supplementary group '$IN_GROUP'"
want_result add    ac61.outgroup.svc SECURITY  "AC-6.1.6b add denied, caller not in group '$OUT_GROUP'"
want_result add    ac61.hidden.svc   OK        "hidden service registered"
want_result find   ac61.allowed.svc  OK        "allowed lookup succeeds"
want_result find   ac61.hidden.svc   NOTFOUND  "AC-6.1.5a denied lookup reports not-registered, not an error"
want_result notify ac61.denied.svc   FAILEDTXN "denied registerForNotifications IS an error"
want_result list   ""                OK        "listServices allowed by the global gate"
want_line   "LIST ac61.allowed.svc"             "AC-6.1.5b list contains a findable name"
reject_line "LIST ac61.hidden.svc"              "AC-6.1.5c list filtered the non-findable name"
listed=$(grep '^LIST ' "$OUT"); sorted=$(printf '%s\n' "$listed" | sort)
[ "$listed" = "$sorted" ] && ok "L-1  listServices output is sorted" || bad "unsorted: $listed"

######################################################################
# Plan 6 H-1 / H-1b: the hub keeps one kernel death subscription per binder,
# reference-counted by how many registry entries depend on it. Observable
# from outside in exactly one way — one binder published under several names
# must still take every one of those names down when it dies.
note "H-2  a dead binder's registrations are actually retired"
run_probe add:ac61.allowed.dies
want_result add ac61.allowed.dies OK "service registered by a probe that then exits"
sleep 1
run_probe list
reject_line "LIST ac61.allowed.dies" "H-2  dead service retired from the registry"

note "H-1b  one binder, many names: death cleans up all of them"
run_probe addsame:ac61.shared.a addsame:ac61.shared.b addsame:ac61.shared.c list
want_result addsame ac61.shared.a OK "shared binder registered under a"
want_result addsame ac61.shared.b OK "shared binder registered under b"
want_result addsame ac61.shared.c OK "shared binder registered under c"
for n in a b c; do want_line "LIST ac61.shared.$n" "ac61.shared.$n is listed while the owner lives"; done

# The probe has exited, so the shared binder is dead. One obituary must
# retire all three names.
sleep 1
run_probe list
for n in a b c; do reject_line "LIST ac61.shared.$n" "ac61.shared.$n retired after the owner died"; done
kill -0 "$HUB_PID" 2>/dev/null && ok "hub survived the multi-name cleanup" || bad "hub died cleaning up"

note "H-1  register/unregister notification cycles stay healthy"
cycle_args=""
for _ in $(seq 1 25); do cycle_args="$cycle_args notify:ac61.allowed.svc unnotify:ac61.allowed.svc"; done
# shellcheck disable=SC2086
run_probe $cycle_args
cycles_ok=$(grep -c '^RESULT notify ac61.allowed.svc OK$' "$OUT")
cycles_un=$(grep -c '^RESULT unnotify ac61.allowed.svc OK$' "$OUT")
[ "$cycles_ok" = 25 ] && [ "$cycles_un" = 25 ] \
    && ok "25 register/unregister cycles all succeeded" \
    || bad "cycles: $cycles_ok registers, $cycles_un unregisters (want 25/25)"
kill -0 "$HUB_PID" 2>/dev/null && ok "hub survived the cycles" || bad "hub died during the cycles"
run_probe add:ac61.allowed.after find:ac61.allowed.after
want_result add  ac61.allowed.after OK "hub still accepting registrations afterwards"
want_result find ac61.allowed.after OK "hub still serving lookups afterwards"

######################################################################
note "AC-6.1.7  SIGHUP reload"
cat > "$POLDIR/20-more.toml" <<EOF
[[rule]]
name = "ac61.reloaded.*"
add  = { uid = [$MY_UID] }
find = { uid = [$MY_UID] }
EOF
run_probe add:ac61.reloaded.svc
want_result add ac61.reloaded.svc SECURITY "new rule not active before SIGHUP"
kill -HUP "$HUB_PID"; sleep 1
grep -q "SIGHUP reloaded" "$LOG" && ok "reload logged" || bad "no reload logged: $(tail -3 "$LOG")"
kill -0 "$HUB_PID" 2>/dev/null   && ok "hub survived the reload" || bad "hub died on SIGHUP"
run_probe add:ac61.reloaded.svc
want_result add ac61.reloaded.svc OK "AC-6.1.7a new rule active after SIGHUP"

# The global list gate, proven by revoking it across a reload.
printf '[global]\nlist = "none"\n' > "$POLDIR/05-noglobal.toml"
python3 - "$POLDIR" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1], "10-rules.toml")
t = p.read_text()
p.write_text(t[t.index("[[rule]]"):])   # 05- owns [global] now
PY
kill -HUP "$HUB_PID"; sleep 1
run_probe debuginfo
want_result debuginfo "" FAILEDTXN "AC-6.1.5d global list gate denies getServiceDebugInfo"

# A broken reload must keep the policy already in force.
echo 'name = [[[' > "$POLDIR/30-broken.toml"
kill -HUP "$HUB_PID"; sleep 1
kill -0 "$HUB_PID" 2>/dev/null && ok "hub survived a broken reload" || bad "hub died on a broken reload"
grep -q "keeping the policy already in force" "$LOG" && ok "broken reload logged as kept" || bad "no keep message: $(tail -3 "$LOG")"
run_probe add:ac61.reloaded.svc2 debuginfo
want_result add       ac61.reloaded.svc2 OK        "AC-6.1.7b previous policy still in force after a broken reload"
want_result debuginfo ""                 FAILEDTXN "AC-6.1.7c previous policy's list denial still in force"

stop_hub
printf '\n==== Plan 6-1 AC gates: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
