// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Multi-session `RpcServer`, real-process e2e, threads,
//! `getRemoteMaxThreads` negotiation, oneway delivery, nested callbacks,
//! timeout, lifecycle, and the no-global gate.
//!
//! Separate test binary. Each test builds its own
//! server + sessions ⇒ parallel-safe, no `--test-threads=1`.
//!
//! # Mutation gates
//!
//! One bullet per test whose mutant, or whose reason for an assertion,
//! does not fit its one-line doc.
//!
//! - `client_timeout_on_hung_server`,
//!   `a_reply_timeout_on_one_connection_ends_the_fan_out_session`: drop the
//!   `fail_session` from `client_transact`'s reply-wait failure and the
//!   session outlives its reply timeout. In `client_timeout_on_hung_server`
//!   the next call times out too (`TimedOut`, the server still inside
//!   `slow`), so only the exact `DeadObject` assert catches it; in the
//!   fan-out test the other connection's call returns `Ok` and no obituary
//!   fires.
//! - `a_session_with_one_busy_connection_is_not_idle`: make
//!   `RpcSessionInner::active_since` report no activity and the quiet
//!   fan-out connection's first idle expiry ends the session while the
//!   founding connection is still busy. The last assertion is the other
//!   half: a session whose connections all went quiet still ends.
//! - `a_dispatch_longer_than_the_idle_period_is_not_idle`: drop the
//!   `OpenCall` from `dispatch_transact` and the quiet connection's idle
//!   expiry ends the session under the running handler (no byte moves for
//!   three periods), so `slow` fails.
//! - `a_handler_waiting_on_its_own_callback_is_not_idle`: make
//!   `active_since` ignore `open` and the quiet connection's idle expiry
//!   ends the session while the handler waits for the client's slow
//!   callback, so `roundtrip` fails. The server's reply timeout is set so
//!   that the callback's own wait is not the bound under test.
//! - `a_client_that_only_waits_for_callbacks_is_idle`: make
//!   `active_since` report activity always and the session outlives the
//!   idle period, so the late call succeeds.
//! - `handshake_timeout_bounds_a_silent_peer`: make
//!   `RpcClientConfig::handshake_deadline` ignore `timeout` and the `timeout`
//!   case blocks on the silent peer; drop the session-timeout fallback from
//!   `RpcSession::attach_parts` and the incoming attach blocks reading the
//!   `"cci"` that never comes. The attach is an incoming one because an
//!   outgoing attach reads nothing in its handshake, and its admission probe
//!   already falls back to the session's timeout.
//! - `max_connections_admission_bound`: deleting the `max_connections`
//!   gate in `RpcServer::run` serves the third client at once, so its
//!   bounded-timeout `get_root` succeeds and the test fails.
//! - `silent_r34_peer_released_by_handshake_deadline`: clearing the read
//!   deadline before the r34 serve loop leaves the silent c1 worker blocked
//!   in `recv`; the single `max_connections` slot never frees and c2's
//!   bounded `get_root` times out.
//! - `tls_android13plus_nested_callback_e2e`: the nested server→client
//!   callback needs full duplex on one `TlsTransport`. A single mutex over
//!   the TLS stream, held by a blocked `recv`, deadlocks it; the split
//!   `Mutex<Connection>` structure is what passes.
//! - `multi_connection_shared_session`: client #1 (empty id) founds a
//!   session and the server registers a `Weak<RpcSessionInner>`, with
//!   `attached`/`rejected` at 0; `get_session_id()` returns the
//!   server-minted id (the client half of AOSP `setupClient`); client #2
//!   echoes it and attaches, so `c2.get_session_id() == sid1` (the id
//!   lives in `SharedSession`, so this holds only if state is shared) and
//!   `attached_count == 1`; client #3 sends an unknown id and is
//!   rejected. Dropping #2 ends the shared session, and #1 fails with
//!   `DeadObject` (plan 2-24 D1); `ac_12_f8_attach_unifies_to_single_inner`
//!   checks the same through the pool. Mutant for both: drop the
//!   `fail_session` a serve loop's end calls
//!   (`RpcSession::serve_blocking_on_inner`), and #1 keeps working. Mutant: make
//!   `RpcServer::run_connection_in_worker`'s attach arm build a fresh
//!   session with `RpcSession::from_android13plus(transport, codec,
//!   client_fd_mode, fd_unix)`, as the empty-id arm does; #2 then gets its
//!   own `SharedSession` and `c2.get_session_id() != sid1`.
//! - `ac_12_f8_attach_unifies_to_single_inner`: one `RpcSessionInner` per
//!   session. With one inner per accepted connection (sharing only
//!   `SharedSession`), a `state.remote_proxies`-cached `RpcProxy` would hold
//!   a `Weak` to the first worker's inner, and a later worker unmarshaling
//!   the same client binder would `find_conn` its nested `proxy.transact`
//!   inside that other worker's slot pool (cross-slot aliasing). An
//!   id-echoing attach adds a slot to the founding inner, so
//!   `session_slot_count(sid) == Some(2)`. Mutant: the attach arm in
//!   `RpcServer::run_connection_in_worker` builds
//!   `RpcSession::with_shared(transport, profile, Arc::clone(&inner.shared))`
//!   — a fresh inner sharing only `SharedSession` — which leaves the
//!   founding inner at `Some(1)`. `multi_connection_shared_session`
//!   asserts the attach itself; this test adds the single-inner shape.
//! - `ac_12_4_set_max_threads_caps_incoming_slots`: `set_max_threads(N)`
//!   both advertises (`GetMaxThreads` returns `max_threads_value()`, so a
//!   client's `negotiate(local_max)` records `min(local_max, cap)`, read
//!   back through `negotiated_max_threads()`) and enforces (the attach arm
//!   refuses an attach that would push `slot_count()` past it). Mutant,
//!   advertise-only: the third attach succeeds, `session_slot_count(sid)`
//!   reaches `Some(3)` and `rejected_unknown_id_count` stays at 0. The
//!   `shutdown` gate is covered by
//!   `shutdown_gate_e2e_rejects_attach_during_handshake_stall`.
//! - `fan_out_creates_n_outgoing_slots_when_local_max_outgoing_is_n`:
//!   skipping the `1..negotiated` fan-out loop body leaves the founding
//!   inner at one slot, so `session_slot_count` is `Some(1)`, not `Some(N)`.
//! - `local_max_outgoing_one_skips_fan_out_byte_identical_to_founding_only`:
//!   the `negotiated_max_threads() == 0` witness catches a mutant that runs
//!   `negotiate` on the single-connection path. Dropping the early return
//!   `if local == 1 && incoming == 0 { return Ok(session); }` alone is not
//!   caught: the fan-out closure skips `negotiate` when `local == 1`, and
//!   the extra `GET_SESSION_ID` round trip it adds is not observed.
//! - `attach_with_a_bogus_session_id_is_refused_at_attach_time`: the
//!   outgoing attach wire has no server→client acknowledgement (the server
//!   refuses by closing), so `add_outgoing_connection_with_config` confirms
//!   with one `GET_SESSION_ID` round trip. A single-threaded client always
//!   draws slot 1 first, so the test drives 4 threads to reach the attached
//!   slot. The client-local id is refused before connecting (`BadValue`);
//!   dropping `confirm_attach` returns `Ok(3)` for the unknown id, so its
//!   `is_err()` assert fails; the echo loop is the control that a confirmed
//!   pool has no dead slot.
//! - `attach_past_the_server_slot_cap_is_refused_at_attach_time`: no
//!   client-side check can catch a valid id past the cap, and a caller that
//!   skips `negotiate()` has no other way to learn it. Without
//!   `confirm_attach` the over-cap attach returns `Ok(3)` while the server
//!   stays at 2 slots.
//! - `shutdown_gate_e2e_rejects_attach_during_handshake_stall`: in
//!   production the attach arm's `shutdown` check sees a sub-microsecond
//!   window between an accepted late attach and a concurrent
//!   `stop_accepting()`. The `__set_attach_shutdown_probe` barrier fires
//!   after the codec check and before that check, so the test parks the
//!   worker, calls `stop_accepting()`, and releases it into the reject
//!   branch. Mutant: removing `if server.shutdown.load() { reject }` from
//!   the attach arm lets the worker add the slot, so
//!   `rejected_unknown_id_count` does not move, `session_slot_count`
//!   reaches `Some(2)`, and the attach succeeds.
//! - `shared_node_survives_sibling_proxy_drop`: two client sessions
//!   attached to one server session (shared `RpcState`) each hold a proxy
//!   to the same root. With AOSP `timesSent` accounting the
//!   server counts each send (strong = 2), so dropping one proxy leaves the
//!   node for the sibling, and it is freed only when both drop (proven at
//!   the state level by `rpc::state::tests::times_sent_balance_frees_node`).
//!   Mutant: revert the `timesSent` bump in `on_binder_leaving`; strong
//!   stays 1, dropping `root1` frees the node and `root2.echo()` is
//!   `DeadObject`.
//! - `excess_receipt_no_leak_single_client`: `get_root()` twice makes
//!   the server's strong count 2 while the client dedups to one proxy, so
//!   the client owes one excess `DEC_STRONG` at the second receipt (AOSP
//!   `flushExcessBinderRefs`) plus one at proxy drop. Mutant: `read_binder`
//!   without the excess-DEC arm sends only the drop DEC, strong sticks at
//!   1 and `live_session_node_count()` never returns to 0. This relies on
//!   dedup: if each receipt minted a fresh proxy, 0 excess + 2 drops would
//!   also balance and the test would pass vacuously, so an identity
//!   assertion pins the dedup precondition.
//! - `pool_distributes_concurrent_calls_across_outgoing_slots`: two
//!   parallel `slow(150)` take ≈150 ms when the pool gives each thread its
//!   own slot (AOSP `findConnection`), ≈300 ms when serialized. The test
//!   asserts `< 380 ms`, which a serialized run also meets, so it does not
//!   catch `find_conn` always returning slot 1 or ignoring `exclusive_tid`.
//! - `pool_exhausted_third_caller_waits_for_a_free_slot`: 2 slots and 3
//!   `slow(200)` ≈ 400 ms. Only the 380 ms lower bound discriminates: a
//!   third caller that does not wait on `slot_cv` finishes in one wave.
//!   The 700 ms upper bound catches a stall, not a serial run (≈600 ms).
//! - `pool_nested_callback_pins_to_forced_slot_single_thread`: slot 1 is
//!   parked under `slow(...)`, so `roundtrip(cb)` runs on slot 2 and its
//!   nested callback must dispatch there (`find_conn`'s reentrant match is
//!   keyed by `(session_ptr, slot_id)`). Only slot 2 unmarshals `cb`, so
//!   this exercises the pin without the cross-slot aliasing that
//!   `ac_12_f8_attach_unifies_to_single_inner` rules out.
//! - `ac_12_2_extended_cross_slot_nested_callback_multi_thread`: with one
//!   inner per connection the two workers' cached `RpcProxy`s would point
//!   at different inners, and the second worker's nested `proxy.transact`
//!   would `find_conn` in the first worker's pool and deadlock or
//!   interleave. `set_timeout(3s)` turns that deadlock into a failure.
//! - `pool_oneway_fifo_under_concurrent_twoway_multi_outgoing`: top-level
//!   oneway `find_conn` does not pin to slot 1; per-object oneway order is
//!   carried by the per-address `asyncNumber` on send and the `asyncTodo`
//!   replay on receive (`rpc::state`). This test gates that no frame is
//!   lost or corrupted (`count == 300`); the ordering mutants are gated by
//!   the unit tests
//!   `rpc::state::tests::out_of_order_enqueues_then_drains_in_priority_order`
//!   and `send_async_number_is_per_address_monotonic`, and by the
//!   STAGE3 live libbinder round-robin path.
//! - `unix_accepted_peer_identity_is_the_client_not_self`: a non-Linux
//!   `resolve_peer` returning `self_identity()` for every socket would make
//!   a macOS/BSD server see itself as the peer; the forked child's pid
//!   check fails on `pid == std::process::id()`. On Linux `SO_PEERCRED`
//!   already reports the client.
//! - `rpc_death_recipient_fires_on_session_drop`: the notified side runs a
//!   serve loop (the AOSP incoming-thread requirement) and a link before it
//!   starts is refused; an unlinked recipient must not fire, and a link
//!   after death is `DeadObject`. Mutant: drop `send_session_obituaries()`
//!   from `RpcSessionInner::on_session_dead`, or stub `RpcProxy::link_to_death` to
//!   `InvalidOperation`; `binder_died` never arrives.
//! - `rpc_death_signal_completes_on_session_drop`: adds the wake-up path
//!   (sent on the session thread, awaited in another runtime) and the
//!   already-dead case, which completes instead of erroring. Mutant:
//!   `DeathSignal::poll` always `Pending`, or no `oneshot` send in
//!   `binder_died`.
//! - `authorizer_gate_rejects_before_any_rpc_byte`: deleting the authorizer
//!   block in `RpcServer::run_connection_in_worker` lets the rejected
//!   client's `get_root()` succeed.
//! - `dec_strong_outside_handler_does_not_block_or_desync`: with no
//!   `Outgoing` slot the `DEC_STRONG` is skipped (AOSP `WOULD_BLOCK`), not
//!   written to the served slot, so the node is released at session end,
//!   or promptly once the client opens an incoming connection (plan 2-20
//!   Phase B). The test pins that safety property; no assertion depends on
//!   the DEC going out.
//! - `preconnected_inet_fd_handshakes_over_tcp_debug`: the android-13+
//!   handshake starts with a raw write; a transport without
//!   `send_raw`/`recv_raw` gets the trait default's `Protocol` refusal.
//! - `terminate_ends_every_session_and_joins_workers`: only the android-13+
//!   session has an id (the r34 one was never in the id registry), and it is
//!   `Live(2)`; the test checks that `terminate` still returns and that
//!   session dies whole instead of being skipped as "still live".
//! - `a_reply_timeout_ends_a_session_with_a_callback_slot`: the callback
//!   slot alone would keep the pool non-empty, but nothing reads a request
//!   written on it. Drop the `fail_session` from `client_transact`'s
//!   reply-wait failure and `__slot_count()` stays 2.
//! - `a_callback_reply_slower_than_the_idle_period_is_not_idle`: drop the
//!   `OpenCall` from `client_transact`'s twoway and the founding
//!   connection's idle expiry ends the session under the callback, so it
//!   fails with `DeadObject`. `a_nested_dispatch_in_a_callback_reply_wait_is_not_idle`
//!   holds both sites open at once and fails when `active_since` ignores
//!   `open`.

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcClientConfig, RpcProxy, RpcServer, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IEcho2";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_BUMP: TransactionCode = FIRST_CALL_TRANSACTION + 1; // oneway
const TX_COUNT: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_SLOW: TransactionCode = FIRST_CALL_TRANSACTION + 3;
const TX_ROUNDTRIP: TransactionCode = FIRST_CALL_TRANSACTION + 4; // nested
const TX_HOLD_CB: TransactionCode = FIRST_CALL_TRANSACTION + 5; // store a callback for later

trait IEcho2: Interface {
    fn echo(&self, s: &str) -> Result<String>;
    fn bump(&self) -> Result<()>; // oneway
    fn count(&self) -> Result<i64>;
    fn slow(&self, ms: i32) -> Result<()>;
    /// Server calls `cb.echo("ping")` and returns `rt:` + its result (nested callback).
    fn roundtrip(&self, cb: &SIBinder) -> Result<String>;
    /// Server stores `cb` for a test to drive from outside any handler (plan 2-20).
    fn hold(&self, cb: &SIBinder) -> Result<()>;
}

struct EchoSvc {
    counter: Arc<AtomicI64>,
    /// Always `false`; `roundtrip` reads it only to silence the unused-field lint.
    deeper: bool,
    /// Set (never reset) on entry to `slow()`: at least one slow call holds a server worker.
    slow_entered: Arc<AtomicBool>,
    /// Callback parked by `hold()` for out-of-handler use.
    held: Arc<Mutex<Option<SIBinder>>>,
}
impl Interface for EchoSvc {}
impl IEcho2 for EchoSvc {
    fn echo(&self, s: &str) -> Result<String> {
        Ok(s.to_string())
    }
    fn bump(&self) -> Result<()> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn count(&self) -> Result<i64> {
        Ok(self.counter.load(Ordering::SeqCst))
    }
    fn slow(&self, ms: i32) -> Result<()> {
        self.slow_entered.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
        Ok(())
    }
    fn roundtrip(&self, cb: &SIBinder) -> Result<String> {
        // Call back into the client-provided callback (server→client).
        let rp = (**cb)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .ok_or(StatusCode::BadType)?;
        let mut d = rp.build_request(DESC)?;
        d.write(&"ping")?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        let got: String = r.read()?;
        let _ = self.deeper;
        Ok(format!("rt:{got}"))
    }
    fn hold(&self, cb: &SIBinder) -> Result<()> {
        *self.held.lock().unwrap() = Some(cb.clone());
        Ok(())
    }
}

