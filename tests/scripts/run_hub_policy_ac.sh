#!/usr/bin/env bash
# rsb_hub acceptance gates: Plan 6-1 access control (AC-6.1.1 .. AC-6.1.9),
# plus the 6-2/6-3 declaration gates, the 6-5 diagnostic CLI, and the 6-4
# service-supervisor integration.
#
# Needs a Linux host with a working binder device: the point of these gates is
# that a denial crosses a real kernel transaction, because the caller uid the
# policy matches on is filled in by the binder driver. Nothing in-process can
# stand in for it, which is why none of this can run on macOS.
#
#   cargo build -p rsbinder-tools --bin rsb_hub --bin rsb_service
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
SVC=./target/debug/rsb_service
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
# Plan 6-5: the CLI is an ordinary binder client, so it answers under the
# same policy as everything else. Capture stdout+stderr together — a denial
# is reported on stderr and the gates below assert on both.
SVC_RC=0
run_svc() { $SVC "$@" >"$OUT" 2>&1; SVC_RC=$?; }
want_rc() { if [ "$SVC_RC" = "$1" ]; then ok "$2"; else bad "$2 (exit $SVC_RC, want $1)"; fi; }
want_out() { if grep -q "$1" "$OUT"; then ok "$2"; else bad "$2 (missing '$1' in: $(head -3 "$OUT"))"; fi; }
reject_out() { if grep -q "$1" "$OUT"; then bad "$2 (leaked '$1')"; else ok "$2"; fi; }

######################################################################
note "AC-6.1.1  no policy -> refuse to start"
out=$($HUB --config /tmp/rsb61-nonexistent 2>&1); rc=$?
[ "$rc" -eq 1 ] && ok "exits non-zero" || bad "exit $rc"
echo "$out" | grep -q "cannot load its configuration" && ok "names the failure" || bad "message: $out"
echo "$out" | grep -q '\[\[rule\]\]'                      && ok "shows a minimal example" || bad "no example"

note "AC-6.1.1b  malformed policy -> refuse to start, point at the file"
mkdir -p "$POLDIR"; echo 'rule = "oops"' > "$POLDIR/10.toml"
out=$($HUB --config "$POLDIR" 2>&1); rc=$?
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

# rsb_service dump manager looks the hub up by name like any other client,
# so it needs find on it. The dump itself is gated on list separately, which
# is what the denial gate at the end of this script revokes.
[[rule]]
name = "manager"
find = { uid = [$MY_UID] }
EOF
cat > "$POLDIR/99-catchall.toml" <<'EOF'
[[rule]]
name = "*"
add  = "none"
find = "none"
EOF

if ! start_hub --config "$POLDIR"; then bad "hub did not start under an enforcing policy"; cat "$LOG"; exit 1; fi
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
# Plan 6-2 / 6-3: declarations answer isDeclared and friends, and a lookup
# that misses a declared service starts it. The activation is a real
# process, so this is the only place it can be exercised.
note "6-3  declarations answer isDeclared / getDeclaredInstances / getConnectionInfo"
MARKER=/tmp/rsb61-started
rm -f "$MARKER"
cat > "$POLDIR/30-declared.toml" <<EOF
[[rule]]
name = "ac61.declared.*"
add  = { uid = [$MY_UID] }
find = { uid = [$MY_UID] }

[[service]]
name = "ac61.declared.IFoo/default"
start = { exec = ["/usr/bin/touch", "$MARKER"] }
connection = { ip = "10.0.0.1", port = 4242 }

[[service]]
name = "ac61.declared.IFoo/secondary"

[[service]]
name = "ac61.declared.IBar/default"
EOF
kill -HUP "$HUB_PID"; sleep 1
grep -q "service declaration(s)" "$LOG" && ok "reload reports the declarations" || bad "no declaration count: $(tail -2 "$LOG")"

run_probe declared:ac61.declared.IFoo/default declared:ac61.declared.INope/default \
          instances:ac61.declared.IFoo conninfo:ac61.declared.IFoo/default
want_result declared  ac61.declared.IFoo/default  YES "declared instance reports true"
want_result declared  ac61.declared.INope/default NO  "undeclared instance reports false"
want_line   "INSTANCE default"                        "getDeclaredInstances returns default"
want_line   "INSTANCE secondary"                      "getDeclaredInstances returns secondary"
want_result conninfo  ac61.declared.IFoo/default  "10.0.0.1:4242" "getConnectionInfo reports the declared pair"