fn echo_on_transact(
    s: &dyn IEcho2,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_ECHO => {
            let a: String = reader.read()?;
            ok_str(reply, s.echo(&a))
        }
        TX_BUMP => {
            // oneway: no reply written.
            let _ = s.bump();
            Ok(())
        }
        TX_COUNT => match s.count() {
            Ok(v) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&v)
            }
            Err(e) => reply.write(&Status::from(e)),
        },
        TX_SLOW => {
            let ms: i32 = reader.read()?;
            match s.slow(ms) {
                Ok(()) => reply.write(&Status::from(StatusCode::Ok)),
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        TX_ROUNDTRIP => {
            let cb: SIBinder = reader.read()?;
            ok_str(reply, s.roundtrip(&cb))
        }
        TX_HOLD_CB => {
            let cb: SIBinder = reader.read()?;
            match s.hold(&cb) {
                Ok(()) => reply.write(&Status::from(StatusCode::Ok)),
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        _ => Err(StatusCode::UnknownTransaction),
    }
}

fn ok_str(reply: &mut Parcel, r: Result<String>) -> Result<()> {
    match r {
        Ok(v) => {
            reply.write(&Status::from(StatusCode::Ok))?;
            reply.write(&v)
        }
        Err(e) => reply.write(&Status::from(e)),
    }
}

fn read_status(reply: &mut Parcel) -> Result<()> {
    let st: Status = reply.read()?;
    if st.is_ok() {
        Ok(())
    } else {
        Err(StatusCode::from(st))
    }
}

struct BnEcho2(Box<dyn IEcho2 + Send + Sync>);
impl Remotable for BnEcho2 {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        echo_on_transact(&*self.0, code, reader, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn make_service(counter: Arc<AtomicI64>) -> SIBinder {
    make_service_with_slow_signal(counter, Arc::new(AtomicBool::new(false)))
}

/// `slow()` sets `slow_entered` on entry, so a test waits on a claimed worker, not a sleep.
fn make_service_with_slow_signal(
    counter: Arc<AtomicI64>,
    slow_entered: Arc<AtomicBool>,
) -> SIBinder {
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
        slow_entered,
        held: Arc::new(Mutex::new(None)),
    }))))
}
/// `hold()` parks the callback in `held`, for a test to drive from outside any handler.
fn make_service_with_hold(counter: Arc<AtomicI64>, held: Arc<Mutex<Option<SIBinder>>>) -> SIBinder {
    Interface::as_binder(&Binder::new(BnEcho2(Box::new(EchoSvc {
        counter,
        deeper: false,
        slow_entered: Arc::new(AtomicBool::new(false)),
        held,
    }))))
}

// ---- client typed proxy --------------------------------------------

struct EchoProxy(SIBinder);
impl EchoProxy {
    fn rp(&self) -> &RpcProxy {
        (*self.0)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy")
    }
    fn echo(&self, s: &str) -> Result<String> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&s)?;
        let mut r = self
            .rp()
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn bump(&self) -> Result<()> {
        let d = self.rp().build_request(DESC)?;
        self.rp()
            .transact(TX_BUMP, &d, rsbinder::FLAG_ONEWAY)
            .map(|_| ())
    }
    fn count(&self) -> Result<i64> {
        let d = self.rp().build_request(DESC)?;
        let mut r = self
            .rp()
            .transact(TX_COUNT, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<i64>()
    }
    fn slow(&self, ms: i32) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(&ms)?;
        let mut r = self
            .rp()
            .transact(TX_SLOW, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
    fn roundtrip(&self, cb: &SIBinder) -> Result<String> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(cb)?;
        let mut r = self
            .rp()
            .transact(TX_ROUNDTRIP, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn hold(&self, cb: &SIBinder) -> Result<()> {
        let mut d = self.rp().build_request(DESC)?;
        d.write(cb)?;
        let mut r = self
            .rp()
            .transact(TX_HOLD_CB, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)
    }
}

fn tmp_sock(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_rpc_{}_{}_{}.sock",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

/// Wait until a server socket file exists (bounded).
fn wait_for_sock(path: &std::path::Path) {
    for _ in 0..400 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("server socket {path:?} never appeared");
}

/// Poll `f` for ~2 s (400 × 5 ms), plus one last check after the final sleep.
fn poll_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

// Kills and reaps a server child on any exit, including a panic: its `server.run()` never returns.
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Stops, joins, terminates and unlinks on any drop, a panic included; one per test.
struct ServeCleanup {
    server: Arc<RpcServer>,
    bg: Option<std::thread::JoinHandle<()>>,
    path: Option<std::path::PathBuf>,
}
impl ServeCleanup {
    fn new(
        server: Arc<RpcServer>,
        bg: std::thread::JoinHandle<()>,
        path: std::path::PathBuf,
    ) -> Self {
        Self {
            server,
            bg: Some(bg),
            path: Some(path),
        }
    }
}
impl Drop for ServeCleanup {
    fn drop(&mut self) {
        self.server.stop_accepting();
        if let Some(h) = self.bg.take() {
            // Print, not `resume_unwind`: the test may already be unwinding its own panic.
            if let Err(p) = h.join() {
                let msg = p
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| p.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                eprintln!(
                    "WARNING: ServeCleanup observed a background accept-loop \
                     panic during teardown: {msg}"
                );
            }
        }
        // Not `join_workers`: a client a test left connected would hang the whole binary.
        self.server.terminate();
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---- real OS process e2e + negotiation -----------------------------

#[test]
fn real_process_e2e_and_negotiation() {
    // Child role: become the server and block.
    if let Ok(path) = std::env::var("RSB_RPC_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_max_threads(2);
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }

    let path = tmp_sock("e2e");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = KillOnDrop(
        std::process::Command::new(exe)
            .args(["--exact", "real_process_e2e_and_negotiation", "--nocapture"])
            .env("RSB_RPC_SERVER", &path)
            .spawn()
            .expect("spawn server child"),
    );
    wait_for_sock(&path);

    {
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        let r34bad = tmp_sock("r34bad");
        assert!(matches!(
            client.add_outgoing_connection_with_config(
                RpcClientConfig::unix(&r34bad, 1).session_id(&[0u8; 32])
            ),
            Err(StatusCode::BadType)
        ));
        // Explicit negotiation, local=8, server advertises 2.
        assert_eq!(client.negotiate(8).expect("negotiate"), 2);
        assert_eq!(client.negotiated_max_threads(), 2);

        let root = EchoProxy(client.get_root().expect("get_root"));
        // Real cross-process AIDL call.
        assert_eq!(
            root.echo("over a real process").unwrap(),
            "over a real process"
        );
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..50 {
            assert_eq!(root.echo(&format!("n{i}")).unwrap(), format!("n{i}"));
        }
    }

    // Killing the client leaves the server able to exit cleanly.
    child.0.kill().expect("kill server child");
    child.0.wait().expect("reap server child");
    let _ = std::fs::remove_file(&path);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn real_process_abstract_unix_socket_e2e() {
    if let Ok(name) = std::env::var("RSB_RPC_ABSTRACT_SERVER") {
        let server = RpcServer::setup_unix_server_abstract(name.as_bytes()).expect("bind abstract");
        server.set_android13plus(2);
        server.set_max_threads(3);
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        let _ = server.run();
        std::process::exit(0);
    }

    let name = format!("rsb_rpc_abs_proc_{}", std::process::id());
    let exe = std::env::current_exe().expect("current_exe");
    let _child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "real_process_abstract_unix_socket_e2e",
                "--nocapture",
            ])
            .env("RSB_RPC_ABSTRACT_SERVER", &name)
            .spawn()
            .expect("spawn abstract server child"),
    );

    let client = 'connect: {
        for _ in 0..400 {
            if let Ok(c) = RpcSession::setup_client_android13plus_with_config(
                RpcClientConfig::unix_abstract(name.as_bytes(), 2),
            ) {
                break 'connect c;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("connect abstract server child");
    };

    assert_eq!(client.wire_protocol_version(), Some(2));
    assert_eq!(client.negotiate(8).expect("negotiate"), 3);
    let sid = client.get_session_id().expect("get_session_id");
    let attached = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix_abstract(name.as_bytes(), 2).session_id(&sid),
    )
    .expect("attach abstract child session");
    assert_eq!(attached.get_session_id().expect("attached id"), sid);
    let root = EchoProxy(attached.get_root().expect("attached get_root"));
    assert_eq!(root.echo("abstract-process").unwrap(), "abstract-process");

    let fan = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix_abstract(name.as_bytes(), 2).outgoing_connections(3),
    )
    .expect("abstract child fan-out");
    assert_eq!(fan.negotiated_max_threads(), 3);
    let root = Arc::new(EchoProxy(fan.get_root().expect("fan get_root")));
    let t0 = std::time::Instant::now();
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let root = Arc::clone(&root);
            std::thread::spawn(move || root.slow(150).expect("slow round-trip"))
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    assert!(
        t0.elapsed() < Duration::from_millis(420),
        "abstract fan-out across process must run 3 slow calls in parallel"
    );
}

// ---- concurrency: many threads, ONE shared session ----------------

#[test]
fn concurrent_calls_single_shared_session() {
    let path = tmp_sock("shared");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // 8 threads share one root proxy, as with a generated `Bp*` stub; calls serialize.
    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = Arc::new(EchoProxy(client.get_root().expect("get_root")));

    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for t in 0..8 {
        let root = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            for i in 0..200 {
                let msg = format!("shared-t{t}-i{i}");
                assert_eq!(
                    root.echo(&msg).expect("echo on shared session"),
                    msg,
                    "reply cross-delivered / wire corrupted on shared session"
                );
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
    assert!(
        t0.elapsed() < Duration::from_secs(30),
        "shared-session concurrency must make progress, not deadlock ({:?})",
        t0.elapsed()
    );
    // _cu handles teardown.
}

// ---- multi-session isolation ---------------------------------------

#[test]
fn concurrent_clients_isolated_sessions() {
    let path = tmp_sock("iso");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // 8 threads, one session each: every thread must see only its own echoes.
    let mut handles = Vec::new();
    for t in 0..8 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let client = RpcSession::setup_unix_client(&p).expect("connect");
            let root = EchoProxy(client.get_root().expect("get_root"));
            for i in 0..200 {
                let msg = format!("t{t}-i{i}");
                assert_eq!(root.echo(&msg).unwrap(), msg, "cross-session contamination");
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
    // _cu handles teardown.
}

// ---- opt-in server-side connection admission bound -----------

/// `set_max_connections(2)` holds a third client in the backlog until a slot frees; see module doc.
#[test]
fn max_connections_admission_bound() {
    let path = tmp_sock("admit");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    server.set_max_connections(2);
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Two served, then held, sessions keep both workers blocked in recv.
    let c1 = RpcSession::setup_unix_client(&path).expect("connect c1");
    assert_eq!(
        EchoProxy(c1.get_root().expect("c1 get_root"))
            .echo("c1")
            .unwrap(),
        "c1"
    );
    let c2 = RpcSession::setup_unix_client(&path).expect("connect c2");
    assert_eq!(
        EchoProxy(c2.get_root().expect("c2 get_root"))
            .echo("c2")
            .unwrap(),
        "c2"
    );

    // c3 lands in the kernel backlog; accept is gated at 2, so its `get_root` must time out.
    let c3 = RpcSession::setup_unix_client(&path).expect("connect c3 (backlog)");
    c3.set_timeout(Some(Duration::from_millis(600)));
    assert!(
        c3.get_root().is_err(),
        "3rd connection must be admission-blocked while 2 workers are live"
    );
    drop(c3);

    // Dropping c1 ends its worker; the next accept tick reaps it and accept resumes.
    drop(c1);
    let c4 = RpcSession::setup_unix_client(&path).expect("connect c4");
    c4.set_timeout(Some(Duration::from_secs(5)));
    assert_eq!(
        EchoProxy(c4.get_root().expect("c4 get_root after a slot freed"))
            .echo("c4")
            .unwrap(),
        "c4",
        "a freed worker slot must let the accept loop resume"
    );

    drop(c2);
    drop(c4);
    // _cu handles teardown.
}

/// A silent r34 peer (no handshake before frame 1) loses its slot to the deadline; see module doc.
#[test]
fn silent_r34_peer_released_by_handshake_deadline() {
    let path = tmp_sock("silent_r34");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    // One slot makes a pinned silent peer observable; a short deadline keeps it fast.
    server.set_max_connections(1);
    server.set_handshake_timeout(Some(Duration::from_millis(300)));
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // An r34 connect writes nothing, so c1 is a silent peer holding the only slot.
    let c1 = RpcSession::setup_unix_client(&path).expect("connect c1 (silent)");

    // c2 waits in the backlog until c1's worker hits the first-frame deadline.
    let c2 = RpcSession::setup_unix_client(&path).expect("connect c2");
    c2.set_timeout(Some(Duration::from_secs(5)));
    assert_eq!(
        EchoProxy(c2.get_root().expect("c2 get_root after c1 deadline"))
            .echo("c2")
            .unwrap(),
        "c2",
        "silent r34 peer's worker must release its slot on the handshake deadline"
    );

    drop(c1);
    drop(c2);
    // _cu handles teardown.
}

// ---- oneway delivery + non-blocking send ---------------------------

#[test]
fn oneway_fifo_and_nonblocking() {
    let path = tmp_sock("ow");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = EchoProxy(client.get_root().expect("get_root"));

    let n = 2000;
    let t0 = std::time::Instant::now();
    for _ in 0..n {
        root.bump().expect("oneway bump");
    }
    // Oneway sends must not wait for a per-call round trip.
    let oneway_elapsed = t0.elapsed();

    // Polled, so this checks eventual delivery of every oneway call, not their order.
    let mut last = root.count().unwrap();
    for _ in 0..200 {
        if last == n {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
        last = root.count().unwrap();
    }
    assert_eq!(last, n, "all oneway calls eventually delivered");
    assert!(
        oneway_elapsed < Duration::from_secs(5),
        "oneway sends should not block per-call (took {oneway_elapsed:?})"
    );
    // _cu handles teardown.
}

// ---- nested server→client callback ---------------------------------

#[test]
fn nested_callback_no_deadlock() {
    let path = tmp_sock("nest");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = EchoProxy(client.get_root().expect("get_root"));

    // The server calls `cb` mid-`roundtrip`; the client's recv loop dispatches it inline.
    let cb = make_service(Arc::new(AtomicI64::new(0)));
    let out = root.roundtrip(&cb).expect("nested roundtrip");
    assert_eq!(out, "rt:ping", "server→client nested callback result");

    // Run it repeatedly to shake out any nesting deadlock.
    for _ in 0..50 {
        assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
    }
    // _cu handles teardown.
}

// ---- timeout (hung server) -----------------------------------------

#[test]
fn client_timeout_on_hung_server() {
    let path = tmp_sock("to");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    client.set_timeout(Some(Duration::from_millis(150)));
    let root = EchoProxy(client.get_root().expect("get_root"));

    // Server sleeps 5s; client deadline is 150ms → deterministic Timeout.
    let t0 = std::time::Instant::now();
    let err = root.slow(5000).expect_err("hung call must time out");
    assert_eq!(err, StatusCode::TimedOut, "got {err:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "must return promptly on timeout, not block for the full 5s"
    );
    // The late `REPLY` has no id to be told apart by, so the session ended (plan 2-24 D2).
    assert_eq!(root.echo("after"), Err(StatusCode::DeadObject));
    // _cu handles teardown.
}

/// Idle is judged per session: one busy connection keeps a quiet one's session up (plan 2-24 D8).
#[test]
fn a_session_with_one_busy_connection_is_not_idle() {
    let path = tmp_sock("idlefan");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_idle_timeout(Some(Duration::from_millis(400)));
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).outgoing_connections(2),
    )
    .expect("fan-out connect");
    assert_eq!(client.__slot_count(), 2, "founding + one fan-out");
    let root = EchoProxy(client.get_root().expect("get_root"));
    // One thread's calls take the first free slot; the other idles for three periods, not evicted.
    let busy_until = Instant::now() + Duration::from_millis(1_200);
    while Instant::now() < busy_until {
        assert_eq!(root.echo("busy").as_deref(), Ok("busy"));
        std::thread::sleep(Duration::from_millis(100));
    }
    // Both quiet now: past the idle period the session is gone.
    std::thread::sleep(Duration::from_millis(1_000));
    assert_eq!(root.echo("late"), Err(StatusCode::DeadObject));
    client.close_session();
}

/// A handler that outlasts the idle period on one connection keeps a quiet one's session up.
#[test]
fn a_dispatch_longer_than_the_idle_period_is_not_idle() {
    let path = tmp_sock("idledisp");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_idle_timeout(Some(Duration::from_millis(300)));
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).outgoing_connections(2),
    )
    .expect("fan-out connect");
    let root = EchoProxy(client.get_root().expect("get_root"));
    // No frame on either connection for three idle periods while the handler runs.
    assert_eq!(root.slow(900), Ok(()));
    assert_eq!(root.echo("after").as_deref(), Ok("after"));
    client.close_session();
}

/// Client callback whose `TX_ECHO` answers after `ms`.
struct SlowEcho {
    ms: u64,
}
impl Interface for SlowEcho {}
impl Remotable for SlowEcho {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        let a: String = r.read()?;
        std::thread::sleep(Duration::from_millis(self.ms));
        ok_str(reply, Ok(a))
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A handler waiting on its own callback to the client keeps a quiet connection's session up.
#[test]
fn a_handler_waiting_on_its_own_callback_is_not_idle() {
    let path = tmp_sock("idlehcb");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server.set_idle_timeout(Some(Duration::from_millis(300)));
    server.set_reply_timeout(Some(Duration::from_secs(10)));
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).outgoing_connections(2),
    )
    .expect("fan-out connect");
    let root = EchoProxy(client.get_root().expect("get_root"));
    let cb: SIBinder = Interface::as_binder(&Binder::new(SlowEcho { ms: 900 }));
    // The callback rides the handler's connection; no byte moves for three idle periods.
    assert_eq!(root.roundtrip(&cb).as_deref(), Ok("rt:ping"));
    assert_eq!(root.echo("after").as_deref(), Ok("after"));
    client.close_session();
}

/// One connection's reply timeout ends a fan-out session: the other call dies, obituaries fire.
#[test]
fn a_reply_timeout_on_one_connection_ends_the_fan_out_session() {
    let path = tmp_sock("tofan");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Two outgoing connections and one callback connection from the client.
    server.set_max_threads(2);
    let slow_entered = Arc::new(AtomicBool::new(false));
    server
        .set_root(make_service_with_slow_signal(
            Arc::new(AtomicI64::new(0)),
            Arc::clone(&slow_entered),
        ))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1)
            .outgoing_connections(2)
            .incoming_connections(1),
    )
    .expect("fan-out connect");
    let root = EchoProxy(client.get_root().expect("get_root"));
    // The incoming connection is what lets a client link (AOSP: max incoming threads >= 1).
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let flag: Arc<DeathFlag> = Arc::new(DeathFlag(tx));
    root.0
        .link_to_death(Arc::downgrade(&flag) as _)
        .expect("link_to_death");

    // No deadline for this call: it can end only through the session's end.
    let other = {
        let root = EchoProxy(root.0.clone());
        std::thread::spawn(move || root.slow(3_000))
    };
    assert!(
        poll_until(|| slow_entered.load(Ordering::SeqCst)),
        "the first slow call must hold a server worker"
    );
    // Well past the send, so that call has read the session's (absent) deadline already.
    std::thread::sleep(Duration::from_millis(100));

    client.set_timeout(Some(Duration::from_millis(200)));
    assert_eq!(root.slow(3_000), Err(StatusCode::TimedOut));
    assert_eq!(
        other.join().expect("other call"),
        Err(StatusCode::DeadObject),
        "a call in flight on the other connection dies with the session"
    );
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the session's end fires the obituary");
    assert_eq!(root.echo("after"), Err(StatusCode::DeadObject));
    client.close_session();
}

// ---- opt-in android-13+ versioned-wire profile ---------------------

/// android-13+ wire over a live server on UDS: v0–v2, `min` negotiation, clamp of an unknown max.
#[test]
fn android13plus_profile_e2e() {
    // (server_max, client_max, expected negotiated version)
    for (i, (smax, cmax, expect)) in [
        (0u32, 0u32, 0u32), // v0 — android-13
        (1, 1, 1),          // v1 — android-14/15
        (1, 0, 0),          // mismatch ⇒ min = v0
        (0, 1, 0),          // mismatch ⇒ min = v0
        (2, 2, 2),          // v2 — android-16
        (2, 1, 1),          // v2↔v1 ⇒ min = v1
        (1, 2, 1),          // v1↔v2 ⇒ min = v1
        (2, 0, 0),          // v2↔v0 ⇒ min = v0
        // An unsupported server max clamps to v2 (client EXPERIMENTAL).
        (3, 0xF000_0000, 2),
    ]
    .into_iter()
    .enumerate()
    {
        // Row index, not the versions: `sun_path` is 104 bytes on macOS.
        let path = tmp_sock(&format!("a13_{i}"));
        let counter = Arc::new(AtomicI64::new(0));
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(smax); // opt in to the versioned wire
        server.set_max_threads(2);
        server
            .set_root(make_service(counter.clone()))
            .expect("set_root");
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);

        // Negotiates min(cmax, smax); AOSP framing has no u32 length prefix.
        let client =
            RpcSession::setup_unix_client_android13plus(&path, cmax).expect("android-13+ connect");
        assert_eq!(
            client.wire_protocol_version(),
            Some(expect),
            "negotiated min({cmax},{smax}) wire version"
        );

        // GET_MAX_THREADS special transact over the android-13+ wire.
        assert_eq!(client.negotiate(8).expect("negotiate"), 2);
        assert_eq!(client.negotiated_max_threads(), 2);

        let root = EchoProxy(client.get_root().expect("get_root"));

        // AIDL scalar/string round-trip over AOSP framing.
        assert_eq!(root.echo("hello android-13+").unwrap(), "hello android-13+");
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..50 {
            assert_eq!(
                root.echo(&format!("v{expect}-n{i}")).unwrap(),
                format!("v{expect}-n{i}")
            );
        }

        // Polled: every one of the 300 oneway bumps is eventually delivered (order not checked).
        let n = 300;
        for _ in 0..n {
            root.bump().expect("oneway bump");
        }
        let mut last = root.count().unwrap();
        for _ in 0..200 {
            if last == n {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            last = root.count().unwrap();
        }
        assert_eq!(last, n, "oneway calls all delivered over android-13+ wire");

        // Nested callback: the client's recv loop dispatches the inbound TRANSACT inline.
        let cb = make_service(Arc::new(AtomicI64::new(0)));
        assert_eq!(root.roundtrip(&cb).expect("nested"), "rt:ping");
        for _ in 0..20 {
            assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
        }

        drop(root);
        drop(client);
        // `_cu` tears this iteration's server down as the loop body ends.
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn abstract_unix_socket_e2e() {
    let r34_name = format!("rsb_rpc_abs_r34_{}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&r34_name).expect("bind abstract r34");
    assert!(server.path().is_none());
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu_r34 = ServeCleanup {
        server: Arc::clone(&server),
        bg: Some(bg),
        path: None,
    };

    let client = RpcSession::setup_unix_client_abstract(&r34_name).expect("connect abstract r34");
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("abstract-r34").unwrap(), "abstract-r34");

    let a13_name = format!("rsb_rpc_abs_a13_{}", std::process::id()).into_bytes();
    let server = RpcServer::setup_unix_server_abstract(&a13_name).expect("bind abstract a13");
    assert!(server.path().is_none());
    server.set_android13plus(2);
    server.set_max_threads(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu_a13 = ServeCleanup {
        server: Arc::clone(&server),
        bg: Some(bg),
        path: None,
    };

    let client = RpcSession::setup_unix_client_android13plus_abstract(&a13_name, 2)
        .expect("connect abstract android13plus");
    assert_eq!(client.wire_protocol_version(), Some(2));
    assert_eq!(client.negotiate(8).expect("negotiate"), 2);
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("abstract-a13").unwrap(), "abstract-a13");

    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    let attached = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix_abstract(&a13_name, 2).session_id(&sid),
    )
    .expect("attach abstract android13plus");
    assert_eq!(attached.get_session_id().expect("attached id"), sid);
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "abstract with_id attach shares the founding session"
    );

    let fan_name = format!("rsb_rpc_abs_fan_{}", std::process::id()).into_bytes();
    let fan_server = RpcServer::setup_unix_server_abstract(&fan_name).expect("bind fan-out");
    fan_server.set_android13plus(2);
    fan_server.set_max_threads(3);
    fan_server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let fan_bg = fan_server.run_background();
    let _cu_fan = ServeCleanup {
        server: Arc::clone(&fan_server),
        bg: Some(fan_bg),
        path: None,
    };

    let fan_client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix_abstract(&fan_name, 2).outgoing_connections(3),
    )
    .expect("abstract fan-out");
    assert_eq!(fan_client.negotiated_max_threads(), 3);
    let fan_sid = fan_client.get_session_id().expect("fan-out id");
    let fan_sid_arr: [u8; 32] = fan_sid.as_slice().try_into().expect("32-byte session id");
    assert!(
        poll_until(|| fan_server.session_slot_count(&fan_sid_arr) == Some(3)),
        "abstract fan-out created founding + 2 attached slots"
    );
    let fan_root = EchoProxy(fan_client.get_root().expect("fan get_root"));
    assert_eq!(fan_root.echo("abstract-fan").unwrap(), "abstract-fan");
}

/// `android13plus_profile_e2e` over TLS/TCP, nested callback included; see module doc.
#[cfg(feature = "rpc-tls")]
#[test]
fn tls_android13plus_nested_callback_e2e() {
    use std::net::TcpListener;

    use rsbinder::rpc::rustls::pki_types::pem::PemObject;
    use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
    use rsbinder::rpc::transport::TlsTransport;

    const CA: &str = include_str!("tls_fixtures/ca.crt");
    const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
    const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<std::result::Result<_, _>>()
            .expect("parse certs")
    }
    fn key(pem: &str) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from_pem_slice(pem.as_bytes()).expect("parse key")
    }
    let srv_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs(SRV_CRT), key(SRV_KEY))
            .expect("server config"),
    );
    let cli_cfg = {
        let mut roots = RootCertStore::empty();
        for c in certs(CA) {
            roots.add(c).expect("add ca");
        }
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    };

    for (smax, cmax, expect) in [
        (0u32, 0u32, 0u32),
        (1, 1, 1),
        (2, 2, 2),
        (2, 1, 1),
        (1, 2, 1),
        (2, 0, 0),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let srv_cfg = Arc::clone(&srv_cfg);

        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept");
            let t = TlsTransport::accept(tcp, srv_cfg).expect("server TLS handshake");
            let session = RpcSession::accept_android13plus(Box::new(t), smax)
                .expect("server android-13+ accept");
            session.set_root(make_service(counter)).expect("set_root");
            let _ = session.serve_blocking();
        });

        // Convenience constructor: TCP connect + TLS handshake + android-13+ handshake.
        let client = RpcSession::setup_tcp_client_tls_android13plus(
            addr,
            "localhost",
            Arc::clone(&cli_cfg),
            cmax,
        )
        .expect("setup_tcp_client_tls_android13plus");
        assert_eq!(
            client.wire_protocol_version(),
            Some(expect),
            "negotiated min({cmax},{smax}) over TLS"
        );

        let root = EchoProxy(client.get_root().expect("get_root over TLS"));
        assert_eq!(root.echo("hi tls a13+").unwrap(), "hi tls a13+");
        assert_eq!(root.echo("").unwrap(), "");
        for i in 0..30 {
            assert_eq!(
                root.echo(&format!("v{expect}-{i}")).unwrap(),
                format!("v{expect}-{i}")
            );
        }

        // Oneway delivery over TLS+android-13+ wire (polled; order not checked).
        let n = 300;
        for _ in 0..n {
            root.bump().expect("oneway bump over TLS");
        }
        let mut last = root.count().unwrap();
        for _ in 0..200 {
            if last == n {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            last = root.count().unwrap();
        }
        assert_eq!(last, n, "oneway calls all delivered over TLS+android-13+");

        // A nested server→client callback over one TLS connection needs a full-duplex transport.
        let cb = make_service(Arc::new(AtomicI64::new(0)));
        assert_eq!(root.roundtrip(&cb).expect("nested over TLS"), "rt:ping");
        for _ in 0..20 {
            assert_eq!(root.roundtrip(&cb).unwrap(), "rt:ping");
        }

        drop(root);
        drop(client);
        server.join().unwrap();
    }
}

/// The default r34 profile reports no android-13+ wire version: the versioned wire is opt-in.
#[test]
fn r34_profile_reports_no_wire_version() {
    let path = tmp_sock("r34_ver");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    assert_eq!(
        client.wire_protocol_version(),
        None,
        "default profile is android-12 r34 (no versioned wire)"
    );
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("r34 still default").unwrap(), "r34 still default");

    drop(root);
    drop(client);
}

/// Id-demux: echoed id attaches, unknown id is refused, one lost connection ends it; module doc.
#[test]
fn multi_connection_shared_session() {
    let path = tmp_sock("a0b");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Founding + one attach need 2 slots (AOSP `setMaxIncomingThreads(2)`); the default is 1.
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // --- client #1: empty id ⇒ new session (default, byte-identical).
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ connect #1");
    let root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    assert_eq!(root1.echo("a0b-1").unwrap(), "a0b-1");
    assert!(
        poll_until(|| server.session_registered_count() >= 1),
        "new-session id registered"
    );
    assert_eq!(server.attached_count(), 0);
    assert_eq!(server.rejected_unknown_id_count(), 0);

    let sid1 = c1.get_session_id().expect("get_session_id #1");
    assert_eq!(sid1.len(), 32, "AOSP kSessionIdBytes");

    // --- client #2: echo #1's id ⇒ server ATTACHES it to #1's SharedSession.
    let c2 = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid1),
    )
    .expect("a13+ connect #2 (attach)");
    let sid2 = c2.get_session_id().expect("get_session_id #2");
    assert_eq!(
        sid2, sid1,
        "attached connection speaks the SAME SharedSession \
         (mutant — fresh-session-on-found — flips this)"
    );
    assert!(
        poll_until(|| server.attached_count() == 1),
        "echoed-id connection took the id-demux ATTACH path"
    );
    assert_eq!(server.rejected_unknown_id_count(), 0, "no false reject");
    // The attached connection is fully functional.
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert_eq!(root2.echo("a0b-2").unwrap(), "a0b-2");

    // --- client #3: an unknown 32-byte id ⇒ no live session ⇒ reject.
    let bogus = [0xABu8; 32];
    assert_ne!(
        &bogus[..],
        &sid1[..],
        "bogus id differs from the minted one"
    );
    // Exactly `DeadObject` (socket close → `EndOfStream`); no deadline is armed, so no `TimedOut`.
    let err = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&bogus),
    )
    .err()
    .expect("unknown id rejected");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "unknown id reject must surface as DeadObject (the EndOfStream path)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 1),
        "unknown id ⇒ rejected as UNKNOWN"
    );
    assert_eq!(server.attached_count(), 1, "attach count stable");

    // Sever only #2; `root2` first, as a proxy holds its session (AOSP `sp<>` in `BpBinder`).
    drop(root2);
    drop(c2);
    // One connection's loss ends the shared session (plan 2-24 D1, AOSP `handleRpcError`).
    let sid1_arr: [u8; 32] = sid1
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    assert!(
        poll_until(|| matches!(server.session_live_conns(&sid1_arr), None | Some(0))),
        "the attached connection's end must end the server's session"
    );
    assert_eq!(
        c1.get_session_id(),
        Err(StatusCode::DeadObject),
        "the founding connection went down with the session"
    );
    assert_eq!(server.attached_count(), 1, "attach count stable post-drop");

    drop(root1);
    drop(c1);
    // `_cu`'s Drop tears down, so a panic above leaks no worker thread or socket file.
}

/// An attach adds a slot to the founding inner (one inner per session); see module doc.
#[test]
fn ac_12_f8_attach_unifies_to_single_inner() {
    let path = tmp_sock("ac12f8");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Opt into 2 incoming slots (default 1 ⇒ founding-only).
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Founding connection (#1): empty id ⇒ new session, one slot.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ #1");
    let _root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let sid = c1.get_session_id().expect("get_session_id #1");
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "founding-only ⇒ single slot in the founding inner"
    );

    // Attached connection (#2): echoing #1's id adds a *slot* to the founding inner.
    let c2 = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid),
    )
    .expect("a13+ #2 (attach)");
    // No `_` prefix: the explicit `drop(root2)` below ends connection #2.
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert!(
        poll_until(|| server.attached_count() == 1),
        "echo-id connection took the id-demux ATTACH path"
    );
    // A *slot on the founding inner*: 2 slots; an inner per connection would stay at 1.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "attached connection adds a slot to the founding inner \
         (mutant: fresh-inner-on-attach leaves slot_count == 1)"
    );

    // One connection's end ends the one inner: its pool empties, the founding slot with it.
    drop(root2);
    drop(c2);
    assert!(
        poll_until(|| matches!(server.session_slot_count(&sid_arr), None | Some(0))),
        "the session's end empties the founding inner's pool"
    );
    assert_eq!(c1.get_session_id(), Err(StatusCode::DeadObject));
}