note "6-2  a lookup that misses a declared service starts it"
[ ! -e "$MARKER" ] || bad "precondition: marker already exists"
# checkService must NOT start anything.
run_probe find:ac61.declared.IFoo/default
sleep 1
[ -e "$MARKER" ] && bad "checkService must not start a service" || ok "checkService started nothing"
# getService must.
run_probe get:ac61.declared.IFoo/default
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -e "$MARKER" ] && break; sleep 0.5; done
[ -e "$MARKER" ] && ok "6-2  getService started the declared service" || bad "activation did not run"
kill -0 "$HUB_PID" 2>/dev/null && ok "hub survived the activation" || bad "hub died activating"
# A declaration without `start` must be inert rather than an error.
run_probe get:ac61.declared.IFoo/secondary
want_result get ac61.declared.IFoo/secondary NOTFOUND "a declaration without start is inert"
######################################################################
# Plan 6-5: the diagnostic CLI. It is a plain binder client, so these gates
# also re-prove the policy from a second, independent implementation of the
# client side.
note "6-5  rsb_service answers the operator's questions"
# A registrant that outlives one shell command, so a *second* process can
# see the registry. Everywhere else in this script the probe registers and
# exits, which is what the death-cleanup gates depend on -- and what makes a
# populated registry impossible to inspect from outside without this.
$PROBE add:ac61.allowed.live add:ac61.hidden.live hold:60 >/tmp/rsb61-hold.out 2>&1 &
HOLD_PID=$!
sleep 2
grep -q "RESULT hold 60 OK" /tmp/rsb61-hold.out \
    && ok "a live registrant is holding two names" \
    || bad "the holding probe did not register: $(cat /tmp/rsb61-hold.out)"

run_svc list
want_rc 0 "list exits 0"
want_out "^ac61.allowed.live$"  "list shows a findable name"
reject_out "^ac61.hidden.live$" "list filtered the non-findable name"

run_svc check ac61.allowed.live
want_rc 0 "check of a registered service exits 0"
want_out "registered" "check says so"

run_svc check ac61.allowed.nonexistent
want_rc 1 "check of an unregistered service exits 1"
want_out "not registered" "check says so"

run_svc info
want_rc 0 "info exits 0"
want_out "pid=" "info reports the registering pid"
want_out "ac61.allowed.live"    "info names a findable service"
reject_out "ac61.hidden.live"   "info filtered the non-findable name"

run_svc declared ac61.declared.IFoo/default
want_rc 0 "declared exits 0 for a declared instance"
run_svc declared ac61.declared.INope/default
want_rc 1 "declared exits 1 for an undeclared instance"

run_svc instances ac61.declared.IFoo
want_rc 0 "instances exits 0"
want_out "ac61.declared.IFoo/default"   "instances lists default"
want_out "ac61.declared.IFoo/secondary" "instances lists secondary"

run_svc connection ac61.declared.IFoo/default
want_rc 0 "connection exits 0"
want_out "10.0.0.1:4242" "connection reports the declared pair"
run_svc connection ac61.declared.IFoo/secondary
want_rc 1 "connection exits 1 when none is declared"

note "6-5  rsb_service check must not start a declared service"
rm -f "$MARKER"
run_svc check ac61.declared.IFoo/default
sleep 1
[ -e "$MARKER" ] && bad "rsb_service check started a service" || ok "check is side-effect free"

note "6-5  dump: the hub reports its own registry"
run_svc dump manager
want_rc 0 "dump manager exits 0"
want_out "^rsb_hub "                  "dump identifies the hub and its version"
want_out "access control: enforcing"  "dump reports the access-control mode"
want_out "^services ("                "dump has a services section"
want_out "ac61.allowed.live"          "dump lists a findable registration"
want_out "pid=$HOLD_PID"              "dump attributes it to the registering pid"
reject_out "ac61.hidden.live"         "dump filtered the non-findable name"
want_out "hidden by policy"           "dump says the view is partial"
want_out "declarations: 3"            "dump counts the loaded declarations"

run_svc dump manager ac61.allowed
want_rc 0 "dump with a filter exits 0"
want_out "filter: ac61.allowed"       "dump echoes the filter"
want_out "ac61.allowed.live"          "dump keeps the matching name"
reject_out "^  manager$"              "dump dropped the non-matching name"

kill "$HOLD_PID" 2>/dev/null; wait "$HOLD_PID" 2>/dev/null

rm -f "$MARKER" "$POLDIR/30-declared.toml"
kill -HUP "$HUB_PID"; sleep 1

note "config trust: a writable configuration is refused"
chmod 777 "$POLDIR"
out=$($HUB --config "$POLDIR" 2>&1); rc=$?
chmod 755 "$POLDIR"
[ "$rc" -eq 1 ] && ok "world-writable config refuses to start" || bad "exit $rc"
echo "$out" | grep -q "world-writable" && ok "and says why" || bad "$out"

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

# Plan 6-5: the CLI must say *denied*, not report an empty registry. This is
# the whole reason `hub::try_*` exists — the swallowing wrappers would print
# nothing here and exit 0.
run_svc list
want_rc 2 "6-5  a denied list exits 2, not 0-with-no-output"
want_out "policy denies this to uid" "6-5  and explains that the policy is the reason"
run_svc info
want_rc 2 "6-5  a denied info exits 2"
run_svc dump manager
want_rc 2 "6-5  a denied dump exits 2"
want_out "policy denies .list." "6-5  and the hub says so on the dump fd itself"

# A broken reload must keep the policy already in force.
echo 'name = [[[' > "$POLDIR/30-broken.toml"
kill -HUP "$HUB_PID"; sleep 1
kill -0 "$HUB_PID" 2>/dev/null && ok "hub survived a broken reload" || bad "hub died on a broken reload"
grep -q "keeping the policy already in force" "$LOG" && ok "broken reload logged as kept" || bad "no keep message: $(tail -3 "$LOG")"
run_probe add:ac61.reloaded.svc2 debuginfo
want_result add       ac61.reloaded.svc2 OK        "AC-6.1.7b previous policy still in force after a broken reload"
want_result debuginfo ""                 FAILEDTXN "AC-6.1.7c previous policy's list denial still in force"

stop_hub

######################################################################
# Plan 6-4: running under a service supervisor. Readiness has to be
# reported at the instant handle 0 becomes reachable -- earlier and every
# unit ordered After= the hub races the registry -- and a deliberate stop
# has to look deliberate, not like a crash.
note "6-4  readiness, clean shutdown, single instance"
# Start from a configuration that loads and grants this user `list` again:
# the reload gates above deliberately left a broken file and a revoked
# global gate behind, and neither is what 6-4 is about.
rm -f "$POLDIR/30-broken.toml" "$POLDIR/05-noglobal.toml"
printf '[global]\nlist = { uid = [%s] }\n' "$MY_UID" > "$POLDIR/00-global.toml"
NOTIFY_SOCK=/tmp/rsb61-notify.sock
NOTIFY_OUT=/tmp/rsb61-notify.out
rm -f "$NOTIFY_SOCK" "$NOTIFY_OUT"
python3 - "$NOTIFY_SOCK" "$NOTIFY_OUT" <<'NOTIFY_LISTENER_PY' &
import socket, sys
sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
sock.bind(sys.argv[1])
sock.settimeout(30)
with open(sys.argv[2], "w") as out:
    for _ in range(2):          # READY=1, then STOPPING=1
        try:
            out.write(sock.recv(4096).decode())
        except OSError as e:
            out.write(f"TIMEOUT {e}\n")
        out.flush()
NOTIFY_LISTENER_PY
NOTIFY_LISTENER=$!
sleep 1

NOTIFY_SOCKET="$NOTIFY_SOCK" RUST_LOG=info nohup $HUB --config "$POLDIR" > "$LOG" 2>&1 &
HUB_PID=$!
for _ in $(seq 1 20); do grep -q '^READY=1' "$NOTIFY_OUT" 2>/dev/null && break; sleep 0.5; done
if grep -q '^READY=1' "$NOTIFY_OUT" 2>/dev/null; then
    ok "6-4  the hub reports READY=1 to \$NOTIFY_SOCKET"
    if grep -q 'STATUS=serving' "$NOTIFY_OUT"; then
        ok "6-4  and a STATUS line for systemctl status"
    else
        bad "no STATUS line: $(cat "$NOTIFY_OUT")"
    fi
    # The contract, not just the datagram: no sleep here on purpose. If
    # readiness were announced before become_context_manager, this is the
    # call that would race it.
    run_svc list
    want_rc 0 "6-4  the registry is reachable the moment READY=1 lands"
else
    bad "6-4  no READY=1 (got: $(cat "$NOTIFY_OUT" 2>/dev/null))"
fi

kill -TERM "$HUB_PID" 2>/dev/null
wait "$HUB_PID"; term_rc=$?
[ "$term_rc" -eq 0 ] && ok "6-4  SIGTERM exits 0, not killed-by-signal" || bad "SIGTERM exit $term_rc"
if grep -q "SIGTERM received, shutting down" "$LOG"; then
    ok "6-4  and says so in the log"
else
    bad "no shutdown line: $(tail -2 "$LOG")"
fi
for _ in $(seq 1 10); do grep -q '^STOPPING=1' "$NOTIFY_OUT" 2>/dev/null && break; sleep 0.5; done
if grep -q '^STOPPING=1' "$NOTIFY_OUT"; then
    ok "6-4  and told the supervisor it was deliberate"
else
    bad "no STOPPING=1: $(cat "$NOTIFY_OUT")"
fi
kill "$NOTIFY_LISTENER" 2>/dev/null; wait "$NOTIFY_LISTENER" 2>/dev/null
rm -f "$NOTIFY_SOCK" "$NOTIFY_OUT"
HUB_PID=""

note "6-4  a second hub on the same device refuses to run"
if start_hub --config "$POLDIR"; then
    out=$($HUB --config "$POLDIR" 2>&1); rc=$?
    [ "$rc" -eq 1 ] && ok "6-4  the second instance exits 1" || bad "second instance exit $rc"
    if echo "$out" | grep -q "exactly one service manager"; then
        ok "6-4  and names the cause instead of an ioctl errno"
    else
        bad "message: $out"
    fi
    run_svc check manager
    want_rc 0 "6-4  the first instance is still serving"
else
    bad "6-4  hub did not restart for the single-instance gate"; cat "$LOG"
fi
stop_hub

printf '\n==== Plan 6 AC gates: %d passed, %d failed ====\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