/// `set_max_threads(N)` (AOSP `setMaxIncomingThreads`) advertises and caps slots; see module doc.
#[test]
fn ac_12_4_set_max_threads_caps_incoming_slots() {
    let path = tmp_sock("ac124");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Founding (#1) + attached (#2): both under the cap.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ #1");
    let _r1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let sid = c1.get_session_id().expect("get_session_id #1");
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");
    let c2 = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid),
    )
    .expect("a13+ #2 (attach within cap)");
    let _r2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(2)),
        "2 slots after founding + attached (cap = 2)"
    );
    let rejected_before = server.rejected_unknown_id_count();

    // A valid id past the cap: only the attach's `GET_SESSION_ID` confirmation can see it.
    let err = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid),
    )
    .err()
    .expect("3rd attach must be rejected by the per-session cap");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "cap-reject surfaces as the same status as the unknown-id reject \
         (post-handshake socket close)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == rejected_before + 1),
        "cap-reject increments the `rejected_unknown_id` observability counter"
    );
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(2),
        "founding inner's slot pool stays at the cap (no overshoot — \
         without the cap check this reaches Some(3))"
    );

    // Advertise == enforce: `negotiate` shows a client the cap it is refused at.
    let negotiated = c1.negotiate(4).expect("negotiate");
    assert_eq!(
        negotiated, 2,
        "negotiate(local=4) records min(local, server_cap=2) = 2"
    );
    assert_eq!(
        c1.negotiated_max_threads(),
        2,
        "negotiated_max_threads() reflects the same cap"
    );
}

/// `outgoing_connections(N)` (AOSP `setupClient` fan-out) mints N-1 extra slots; see module doc.
#[test]
fn fan_out_creates_n_outgoing_slots_when_local_max_outgoing_is_n() {
    let path = tmp_sock("b2fan");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // N = 3; the helper learns the cap via `negotiate` and mints exactly N - 1 extras.
    server.set_max_threads(3);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).outgoing_connections(3),
    )
    .expect("setupClient fan-out");
    let badid = tmp_sock("badid");
    assert!(matches!(
        client.add_outgoing_connection_with_config(
            RpcClientConfig::unix(&badid, 1).session_id(b"bad")
        ),
        Err(StatusCode::BadValue)
    ));
    // The helper's `negotiate` recorded a value equal to the fan-out pool size.
    assert_eq!(
        client.negotiated_max_threads(),
        3,
        "helper performed GET_MAX_THREADS and recorded min(local=3, server=3) = 3"
    );
    let sid = client.get_session_id().expect("get_session_id");
    let badfd = tmp_sock("badfd");
    assert!(matches!(
        client.add_outgoing_connection_with_config(
            RpcClientConfig::unix(&badfd, 1)
                .session_id(&sid)
                .fd_mode(rsbinder::rpc::FileDescriptorTransportMode::Unix),
        ),
        Err(StatusCode::BadValue)
    ));
    let sid_arr: [u8; 32] = sid
        .as_slice()
        .try_into()
        .expect("32-byte session id (AOSP kSessionIdBytes)");

    // Single-threaded, so all six ride slot 1; the pool count below is the fan-out witness.
    let root = EchoProxy(client.get_root().expect("get_root"));
    for i in 0..6 {
        let msg = format!("b2-{i}");
        assert_eq!(root.echo(&msg).unwrap(), msg, "fan-out echo #{i}");
    }

    // The founding inner's pool holds all N connections; a skipped fan-out leaves `Some(1)`.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(3)),
        "founding inner's pool has 3 slots (founding + 2 fan-out attaches) — \
         mutant: helper skipping the fan-out loop leaves this at Some(1)"
    );
    assert_eq!(
        server.attached_count(),
        2,
        "exactly 2 id-echoing attaches reached the server-side attach arm"
    );
    assert_eq!(
        server.rejected_unknown_id_count(),
        0,
        "fan-out connections all pass under the cap"
    );
}

/// `outgoing_connections` 1 (or 0) skips `GET_MAX_THREADS` and fan-out; see module doc.
#[test]
fn local_max_outgoing_one_skips_fan_out_byte_identical_to_founding_only() {
    let path = tmp_sock("b2one");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // The server allows multi-conn, but the client opts out: fan-out must NOT run.
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).outgoing_connections(1),
    )
    .expect("setupClient (single-conn)");
    // Byte-identical witness: skipped negotiation leaves the default 0 ("not negotiated").
    assert_eq!(
        client.negotiated_max_threads(),
        0,
        "local_max_outgoing == 1 must skip GET_MAX_THREADS entirely \
         (mutant: a negotiate on the single-connection path records 1 here)"
    );
    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    // Server side: exactly the founding slot; extra outgoing attaches would exceed 1.
    assert!(
        poll_until(|| server.session_slot_count(&sid_arr) == Some(1)),
        "founding-only session has exactly one slot on the server side"
    );
    assert_eq!(
        server.attached_count(),
        0,
        "no id-echoing attach reached the server (no fan-out ran)"
    );
    let root = EchoProxy(client.get_root().expect("get_root"));
    assert_eq!(root.echo("b2-one").unwrap(), "b2-one");

    // `local_max_outgoing == 0` is normalized to 1 (the founding connection); same wire.
    let path2 = tmp_sock("b2zero");
    let server2 = RpcServer::setup_unix_server(&path2).expect("bind");
    server2.set_android13plus(1);
    server2.set_max_threads(2);
    server2
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg2 = server2.run_background();
    let _cu2 = ServeCleanup::new(Arc::clone(&server2), bg2, path2.clone());
    wait_for_sock(&path2);
    let client_zero = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path2, 1).outgoing_connections(0),
    )
    .expect("local_max_outgoing == 0 normalized to 1");
    assert_eq!(
        client_zero.negotiated_max_threads(),
        0,
        "0 normalized to 1 ⇒ no negotiation"
    );
}

/// A refused outgoing attach is an error at attach time, not a kept slot; see module doc.
#[test]
fn attach_with_a_bogus_session_id_is_refused_at_attach_time() {
    let path = tmp_sock("badattach");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(4);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = Arc::new(RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect"));
    let sid = client.get_session_id().expect("get_session_id");
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");

    // (a) The client-local `session_id()` is NOT the server-minted id.
    assert_ne!(
        client.session_id().as_slice(),
        sid.as_slice(),
        "a client session's `session_id()` is a local value, never the peer's"
    );
    // Refused before connecting: the session goes on and the server never sees it.
    assert!(matches!(
        client.add_outgoing_connection_with_config(
            RpcClientConfig::unix(&path, 1).session_id(&client.session_id())
        ),
        Err(StatusCode::BadValue)
    ));
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "the refused attach left the server session at its founding slot"
    );

    // (b) Control: the server-minted id attaches; 200 calls on 4 threads, zero failures.
    assert_eq!(
        client
            .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
            .expect("attach with the server-minted id"),
        2
    );
    let mut handles = Vec::new();
    for t in 0..4 {
        let c = Arc::clone(&client);
        handles.push(std::thread::spawn(move || {
            let root = EchoProxy(c.get_root().expect("get_root"));
            for i in 0..50 {
                let msg = format!("clean-{t}-{i}");
                assert_eq!(root.echo(&msg).expect("echo on a confirmed pool"), msg);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    // (c) Plain garbage: a close past the header may be a refusal that ended the server's session.
    assert!(
        client
            .add_outgoing_connection_with_config(
                RpcClientConfig::unix(&path, 1).session_id(&[0xABu8; 32])
            )
            .is_err(),
        "attaching with an unknown id must fail here, not later"
    );
    assert_eq!(
        client.__slot_count(),
        0,
        "the refused attach ended the session"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 1),
        "server counted exactly the one unknown-id reject"
    );
}

/// A valid id attached past the server's `set_max_threads` cap also fails at attach time.
#[test]
fn attach_past_the_server_slot_cap_is_refused_at_attach_time() {
    let path = tmp_sock("capattach");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Founding + exactly one attach.
    server.set_max_threads(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = client.get_session_id().expect("get_session_id");

    assert_eq!(
        client
            .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
            .expect("first attach is under the cap"),
        2
    );
    assert!(
        client
            .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
            .is_err(),
        "an attach past max_threads=2 must be refused at attach time"
    );
    // A monotonic counter: the slot count races the server's teardown of the ended session.
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == 1),
        "the server's cap arm refused the attach"
    );
    // libbinder `android-16.0.0_r3`+ ends its session at this refusal, so the client ends its own.
    assert_eq!(
        client.__slot_count(),
        0,
        "the refused attach ended the session"
    );
}

/// An attach cannot negotiate: `max_version` below the session's is `BadType`; its own works.
#[test]
fn attach_max_version_below_the_session_version_is_bad_type() {
    let path = tmp_sock("attachver");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server.set_max_threads(3);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client_android13plus(&path, 2).expect("connect");
    assert_eq!(client.wire_protocol_version(), Some(2));
    let sid = client.get_session_id().expect("get_session_id");
    assert!(matches!(
        client
            .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid)),
        Err(StatusCode::BadType)
    ));
    // `wire_protocol_version()` is the value that works.
    assert_eq!(
        client
            .add_outgoing_connection_with_config(
                RpcClientConfig::unix(&path, client.wire_protocol_version().expect("versioned"))
                    .session_id(&sid),
            )
            .expect("attach at the session's version"),
        2
    );
}

/// r34 server, android-13+ client ⇒ `DeadObject`; reverse: `android13plus_spec_golden_vectors`.
#[test]
fn r34_server_reports_an_android13plus_client_as_a_dead_peer() {
    let path = tmp_sock("profmix");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    // No `set_android13plus`: the default r34 wire.
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    for v in [0u32, 1, 2] {
        match RpcSession::setup_unix_client_android13plus(&path, v) {
            Ok(_) => panic!("an r34 server must not complete an android-13+ handshake (v{v})"),
            Err(e) => assert_eq!(
                e,
                StatusCode::DeadObject,
                "profile mismatch at max_version={v} must report the peer as dead, whichever \
                 side of the handshake notices first"
            ),
        }
    }
}

/// A refused standalone attach session (`RpcClientConfig::session_id`) fails in the constructor.
#[test]
fn standalone_attach_session_with_a_bogus_id_fails_to_build() {
    let path = tmp_sock("standalone");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(3);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let founding = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = founding.get_session_id().expect("get_session_id");
    assert!(
        RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix(&path, 1).session_id(&[0xCDu8; 32])
        )
        .is_err(),
        "an unknown id must not yield a session object"
    );
    // Control: the real id builds a usable second handle on the same server-side session.
    let attached = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid),
    )
    .expect("attach session with the server-minted id");
    let root = EchoProxy(attached.get_root().expect("get_root"));
    assert_eq!(root.echo("standalone").unwrap(), "standalone");
}

/// An attach parked at the shutdown probe while `stop_accepting` runs is refused; see module doc.
#[test]
fn shutdown_gate_e2e_rejects_attach_during_handshake_stall() {
    let path = tmp_sock("shutgate");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Within the cap, so the only `rejected_unknown_id` increment is the shutdown arm's.
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // Kept to function end: once `c1` drops the attach hits the unknown-id arm, not shutdown.
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("a13+ founding connect");
    let sid = c1.get_session_id().expect("get_session_id founding");

    // Barrier: the probe signals, blocks on `release_rx`; the test flips `shutdown`, releases.
    let (hs_done_tx, hs_done_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    // The probe is `Fn`: only its first fire takes the receiver, later ones fall through.
    let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
    server.__set_attach_shutdown_probe({
        let release_rx = Arc::clone(&release_rx);
        let hs_done_tx = hs_done_tx.clone();
        move || {
            let _ = hs_done_tx.send(());
            if let Some(rx) = release_rx.lock().expect("release_rx poisoned").take() {
                let _ = rx.recv_timeout(Duration::from_secs(5));
            }
        }
    });
    let rejected_before = server.rejected_unknown_id_count();

    // Off-thread: the attach blocks on the parked worker.
    let attach_path = path.clone();
    let attach_sid = sid.clone();
    let attach_handle = std::thread::spawn(move || -> std::result::Result<(), StatusCode> {
        // The attach's `GET_SESSION_ID` confirmation sees the reject; either step may fail.
        let c2 = RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix(&attach_path, 1).session_id(&attach_sid),
        )?;
        c2.set_timeout(Some(Duration::from_secs(3)));
        c2.get_root().map(|_| ())
    });

    // Wait for the attach worker to reach the barrier.
    hs_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("attach worker should reach the shutdown barrier");

    // Flip shutdown *while* the worker is parked at the probe.
    server.stop_accepting();

    // The released worker re-reads `shutdown` and rejects (drops the transport).
    let _ = release_tx.send(());

    let attach_result = attach_handle
        .join()
        .expect("attach thread should not panic");
    let err = attach_result.expect_err("attach during shutdown must be rejected (mutant: success)");
    assert_eq!(
        err,
        StatusCode::DeadObject,
        "shutdown-reject surfaces as the same status as the cap/unknown-id reject \
         (post-handshake socket close)"
    );
    assert!(
        poll_until(|| server.rejected_unknown_id_count() == rejected_before + 1),
        "shutdown-reject increments the `rejected_unknown_id` observability counter \
         (mutant: removing `if server.shutdown.load() {{ reject }}` leaves this flat)"
    );
    // Independent of the counter: the rejected attach never reached `add_incoming_slot`.
    let sid_arr: [u8; 32] = sid.as_slice().try_into().expect("32-byte session id");
    assert_eq!(
        server.session_slot_count(&sid_arr),
        Some(1),
        "shutdown-rejected attach did not enqueue a slot (mutant: would reach Some(2))"
    );
}

/// Two attached clients' proxies to one root: the node outlives the first drop; see module doc.
#[test]
fn shared_node_survives_sibling_proxy_drop() {
    let path = tmp_sock("f7");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Opt into 2 incoming slots (founding + attached).
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    // c2 echoes c1's id: both are connections of ONE server session (shared RpcState).
    let c1 = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect #1");
    let sid1 = c1.get_session_id().expect("session id");
    let c2 = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 1).session_id(&sid1),
    )
    .expect("connect #2 (attach)");
    assert!(
        poll_until(|| server.attached_count() == 1),
        "c2 attached to c1's shared session"
    );

    // The root is sent twice ⇒ strong = 2 (timesSent), one proxy per client, no excess.
    let root1 = EchoProxy(c1.get_root().expect("get_root #1"));
    let root2 = EchoProxy(c2.get_root().expect("get_root #2"));
    assert_eq!(root1.echo("f7-1").unwrap(), "f7-1");
    assert_eq!(root2.echo("f7-2").unwrap(), "f7-2");

    // c1 stays open to carry the DEC_STRONG; a round-trip on c1 orders it before the probe.
    drop(root1);
    let _ = c1
        .get_session_id()
        .expect("c1 still alive (ordering barrier)");

    // The shared node survives the sibling's DEC (strong 2→1).
    assert_eq!(
        root2.echo("f7-after-sibling-drop").unwrap(),
        "f7-after-sibling-drop",
        "a shared node must outlive one connection's proxy DEC"
    );

    // Probed while both connections are open (the registry `Weak` upgrades): 0 live nodes.
    drop(root2);
    let _ = c2.get_session_id().expect("c2 ordering barrier");
    assert!(
        poll_until(|| server.live_session_node_count() == 0),
        "no leak: shared root node freed after all proxies dropped"
    );

    drop(c1);
    drop(c2);
    // _cu handles teardown.
}

/// A duplicate receipt of one binder sends an excess `DEC_STRONG` (AOSP `flushExcessBinderRefs`).
#[test]
fn excess_receipt_no_leak_single_client() {
    let path = tmp_sock("f7x");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    // The 2nd receipt is an excess: the client owes one `flushExcessBinderRefs` DEC now.
    let r1 = EchoProxy(c.get_root().expect("get_root #1"));
    let r2 = EchoProxy(c.get_root().expect("get_root #2"));
    // Dedup precondition (module doc "Mutation gates"): without it the test passes vacuously.
    assert!(
        std::ptr::eq(r1.rp(), r2.rp()),
        "dedup precondition: both get_root() calls must return the \
         same RpcProxy for the excess-DEC mutant gate to be sound"
    );
    assert_eq!(r1.echo("f7x-1").unwrap(), "f7x-1");
    assert_eq!(r2.echo("f7x-2").unwrap(), "f7x-2");

    // One `RpcProxy` ⇒ one drop DEC; the round-trip orders it and the excess DEC first.
    drop(r1);
    drop(r2);
    let _ = c.get_session_id().expect("ordering barrier");
    assert!(
        poll_until(|| server.live_session_node_count() == 0),
        "flushExcessBinderRefs: 1 excess + 1 drop DEC = timesSent(2) \
         ⇒ root node freed (no leak). Stuck >0 ⇒ client excess-DEC mutant."
    );

    drop(c);
    // _cu handles teardown.
}

/// Two `slow(150)` on a 2-slot pool finish under 380 ms; see module doc "Mutation gates".
#[test]
fn pool_distributes_concurrent_calls_across_outgoing_slots() {
    let path = tmp_sock("a1pool");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // Founding + one attach = 2 incoming slots; the default cap of 1 would reject the attach.
    server.set_max_threads(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
        .expect("add outgoing slot");
    assert_ne!(slot2, 1, "second slot has a fresh id (founding == 1)");

    // With the pool, `find_conn` gives each thread its own slot, so both sleeps overlap.
    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let r = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            r.slow(150).expect("slow round-trip")
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let elapsed = t0.elapsed();
    // Parallel ≈ 150 ms; serialized ≥ 300 ms; 380 absorbs a loaded scheduler's overhead.
    assert!(
        elapsed < Duration::from_millis(380),
        "AC-12.1: 2 concurrent slow(150) on a 2-slot pool must run in \
         parallel (≈150 ms), got {elapsed:?}. Pre-pool / serialized \
         path would be ≈300 ms + overhead — that's the mutant signature."
    );

    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// Two slots, three callers: the third waits for a free slot (two waves); see module doc.
#[test]
fn pool_exhausted_third_caller_waits_for_a_free_slot() {
    let path = tmp_sock("a1exh");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // 2 incoming slots (founding + attached); 3 client threads observe the cv-wait band.
    server.set_max_threads(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id (founding == 1)");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let t0 = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..3 {
        let r = Arc::clone(&root);
        handles.push(std::thread::spawn(move || {
            r.slow(200).expect("slow");
            // The wire must survive the slot release / re-pick.
            let msg = format!("exh-{i}");
            assert_eq!(r.echo(&msg).unwrap(), msg);
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let elapsed = t0.elapsed();
    // Two waves ≈ 400 ms; < 380 is one wave (no wait); > 700 is a stall.
    assert!(
        elapsed >= Duration::from_millis(380) && elapsed < Duration::from_millis(700),
        "AC-12.1 cv-wait: 3 concurrent slow(200) on 2 slots should be \
         2 parallel waves ≈ 400 ms (got {elapsed:?}); mutant (serial) is ≈600 ms"
    );

    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// A nested callback arriving on slot 2 dispatches on slot 2; see module doc "Mutation gates".
#[test]
fn pool_nested_callback_pins_to_forced_slot_single_thread() {
    let path = tmp_sock("a2pin");
    // Not a sleep: one could end before the parker reaches `find_conn` (a false pass).
    let slow_entered = Arc::new(AtomicBool::new(false));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    // 2 incoming slots (founding + forced slot-2 echo).
    server.set_max_threads(2);
    server
        .set_root(make_service_with_slow_signal(
            Arc::new(AtomicI64::new(0)),
            Arc::clone(&slow_entered),
        ))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
        .expect("slot 2");
    assert_ne!(
        slot2, 1,
        "second slot has a fresh id (founding == 1) — otherwise the \
         'park slot 1, force main onto slot 2' geometry collapses"
    );

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));

    // Park slot 1 under a long slow() so `find_conn` on this thread can only pick slot 2.
    let parked = Arc::clone(&root);
    let parker = std::thread::spawn(move || {
        let _ = parked.slow(800);
    });
    // The only transact in flight, so the parker took `find_conn`'s first free slot: 1.
    assert!(
        poll_until(|| slow_entered.load(Ordering::SeqCst)),
        "parker failed to enter server-side slow() within budget"
    );

    let cb_counter = Arc::new(AtomicI64::new(0));
    let cb = make_service(cb_counter);
    // Slot 2's reply loop must dispatch the callback inline; a wrong pin errs or deadlocks.
    assert_eq!(
        root.roundtrip(&cb).expect("nested callback on slot 2"),
        "rt:ping"
    );

    let _ = parker.join();
    drop(root);
    drop(c);
    // _cu handles teardown.
}

/// Two threads' concurrent `roundtrip(cb)`: each nested send stays in its own slot; see module doc.
#[test]
fn ac_12_2_extended_cross_slot_nested_callback_multi_thread() {
    let path = tmp_sock("a2ext");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    c.set_timeout(Some(Duration::from_secs(3)));
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let cb_a = make_service(Arc::new(AtomicI64::new(0)));
    let cb_b = make_service(Arc::new(AtomicI64::new(0)));

    let r1 = Arc::clone(&root);
    let cba = cb_a.clone();
    let h1 = std::thread::spawn(move || r1.roundtrip(&cba).expect("rt thread 1"));
    let r2 = Arc::clone(&root);
    let cbb = cb_b.clone();
    let h2 = std::thread::spawn(move || r2.roundtrip(&cbb).expect("rt thread 2"));

    assert_eq!(h1.join().expect("join 1"), "rt:ping");
    assert_eq!(h2.join().expect("join 2"), "rt:ping");

    drop(root);
    drop(c);
}

/// 300 oneways interleaved with twoway echoes on a 2-slot pool all arrive; see module doc.
#[test]
fn pool_oneway_fifo_under_concurrent_twoway_multi_outgoing() {
    let path = tmp_sock("a3one");
    let counter = Arc::new(AtomicI64::new(0));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(1);
    server.set_max_threads(2);
    server
        .set_root(make_service(counter.clone()))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);

    let c = RpcSession::setup_unix_client_android13plus(&path, 1).expect("connect");
    let sid = c.get_session_id().expect("get_session_id");
    let slot2 = c
        .add_outgoing_connection_with_config(RpcClientConfig::unix(&path, 1).session_id(&sid))
        .expect("slot 2");
    assert_ne!(slot2, 1, "second slot has a fresh id");

    let root = Arc::new(EchoProxy(c.get_root().expect("get_root")));
    let n = 300i64;

    let r1 = Arc::clone(&root);
    let bump_h = std::thread::spawn(move || {
        for _ in 0..n {
            r1.bump().expect("oneway bump");
        }
    });
    let mut echo_handles = Vec::new();
    for t in 0..4 {
        let r = Arc::clone(&root);
        echo_handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                let msg = format!("a3-t{t}-i{i}");
                assert_eq!(r.echo(&msg).expect("echo"), msg);
            }
        }));
    }
    bump_h.join().expect("bump thread");
    for h in echo_handles {
        h.join().expect("echo thread");
    }

    assert!(
        poll_until(|| root.count().unwrap() == n),
        "AC-12.3: all oneway bumps delivered across the slot pool; \
         last observed count = {}",
        root.count().unwrap()
    );

    drop(root);
    drop(c);
}

// ---- no globals anywhere in the RPC stack --------------------------

// Reads `src/rpc` at runtime, which a cross-compiled Android device lacks.
#[cfg(not(target_os = "android"))]
#[test]
fn rpc_stack_has_no_globals() {
    // src/rpc owns no process-global state; the exemptions below hold no session data.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc");
    let mut offenders = Vec::new();
    fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                scan(&p, offenders);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(&p).unwrap();
            for (lineno, line) in src.lines().enumerate() {
                let l = line.trim_start();
                if l.starts_with("//") || l.starts_with("///") || l.starts_with("*") {
                    continue;
                }
                // Only a `static` *item* counts; `&'static str` also contains "static ".
                let static_item = (l.contains("static ") || l.starts_with("static"))
                    && !l.contains("'static")
                    && !l.contains("static_assertions");
                let has_global = static_item
                    || l.contains("OnceLock")
                    || l.contains("lazy_static")
                    || l.contains("OnceCell");
                if has_global {
                    // tcp_debug's INSECURE_WARNED latch is the documented exception.
                    if name == "tcp_debug.rs" && line.contains("INSECURE_WARNED") {
                        continue;
                    }
                    // proxy.rs `OnceLock` is a per-`RpcProxy` descriptor stamp.
                    if name == "proxy.rs" && l.contains("OnceLock") && !static_item {
                        continue;
                    }
                    // `DRIVING`: a per-thread nesting marker with no session data.
                    if name == "session.rs" && line.contains("DRIVING") {
                        continue;
                    }
                    offenders.push(format!("{name}:{}: {}", lineno + 1, line.trim()));
                }
            }
        }
    }
    scan(&dir, &mut offenders);
    assert!(
        offenders.is_empty(),
        "RPC stack must own all state per-session, found globals:\n{}",
        offenders.join("\n")
    );
}

// ---- accepted peer identity is the CLIENT --------------------------

/// A forked client's accepted socket reports the child's pid, never the server's; see module doc.
#[test]
#[cfg(unix)]
fn unix_accepted_peer_identity_is_the_client_not_self() {
    use rsbinder::rpc::transport::UnixTransport;
    use rsbinder::rpc::{PeerIdentity, RpcTransport};

    // Child role: connect, hold the connection briefly, exit.
    if let Ok(path) = std::env::var("RSB_RPC_PEERID_CLIENT") {
        let _s = std::os::unix::net::UnixStream::connect(&path).expect("child connect");
        std::thread::sleep(Duration::from_millis(1500));
        std::process::exit(0);
    }

    let path = tmp_sock("peerid");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "unix_accepted_peer_identity_is_the_client_not_self",
            "--nocapture",
        ])
        .env("RSB_RPC_PEERID_CLIENT", &path)
        .spawn()
        .expect("spawn client child");

    let (stream, _addr) = listener.accept().expect("accept");
    let t = UnixTransport::from_stream(stream).expect("from_stream");
    let id = t.peer_identity();

    let self_pid = std::process::id() as i32;
    let child_pid = child.id() as i32;
    match id {
        PeerIdentity::Local { pid, .. } => {
            assert_ne!(
                pid, self_pid,
                "AC-9.1: accepted peer must NOT be the server itself \
                 (the §0 forged-self defect)"
            );
            // LOCAL_PEERPID / SO_PEERCRED give the exact pid (`-1` only on BSDs CI never runs).
            assert_eq!(
                pid, child_pid,
                "accepted peer pid must be the client child's pid"
            );
        }
        other => panic!("expected Local peer identity for an accepted UDS, got {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
}

// ---- RPC death notification (link/unlink_to_death over RPC) --------

struct DeathFlag(std::sync::mpsc::SyncSender<()>);
impl rsbinder::DeathRecipient for DeathFlag {
    fn binder_died(&self, _who: &rsbinder::WIBinder) {
        // Best-effort: the receiver may already be gone on a late fire.
        let _ = self.0.try_send(());
    }
}

/// Server death fires a linked `DeathRecipient` (AOSP `RpcState::sendObituaries`); see module doc.
#[test]
fn rpc_death_recipient_fires_on_session_drop() {
    if let Ok(path) = std::env::var("RSB_RPC_DEATH_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }

    let path = tmp_sock("death");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "rpc_death_recipient_fires_on_session_drop",
                "--nocapture",
            ])
            .env("RSB_RPC_DEATH_SERVER", &path)
            .spawn()
            .expect("spawn server child"),
    );
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = client.get_root().expect("get_root");

    // An unlinked recipient must NOT fire; a linked one must.
    let (tx_live, rx_live) = std::sync::mpsc::sync_channel::<()>(1);
    let (tx_dead, rx_dead) = std::sync::mpsc::sync_channel::<()>(1);
    let live: Arc<DeathFlag> = Arc::new(DeathFlag(tx_live));
    let dead: Arc<DeathFlag> = Arc::new(DeathFlag(tx_dead));

    let weak_live: std::sync::Weak<dyn rsbinder::DeathRecipient> = Arc::downgrade(&live) as _;
    let weak_dead: std::sync::Weak<dyn rsbinder::DeathRecipient> = Arc::downgrade(&dead) as _;

    // Nothing reads this session yet, so its loss would go unnoticed (AOSP: no incoming threads).
    assert_eq!(
        root.link_to_death(weak_live.clone()),
        Err(rsbinder::StatusCode::InvalidOperation)
    );
    // The "incoming thread": recorded before the thread starts, so the links below never race it.
    let serve = client.spawn_serve().expect("spawn_serve");

    root.link_to_death(weak_live.clone()).expect("link live");
    root.link_to_death(weak_dead.clone()).expect("link dead");
    // Single-position removal: only `dead` is unlinked.
    root.unlink_to_death(weak_dead).expect("unlink dead");
    assert!(
        matches!(
            root.unlink_to_death(Arc::downgrade(&dead) as _),
            Err(rsbinder::StatusCode::NameNotFound)
        ),
        "a second unlink of an already-removed recipient is NameNotFound"
    );

    // Kill the server ⇒ socket closes ⇒ client serve loop ends ⇒ obituary delivered.
    child.0.kill().expect("kill server");
    child.0.wait().expect("reap server");

    rx_dead
        .recv_timeout(Duration::from_secs(5))
        .expect_err("unlinked recipient must NOT receive binder_died");
    rx_live
        .recv_timeout(Duration::from_secs(5))
        .expect("linked recipient must receive binder_died on session drop");

    // After the obituary, a new link is DEAD_OBJECT (AOSP parity).
    let (tx_late, _rx_late) = std::sync::mpsc::sync_channel::<()>(1);
    let late: Arc<DeathFlag> = Arc::new(DeathFlag(tx_late));
    assert!(
        matches!(
            root.link_to_death(Arc::downgrade(&late) as _),
            Err(rsbinder::StatusCode::DeadObject)
        ),
        "link_to_death after the obituary must be DeadObject"
    );

    let _ = serve.join();
    let _ = std::fs::remove_file(&path);
}

/// `death_signal` completes on connection drop (plan 11-1 AC-11-1.2, RPC leg); see module doc.
#[cfg(feature = "tokio")]
#[test]
fn rpc_death_signal_completes_on_session_drop() {
    if let Ok(path) = std::env::var("RSB_RPC_DEATH_SIGNAL_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }

    let path = tmp_sock("death_signal");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "rpc_death_signal_completes_on_session_drop",
                "--nocapture",
            ])
            .env("RSB_RPC_DEATH_SIGNAL_SERVER", &path)
            .spawn()
            .expect("spawn server child"),
    );
    wait_for_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = client.get_root().expect("get_root");
    // The "incoming thread" requirement, as in the test above: before the link.
    let serve = client.spawn_serve().expect("spawn_serve");
    let signal = rsbinder::death_signal(&root).expect("death_signal on a live RPC proxy");

    // Own runtime: completion must cross from the session thread via the `oneshot` waker.
    let (tx_done, rx_done) = std::sync::mpsc::sync_channel::<()>(1);
    let awaiting = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(signal);
        let _ = tx_done.try_send(());
    });

    child.0.kill().expect("kill server");
    child.0.wait().expect("reap server");

    rx_done
        .recv_timeout(Duration::from_secs(5))
        .expect("death_signal must complete when the session connection drops");
    let _ = awaiting.join();

    // Already dead: not an error, and the future is complete on arrival.
    let late = rsbinder::death_signal(&root).expect("death_signal on a dead proxy is not an error");
    let (tx_late, rx_late) = std::sync::mpsc::sync_channel::<()>(1);
    let late_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(late);
        let _ = tx_late.try_send(());
    });
    rx_late
        .recv_timeout(Duration::from_secs(5))
        .expect("a death_signal for an already-dead binder must be complete at once");
    let _ = late_thread.join();

    let _ = serve.join();
    let _ = std::fs::remove_file(&path);
}

// ---- opt-in authorization hook -------------------------------------

/// `set_authorizer`: a rejecting hook closes before any RPC byte, an accepting one is transparent.
#[test]
fn authorizer_gate_rejects_before_any_rpc_byte() {
    use rsbinder::rpc::PeerIdentity;

    // (1) Rejecting hook ⇒ connection refused, no RPC exchanged.
    {
        let path = tmp_sock("authz_no");
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        server.set_authorizer(|_peer| false);
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        client.set_timeout(Some(Duration::from_secs(3)));
        assert!(
            client.get_root().is_err(),
            "a rejecting authorizer must close the connection (zero RPC bytes)"
        );
        // _cu (scope-end) handles teardown.
    }

    // (2) Accepting hook (inspects the real PeerIdentity) ⇒ transparent.
    {
        let path = tmp_sock("authz_yes");
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        server.set_authorizer(|peer| matches!(peer, PeerIdentity::Local { .. }));
        let bg = server.run_background();
        let _cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
        wait_for_sock(&path);
        let client = RpcSession::setup_unix_client(&path).expect("connect");
        let root = EchoProxy(client.get_root().expect("get_root (authorized)"));
        assert_eq!(root.echo("authorized").unwrap(), "authorized");
        // _cu (scope-end) handles teardown.
    }
}

// ---- plan 2-20 Phase A: slot roles + fast-fail --------------------------

/// A server holding one client's callback via `hold()`, for driving it from outside any handler.
struct HeldSetup {
    client: RpcSession,
    root: EchoProxy,
    /// The client's local callback (`EchoSvc` by default: `TX_ECHO`/`TX_BUMP`/`TX_SLOW` answer).
    cb: SIBinder,
    /// `cb`'s `bump()` counter (oneway delivery check).
    cb_counter: Arc<AtomicI64>,
    /// The server's proxy to `cb`; `Option` so a test can `take()` it out of this `Drop` type.
    server_cb: Option<SIBinder>,
    held: Arc<Mutex<Option<SIBinder>>>,
    /// The server's root as a local binder, to hand back to the client inside a callback.
    root_local: SIBinder,
    server: Arc<RpcServer>,
    /// Must stay last: fields drop in order, and its join waits for `client` and the proxies.
    _cu: ServeCleanup,
}
/// Close the client's incoming threads first, before `ServeCleanup`'s `terminate` runs under them.
impl Drop for HeldSetup {
    fn drop(&mut self) {
        self.client.close_session();
    }
}
/// Everything `boot_held_cfg` can vary.
struct HeldCfg {
    /// android-13+ profile (needed for any multi-connection option).
    a13: bool,
    incoming: u32,
    fan_out: u32,
    max_threads: u32,
    /// Custom callback object; default an `EchoSvc`.
    cb: Option<SIBinder>,
    /// The server's `set_idle_timeout`.
    idle: Option<Duration>,
}
impl Default for HeldCfg {
    fn default() -> Self {
        Self {
            a13: false,
            incoming: 0,
            fan_out: 1,
            max_threads: 1,
            cb: None,
            idle: None,
        }
    }
}
fn boot_held(tag: &str) -> HeldSetup {
    boot_held_cfg(tag, HeldCfg::default())
}
fn boot_held_cfg(tag: &str, cfg: HeldCfg) -> HeldSetup {
    let path = tmp_sock(tag);
    let held = Arc::new(Mutex::new(None));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    if cfg.a13 {
        server.set_android13plus(2);
    }
    server.set_max_threads(cfg.max_threads);
    server.set_idle_timeout(cfg.idle);
    let root_local = make_service_with_hold(Arc::new(AtomicI64::new(0)), Arc::clone(&held));
    server.set_root(root_local.clone()).expect("set_root");
    let bg = server.run_background();
    let cu = ServeCleanup::new(Arc::clone(&server), bg, path.clone());
    wait_for_sock(&path);
    let client = if cfg.a13 {
        RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix(&path, 2)
                .outgoing_connections(cfg.fan_out)
                .incoming_connections(cfg.incoming),
        )
        .expect("connect android13plus")
    } else {
        RpcSession::setup_unix_client(&path).expect("connect")
    };
    // A setup panic must still close the session, or its incoming threads leak into the suite.
    struct ClientGuard(Option<RpcSession>);
    impl Drop for ClientGuard {
        fn drop(&mut self) {
            if let Some(c) = self.0.take() {
                c.close_session();
            }
        }
    }
    let mut guard = ClientGuard(Some(client));
    let client_ref = guard.0.as_ref().expect("client");
    let root = EchoProxy(client_ref.get_root().expect("get_root"));
    let cb_counter = Arc::new(AtomicI64::new(0));
    let cb = cfg
        .cb
        .unwrap_or_else(|| make_service(Arc::clone(&cb_counter)));
    root.hold(&cb).expect("hold");
    let server_cb = held
        .lock()
        .unwrap()
        .clone()
        .expect("server parked the callback");
    // Setup succeeded: hand ownership to `HeldSetup`, which shuts it down.
    let client = guard.0.take().expect("client");
    HeldSetup {
        client,
        root,
        cb,
        cb_counter,
        server_cb: Some(server_cb),
        held,
        root_local,
        server,
        _cu: cu,
    }
}
impl HeldSetup {
    fn cb_proxy(&self) -> SIBinder {
        self.server_cb.clone().expect("server callback proxy")
    }
}
fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}
/// `TX_SLOW` on a proxy.
fn drive_slow(cb: &SIBinder, ms: i32) -> Result<()> {
    let rp = rpc_of(cb);
    let mut d = rp.build_request(DESC)?;
    d.write(&ms)?;
    let mut r = rp
        .transact(TX_SLOW, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    read_status(&mut r)
}
/// `TX_ROUNDTRIP` on `cb` with `target`: the callee calls `target.echo("ping")` back.
fn drive_roundtrip(cb: &SIBinder, target: &SIBinder) -> Result<String> {
    let rp = rpc_of(cb);
    let mut d = rp.build_request(DESC)?;
    d.write(target)?;
    let mut r = rp
        .transact(TX_ROUNDTRIP, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    read_status(&mut r)?;
    r.read::<String>()
}
/// Drive `cb` (a proxy) with `TX_ECHO` (twoway) or `TX_BUMP` (oneway).
fn drive(cb: &SIBinder, oneway: bool) -> Result<()> {
    let rp = (**cb)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("RpcProxy");
    let mut d = rp.build_request(DESC)?;
    if oneway {
        rp.transact(TX_BUMP, &d, rsbinder::FLAG_ONEWAY).map(|_| ())
    } else {
        d.write(&"x")?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        let got: String = r.read()?;
        assert_eq!(got, "x");
        Ok(())
    }
}

/// AC-20.1: no `Outgoing` slot ⇒ an out-of-handler call fails at once (`WOULD_BLOCK`), slot intact.
#[test]
fn outside_handler_call_without_outgoing_slot_fails_fast() {
    let h = boot_held("a_fastfail");
    for oneway in [false, true] {
        // Off-thread: a server session has no reply deadline, so a `find_conn` park would hang.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cb = h.cb_proxy();
        let driver = std::thread::spawn(move || {
            let _ = tx.send(drive(&cb, oneway));
        });
        let r = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                panic!("oneway={oneway}: must fail immediately, still blocked")
            }
            // A broken channel is `drive`'s assertion panic, not a block: re-raise it.
            Err(RecvTimeoutError::Disconnected) => match driver.join() {
                Err(p) => std::panic::resume_unwind(p),
                Ok(()) => panic!("oneway={oneway}: the driving thread sent no result"),
            },
        };
        assert!(
            matches!(r, Err(StatusCode::WouldBlock)),
            "oneway={oneway}: expected WouldBlock, got {r:?}"
        );
    }
    assert_eq!(h.root.echo("still in sync").unwrap(), "still in sync");
    assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
}

/// AC-20.9: an out-of-handler transact never claims the served slot between two messages.
#[test]
fn served_slot_never_taken_by_outside_transact() {
    let h = boot_held("a_theft");
    let stop = Arc::new(AtomicBool::new(false));
    let progressed = Arc::new(AtomicBool::new(false));
    let hammer = {
        let root = EchoProxy(h.client.get_root().expect("root"));
        let stop = Arc::clone(&stop);
        let progressed = Arc::clone(&progressed);
        std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::SeqCst) {
                let s = format!("m{n}");
                assert_eq!(root.echo(&s).expect("echo under contention"), s);
                n += 1;
                progressed.store(true, Ordering::SeqCst);
            }
            n
        })
    };
    // First echo first: the fast-fail loop could finish before the client thread runs.
    let spun_up = Instant::now();
    while !progressed.load(Ordering::SeqCst) {
        assert!(
            spun_up.elapsed() < Duration::from_secs(10),
            "the client thread never completed an echo"
        );
        std::thread::yield_now();
    }
    for i in 0..200 {
        // Bounded: an outside call that claims the slot would park here forever.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cb = h.cb_proxy();
        std::thread::spawn(move || {
            let _ = tx.send(drive(&cb, i % 2 == 1));
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(StatusCode::WouldBlock)) => {}
            other => panic!("attempt {i}: expected WouldBlock, got {other:?}"),
        }
    }
    stop.store(true, Ordering::SeqCst);
    // The join surfaces a hammer-thread panic (a failed echo under contention).
    hammer.join().expect("hammer thread");
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// `timeout`, and the deprecated `handshake_timeout` in its place, bound a silent peer's handshake.
#[test]
#[allow(deprecated)] // The deprecated setter is still honored, so it is still checked.
fn handshake_timeout_bounds_a_silent_peer() {
    let path = tmp_sock("hs_silent");
    // A raw listener that accepts and says nothing: the peer this deadline exists for.
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let keep = Arc::new(Mutex::new(Vec::new()));
    let acceptor = {
        let keep = Arc::clone(&keep);
        std::thread::spawn(move || {
            // Held open: an EOF would let the client fail for the wrong reason.
            while let Ok((sock, _)) = listener.accept() {
                keep.lock().unwrap().push(sock);
            }
        })
    };
    wait_for_sock(&path);

    let via_timeout: fn(&std::path::Path) -> RpcClientConfig<'_> =
        |p| RpcClientConfig::unix(p, 2).timeout(Duration::from_millis(300));
    let via_handshake_timeout: fn(&std::path::Path) -> RpcClientConfig<'_> =
        |p| RpcClientConfig::unix(p, 2).handshake_timeout(Duration::from_millis(300));
    for (name, config) in [
        ("timeout", via_timeout),
        ("handshake_timeout", via_handshake_timeout),
    ] {
        // Channel deadline: an unbounded connect must fail the test, not hang the run.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let p = path.clone();
        std::thread::spawn(move || {
            let r = RpcSession::setup_client_android13plus_with_config(config(&p));
            let _ = tx.send(r.map(|_| ()));
        });
        let r = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{name} must bound the handshake; still blocked"));
        assert_eq!(
            r,
            Err(StatusCode::TimedOut),
            "{name}: a silent peer's handshake"
        );
    }

    // A leftover handshake `SO_RCVTIMEO` would silently bound every later reply wait.
    let path2 = tmp_sock("hs_ok");
    let server = RpcServer::setup_unix_server(&path2).expect("bind");
    server.set_android13plus(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let bg = server.run_background();
    let _cu = ServeCleanup::new(Arc::clone(&server), bg, path2.clone());
    wait_for_sock(&path2);
    let client = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path2, 2).handshake_timeout(Duration::from_millis(300)),
    )
    .expect("connect");
    let root = EchoProxy(client.get_root().expect("root"));
    // Longer than the handshake deadline: a leaked deadline fails here.
    assert_eq!(root.slow(500).map(|_| "ok"), Ok("ok"));
    client.close_session();

    // The session's timeout bounds an incoming attach, whose `"cci"` the silent peer never writes.
    let founded =
        RpcSession::setup_client_android13plus_with_config(RpcClientConfig::unix(&path2, 2))
            .expect("connect");
    founded.set_timeout(Some(Duration::from_millis(300)));
    let sid = founded.get_session_id().expect("session id");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let p = path.clone();
    std::thread::spawn(move || {
        let r = founded
            .add_incoming_connection_with_config(RpcClientConfig::unix(&p, 2).session_id(&sid));
        let _ = tx.send(r.map(|_| ()));
        founded.close_session();
    });
    let r = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the session's timeout must bound an attach's handshake; still blocked");
    assert_eq!(r, Err(StatusCode::TimedOut));

    drop(keep);
    std::mem::drop(acceptor);
    let _ = std::fs::remove_file(&path);
}

/// AC-20.10: dropping the parked proxy outside a handler neither blocks nor desyncs (module doc).
#[test]
fn dec_strong_outside_handler_does_not_block_or_desync() {
    let h = boot_held("a_dec");
    assert_eq!(
        h.client.local_node_count(),
        1,
        "the parked callback is the one local node"
    );
    let held = Arc::clone(&h.held);
    let mut h = h;
    drop(h.server_cb.take());
    let t0 = Instant::now();
    std::thread::spawn(move || {
        *held.lock().unwrap() = None;
    })
    .join()
    .expect("drop thread");
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "dropping a proxy outside a handler must not block: {:?}",
        t0.elapsed()
    );
    // The wire on the served slot is intact.
    for i in 0..20 {
        let s = format!("tick{i}");
        assert_eq!(h.root.echo(&s).unwrap(), s);
    }
    assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
}

// ---- plan 2-20 Phase B/C: client incoming connections ------------------

/// AC-20.2: one incoming conn serves out-of-handler callbacks (twoway, oneway), fan-out too.
#[test]
fn outside_handler_callback_completes() {
    for (fan_out, max_threads) in [(1, 1), (2, 2)] {
        let h = boot_held_cfg(
            &format!("b_complete{fan_out}"),
            HeldCfg {
                a13: true,
                incoming: 1,
                fan_out,
                max_threads,
                ..Default::default()
            },
        );
        assert_eq!(h.client.__incoming_thread_count(), 1);
        let cb = h.cb_proxy();
        let worker = std::thread::spawn(move || (drive(&cb, false), drive(&cb, true)));
        let (twoway, oneway) = worker.join().expect("worker");
        assert_eq!(
            twoway,
            Ok(()),
            "fan_out={fan_out}: twoway outside a handler"
        );
        assert_eq!(
            oneway,
            Ok(()),
            "fan_out={fan_out}: oneway outside a handler"
        );
        let counter = Arc::clone(&h.cb_counter);
        assert!(
            poll_until(|| counter.load(Ordering::SeqCst) == 1),
            "fan_out={fan_out}: the oneway never reached the callback"
        );
        // The served connection is untouched by all of that.
        assert_eq!(h.root.echo("still").unwrap(), "still");
        assert_eq!(h.root.roundtrip(&h.cb).unwrap(), "rt:ping");
    }
}

/// AC-20.3: concurrent server threads: one incoming conn serialises them, two run in parallel.
#[test]
fn outside_handler_parallel_callbacks() {
    let h = boot_held_cfg(
        "b_par1",
        HeldCfg {
            a13: true,
            incoming: 1,
            ..Default::default()
        },
    );
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let cb = h.cb_proxy();
            std::thread::spawn(move || (0..3).map(|_| drive(&cb, false)).collect::<Vec<_>>())
        })
        .collect();
    for w in workers {
        for r in w.join().expect("worker") {
            assert_eq!(r, Ok(()));
        }
    }
    // Serial ≥ 2000 ms vs parallel < 1600 ms: 400 ms margin, 600 ms concurrent-load headroom.
    let t0 = Instant::now();
    let a = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    let b = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    assert_eq!(a.join().unwrap(), Ok(()));
    assert_eq!(b.join().unwrap(), Ok(()));
    assert!(
        t0.elapsed() >= Duration::from_millis(2000),
        "one incoming connection must serialise: {:?}",
        t0.elapsed()
    );
    drop(h);
    // … and in parallel on two (the default server budget is 2 × max_threads).
    let h = boot_held_cfg(
        "b_par2",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__slot_count(), 3);
    let t0 = Instant::now();
    let a = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    let b = {
        let cb = h.cb_proxy();
        std::thread::spawn(move || drive_slow(&cb, 1000))
    };
    assert_eq!(a.join().unwrap(), Ok(()));
    assert_eq!(b.join().unwrap(), Ok(()));
    assert!(
        t0.elapsed() < Duration::from_millis(1600),
        "two incoming connections must run in parallel: {:?}",
        t0.elapsed()
    );
}

/// AC-20.4: a callback handler's call back to the server re-enters the incoming slot.
#[test]
fn nested_call_from_callback_handler() {
    let h = boot_held_cfg(
        "b_nested",
        HeldCfg {
            a13: true,
            incoming: 1,
            ..Default::default()
        },
    );
    let cb = h.cb_proxy();
    let root = h.root_local.clone();
    let got = std::thread::spawn(move || drive_roundtrip(&cb, &root))
        .join()
        .expect("worker");
    assert_eq!(got, Ok("rt:ping".to_string()));
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// A callback outside a handler whose reply outlasts the idle period keeps the session up.
#[test]
fn a_callback_reply_slower_than_the_idle_period_is_not_idle() {
    let h = boot_held_cfg(
        "b_idlecb",
        HeldCfg {
            a13: true,
            incoming: 1,
            idle: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    );
    let cb = h.cb_proxy();
    // No frame on any connection for three idle periods while the client's handler runs.
    let got = std::thread::spawn(move || drive_slow(&cb, 900))
        .join()
        .expect("worker");
    assert_eq!(got, Ok(()));
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// A client that only waits for callbacks is idle: its session ends (`set_idle_timeout` rustdoc).
#[test]
fn a_client_that_only_waits_for_callbacks_is_idle() {
    let h = boot_held_cfg(
        "b_idlewait",
        HeldCfg {
            a13: true,
            incoming: 1,
            idle: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    );
    // Past two idle periods with the incoming connection open and nothing sent either way.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(h.root.echo("late"), Err(StatusCode::DeadObject));
}

/// Callback whose `TX_ECHO` calls the server's `slow(ms)` back before it answers.
struct SlowNest {
    /// The client's proxy to the server root.
    root: Arc<Mutex<Option<SIBinder>>>,
    ms: i32,
}
impl Interface for SlowNest {}
impl Remotable for SlowNest {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        let a: String = r.read()?;
        let root = self.root.lock().unwrap().clone().expect("root installed");
        match EchoProxy(root).slow(self.ms) {
            Ok(()) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&a)
            }
            Err(e) => reply.write(&Status::from(e)),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A nested call the server dispatches inside a callback's reply wait is activity too.
#[test]
fn a_nested_dispatch_in_a_callback_reply_wait_is_not_idle() {
    let root = Arc::new(Mutex::new(None));
    let cb: SIBinder = Interface::as_binder(&Binder::new(SlowNest {
        root: Arc::clone(&root),
        ms: 900,
    }));
    let h = boot_held_cfg(
        "b_idlenest",
        HeldCfg {
            a13: true,
            incoming: 1,
            cb: Some(cb),
            idle: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    );
    *root.lock().unwrap() = Some(h.root.0.clone());
    let cb = h.cb_proxy();
    // The nested `slow(900)` rides the callback connection; the founding one stays quiet.
    let got = std::thread::spawn(move || drive(&cb, false))
        .join()
        .expect("worker");
    assert_eq!(got, Ok(()));
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// Handler calls back in; from a oneway it must not pin (AOSP `RpcConnection::allowNested`).
struct NestFromHandler {
    /// The client's proxy back to the server root.
    root: Mutex<Option<SIBinder>>,
    /// One slot per dispatch, so the test can tell twoway from oneway.
    done: std::sync::mpsc::SyncSender<std::result::Result<String, StatusCode>>,
}
impl Interface for NestFromHandler {}
impl Remotable for NestFromHandler {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // `TX_ECHO` is twoway (reply expected), `TX_BUMP` oneway.
        let echoed = if code == TX_ECHO {
            Some(r.read::<String>()?)
        } else {
            None
        };
        let root = self.root.lock().unwrap().clone().expect("root installed");
        let _ = self.done.try_send(EchoProxy(root).echo("nested"));
        match echoed {
            Some(a) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&a)
            }
            None => Ok(()),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}
/// `Binder<T>` wants a value; forward to the shared `NestFromHandler`.
struct NestFromHandlerRef(Arc<NestFromHandler>);
impl Interface for NestFromHandlerRef {}
impl Remotable for NestFromHandlerRef {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        self.0.on_transact(c, r, reply)
    }
    fn on_dump(&self, w: &mut dyn std::io::Write, a: &[String]) -> Result<()> {
        self.0.on_dump(w, a)
    }
}
#[test]
fn nested_call_from_oneway_callback_handler() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let holder = Arc::new(NestFromHandler {
        root: Mutex::new(None),
        done: tx,
    });
    let cb: SIBinder = Interface::as_binder(&Binder::new(NestFromHandlerRef(Arc::clone(&holder))));
    let h = boot_held_cfg(
        "b_onenest",
        HeldCfg {
            a13: true,
            incoming: 1,
            cb: Some(cb),
            ..Default::default()
        },
    );
    *holder.root.lock().unwrap() = Some(h.root.0.clone());

    // Control: TWOWAY; the peer's reply wait on that connection lets the nested call ride it.
    let cb_tw = h.cb_proxy();
    let tw = std::thread::spawn(move || drive(&cb_tw, false));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(Ok("nested".to_string())),
        "a nested call from a twoway handler must complete"
    );
    assert_eq!(tw.join().expect("twoway worker"), Ok(()));

    // Regression: the same handler, reached by a ONEWAY callback.
    let cb_ow = h.cb_proxy();
    std::thread::spawn(move || drive(&cb_ow, true))
        .join()
        .expect("oneway worker")
        .expect("oneway send");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(Ok("nested".to_string())),
        "a nested call from a oneway handler must not block on the \
         connection the oneway arrived on"
    );
    assert_eq!(h.root.echo("after").unwrap(), "after");
}

/// r34 + session id ⇒ `BadType`; attach + fan-out or incoming ⇒ `BadValue`, helpers included.
#[test]
fn incoming_config_validation() {
    let h = boot_held_cfg(
        "b_cfg",
        HeldCfg {
            a13: true,
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let sid = h.client.get_session_id().expect("session id");
    assert!(matches!(
        RpcSession::setup_client_android13plus_with_config(
            RpcClientConfig::unix(&path, 2)
                .session_id(&sid)
                .incoming_connections(1)
        ),
        Err(StatusCode::BadValue)
    ));
    assert!(matches!(
        h.client.add_outgoing_connection_with_config(
            RpcClientConfig::unix(&path, 2)
                .session_id(&sid)
                .incoming_connections(1)
        ),
        Err(StatusCode::BadValue)
    ));
    assert!(matches!(
        h.client.add_incoming_connection_with_config(
            RpcClientConfig::unix(&path, 2)
                .session_id(&sid)
                .outgoing_connections(2)
        ),
        Err(StatusCode::BadValue)
    ));
    // Manual attach works and is served.
    h.client
        .add_incoming_connection_with_config(RpcClientConfig::unix(&path, 2).session_id(&sid))
        .expect("manual incoming attach");
    // 1 is the founding slot; the attach must mint a fresh one.
    assert!(
        h.client.__slot_count() >= 2,
        "the attach must add a slot, not reuse the founding one"
    );
    assert_eq!(h.client.__incoming_thread_count(), 1);
    assert_eq!(h.client.__incoming_thread_live_count(), 1);
    let cb = h.cb_proxy();
    assert_eq!(
        std::thread::spawn(move || drive(&cb, false))
            .join()
            .unwrap(),
        Ok(())
    );
    // r34: no session id to echo. The path is unused (checked before `connect()`) but correct.
    let r34 = boot_held("b_cfg_r34");
    let r34_path = r34.server.path().expect("r34 server path").to_path_buf();
    assert!(matches!(
        r34.client.add_incoming_connection_with_config(
            RpcClientConfig::unix(&r34_path, 2).session_id(&[7u8; 32])
        ),
        Err(StatusCode::BadType)
    ));
}

/// Manual attach: `RpcClientConfig` calls refuse a `timeout`; released deprecated ones ignore it.
#[allow(deprecated)]
#[test]
fn a_manual_attach_with_a_session_timeout() {
    let h = boot_held_cfg(
        "b_cfg_timeout",
        HeldCfg {
            a13: true,
            // Room for the outgoing attach beside the founding slot.
            max_threads: 2,
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let sid = h.client.get_session_id().expect("session id");
    let timeout = std::time::Duration::from_secs(5);

    assert!(matches!(
        h.client.add_outgoing_connection_with_config(
            RpcClientConfig::unix(&path, 2)
                .session_id(&sid)
                .timeout(timeout)
        ),
        Err(StatusCode::BadValue)
    ));
    assert!(matches!(
        h.client.add_incoming_connection_with_config(
            RpcClientConfig::unix(&path, 2)
                .session_id(&sid)
                .timeout(timeout)
        ),
        Err(StatusCode::BadValue)
    ));

    h.client
        .add_outgoing_connection_android13plus_with_config(
            rsbinder::rpc::RpcUnixClientConfig::path(&path, 2)
                .session_id(&sid)
                .timeout(timeout),
        )
        .expect("the deprecated outgoing attach still ignores it");
    h.client
        .add_incoming_connection_android13plus_with_config(
            rsbinder::rpc::RpcUnixClientConfig::path(&path, 2)
                .session_id(&sid)
                .timeout(timeout),
        )
        .expect("the deprecated incoming attach still ignores it");
}

/// Callback slots cap at `2 * max_threads`: on a default server two are admitted, a third refused.
// Pins the deprecated `RpcUnixClientConfig` wrapper's delegation to `RpcClientConfig`.
#[allow(deprecated)]
#[test]
fn incoming_over_server_cap_is_refused() {
    let h = boot_held_cfg(
        "b_cap",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    let sid: [u8; 32] = h
        .client
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert!(poll_until(|| h.server.session_slot_count(&sid) == Some(3)));
    let path = h.server.path().expect("unix path").to_path_buf();
    let r = RpcSession::setup_unix_client_android13plus_with_config(
        rsbinder::rpc::RpcUnixClientConfig::path(&path, 2).incoming_connections(3),
    );
    // If admitted, close it: its incoming threads would outlive the panic below.
    if let Ok(s) = &r {
        s.close_session();
    }
    assert!(
        matches!(r, Err(StatusCode::DeadObject)),
        "third callback slot must be refused by the cap (server closes the \
         connection, so the attach handshake dies): {:?}",
        r.map(|_| ())
    );
}

/// `ClientOptions::incoming_connections` reaches the session; it needs the android-13+ profile.
#[test]
fn entry_client_open_with_incoming() {
    let h = boot_held_cfg(
        "b_entry",
        HeldCfg {
            a13: true,
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let uri = format!("unix://{}?profile=android13plus", path.display());
    let client = rsbinder::Client::open_with(&uri, |o, _| o.incoming_connections = Some(1))
        .expect("open with incoming");
    let session = client.session().expect("rpc session");
    // Close before asserting: an unclosed session leaks its incoming thread into the suite.
    let slots = session.__slot_count();
    let threads = session.__incoming_thread_count();
    session.close_session();
    assert_eq!(slots, 2);
    assert_eq!(threads, 1);
    let r34 = format!("unix://{}", path.display());
    assert!(matches!(
        rsbinder::Client::open_with(&r34, |o, _| o.incoming_connections = Some(1)).map(|_| ()),
        Err(StatusCode::BadValue)
    ));
}

/// AC-20.5: incoming conn ⇒ eager death; none ⇒ no death link, and a failed call ends the session.
#[test]
fn server_death_is_eager_with_incoming() {
    if let Ok(path) = std::env::var("RSB_RPC_DEATH_A13_SERVER") {
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(2);
        server
            .set_root(make_service(Arc::new(AtomicI64::new(0))))
            .expect("set_root");
        let _ = server.run(); // blocks until killed
        std::process::exit(0);
    }
    let path = tmp_sock("c_death");
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = KillOnDrop(
        std::process::Command::new(exe)
            .args([
                "--exact",
                "server_death_is_eager_with_incoming",
                "--nocapture",
            ])
            .env("RSB_RPC_DEATH_A13_SERVER", &path)
            .spawn()
            .expect("spawn server child"),
    );
    wait_for_sock(&path);
    let eager = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&path, 2).incoming_connections(1),
    )
    .expect("eager client");
    let lazy = RpcSession::setup_unix_client_android13plus(&path, 2).expect("lazy client");
    let eager_root = eager.get_root().expect("root");
    let lazy_root = lazy.get_root().expect("root");
    let (tx_e, rx_e) = std::sync::mpsc::sync_channel::<()>(1);
    let (tx_l, _rx_l) = std::sync::mpsc::sync_channel::<()>(1);
    let flag_e: Arc<DeathFlag> = Arc::new(DeathFlag(tx_e));
    let flag_l: Arc<DeathFlag> = Arc::new(DeathFlag(tx_l));
    eager_root
        .link_to_death(Arc::downgrade(&flag_e) as _)
        .expect("link eager");
    assert_eq!(
        lazy_root.link_to_death(Arc::downgrade(&flag_l) as _),
        Err(StatusCode::InvalidOperation),
        "without an incoming connection nothing would observe the drop"
    );
    child.0.kill().expect("kill server child");
    child.0.wait().expect("reap server child");
    assert!(
        rx_e.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the incoming connection's drop must fire the obituary at once"
    );
    assert!(EchoProxy(lazy_root.clone()).echo("x").is_err());
    assert_eq!(
        EchoProxy(lazy_root.clone()).echo("y"),
        Err(StatusCode::DeadObject),
        "the failed call declares the session dead"
    );
    eager.close_session();
    assert_eq!(eager.__incoming_thread_live_count(), 0);
    assert_eq!(eager.__incoming_thread_count(), 0);
    lazy.close_session();
    let _ = std::fs::remove_file(&path);
}

/// Plan 2-21 C-0: local `shutdown` is `EndedBy::Local`, `Ok(())`, parked in `recv` or in a handler.
#[test]
fn local_shutdown_ends_the_serve_loop_the_same_way_parked_or_dispatching() {
    use rsbinder::rpc::transport::UnixTransport;
    use rsbinder::rpc::{AddressSpace, EndedBy};

    for dispatching in [false, true] {
        let (srv_t, cli_t) = UnixTransport::pair().expect("socketpair");
        let server = Arc::new(
            RpcSession::new(Box::new(srv_t), AddressSpace::Acceptor).expect("server session"),
        );
        let slow_entered = Arc::new(AtomicBool::new(false));
        server
            .set_root(make_service_with_slow_signal(
                Arc::new(AtomicI64::new(0)),
                slow_entered.clone(),
            ))
            .expect("set_root");
        let serving = Arc::clone(&server);
        let serve = std::thread::spawn(move || serving.serve_blocking());
        let client =
            RpcSession::new(Box::new(cli_t), AddressSpace::Initiator).expect("client session");
        let root = EchoProxy(client.get_root().expect("root"));
        assert_eq!(root.echo("warm").unwrap(), "warm");

        let caller = dispatching.then(|| {
            let root = EchoProxy(root.0.clone());
            std::thread::spawn(move || {
                let mut d = root.rp().build_request(DESC).expect("request");
                d.write(&300i32).expect("arg");
                // Ends in an error once the session is shut down under it.
                let _ = root.rp().transact(TX_SLOW, &d, 0);
            })
        });
        if dispatching {
            assert!(
                poll_until(|| slow_entered.load(Ordering::SeqCst)),
                "the handler must be running when the session is shut down"
            );
        }
        server.close_session();
        let end = serve.join().expect("serve thread");
        assert_eq!(end.by, EndedBy::Local, "dispatching={dispatching}: {end}");
        assert!(end.is_clean(), "dispatching={dispatching}: {end}");
        assert_eq!(end.into_result(), Ok(()), "dispatching={dispatching}");
        if let Some(c) = caller {
            c.join().expect("caller thread");
        }
        client.close_session();
    }
}

/// Plan 2-21 B-4: an `AF_INET` fd in `from_preconnected_fd` handshakes via `TcpDebugTransport`.
#[cfg(feature = "rpc-tcp-debug")]
#[test]
fn preconnected_inet_fd_handshakes_over_tcp_debug() {
    use rsbinder::rpc::transport::TcpDebugTransport;
    use std::os::fd::OwnedFd;

    let listener = TcpDebugTransport::bind_loopback().expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let client_stream = std::net::TcpStream::connect(addr).expect("connect");
    let (server_stream, _) = listener.accept().expect("accept");
    let server_t = TcpDebugTransport::from_stream(server_stream).expect("server transport");

    // The server object needs a listener to exist; it is never run.
    let path = tmp_sock("pcfd");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    server.serve_connection(Box::new(server_t));

    let client = RpcSession::from_preconnected_fd(OwnedFd::from(client_stream), 2)
        .expect("android-13+ handshake over a preconnected AF_INET fd");
    assert_eq!(client.wire_protocol_version(), Some(2));
    let root = EchoProxy(client.get_root().expect("root"));
    assert_eq!(root.echo("inet").unwrap(), "inet");
    drop(root);
    drop(client);
    server.terminate();
    let _ = std::fs::remove_file(&path);
}

/// Plan 2-21 B-2: `terminate` ends connected r34 and android-13+ sessions and joins the workers.
#[test]
fn terminate_ends_every_session_and_joins_workers() {
    // r34 mints no session id (absent from the id registry); one profile per server.
    let r34_path = tmp_sock("term34");
    let r34_server = RpcServer::setup_unix_server(&r34_path).expect("bind r34");
    r34_server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let r34_bg = r34_server.run_background();
    wait_for_sock(&r34_path);
    let r34 = RpcSession::setup_unix_client(&r34_path).expect("r34 connect");
    let r34_root = EchoProxy(r34.get_root().expect("root"));
    assert_eq!(r34_root.echo("r34").unwrap(), "r34");

    // Two outgoing connections: a `Live(2)` session `terminate` must end although "still live".
    let a13_path = tmp_sock("term13");
    let a13_server = RpcServer::setup_unix_server(&a13_path).expect("bind a13");
    a13_server.set_android13plus(2);
    a13_server.set_max_threads(2);
    a13_server
        .set_root(make_service(Arc::new(AtomicI64::new(0))))
        .expect("set_root");
    let a13_bg = a13_server.run_background();
    wait_for_sock(&a13_path);
    let a13 = RpcSession::setup_client_android13plus_with_config(
        RpcClientConfig::unix(&a13_path, 2).outgoing_connections(2),
    )
    .expect("android-13+ connect");
    let a13_root = EchoProxy(a13.get_root().expect("root"));
    assert_eq!(a13_root.echo("a13").unwrap(), "a13");
    let sid: [u8; 32] = a13
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert!(
        poll_until(|| a13_server.session_live_conns(&sid) == Some(2)),
        "two outgoing connections drive the android-13+ session: {:?}",
        a13_server.session_live_conns(&sid)
    );

    // Channel deadline: a `terminate` blocked on a connected client must fail, not hang.
    for (name, server) in [("r34", &r34_server), ("android-13+", &a13_server)] {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let terminating = Arc::clone(server);
        let t = std::thread::spawn(move || {
            terminating.terminate();
            let _ = tx.send(());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("{name}: terminate blocked with a client still connected")
            }
        }
        t.join().expect("terminate thread");
    }
    r34_bg.join().expect("r34 accept loop");
    a13_bg.join().expect("android-13+ accept loop");
    // Every worker exited (its `Arc<RpcServer>` clone is gone); the a13 session is dead.
    for (name, server) in [("r34", &r34_server), ("android-13+", &a13_server)] {
        assert!(
            poll_until(|| Arc::strong_count(server) == 1),
            "{name}: a worker still holds the server ({} refs)",
            Arc::strong_count(server)
        );
    }
    assert!(
        poll_until(|| matches!(a13_server.session_live_conns(&sid), None | Some(0))),
        "the android-13+ session must be dead after terminate"
    );
    assert!(
        r34_root.echo("after").is_err(),
        "the r34 connection was ended"
    );
    assert!(
        a13_root.echo("after").is_err(),
        "the android-13+ connection was ended"
    );
    let _ = std::fs::remove_file(&r34_path);
    let _ = std::fs::remove_file(&a13_path);
}

/// Plan 2-21 B-2: `terminate` from a handler skips its own worker's handle; no deadlock.
#[test]
fn terminate_from_a_handler_does_not_join_itself() {
    const STOP_DESC: &str = "rsbinder.test.IStopper";
    struct Stopper(Mutex<Option<Arc<RpcServer>>>);
    impl Remotable for Stopper {
        fn descriptor() -> &'static str {
            STOP_DESC
        }
        fn on_transact(
            &self,
            _code: TransactionCode,
            _reader: &mut Parcel,
            reply: &mut Parcel,
        ) -> Result<()> {
            if let Some(server) = self.0.lock().unwrap().take() {
                server.terminate();
            }
            reply.write(&Status::from(StatusCode::Ok))
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
            Ok(())
        }
    }

    let path = tmp_sock("termre");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    // The call takes the server out of the service, so the strong count can reach 1.
    server
        .set_root(Interface::as_binder(&Binder::new(Stopper(Mutex::new(
            Some(Arc::clone(&server)),
        )))))
        .expect("set_root");
    let bg = server.run_background();
    wait_for_sock(&path);
    let client = RpcSession::setup_unix_client(&path).expect("connect");
    let root = client.get_root().expect("root");

    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let caller = std::thread::spawn(move || {
        let rp = (*root)
            .as_any()
            .downcast_ref::<RpcProxy>()
            .expect("RpcProxy");
        let d = rp.build_request(STOP_DESC).expect("request");
        // Errors (the transport is down before the reply); what matters is that it ends.
        let _ = rp.transact(FIRST_CALL_TRANSACTION, &d, 0);
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => panic!("terminate from a handler deadlocked"),
    }
    caller.join().expect("caller thread");
    bg.join().expect("accept loop");
    assert!(
        poll_until(|| Arc::strong_count(&server) == 1),
        "the handler's own worker must exit on its own ({} refs)",
        Arc::strong_count(&server)
    );
    client.close_session();
    let _ = std::fs::remove_file(&path);
}

/// AC-20.6: `shutdown` ends and joins the incoming threads; the server sees the session go.
#[test]
fn shutdown_joins_incoming_threads() {
    let h = boot_held_cfg(
        "c_join",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    let sid: [u8; 32] = h
        .client
        .get_session_id()
        .expect("sid")
        .as_slice()
        .try_into()
        .unwrap();
    assert_eq!(h.client.__incoming_thread_count(), 2);
    assert_eq!(h.client.__incoming_thread_live_count(), 2);
    // Channel deadline: a deadlocked shutdown must fail the test, not hang until CI timeout.
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let shutting = h.client.clone();
    let t = std::thread::spawn(move || {
        shutting.close_session();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        // A panicked `shutdown` breaks the channel too; the `join` below re-raises it.
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            // Leak `h`: its drop runs `terminate`, which joins the same wedged workers.
            std::mem::forget(h);
            panic!("shutdown deadlocked");
        }
    }
    t.join().expect("shutdown thread");
    // Only the join counter tells joining from detaching; the other two read 0 either way.
    assert_eq!(
        h.client.__incoming_thread_joined_count(),
        2,
        "shutdown must join the incoming threads, not detach them"
    );
    assert_eq!(h.client.__incoming_thread_live_count(), 0);
    assert_eq!(h.client.__incoming_thread_count(), 0);
    // Idempotent.
    h.client.close_session();
    let server = Arc::clone(&h.server);
    assert!(
        poll_until(|| server.session_slot_count(&sid).is_none_or(|n| n == 0)),
        "the server still holds slots for the shut-down session"
    );
    assert!(matches!(h.root.echo("dead"), Err(StatusCode::DeadObject)));
}

/// Shuts the session down inside `binder_died`: the cycle break the `RpcSession` docs name.
struct ShutdownOnDeath(Mutex<Option<RpcSession>>, Arc<AtomicBool>);
impl rsbinder::DeathRecipient for ShutdownOnDeath {
    fn binder_died(&self, _who: &rsbinder::WIBinder) {
        self.1.store(true, Ordering::SeqCst);
        if let Some(s) = self.0.lock().unwrap().take() {
            s.close_session();
        }
    }
}

/// Death shuts transports down before `binder_died`, so a `shutdown` there joins woken threads.
#[test]
fn shutdown_from_death_recipient_does_not_deadlock() {
    let h = boot_held_cfg(
        "c_death_sd",
        HeldCfg {
            a13: true,
            incoming: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__incoming_thread_live_count(), 2);
    let fired = Arc::new(AtomicBool::new(false));
    let recip: Arc<ShutdownOnDeath> = Arc::new(ShutdownOnDeath(
        Mutex::new(Some(h.client.clone())),
        Arc::clone(&fired),
    ));
    h.root
        .0
        .link_to_death(Arc::downgrade(&recip) as _)
        .expect("link");

    // Channel deadline: a deadlock in the obituary's `shutdown` must fail, not hang the run.
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let victim = h.client.clone();
    let t = std::thread::spawn(move || {
        victim.close_session();
        let _ = tx.send(());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => {}
        // A panicked `shutdown` breaks the channel too; the `join` below re-raises it.
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            // Wedged: dropping the fixture would block in `ServeCleanup`'s `terminate`.
            std::mem::forget(h);
            panic!("shutdown from a death recipient deadlocked");
        }
    }
    t.join().expect("shutdown thread");
    assert_eq!(h.client.__incoming_thread_live_count(), 0);
    // Without the obituary the outer `shutdown` joins the threads and all else still holds.
    assert!(
        fired.load(Ordering::SeqCst),
        "the obituary never ran; this test would gate nothing"
    );
}

/// Callback that shuts its own session down: no self-join, and the server's call gets `DeadObject`.
struct ShutdownCb(Mutex<Option<RpcSession>>);
impl Interface for ShutdownCb {}
impl Remotable for ShutdownCb {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, _c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // Echo like `EchoSvc` so `drive(.., false)` can assert on the reply.
        let a: String = r.read()?;
        if let Some(s) = self.0.lock().unwrap().as_ref() {
            s.close_session();
        }
        reply.write(&Status::from(StatusCode::Ok))?;
        reply.write(&a)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}
#[test]
fn shutdown_from_callback_handler_does_not_self_join() {
    let holder = Arc::new(ShutdownCb(Mutex::new(None)));
    let cb: SIBinder = Interface::as_binder(&Binder::new(ShutdownCbRef(Arc::clone(&holder))));
    let h = boot_held_cfg(
        "c_selfjoin",
        HeldCfg {
            a13: true,
            incoming: 1,
            cb: Some(cb),
            ..Default::default()
        },
    );
    *holder.0.lock().unwrap() = Some(h.client.clone());
    let server_cb = h.cb_proxy();
    // Channel deadline: a self-join deadlock must fail here rather than hang the run.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let worker = std::thread::spawn(move || {
        let r = drive(&server_cb, false);
        let _ = tx.send(r);
    });
    let r = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => {
            panic!("shutdown inside the handler deadlocked")
        }
        // A broken channel is `drive`'s assertion panic: re-raise it, not a deadlock.
        Err(RecvTimeoutError::Disconnected) => match worker.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => panic!("the driving thread sent no result"),
        },
    };
    // The session died before the reply went out; the point is that the call *returns*.
    assert_eq!(
        r,
        Err(StatusCode::DeadObject),
        "the server's call must return once the handler's shutdown lands"
    );
    worker.join().expect("worker");
    // The handler's thread is not self-joined, so it ends slightly after `shutdown`; poll.
    assert!(poll_until(|| h.client.__incoming_thread_live_count() == 0));
    assert_eq!(h.client.__incoming_thread_count(), 0);
    h.client.close_session();
    assert!(matches!(h.root.echo("dead"), Err(StatusCode::DeadObject)));
    *holder.0.lock().unwrap() = None;
}
/// `Binder<T>` wants a value; forward to the shared `ShutdownCb`.
struct ShutdownCbRef(Arc<ShutdownCb>);
impl Interface for ShutdownCbRef {}
impl Remotable for ShutdownCbRef {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, c: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        self.0.on_transact(c, r, reply)
    }
    fn on_dump(&self, w: &mut dyn std::io::Write, a: &[String]) -> Result<()> {
        self.0.on_dump(w, a)
    }
}

/// A reply deadline ends a session that also holds a callback slot, that slot with it.
#[test]
fn a_reply_timeout_ends_a_session_with_a_callback_slot() {
    let h = boot_held_cfg(
        "c_lastout",
        HeldCfg {
            a13: true,
            incoming: 1,
            max_threads: 2,
            ..Default::default()
        },
    );
    assert_eq!(h.client.__slot_count(), 2, "founding outgoing + callback");
    // The reply deadline ends the session, the callback slot with it.
    h.client.set_timeout(Some(Duration::from_millis(50)));
    assert_eq!(h.root.slow(400), Err(StatusCode::TimedOut));
    // Death, not a live session with only callback slots left.
    assert_eq!(
        h.client.__slot_count(),
        0,
        "a reply timeout must run the death sequence"
    );
    assert_eq!(h.root.echo("after"), Err(StatusCode::DeadObject));
}

// ---- Unix socket path ownership ------------------------------------------

fn file_id(path: &std::path::Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).expect("stat");
    (meta.dev(), meta.ino())
}

/// A regular file at the path is never removed to make room for a socket.
#[test]
fn a_unix_server_refuses_a_path_that_is_not_a_socket() {
    let path = tmp_sock("notsock");
    std::fs::write(&path, b"keep me").expect("write");
    assert_eq!(
        RpcServer::setup_unix_server(&path).err(),
        Some(StatusCode::AlreadyExists)
    );
    assert_eq!(std::fs::read(&path).expect("still there"), b"keep me");
    let _ = std::fs::remove_file(&path);
}

/// A second server cannot take a live server's path; the first keeps its socket.
#[test]
fn a_unix_server_refuses_a_path_another_server_listens_on() {
    let path = tmp_sock("live");
    let first = RpcServer::setup_unix_server(&path).expect("first server");
    let bound = file_id(&path);
    assert_eq!(
        RpcServer::setup_unix_server(&path).err(),
        Some(StatusCode::Errno(
            -(rustix::io::Errno::ADDRINUSE.raw_os_error())
        ))
    );
    assert_eq!(
        file_id(&path),
        bound,
        "the first server's socket must be untouched"
    );
    std::os::unix::net::UnixStream::connect(&path).expect("the first server still answers");
    drop(first);
    assert!(!path.exists(), "the first server removes its own socket");
}

/// A socket nobody listens on is what a crashed server leaves; it is replaced.
#[test]
fn a_unix_server_replaces_a_stale_socket() {
    let path = tmp_sock("stale");
    drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
    assert!(path.exists(), "a dropped listener leaves its socket file");
    let server = RpcServer::setup_unix_server(&path).expect("a stale socket is replaced");
    drop(server);
    assert!(!path.exists());
}

/// Dropping a server whose path was taken over since does not delete the successor's socket.
#[test]
fn dropping_a_unix_server_spares_a_successor_at_the_same_path() {
    let path = tmp_sock("successor");
    let first = RpcServer::setup_unix_server(&path).expect("first server");
    std::fs::remove_file(&path).expect("unlink behind the first server's back");
    let second = RpcServer::setup_unix_server(&path).expect("second server");
    let successor = file_id(&path);
    drop(first);
    assert_eq!(
        file_id(&path),
        successor,
        "the successor's socket must survive"
    );
    drop(second);
    assert!(!path.exists());
}

// ---- send state: a parcel owns its binder bumps until it is sent -----------

/// A request refused with `WouldBlock` keeps its bump; the same parcel resent delivers a live node.
#[test]
fn resend_after_would_block_delivers_a_live_node() {
    let held = Arc::new(Mutex::new(None));
    let h = boot_held_cfg(
        "resend_wb",
        HeldCfg {
            a13: true,
            cb: Some(make_service_with_hold(
                Arc::new(AtomicI64::new(0)),
                Arc::clone(&held),
            )),
            ..Default::default()
        },
    );
    let path = h.server.path().expect("unix path").to_path_buf();
    let sid = h.client.get_session_id().expect("session id");
    // The root the client holds is the server's only node so far.
    let base = h.server.live_session_node_count();

    let ctr = Arc::new(AtomicI64::new(0));
    let svc = make_service(Arc::clone(&ctr));
    let cb = h.cb_proxy();
    let rp = rpc_of(&cb);
    let mut d = rp.build_request(DESC).expect("build_request");
    d.write(&svc).expect("write the server's local binder");
    assert_eq!(
        h.server.live_session_node_count(),
        base + 1,
        "the write took its bump"
    );

    // No outgoing slot on the server yet: `WouldBlock`, from outside any handler.
    let first = std::thread::scope(|s| s.spawn(|| rp.transact(TX_HOLD_CB, &d, 0)).join().unwrap());
    assert_eq!(first.err(), Some(StatusCode::WouldBlock));
    assert_eq!(
        h.server.live_session_node_count(),
        base + 1,
        "a request never sent keeps its bump; the same parcel is going out again"
    );

    h.client
        .add_incoming_connection_with_config(RpcClientConfig::unix(&path, 2).session_id(&sid))
        .expect("manual incoming attach");
    let second = std::thread::scope(|s| s.spawn(|| rp.transact(TX_HOLD_CB, &d, 0)).join().unwrap());
    assert!(
        second.is_ok(),
        "the same parcel is sent once the slot exists: {second:?}"
    );
    assert_eq!(
        rp.transact(TX_HOLD_CB, &d, 0).err(),
        Some(StatusCode::InvalidOperation),
        "sent: a third attempt is refused"
    );
    // The client still holds its proxy, so a drop that settled the sent bump would free the node.
    drop(d);
    assert_eq!(
        h.server.live_session_node_count(),
        base + 1,
        "a sent parcel settles nothing on drop"
    );

    // The client's proxy to `svc` names a live node: its calls run.
    let client_view = EchoProxy(
        held.lock()
            .unwrap()
            .clone()
            .expect("the client parked the server's binder"),
    );
    assert_eq!(client_view.echo("live").as_deref(), Ok("live"));
    client_view
        .bump()
        .expect("oneway on the client's outgoing slot");
    assert_eq!(client_view.count(), Ok(1), "the oneway reached `svc`");
    assert_eq!(ctr.load(Ordering::SeqCst), 1);

    // Its DEC_STRONG releases the node the send handed over.
    *held.lock().unwrap() = None;
    drop(client_view);
    assert!(
        poll_until(|| h.server.live_session_node_count() == base),
        "the node the resend delivered is released once by the peer's DEC_STRONG"
    );
}
