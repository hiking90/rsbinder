// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcSession` — RPC session driver over a pool of connections (one by default).
//!
//! Ties one [`RpcTransport`] + `R34Codec` + per-session `RpcState`
//! together and provides:
//! * client outbound transactions ([`RpcSession::get_root`], and
//!   [`super::proxy::RpcProxy::transact`]),
//! * a blocking server serve loop ([`RpcSession::serve_blocking`]),
//! * the `RpcParcelOps` bridge that lets the `SIBinder`
//!   (de)serializers marshal binders as `RpcAddress`.
//!
//! All state is owned here (no global).
//!
//! # Connection selection
//!
//! A new outbound transaction is written either reentrantly on the slot this
//! thread already drives (on a serve-driven slot: a twoway transaction only
//! while its dispatch grants `allow_nested`, a oneway never) or on a free
//! `Outgoing` slot; a `DEC_STRONG` has its own non-blocking rule ("Deferred
//! `DEC_STRONG`"). A serve-driven (incoming) slot is never
//! taken from a free scan, matching AOSP `ExclusiveConnection::find`, which
//! looks `mIncoming` up with `available = nullptr`: the peer reads that
//! socket only inside its own reply wait (and, while a send of its own there
//! waits, only for a `DEC_STRONG`: "Draining sends"), so an unsolicited
//! request is not merely delayed — once the socket buffer fills, the send
//! blocks and that slot's serve loop is locked out of its own connection.
//!
//! Each slot carries a role: `Incoming` (this end serves it, AOSP `mIncoming`: a server's
//! founding slot and attaches, a client's callback connections) or `Outgoing` (this end sends on
//! it, AOSP `mOutgoing`: a client's founding slot and fan-out, a server's callback slots). The
//! selector (AOSP `RpcSession::ExclusiveConnection::find`) returns a guard that owns the slot
//! until drop, in this order:
//!
//! 1. **Reentrant pin.** If this thread already drives a slot of this session (the `DRIVING`
//!    marker), the nested call re-enters it: a handler's callback returns on the inbound
//!    socket, a same-thread recursive call reuses the outer slot. A serve-driven pin is
//!    re-entered only while its dispatch grants `allow_nested` (AOSP
//!    `exclusiveIncoming->allowNested`, set by `AllowNestedGuard` for exactly the twoway
//!    handler's run, save-and-restore like AOSP `origAllowNested` so a oneway dispatched inside
//!    a nested reply wait restores the outer grant). A oneway never takes a serve-driven pin; a
//!    reply always does. A pin whose session ended answers `DeadObject`.
//! 2. **First free `Outgoing` slot.** Claim it (`exclusive_tid`), push the `DRIVING` marker,
//!    return. The free predicate matches this thread's own `exclusive_tid` only when it holds
//!    no pin: reaching the scan with a pin means step 1 declined it, so the outer frame still
//!    owns that `exclusive_tid`, and handing the slot back as non-reentrant would let the new
//!    guard's drop release it.
//! 3. **No `Outgoing` slot at all.** Fail at once with `WouldBlock` (AOSP `WOULD_BLOCK`,
//!    "Session has no outgoing connections"): only a peer attach can add one, so waiting would
//!    never end. The same holds when every `Outgoing` slot is held by a serve loop
//!    (`serve_blocking`, `spawn_serve`, marked `serving`): the loop keeps its slot while the
//!    peer is quiet, so the wait would end only on the peer's next frame. AOSP never reaches
//!    this, since its `join` serves incoming connections only.
//! 4. **Pool exhausted.** Wait on `slot_cv` (AOSP `mAvailableConnectionCv`), bounded by the
//!    session timeout (then `WouldBlock`). Every wake-up re-checks the lifecycle and an empty
//!    pool and returns `DeadObject`, since no attach refills a dead session.
//!
//! Oneway sends share the twoway distribution; per-object oneway order comes from the
//! `asyncNumber` counter and the receiver's replay (`super::state`), not from a slot.
//!
//! # The `DRIVING` marker
//!
//! A thread-local stack of the `(session, slot)` pairs this thread drives (outermost
//! `client_transact` / `serve_once_on_slot`). It lets a same-thread nested call re-enter the
//! slot the inbound transaction arrived on, instead of deadlocking on its `exclusive_tid` or
//! routing the callback over another slot, which would break AOSP's
//! `exclusiveIncoming->allowNested` ordering. It is a recursion marker holding no node, address
//! or refcount data (those stay in the per-session `RpcState`), mirroring the kernel path's
//! thread-local `IPCThreadState`; it is the documented exception in the
//! `rpc_stack_has_no_globals` gate. It is bound by the borrow-discipline invariant R1
//! (`thread_state` module doc): every access goes through the `stack_*` helpers, whose borrow
//! ends inside them, and no borrow is held across the user callback a nested dispatch runs.
//!
//! A proxy kept in a user `thread_local!` may drop after `DRIVING` is destroyed. The helpers
//! report that apart from an empty stack, and the rule follows the kernel path's `queue_cmd`
//! and `send_flushed`: a `DEC_STRONG` or a `try_oneway` is still sent, on a free `Outgoing` slot
//! claimed with no marker, and any other call or a reply is refused with `InvalidOperation`.
//! Treating the stack as empty instead would let a nested call made from that destructor pick a
//! slot this thread already holds and, on its guard's drop, release the outer frame's
//! `exclusive_tid`.
//!
//! # Slot pool
//!
//! One mutex (AOSP `RpcSession::mMutex`) guards the pool and one condvar wakes its waiters; the
//! lock is held to pick a slot and never across a send or recv, and each guard holds an `Arc`
//! of its transport so a slot's transport outlives any in-flight guard once the pool is cleared.
//!
//! * **Adding.** Every push notifies with `notify_all`: a new slot satisfies an any-slot waiter
//!   but not one pinned to another slot, and `Condvar` makes no FIFO promise, so `notify_one`
//!   could wake the wrong waiter. The teardown gate (`is_torn_down`) is read in the push's
//!   critical section: an attach's own pre-check is a snapshot taken before a connect and
//!   handshake, and `on_session_dead` shuts down and empties the pool once, right after
//!   `Dying`, under this lock, so a lock-free check could push onto a dying session after that
//!   clear; that slot is never shut down, its serve loop blocks in `recv` forever pinning the
//!   session graph, and the caller would confirm the attach to the peer. Refusals: `DeadObject`
//!   when torn down, `BadType` when the transport's fd passing or its peer's principal differs
//!   from the founding connection's (a live session must not read as dead), `FailedTransaction`
//!   at a cap. `ConnState::admits` is the one place that decides admission, for an r34 join, an
//!   android-13+ attach and a client's own fan-out alike. The principal is the uid of a
//!   `PeerIdentity::Local` peer, a certificate's fingerprint, a vsock cid (one VM), or none for
//!   `PeerIdentity::Anonymous`; an anonymous connection joins an anonymous founder, since there
//!   is nothing to compare (AOSP 13+ admits on the 32-byte id alone everywhere). Only a server
//!   attach bumps `live_conns` (`try_bump_live_conns`, in the same critical section): a
//!   client's outgoing slots and a server's callback slots are not serve-driven, and a client's
//!   incoming slots are served by threads the session owns.
//! * **Caps.** A server attach is capped at `max_threads` `Incoming` slots (AOSP
//!   `setMaxIncomingThreads` caps `mIncoming.size()`, not the client's callback connections).
//!   Callback slots are capped at `2 * max_threads` `Outgoing` slots, a client's fan-out never
//!   counting. Callback slots have no serve loop and are reclaimed only at teardown, so a
//!   separate pre-check and push would let concurrent attach workers overshoot the cap and an
//!   untrusted peer grow the pool and its fds without bound; check and push are one critical
//!   section.
//! * **Leaving.** A slot leaves the pool only with the session (`on_session_dead` empties it,
//!   "Session end"), with one exception: a server's callback slot whose `"cci"` did not go out
//!   is un-pushed and the session goes on. Neither end's pool holds that connection as a
//!   working slot (the client gets no `"cci"` and fails its attach), and no frame rode it. The
//!   exception is safe only while no other thread can reach the slot, so it is pushed claimed
//!   (`SlotClaim`), and `SlotClaim::retire` takes it out of the pool before the claim ends and
//!   only then shuts it down: a sender that picked a dying slot would fail its send and end the
//!   session ("Failed sends"). A client's incoming attach has no such exception once the
//!   server may hold the connection: the server pools it as a callback slot before its `"cci"`
//!   goes out, and would end the session at the first callback it sends there. So a failed
//!   incoming attach ends the session itself unless the server cannot hold the connection,
//!   which is the case in exactly two ways. The attach never got its whole header out: a
//!   pre-check refused it, the connect failed, or the header write failed (a server admits
//!   nothing before the whole header). Or the connection closed before any `"cci"` byte
//!   arrived: a server that refused the attach never pooled it, and one whose `"cci"` write
//!   failed retired it. Any `"cci"` byte shows the server pooled the slot, and a cut after it
//!   cannot be told from a reset on the path that left the server holding it. A reset before
//!   any byte counts as a close, because a refusal can produce one: a server that refuses
//!   ahead of the header (an authorizer, AOSP's `setConnectionFilter`) closes with the header
//!   unread, which the kernel reports to the client as a reset. A reset on the path before the
//!   first `"cci"` byte is indistinguishable from it and leaves the session up. A client's
//!   outgoing attach has no close exception: libbinder pools it once it has the whole header,
//!   before it reads the client's `"cci"` (`RpcServer::establishConnection` →
//!   `RpcSession::preJoinSetup`), and ends its session when that connection fails; from
//!   `android-16.0.0_r3` it also ends its session when it refuses the attach at its
//!   `setMaxThreads` cap (`RpcSession::join` → `shutdownAndWait`), which the client sees as a
//!   close before any reply byte. A close cannot tell that server from one that refused and
//!   kept its session (rsbinder, libbinder up to `android-16.0.0_r2`), so a failure before the
//!   whole header went out leaves the session up and every later one ends it, a close
//!   included. The attach's own inconsistencies (a `max_version` below the session's, a
//!   transport unlike the founding one, an id other than the session's server-minted one) are
//!   refused before the header for that reason. The vector is private to `SlotPool` (child module
//!   `slot_pool`), which removes slots only by `unpush_retired`, `retire`'s, and
//!   `clear_at_session_end`, `on_session_dead`'s.
//!
//! # Failed sends
//!
//! One rule covers every frame written to a pooled slot: a request, a reply, and a
//! `DEC_STRONG` from any of its senders. Only a codec or protocol error
//! (`FrameTooLarge`, `Protocol`), raised before any byte, leaves the session up. Every
//! transport failure ends the session (`end_after_failed_send` → `fail_session`), as
//! libbinder's `rpcSend` does through `handleRpcError`: a write that stopped part-way left the
//! peer a header it will complete from whatever bytes come next, and one that sent nothing
//! still dropped a frame the peer's books count on (a oneway number, a `DEC_STRONG`). A send
//! deadline that expired before the first byte (`RpcError::Timeout`) ends it too: the peer did
//! not read for the whole deadline. The failing call gets `TimedOut` for an expired send
//! deadline wherever it stopped (a mid-frame `EAGAIN` would otherwise project to `WouldBlock`,
//! which says nothing was sent) and the transport error's status otherwise.
//!
//! `Protocol` includes the kernel's refusal of a frame's fds, as the uid has too many in flight
//! (`ETOOMANYREFS`). AOSP's `rpcSend` would end the session; here no byte went out, and the count
//! is of every session this process has, so one peer that stops reading ends no other peer's. The
//! call that had those fds fails, and a reply goes out as `FAILED_TRANSACTION` like an oversized
//! one.
//!
//! A write that fails takes the lifecycle out of `Live` before its slot goes back, and `send_msg`
//! and `send_msg_no_wait`, which every frame on a pooled slot passes, write nothing once it is out
//! of `Live`. So no frame can follow one that was cut, on any transport, from any thread: the
//! one that cut it, through a pin, included. (The reaper's write has its own check: its claim of
//! a slot looks at the lifecycle, and nothing else writes there while it holds the slot.)
//!
//! `send_dec_strong` and `RpcProxy::__try_oneway` run under whatever lock their callers hold, so
//! a failure there runs no death sequence on that thread. It only stops the session
//! (`stop_for_the_reaper`): `force_dying` and every connection cut that has a `shutdown_handle`;
//! the reaper runs the rest (`ReaperJob::End`, "Deferred `DEC_STRONG`"). A send that found no room
//! and sent nothing is no failure when it does not wait: the frame is still owed, and the session
//! goes on.
//!
//! # Reply deadlines
//!
//! * The reply deadline (`set_timeout`) bounds only the outermost reply wait, and its expiry
//!   ends the session ("Session end"): the late `REPLY` carries no id to tell it from the next
//!   call's, and a connection left unread would stall the peer's next write on it (plan 2-24
//!   D2). The call gets `TimedOut`, the others in flight `DeadObject`. A guard arms it
//!   and, on every exit (return, `?`, panic), restores the read deadline it replaced: the one an
//!   enclosing guard on this thread set on the same transport, else the slot's baseline, never
//!   `None` by default. A call made inside a reply wait on the same connection (a `Drop` that a
//!   `DEC_STRONG` read there releases, which AOSP also runs inline, `RpcState.cpp:1455`) thus
//!   leaves the outer wait its deadline. A callback made from a twoway handler rides the serve
//!   connection through the `DRIVING` pin, and clearing would disable that connection's idle
//!   deadline for good (the serve loop arms it once, not per frame). A deadline that cannot be
//!   armed because the peer has closed (XNU refuses `SO_RCVTIMEO` once both directions are
//!   shut) is skipped: a read there returns what is left and then the end of stream, so the
//!   call ends as `DeadObject`. Serve slots' baseline is the server's
//!   `set_idle_timeout` on the android-13+ path; an r34 server's serve slots and all other
//!   slots have none. With no reply deadline set, a twoway call on this session by a thread
//!   whose `DRIVING` stack holds one of this session's serve slots waits under the baseline
//!   (`deadline.or(baseline)`, armed explicitly, not inherited from whatever the socket holds)
//!   on whichever slot it leaves by: the serve slot for a twoway handler, a callback slot for a
//!   oneway one (its dispatch forbids the serve slot's reuse), and the same slot again for a
//!   handler nested in either wait. Such a thread runs a handler, or whatever else its serve
//!   loop runs on it, such as a local object's `Drop` released by a `DEC_STRONG` the loop read.
//!   Expiry is a reply timeout that ends the session like any other. A call from a thread that
//!   drives none of this session's serve slots, work a handler hands to another thread
//!   included, has no such default, and a call on another session follows that session's own
//!   deadline and baseline.
//! * A nested inbound call dispatched during that wait is forward progress, not a stall, and
//!   time-bounding its reply write could leave a half-frame; so the deadline is lifted for the
//!   dispatch and restored by `Drop`, which an early `?` or panic cannot skip. It lifts only
//!   this transport's deadline: a handler that transacts on a different session waits on that
//!   session's deadline, forever if it has none, and the outer caller waits with it. A
//!   multi-session relay sets a deadline on every session it transacts through.
//! * A handshake is bounded when its caller gives it a deadline: an attach by the session's
//!   `set_timeout`, a config connect by `RpcClientConfig::timeout`; `from_preconnected_fd` arms a
//!   fixed 10 s, and the transport-taking `connect_android13plus*` entries arm none. That
//!   deadline covers both directions, as the server arms both for its half. It is cleared on
//!   drop: the reply guard restores only a deadline it armed, and arms none when the session has
//!   neither a timeout nor an idle value (every client session), so a leftover handshake deadline
//!   would bound every later `recv` and send on the slot and break a client's callback serve loop
//!   outright.
//!
//! # Liveness
//!
//! libbinder has no timeout on the RPC path, so these are rsbinder's (plan 2-24 D4, D5).
//! `arm_liveness` sets two things on a slot's transport from two session values: the send
//! deadline (`SO_SNDTIMEO`) is the smaller of `set_timeout` and the server's idle deadline
//! (`set_serve_read_deadline`), and `RpcTransport::set_liveness` gets `set_timeout` (the kernel's
//! check on TCP; that method's rustdoc has the values and platform differences). It
//! runs on every slot as it joins the pool, after the push and outside the lock, and on every
//! slot again whenever either value changes. One lock (`SharedSession::liveness`) covers a store
//! together with its re-arm of the pool, and one slot's read of both values together with its two
//! syscalls, so the last value stored is the one every slot ends up with: a slot in the store's
//! snapshot is re-armed after the store, and a slot pushed after it reads the stored value. A
//! handshake deadline never outlives this: the connect and attach handshakes finish (and their
//! guard drops) before the transport moves into the pool, and `clear_handshake_timeouts`
//! re-arms rather than clears the send side. `on_session_dead` holds the same lock across
//! `shutdown_all_transports`, so an arm in progress finishes before any transport's `shutdown`
//! bounds its closing writes (TLS `close_notify`) and no arm lands between that bound and the
//! writes; an arm after it touches only transports whose `shutdown` has returned.
//!
//! # Idle
//!
//! A server's `set_idle_timeout(d)` is judged per session, not per connection (plan 2-24 D8);
//! `RpcServer::set_idle_timeout` states what it promises. Two inputs in `SharedSession` decide
//! it, and the wire has no third state to track: a call runs here, a call runs at the peer, or
//! bytes are crossing.
//!
//! * `open`, the calls in flight either way and the frames being written. An `OpenCall` is
//!   held from `dispatch_transact`'s entry to its return (a call running here, nested ones and
//!   its reply send included), from a twoway's send in `client_transact` to its return (a
//!   call running at the peer, with every dispatch nested in that wait), and by the
//!   android-13+ `send_msg` for the whole of each frame's write, of any kind (a oneway, a
//!   `DEC_STRONG`): a send to a slow reader stays activity however long its `send_raw` takes,
//!   and a stalled one fails on its send deadline (`SO_SNDTIMEO`, at most `d`) and ends the
//!   session ("Failed sends"). That bound is the transport's `set_write_timeout`: a transport
//!   that keeps the trait's no-op default gives a stalled send none, so the send stays
//!   activity and the session never idles out while it is stuck. An `OpenCall`'s drop bumps
//!   `io_gen` before the decrement, so a call or write that ended during a wait counts as
//!   activity in that wait.
//! * `io_gen`, bumped (`Relaxed`) by every `OpenCall` drop and by the android-13+ read funnel
//!   (`recv_msg`; the only framing an idle deadline is armed on) once per transport read that
//!   moved bytes, on any slot. A connection joining a server session counts too:
//!   `add_incoming_slot_capped` bumps it as a serve connection joins, and
//!   `add_callback_slot_and_init` once its `"cci"` is written, so the handshake bytes (read
//!   before the push, outside the funnel) are not quiet time. A stream ring's commits, refills
//!   and each of its waits that parks bump it too (`SessionActivity`, plan 10-7c B7).
//!
//! A serve slot's read deadline is always the full `d`. The loop records `io_gen` as each wait
//! for the next frame begins. On an expiry between frames `active_since` loads `open` first
//! (`SeqCst`, so the end bump of a call whose decrement it reads is visible) and then
//! `io_gen`: with a call open or the count moved it records the new count and reads on under
//! the same `d`; otherwise the end stands as an idle eviction (`Local`, `InSync`), which ends
//! the session. A wait spans `d`, so an eviction lands at least `d` and less than `2d` after
//! the last activity. An expiry part-way through a frame the loop reads (`DeadlineMidFrame`)
//! is a lost position and ends the session ("Session end"), so a gap inside such a frame is
//! bounded by `d`, as on a one-connection session. A twoway call on this session from a thread
//! driving one of its serve slots waits under `deadline.or(baseline)` ("Reply deadlines"), and
//! a frame read in that wait is bounded by the same deadline. A server-side stream wait that
//! does not ping moves no byte. Outside a handler it holds no call either, so it is idle;
//! inside one, the handler's `OpenCall` stays held for the whole wait, so the session is not
//! idle and only the wait's own deadline bounds it. On a ring each wait that parks is activity
//! (a wait longer than the ring's short spin always parks), so such waits shorter than `d`,
//! repeated, keep a stalled ring stream's session from idling. No
//! timer thread is involved: each quiet slot wakes on its own deadline. The admission deadline
//! on an r34 server's first frame is not an idle deadline and is not extended.
//!
//! # Attach confirmation
//!
//! An outgoing attach (a connection whose header echoes a server-minted `session_id`) gets no
//! acknowledgement: AOSP `RpcServer.cpp` writes `RpcNewSessionResponse` only for
//! `requestingNewSession`, and an outgoing connection's `"cci"` flows client to server. A peer
//! that refuses (unknown or stale id, its `set_max_threads` cap spent, shutdown, a teardown
//! race) can only close the socket, and the client would keep a dead slot until an unrelated
//! call lands on it. So the attach sends one `GET_SESSION_ID`, which libbinder answers on any
//! connection, and requires the reply to carry the id it echoed. A failed probe is still a
//! connection the server may hold, or a refusal that ended the server's session, so it ends
//! the session, a close included ("Slot pool" "Leaving"). The incoming (callback)
//! direction needs no probe: the server writes `"cci"` after admitting it (plan 2-20), so a
//! client that fails once the server may hold it ends the session ("Slot pool" "Leaving").
//!
//! Both directions echo only the session's own server-minted id, which the client keeps from
//! its first `get_session_id` (AOSP `setupClient` attaches with its own `mId`). Any other id is
//! `BadValue` before the connect: another session's id would add this session's connection
//! to a server session it did not found, splitting one server session across two client
//! states with separate oneway numbering, binder addresses and lifetimes.
//!
//! The crate-internal `session_id()` is this session's own 32 bytes (AOSP `RpcSession::mId`,
//! `kSessionIdBytes == 32`). On a server session it is the id minted at session build, replied
//! by `GET_SESSION_ID` and used as the `RpcServer` registry key. On a client session it is NOT
//! the peer's id: the client mints it locally and never puts it on the wire (it exists so a
//! client session that serves callbacks can answer `GET_SESSION_ID`), and the server's id is a
//! different 32 bytes that only `get_session_id` (one round trip, AOSP `setupClient` →
//! `readId()`) returns. An attach given it is refused with `BadValue` before connecting, as
//! every id other than the server-minted one is.
//!
//! # One inner per session
//!
//! One `RpcSessionInner` owns a session's whole slot pool (AOSP `mOutgoing` and `mIncoming` in
//! one `Vec`; the `DRIVING` pin keeps it wire-equivalent to a split pool) and one
//! `SharedSession` holds its nodes, root, id and lifecycle. The 32-byte session id is an attach
//! capability: a peer that echoes it joins this session's state, so it is CSPRNG-minted and
//! never logged. A server resolves the echoed id through `RpcServer.sessions`, which holds a
//! `Weak` of the founding inner, and adds the connection there as a serve-driven slot; it never
//! builds a second inner over the same shared state, because proxies minted by one inner are
//! refused by another's `write_binder`, every cached `RpcProxy` must point at the one inner,
//! and a worker's nested `proxy.transact` must select within its own pool. The cap check, the
//! anti-resurrection gate and the push are one critical section, and a torn-down session
//! refuses the attach with `DeadObject` rather than resurrect. An attach whose handshake
//! settled on another wire version is refused: the profile is fixed per session.
//!
//! # Callback slots
//!
//! A callback connection carries requests in the direction the founding connection does not,
//! so a binder handed across the session can be called outside a dispatch. One physical
//! connection is `Incoming` to the client that opened it and `Outgoing` to the server:
//!
//! | This endpoint | Founding slot | Callback slots |
//! |---|---|---|
//! | Initiator (client) | `Outgoing` | `Incoming` |
//! | Acceptor (server) | `Incoming` | `Outgoing` |
//!
//! A default one-connection session thus counts zero callback connections on both ends, which
//! is what `TransportCaps::CALLBACKS` needs.
//!
//! A server-side callback slot mirrors the peer's `mIncoming` (AOSP `RpcServer.cpp`:
//! `addOutgoingConnection(client, init=true)` for `incoming` headers) and does not bump the
//! lifecycle count (not serve-driven; AOSP does not gate lifetime on `mOutgoing.size()`).
//! Admission comes first and the server's `"cci"` second, so a refused client sees an error
//! instead of a silently dead connection (the accept handshake defers that write; see
//! `wire_android13::server_write_connection_init`); the slot is held by the admitting thread
//! while `"cci"` goes out, so no callback overtakes it. A failed `"cci"` retires the slot
//! before the hold ends ("Slot pool" "Leaving"): a dead slot would count against the budget
//! and, as the first free `Outgoing` slot, draw the next callback.
//!
//! # Deferred `DEC_STRONG`
//!
//! Every `DEC_STRONG` this end owes, but for a twoway's target (below), goes through
//! `send_dec_strong`: a proxy's drop, an excess receipt, one of our own binders coming home, on the
//! android-13+ wire the target of each inbound oneway, and a proxy's release held until the peer
//! paid back the sends of its address (`super::state` module doc "Ref-count model"). The route
//! below never orders a release after a send on another connection; the hold does.
//!
//! `RpcProxy::drop` runs on arbitrary user threads, under whatever locks they hold, so a
//! `DEC_STRONG` never waits for a slot, and its write waits for no peer. A transport with a send
//! that does not wait (`__send_no_wait`) has the frame taken whole or not at all. On a free slot
//! part of a taken frame can stay in the transport for what next holds the slot: a send writes it
//! first, a serve loop before it reads, and the reaper, which never waits for a holder, since the
//! holder's guard queues it again ("A send that does not wait" in `transport::unix`). A slot's
//! holder reads it next and a read writes nothing, so a pin keeps no part of a frame: it writes
//! only where `__whole_small_frame_now` says a small frame goes whole, which the kernel promises on
//! Linux and Android `AF_UNIX`, and which `POLLOUT` tells on their TCP and TLS. There only a kernel
//! short of memory stops a small send, and the holder finishes what that left on the spot, the one
//! wait on this path, bounded by the send deadline. vsock's `POLLOUT` means one byte of the peer's
//! credit, so a peer could cut a frame there, TLS over vsock included, and Apple's kernels were not
//! read for this: a pin holds on both. A transport that cannot send without waiting finds no room
//! on every release, so a pin holds and the others take step 4: a custom transport without that send, and TLS over a
//! `TlsStream` with no `socket`. On every transport a failed write runs no death sequence on this
//! thread ("Failed sends").
//!
//! It writes only where the peer is reading. A peer reads a connection none of its serve loops
//! drives inside its reply wait, and, if it drains, while a send of its own there waits for room
//! ("Draining sends"); at any other time nothing reads it, so frames written there fill the socket
//! buffer until the writer blocks and its serve loop stops reading too. `dec_route` picks, in this
//! order:
//!
//! 1. **The pin, when the peer reads it**: an `Outgoing` pin, which the peer serves, or an
//!    `Incoming` pin whose dispatch grants `allow_nested`, where the peer waits for the reply of
//!    the twoway being handled. That covers a twoway handler, the arguments it received (the
//!    request parcel drops before the reply, as AOSP destroys `data` before it, so their
//!    `DEC_STRONG`s precede it), and every binder entered while a reply is read. What finds no
//!    room, or all of it where the transport cannot say (above), is held on the pin: step 3 for an
//!    `Incoming` one. An `Outgoing` pin's hold goes before the slot's next request or `REPLY` and,
//!    whichever comes first, as its holder gives the slot back, on whatever route the thread then
//!    has. A serve loop keeps its slot, so it writes the hold before its next read. A handler that
//!    waits for what its `drop` does at the peer waits past a hold until its reply (AOSP writes
//!    at once): on a full socket anywhere, and always where the pin cannot write.
//! 2. **A free `Outgoing` slot**, one with no `exclusive_tid` at all (claiming one this thread
//!    already drives would, on the guard's drop, clear an `exclusive_tid` the outer frame still
//!    owns). The peer's serve loop reads it. This pays for an `Incoming` pin without
//!    `allow_nested` (a oneway handler, the accounting after a oneway drain) and for a thread
//!    that drives no slot. What finds no room goes to the reaper, or from that pin, once the reaper
//!    owes `HELD_DEC_STRONG_LIMIT` addresses, is held there (step 4 says why).
//! 3. **Held on that `Incoming` pin**, when no `Outgoing` slot is free: added to the slot's
//!    `pending_dec`, one entry per address with the amounts summed, and written just before the
//!    next `REPLY` or nested request on the slot, which the peer is waiting to read. `write_reply`
//!    writes every `REPLY` and `client_transact` every request, so they are the two flushes. AOSP
//!    `ExclusiveConnection::find` writes a
//!    `CLIENT_REFCOUNT` on the incoming connection here at once and relies on the peer's
//!    `drainCommands` (android-17.0.0_r1 `RpcSession.cpp:933-942`). The hold is rsbinder's: it
//!    keeps this handler out of a write that only the peer's next send could unblock. A peer
//!    reads nothing there while it sends nothing, and some never drain: libbinder from
//!    android-16.0.0_r4 with incoming threads, android-12. A serve loop back at its read has no
//!    reply to come on the slot, so while the session has an `Outgoing` slot no serve loop reads
//!    and the reaper owes under the limit, it hands the hold to the reaper (step 4). A
//!    peer that sends only oneways, with no callback connection, gets the held `DEC_STRONG`s
//!    with its next twoway on that connection. A hold dies with its slot, which leaves the pool
//!    only with the session, and a session end resets the peer's counts anyway. The peer picks
//!    the addresses, so a pin holds `HELD_DEC_STRONG_LIMIT` of them, which bounds a map at 16 384
//!    buckets of 37 bytes, about 600 KiB; the value is that bound, not a measured burst. A new
//!    address past that is written on the pin itself, as AOSP does. A peer that drains reads it
//!    at its next send there.
//!    What finds no room is held past the limit, and the loop that reads the slot, a serve loop or
//!    a reply wait, writes only that excess before it reads the next frame: a peer cannot raise the
//!    excess past what one dispatch drops, since nothing more is read. A serve loop holds no
//!    handler or caller lock there, and a reply wait is already waiting for this peer. One that
//!    does not drain, or sends nothing more, fills the socket buffer and leaves this worker blocked
//!    in that write until it next reads the connection or the send deadline ends the session, as
//!    AOSP leaves its own writer.
//! 4. **The reaper**, for a thread that drives no slot when no `Outgoing` slot is free, and for
//!    what found no room above: the amount is added to what the session owes that address, one
//!    entry per address, and a reaper thread per session waits for a free `Outgoing` slot and room
//!    on it. The amounts add as they do in one `RpcDecStrong` (AOSP `sendDecStrongToTarget`
//!    combines concurrent releases the same way, android-17.0.0_r1 `RpcState.cpp:887-891`), and the
//!    module doc's next paragraphs hold of the sum: it never passes what this end received. No
//!    burst of drops ends a session. What a thread that drives no slot adds is the release of a
//!    proxy it held, so what it costs is what those proxies cost. A peer's frames can only add
//!    through a thread that reads them, and each such thread stops at `HELD_DEC_STRONG_LIMIT`:
//!    from an `Incoming` pin the amount is held instead, which step 3 limits; on an `Outgoing` pin,
//!    whose slot the reaper may be waiting for, the loop that reads the slot writes what the
//!    session owes past the limit itself before it reads the next frame. An amount is never
//!    dropped: the peer's node would keep its count until the session ends. A session with no
//!    `Outgoing` slot at all (a server whose client opened no callback connection) keeps what it
//!    owes, and its book, until the next `REPLY` or nested request on a connection the peer has made
//!    a twoway call on, or the peer's attach, which wakes the reaper. A connection with no such
//!    call is left alone: an attach's probe reads one `REPLY` there. AOSP keeps the count there
//!    too: its `ExclusiveConnection::find` fails before `RpcState::sendDecStrongToTarget` takes the amount
//!    off `timesRecd` (android-17.0.0_r1 `RpcSession.cpp:293-297`). That costs an entry per address
//!    released, and the loop reading an `Incoming` slot writes what is past the limit there.
//!    A serve loop keeps its `Outgoing` slot through each read, so where every such slot is in a
//!    serve loop's read the reaper waits until the peer's next frame lets one go: a quiet peer gets
//!    the amount then or not before the session ends, with its node and this end's book open
//!    meanwhile. That loop writes only what is past the limit itself.
//!
//! On the android-13+ wire the target of a twoway is held (step 3's mechanism) from every exit that
//! replies to a target it found, whatever the status, so it goes out right before its `REPLY`, past
//! any nested call, where AOSP `flushExcessBinderRefs` sends it; it stays its own frame,
//! since a reply's fds ride its first `sendmsg` and AOSP `processDecStrong` takes none
//! (android-17.0.0_r1 `RpcState.cpp:948-983`). A oneway drain pays its targets once, after the
//! drain, as one `DEC_STRONG` with the summed amount (AOSP `RpcState.cpp:1284`); a transaction
//! still parked is paid when it runs or is dropped. The r34 wire has no amount field, so its
//! codec repeats a one-reference frame.
//!
//! While its socket has room a `DEC_STRONG` goes out synchronously, keeping the order callers rely
//! on (after a drop, the peer has processed the `DEC_STRONG` before the next reply). On a socket
//! with no room, or a transport with no send that does not wait, it goes out later, as one
//! does when no slot is free. The peer subtracts it from its
//! `timesSent`, and a later send of the same address adds to that count, so the sum never passes
//! what this end received (AOSP `processDecStrong`), and the node stays while either is
//! outstanding, at its next async number. So this end keeps that number until the address's last
//! amount is on the wire, on every route: each write pays the receipts it covers
//! (`RpcState::pay_receipts`, "Send-side async numbers"). A proxy made for the address
//! before then numbers on. The peer's abort on a binder it never sent is the hold's to prevent,
//! which `RpcState::release_proxy` decides before any route.
//!
//! A hold lasts until the peer pays. A peer that does not pay a send of an address, a target send
//! included, leaves that address's `remote_sends` and number-book entries until the session ends,
//! as AOSP keeps `sentRef`. A host of many peers bounds that with
//! `RpcServer::__limit_unpaid_addresses`, which ends the session of a peer past it. No such limit
//! holds by default: a peer's payment can wait for a read this end only does in a reply wait, so a
//! count of unpaid addresses says nothing of a peer that is not known to pay where this end reads.
//!
//! The reaper does not keep the session alive. Between jobs it holds a `Weak`; a wait on a socket
//! holds that `Weak`, the transport and its claim on the slot (`wait_without_the_session`), so the
//! last handle or proxy can drop while it waits. That drop cuts the socket the reaper waits on
//! (`reaper_wake`) and closes the channel, which ends the reaper; `close_session` wakes it by
//! shutting every connection down, and nothing joins it. The one wait with a strong reference is
//! for a slot, and a slot is only ever held by a thread with a strong reference of its own.
//! With no `shutdown_handle`, from a transport that has none or a duplicate that failed for want
//! of a descriptor, nothing can cut that wait from a drop: the reaper stays in that send until it
//! returns or `close_session` shuts the connection down, and the session is released all the same.
//!
//! The same goes for a stop with nothing to cut. Its `ReaperJob::End` waits behind the reaper's
//! write, so its recipients run when that write returns, which `close_session` can always make it
//! do: `close` shuts every connection down even when a stop already took the session out of `Live`.
//!
//! Send failures are not reported to the dropper (a dead session means the peer is gone,
//! AOSP parity), but a transport failure still ends the session ("Failed sends"), from the reaper.
//! A connection joins the pool only after its attach is confirmed, and a peer writes a `DEC_STRONG`
//! on one only once a twoway call has been made there, so `confirm_attach`, which reads nothing but
//! a `REPLY`, never meets a `DEC_STRONG`.
//!
//! `RpcProxy::__try_oneway` is the same send for a oneway, which is how `rsbinder-topic`'s hub
//! tells a stopped watcher's session nothing and waits for nothing. It takes a free `Outgoing`
//! slot or answers `WouldBlock`. What it could not send it takes back: the async number and the
//! target's send, so the frames and numbers the peer sees are those of a `transactAddress` (AOSP
//! `RpcState.cpp:638-649`), which has no "not sent, go on" path because its write waits. A number
//! that cannot be taken back, because another send reserved the next one, would park every later
//! oneway to the address at the peer (`:1099`), so that ends the session, here and in
//! `client_transact`, whose frame the codec or the kernel can refuse the same way.
//!
//! Nothing waiting in a send means nothing for the send deadline to judge, so a slot with no
//! room also queues one `ReaperJob::AwaitRoom`. The reaper claims the slot, so that no other
//! writer changes what it compares, and waits in `__wait_for_room` by the rule of a send that
//! waits ("A slow peer" in `transport`): a peer that takes a frame in time goes on, and one that
//! does not loses its session when the deadline passes, at the time a send would have. While it
//! waits, that slot's senders find no free slot. A slot a serve loop keeps or another thread holds
//! is not judged: the job ends at once, since waiting for that slot could outlast a quiet peer and
//! would hold up the reaper's other jobs. A holder's send has the deadline itself, and the next
//! `__try_oneway` to find no room queues the job again.
//!
//! # Draining sends
//!
//! A transaction's send (`client_transact`: oneway or twoway, on any slot) reads its own
//! connection while it waits for room, as AOSP `RpcState::transactAddress` does through its
//! write's `altPoll` (android-17.0.0_r1 `RpcState.cpp:703-748`; `drainCommands` at `:928-943`).
//! `RpcTransport::send_raw_draining` calls `drain_one` for each
//! message that has begun to arrive, and retries the write after it. Only a `DEC_STRONG` belongs
//! there: the peer sends a `TRANSACT` on this connection only nested in a call of ours, and a
//! `REPLY` only for one, and neither can start before this request is out. Anything else ends
//! the session with AOSP's `CONTROL_ONLY` status (`processCommand`, `RpcState.cpp:948-1005`):
//! `BadType` for a `TRANSACT`, `DeadObject` for any other command, and `BadValue` for a
//! `DEC_STRONG` whose `bodySize` is not that of the wire's `DEC_STRONG` body (`RpcDecStrong` on
//! android-13+, the 32-byte address on r34; `processDecStrong`, `:1395-1400`). It is judged from
//! the header, before any body byte, as AOSP's `getAndExecuteCommand` reads only the header
//! before `processCommand` (`:907-926`): a peer that announces a large body and never sends it
//! cannot hold the send. On r34 the rule is rsbinder's: android-12 `rpcSend` is a plain blocking
//! `send` that never drains, so the statuses are android-13+'s. A peer that stops inside a header
//! or a `DEC_STRONG` body holds the send until the send deadline.
//!
//! "Neither can start" does not hold for a send made inside this thread's own reply wait on
//! the same connection. A `Drop` that a `DEC_STRONG` read in that wait releases may transact on
//! this session, and `find_conn` hands it the connection the wait drives, as AOSP
//! `ExclusiveConnection::find` does for every use, `CLIENT_ASYNC` included (`RpcSession.cpp:948`
//! after `findConnection`, `:1001-1006`). The peer may have written the outer call's `REPLY`
//! right after that `DEC_STRONG` (AOSP flushes a target's refs just before the reply,
//! `RpcState.cpp:1301`). If the inner send then waits for room, its drain reads that `REPLY`
//! and the session ends with `DeadObject`, which the outer call returns too. AOSP does the same
//! where it drains: `doDecStrong` drops the node inline in `waitForReply` (`:1455`), the inner
//! `transactAddress` drains through `altPoll`, and `processCommand` ends the session on the
//! `REPLY` as an unknown command (`:986-996`). libbinder from android-16.0.0_r4 with incoming
//! threads does not drain; its inner send waits until the peer's serve loop reads, and the
//! `REPLY` stays for the outer wait. An inner send that never waits for room drains nothing. The
//! drain's read deadline is set back to the outer wait's when the inner send ends ("Reply
//! deadlines").
//!
//! The drain's reads are bounded by the send deadline (`set_timeout` and the idle deadline,
//! "Liveness"), armed as the read deadline at the first message drained and set back to the
//! one it replaced when the send ends ("Reply deadlines"); a peer that has closed skips the
//! arm, and the drain reads the end of stream. AOSP has no deadline on this path; rsbinder's send
//! deadline bounds a send whose peer stops reading, and a send waiting on a read the peer
//! never completes is the same stall. Each read that moves no byte for that long fails, which
//! ends the session as a failed send.
//!
//! A `DEC_STRONG`'s counts apply at once under the state lock, but the objects it releases
//! drop, and the proxy releases it lets go are sent, only after the send returns: a user `Drop`
//! may transact and a release is written on a slot, either of which would put a frame inside
//! the one being written (and `tls` holds its write lock throughout). When the send failed, the
//! session ends first ("Failed sends"), as AOSP's `rpcSend` shuts the session down before it
//! returns (`handleRpcError`, `RpcState.cpp:403-438`); the releases then drop with the peer's
//! counts instead of being written after a cut frame. A `REPLY` and a `DEC_STRONG` of its own are
//! written without draining, as AOSP's are (`rpcSend` with no `altPoll`, `RpcState.cpp:904` and
//! `:1385`). What a slot holds is written in front of a request through that request's drain, and
//! by a serve loop on an `Outgoing` slot through one of its own, since two ends that only write
//! would wait for the send deadline: those are rsbinder's. r34 has no such loop drain: an
//! android-12 oneway handler can nest a `TRANSACT` there (`RpcSession.cpp:336-338`), which a drain
//! would refuse.
//!
//! AOSP drains on every transaction from android-13.0.0_r1 to android-16.0.0_r3, and from
//! android-16.0.0_r4 only while `getMaxIncomingThreads() == 0` (17_r1 `RpcState.cpp:709`).
//! rsbinder drains on every transaction: its wait is a `poll` for room or input instead of
//! AOSP's doubling sleep, so the read adds no wait, and a peer writes a `DEC_STRONG` on this
//! connection whatever this end's incoming connections (libbinder when its other connections
//! are busy, rsbinder past its hold).
//!
//! # Session death
//!
//! `on_session_dead` runs once the lifecycle is `Dying`: it fires the obituaries, settles to
//! `Dead`, and releases every local object the peer held (AOSP `RpcState::clear`), which breaks
//! the `session -> local service -> stored proxy -> session` cycle; strong references drop
//! outside every lock because user `Drop` code may re-enter. Every step that can wake a thread
//! of this session runs before any user code, because an obituary or a local `Drop` may call
//! `close_session`, which joins those threads. Two steps are needed: the transport shutdown
//! wakes threads blocked in `recv`/`send`; emptying the pool and notifying `slot_cv` wakes
//! `find_conn_pinned` waiters, which re-check only whether their slot is still pooled
//! (`find_conn` and the reaper's selector also re-check the lifecycle). Emptying the pool this
//! early is safe: this is the only place a served slot leaves the pool, and a dead pool is
//! read only by paths that already answer `DeadObject` on `is_torn_down`. Nothing here sends.
//!
//! # A peer's close
//!
//! What reads a connection sees the peer's end of stream. A client session with no incoming
//! connection and no serve loop reads only in its calls, so a peer that closes between them goes
//! unnoticed until the next call fails. A `link_to_death` there starts a watcher thread on Linux
//! and Android (`peer_watch`): AOSP refuses such a link (`BpBinder::linkToDeath`,
//! `getMaxIncomingThreads() < 1`), so this is rsbinder's. `notices_connection_loss` keeps saying
//! whether something reads, which the reconnect helper's choice of watch reads too. The thread
//! starts under the pool lock, whose critical section also holds the copy of the socket list, its
//! flag (`ConnState::watcher`), a later slot's push, and the taking of the watcher at the session's
//! end, so no slot is missed by a race and no watcher outlives the session on a `shutdown` that does
//! nothing. A later slot whose socket cannot be duplicated, or that has none, goes unwatched, with a
//! log. The lock is held across a `dup` and a `clone`, neither of which waits for a peer.
//!
//! The watcher's signal queues `ReaperJob::PeerClosed` and the thread ends. The job records the
//! `Outgoing` slots held at that moment in `holder_wait`, and each holder's `ConnGuard::drop` takes
//! its slot out of the list in the critical section of its release; the one that empties it queues
//! `ReaperJob::PeerClosedReady`, and that job, or the first if none was held, ends the session
//! (`fail_session`). So the reaper waits for no slot and its other jobs go on, a later claim does
//! not hold the end back, and a release cannot be missed as a free moment could be. A session end
//! at the signal would lose a `REPLY` the peer wrote before it closed: a binder in it enters a
//! torn-down session as `DeadObject`, and vsock reads nothing past its own `RCV_SHUTDOWN`. That
//! holds of the watcher's end only: a failed write ends a session at once, as on any session
//! ("Failed sends"), and the call in flight may lose its `REPLY` to that. A call that starts after
//! the signal is cut by the end.
//!
//! A kept `REPLY` keeps its binders on wire v2, where a binder enters as the frame arrives
//! (`validate_objects`), with the slot held. Below v2 no position table says where a binder is, so
//! it enters as the caller reads it from the reply, after the call returned and the slot went back.
//! The end can come first, and that read is `DeadObject` (plan 2-27 R-10). A session with a reader
//! has the same race: its loop can end the session between a call's return and the read.
//!
//! The reaper reads no frame there. On android-13+ a free `Outgoing` slot holds at most the
//! `DEC_STRONG`s of a peer: the frames of a call are read by that call, a oneway does not take a
//! peer's `Incoming` pin (`find_conn_impl`; libbinder's `ExclusiveConnection::find`), and
//! `clear_local` releases what they would. On r34 it can also hold the `TRANSACT` an android-12
//! oneway handler nests there, which goes unread with the rest: the peer that wrote it has closed.
//! Reading would run a handler or a `Drop` on the reaper, and a local `Drop` there comes
//! after the obituary instead of before it. If either job finds something reading by then, it
//! leaves the end to that. A loop whose thread cannot start takes its declared count back and
//! looks again (`after_a_failed_serve_spawn`), since a signal may have gone while it counted.
//!
//! # Interface token
//!
//! On an RPC parcel (`isForRpc()`, no `kernelFields`) AOSP `Parcel::writeInterfaceToken` skips
//! the strict-mode / work-source / `kHeader` triple, which is kernel-only: "the interface
//! identification token is just its name as a string", i.e. `writeString16(descriptor)` and
//! nothing else (checked for `android-12.0.0_r34` through `android-16.0.0_r4`). rsbinder's
//! `&str` serializer is byte-identical to `writeString16`, so the token is correct against
//! libbinder on every profile; a three-int header would round-trip only rsbinder to rsbinder.
//!
//! # Binders in an RPC parcel
//!
//! The encoding follows AOSP `Parcel::flattenBinder` / `unflattenBinder`
//! (RPC branch):
//!
//! * The address after the `present` flag is r34's 32-byte `RpcAddress` on the r34 profile,
//!   and on android-13+ the 8-byte `RpcWireAddress` (`{u32 options; u32 address}`,
//!   `flattenBinder`'s `writeUint64(address)`); libbinder rejects the 32-byte form there as an
//!   "unrecognized address".
//! * The object position is captured *before* the `present`/`TYPE_BINDER`
//!   `int32`, so it points at that `int32`, and it enters the object table
//!   only at v2 (`>= INCLUDES_BINDER_POSITIONS`); a null binder gets no
//!   position. A kernel-backed parcel refuses `rpc_record_object_position`,
//!   so the kernel wire never grows a table.
//! * At v2 the receiver reads a binder only from a recorded position
//!   (`std::binary_search(mObjectPositions, objectPos)`, else `BAD_VALUE`),
//!   AOSP's `bindersInObjectPositions` gate. v0/v1/r34 record no binder
//!   positions. Interop does not need the check (a lenient decoder still
//!   round-trips); it hardens v2 conformance.
//! * A stability `int32` follows every binder, null included
//!   (`finishFlattenBinder` → `Stability::getRepr`; android-12.0.0_r34
//!   `Parcel.cpp:198-214`). libbinder's `finishUnflattenBinder` requires it:
//!   without it the short read yields a null root. rsbinder sends a local
//!   binder's declared stability, not a hardcoded 0; its default
//!   `Stability::System` is a level libbinder accepts for an RPC binder, and a
//!   binder built with `Stability::Local` holds `System` too. The level
//!   written is the binder's `IBinder::stability()`, for every kind of binder.
//!   An RPC proxy's is the first declared level it arrived with, as AOSP
//!   `getRepr` reads a `BpBinder`'s (libbinder refuses its own binder coming
//!   home with another level), or this process's level (`System`) until one
//!   arrives; so the getter and the word sent back agree. A null binder's is
//!   always `UNDECLARED` (level 0). The word follows the profile, whatever
//!   the host, Android SDK 31/32 included: android-13+ writes the bare level
//!   (`0b001100` for System; libbinder 13+ reads it as
//!   an `int16_t`, so a `Category` there is `BAD_TYPE`); r34 writes the
//!   android-12 `Category`, `version (1) | level << 24` (`0x0c000001`, a
//!   null binder `0x00000001`). The kernel parcel's choice by host SDK is a
//!   private function of `parcelable.rs`, out of this codec's reach.
//! * Reading it follows AOSP `Stability::setRepr`: a null binder with any
//!   level but `UNDECLARED` is `BAD_TYPE` (android-13+ takes the level from
//!   the word's low 16 bits, as libbinder reads an `int16_t`), and on r34 so
//!   is a `Category` whose version is below android-12's
//!   `kBinderWireFormatOldest` (1). A non-null
//!   binder's level is not checked (AOSP also requires VENDOR, SYSTEM or VINTF):
//!   an RPC proxy fills its level once, from the first word naming one of the
//!   three, as `setRepr` fills a `BpBinder`'s once. `UNDECLARED` and a level
//!   naming no `Stability` fill nothing, where the kernel path refuses the
//!   latter with `BadValue` (`Stability` rustdoc, "What rsbinder checks"). Only
//!   a word at a position the parcel received fills it: an unsent parcel read
//!   back holds the proxy's own `stability()`, not a level a peer sent. The
//!   word is read after the binder has entered, as
//!   `finishUnflattenBinder` runs after `onBinderEntering`: a refused binder
//!   stays in the parcel, which pays its receipt back when it drops.
//! * A local binder written into a parcel takes one `timesSent` bump
//!   (`RpcState::on_binder_leaving`), and the parcel owns it while unsent
//!   (AOSP `mSendState`): `Parcel::drop` hands it back through
//!   `cancel_binder_leaving`, and a successful `client_transact` or
//!   `send_reply_parcel` marks the parcel `Sent`, which leaves the bump to the
//!   peer's `DEC_STRONG`. One parcel is sent once; a second send is
//!   `InvalidOperation`, and so is sending a parcel built from received bytes
//!   (AOSP `RECEIVED`). A parcel of another session is `BadType` at the send
//!   (AOSP `RpcState::validateParcel`), as is a local binder written into a
//!   parcel that is no longer `NotSent`. A request refused before the send (`WouldBlock`,
//!   `DeadObject`, an encode failure) stays unsent, so the same parcel can be
//!   sent again without a second bump. A reply is marked `Sent` after its
//!   socket write, not before it as in AOSP, because a send refused before
//!   any byte (a codec error) keeps this session ("Failed sends") and no
//!   `clear()` follows to reclaim the node.
//!   Writing a local binder into a parcel of a torn-down session is
//!   `DeadObject` (AOSP `onBinderLeaving`, `mTerminated`). A proxy written
//!   into a parcel that is no longer `NotSent` is `InvalidOperation`: read
//!   back from a received parcel it would pass for a receipt and owe the peer
//!   a `DEC_STRONG` for a send that never happened.
//! * Reading a binder is a receipt only on a parcel that arrived from the
//!   peer (AOSP android-16.0.0_r4 `unflattenBinder`, `RECEIVED`), and only
//!   once per object position: `read_binder` enters the address (one of our
//!   nodes is paid back with `DEC_STRONG` 1 at once, a peer's resolves to its
//!   deduped proxy) and records it in the parcel's `entered` table, and a
//!   later read of the position returns the recorded binder. On any other
//!   parcel a read only looks the address up. At v2 `receive_parcel` enters
//!   every `TYPE_BINDER` position as the parcel arrives (AOSP
//!   `rpcSetDataReference`), read or not. A position that fails, a type the
//!   wire does not have and a binder in the table below v2 end the session, as
//!   AOSP ends it, so no count is left to settle. Fds other than the table names
//!   only refuse the parcel, as there: a reply fails its call, a twoway request
//!   gets a `BadValue` reply, a oneway is dropped.
//!   A parcel holding entered proxies drops outside every lock. The `parcel`
//!   module doc "RPC fields" has the table.
//!
//! # Session end
//!
//! A session ends as a whole, never one connection at a time, as libbinder's does
//! (`RpcState::handleRpcError`: "MUST ALWAYS SHUTDOWN ON ERROR"). A connection that lost a
//! frame cannot be put back in step: the wire has no frame sequence, no receipt and no reply
//! id, so a oneway number the peer never received parks every later oneway to that object in
//! its `async_todo`, and a lost `DEC_STRONG` or binder reply leaves the two ends' counts apart
//! (plan 2-24 §1.2). Three entry points take the lifecycle from `Live` to `Dying` with
//! `force_dying`, and whichever wins the edge gets `on_session_dead` run:
//!
//! - `close` is this end's decision (`close_session`, `RpcServer::terminate`, a serve loop that
//!   found the whole session idle, "Idle", and a serve loop whose own armed read deadline cut
//!   a frame, `DeadlineMidFrame`; the `end` module's table): it sets `ended_locally` first, so
//!   every loop it wakes reports `EndedBy::Local`. It runs the sequence on its own thread. If it
//!   lost the edge it shuts every connection down all the same, as the sequence may be waiting
//!   behind the reaper's write, and returns.
//! - `fail_session` is a fault on a thread free to run the sequence: a send's transport failure
//!   ("Failed sends") in `client_transact`, `write_reply` or the reaper, any failure of a reply
//!   wait after its request went out (a lost stream, an undecodable frame, an expired reply
//!   deadline, a nested dispatch that could not reply), any other serve loop end, the reaper's end
//!   at a peer's close ("A peer's close"), and a client's
//!   attach, in either direction, that fails once the server may hold the connection ("Slot pool"
//!   "Leaving"). It leaves `ended_locally` alone, so the loops it wakes report `NotLocal`. It runs
//!   the sequence on its own thread, or returns at once if it lost the edge.
//! - `stop_for_the_reaper` is the same fault on a thread under its caller's locks, a drop or
//!   `__try_oneway`: it queues `ReaperJob::End` and the reaper runs the sequence.
//!
//! A serve loop's end ends the session whichever connection it served: its worker calls
//! `close` when `SessionEnd::by` is `Local` and `fail_session` otherwise. A panic that escapes
//! a frame's dispatch (a handler's own is caught by `binder::run_handler`; this is user code
//! run by a release, such as a node's `Drop`) is caught in the loop and ends it as
//! `EndReason::Dispatch(Unknown)`: unwinding past the worker would leave the session live with
//! no reader on that connection, so a peer's call on it would wait for good. A loop's write ahead
//! of its read that fails ends it with no read, as `EndReason::Dispatch(status)`, the value a
//! failed `REPLY` gives, since that read would report only the shutdown the failure caused and a
//! lost stream would read as clean. A drain's refusal keeps its status. A write that finds the peer
//! closed (`RpcError::EndOfStream`) goes on to the read, so a peer's plain close stays clean. That
//! read runs no frame, since the session is over: what the peer queued before its close goes
//! undispatched, and the end the read comes to is the loop's. A refusal before any byte leaves the
//! session up and goes on too. A release that fails on the loop's own slot with no wait, in a
//! frame's dispatch or a drop, or on another thread between its frames, is such a write: the loop
//! ends the same way, whether or not the reaper has emptied the pool by then, except that with
//! the pool empty no read is left to tell a frame the peer cut from its close. A reply wait's write
//! of the same kind fails its call with that status. Past `Live`, a `TRANSACT` that a loop or a
//! reply wait reads goes undispatched: its target may be cleared already and its reply has no
//! reader. The death sequence
//! empties the pool, so the other loops stop with what their transport's shutdown gives them,
//! or with `EndReason::SessionEnded` when `find_conn_pinned` finds their slot gone between
//! frames; a loop that ends that way touches nothing, because whoever ended the session ran the
//! sequence. A concurrent `RpcProxy::drop`'s best-effort `DEC_STRONG` sees either the slot
//! still pooled (the send fails on the shut transport) or the lifecycle already `Dying`/`Dead`
//! (its early-out skips the send); `find_conn` re-checks the lifecycle and an empty pool on
//! every wake-up, so neither order parks it.
//!
//! `close` and `fail_session` re-enter user `Drop` code through `on_session_dead`, so a caller
//! holds no session lock. A client's incoming (callback) connection ends the session like any
//! other: AOSP's client-side `onSessionAllIncomingThreadsEnded` is a no-op, but a session that lost
//! a connection is not left running here in any direction.

use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{AccessError, LocalKey};
use std::time::{Duration, Instant};

use super::deadline::PhaseDeadline;
use super::end::{EndReason, EndedBy, ServeStep, SessionEnd};
use super::fd_mode::FileDescriptorTransportMode;
use super::lifecycle::SessionLifecycle;
use crate::binder::{SIBinder, Stability, FLAG_ONEWAY, INTERFACE_TRANSACTION, PING_TRANSACTION};
use crate::error::{Result, StatusCode};
use crate::parcel::{CopiedBinders, Parcel, RpcParcelOps};

use super::address::{
    AddressSpace, RpcAddress, SpecialTransaction, RPC_ADDR_LEN, RPC_SESSION_ID_NEW,
};
use super::proxy::RpcProxy;
use super::state::RpcState;
use super::transport::{PeerIdentity, RpcTransport};
use super::wire::{
    R34Codec, WireCodec, WireMessage, WireReply, WireReplyRef, WireTransaction, WireTransactionRef,
};
use super::wire_android13::{
    client_connect_with_id, client_read_connection_init, client_write_connection_header,
    control_only_refusal, read_aosp_message, read_aosp_message_gated, read_aosp_message_with_fds,
    read_r34_session_preamble, server_accept_deferred_init, write_aosp_message,
    write_aosp_message_with_fds, Android13PlusCodec, RawTransportIo, A13_ADDR_LEN,
    A13_DEC_STRONG_LEN, FD_MODE_NONE, FD_MODE_UNIX, PROTOCOL_V1, PROTOCOL_V2,
};
use super::{RpcError, RpcResult};

/// Server accept result: transport, codec, client fd mode, client `session_id`, `INCOMING` flag.
type Android13PlusAccept = (Box<dyn RpcTransport>, Android13PlusCodec, u8, Vec<u8>, bool);

#[derive(Clone)]
enum RpcUnixAddr {
    Path(std::path::PathBuf),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Abstract(Vec<u8>),
}

impl std::fmt::Debug for RpcUnixAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcUnixAddr::Path(path) => f.debug_tuple("Path").field(path).finish(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            RpcUnixAddr::Abstract(name) => f
                .debug_tuple("Abstract")
                .field(&format_args!("{}", name.escape_ascii()))
                .finish(),
        }
    }
}

fn unix_connect(addr: &RpcUnixAddr) -> Result<Box<dyn RpcTransport>> {
    let t = match addr {
        RpcUnixAddr::Path(path) => super::transport::UnixTransport::connect(path),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        RpcUnixAddr::Abstract(name) => super::transport::UnixTransport::connect_abstract(name),
    }?;
    Ok(Box::new(t))
}

/// Opens one connection; `Send` so a config built on one thread can be consumed on another.
type Connector = Box<dyn FnMut() -> Result<Box<dyn RpcTransport>> + Send>;

/// A session's founding connector, kept for `add_outgoing_connection`/`add_incoming_connection`.
struct Redial {
    /// Locked only around one call, never across the handshake that follows it.
    connect: Mutex<Connector>,
    handshake_timeout: Option<Duration>,
}

/// Where a config connects; the connector is built once the handshake deadline is known.
enum ClientSource {
    Unix(RpcUnixAddr),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    UnixFd(std::os::fd::OwnedFd),
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    Vsock {
        cid: u32,
        port: u32,
    },
    #[cfg(feature = "rpc-tcp-debug")]
    TcpDebug(std::net::SocketAddr),
    #[cfg(feature = "rpc-tls")]
    Tls {
        host: String,
        port: u16,
        server_name: String,
        config: std::sync::Arc<rustls::ClientConfig>,
    },
    Custom(Connector),
}

impl std::fmt::Debug for ClientSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientSource::Unix(addr) => f.debug_tuple("Unix").field(addr).finish(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ClientSource::UnixFd(fd) => f.debug_tuple("UnixFd").field(fd).finish(),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ClientSource::Vsock { cid, port } => f
                .debug_struct("Vsock")
                .field("cid", cid)
                .field("port", port)
                .finish(),
            #[cfg(feature = "rpc-tcp-debug")]
            ClientSource::TcpDebug(addr) => f.debug_tuple("TcpDebug").field(addr).finish(),
            #[cfg(feature = "rpc-tls")]
            ClientSource::Tls {
                host,
                port,
                server_name,
                ..
            } => f
                .debug_struct("Tls")
                .field("host", host)
                .field("port", port)
                .field("server_name", server_name)
                .finish_non_exhaustive(),
            ClientSource::Custom(_) => f.debug_tuple("Custom").finish_non_exhaustive(),
        }
    }
}

impl ClientSource {
    fn into_connector(self, handshake_timeout: Option<Duration>) -> Connector {
        let _ = handshake_timeout;
        match self {
            ClientSource::Unix(addr) => Box::new(move || unix_connect(&addr)),
            // The closure owns `fd`, so its number names this file for as long as the session.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ClientSource::UnixFd(fd) => Box::new(move || {
                use std::os::fd::AsRawFd;
                let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
                Ok(Box::new(super::transport::UnixTransport::connect(path)?)
                    as Box<dyn RpcTransport>)
            }),
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            ClientSource::Vsock { cid, port } => Box::new(move || {
                Ok(
                    Box::new(super::transport::VsockTransport::connect(cid, port)?)
                        as Box<dyn RpcTransport>,
                )
            }),
            #[cfg(feature = "rpc-tcp-debug")]
            ClientSource::TcpDebug(addr) => Box::new(move || {
                let tcp = tcp_connect(&addr, handshake_timeout).map_err(connect_status)?;
                Ok(
                    Box::new(super::transport::TcpDebugTransport::from_stream(tcp)?)
                        as Box<dyn RpcTransport>,
                )
            }),
            #[cfg(feature = "rpc-tls")]
            ClientSource::Tls {
                host,
                port,
                server_name,
                config,
            } => {
                let mut pinned = None;
                Box::new(move || {
                    connect_tls(
                        &host,
                        port,
                        &mut pinned,
                        &server_name,
                        &config,
                        handshake_timeout,
                    )
                })
            }
            ClientSource::Custom(connect) => connect,
        }
    }
}

/// `connect(2)`, bounded by `deadline` when there is one.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
fn tcp_connect(
    addr: &std::net::SocketAddr,
    deadline: Option<Duration>,
) -> std::io::Result<std::net::TcpStream> {
    match deadline {
        Some(d) => std::net::TcpStream::connect_timeout(addr, d),
        None => std::net::TcpStream::connect(addr),
    }
}

/// Go `net`'s floor: past the first SYN retransmit (Linux initial RTO 1 s) and its answer.
#[cfg(feature = "rpc-tls")]
const CONNECT_SHARE_FLOOR: Duration = Duration::from_secs(2);

/// One try's part of `left` with `tries` to go: an even share, at least the floor or all of `left`.
#[cfg(feature = "rpc-tls")]
fn connect_share(left: Duration, tries: u32) -> Duration {
    (left / tries).max(left.min(CONNECT_SHARE_FLOOR))
}

/// First of `addrs` to connect, all within `deadline`; each try's part is [`connect_share`].
#[cfg(feature = "rpc-tls")]
fn tcp_connect_any(
    addrs: &[std::net::SocketAddr],
    deadline: Option<Duration>,
) -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
    // `None` inside: past what an `Instant` holds, so each try waits without a deadline.
    let end = deadline.map(|d| Instant::now().checked_add(d));
    let mut last = None;
    for (i, addr) in addrs.iter().enumerate() {
        let share = match end {
            Some(Some(end)) => {
                let left = end.saturating_duration_since(Instant::now());
                let tries = u32::try_from(addrs.len() - i).unwrap_or(u32::MAX);
                let share = connect_share(left, tries);
                // `connect_timeout` refuses a zero duration with `InvalidInput`.
                if share.is_zero() {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                Some(share)
            }
            Some(None) | None => None,
        };
        match tcp_connect(addr, share) {
            Ok(t) => return Ok((t, *addr)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no address resolved")
    }))
}

/// A failed connect's status; std's own `connect_timeout` expiry has no errno, so no `Unknown`.
#[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
fn connect_status(e: std::io::Error) -> StatusCode {
    if e.kind() == std::io::ErrorKind::TimedOut {
        StatusCode::TimedOut
    } else {
        StatusCode::from(e)
    }
}

/// TCP then TLS, each bounded by `handshake_timeout`; for `pinned` see [`RpcClientConfig::tls`].
#[cfg(feature = "rpc-tls")]
fn connect_tls(
    host: &str,
    port: u16,
    pinned: &mut Option<std::net::SocketAddr>,
    server_name: &str,
    config: &std::sync::Arc<rustls::ClientConfig>,
    handshake_timeout: Option<Duration>,
) -> Result<Box<dyn RpcTransport>> {
    let tcp = match *pinned {
        Some(addr) => tcp_connect(&addr, handshake_timeout).map_err(connect_status)?,
        None => {
            use std::net::ToSocketAddrs;
            let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
            let (t, addr) = tcp_connect_any(&addrs, handshake_timeout).map_err(connect_status)?;
            *pinned = Some(addr);
            t
        }
    };
    use super::transport::TlsTransport;
    tcp.set_nodelay(true)?;
    // A deadline cut is `RpcError::Timeout`, which projects to `TimedOut`.
    let t = TlsTransport::connect_stream_within(
        Box::new(tcp),
        server_name,
        config.clone(),
        handshake_timeout,
    )?;
    Ok(Box::new(t))
}

/// What a manual attach needs out of a [`RpcClientConfig`].
struct AttachParts {
    connect: Connector,
    max_version: u32,
    session_id: Vec<u8>,
    fd_mode: FileDescriptorTransportMode,
    handshake_timeout: Option<Duration>,
}

/// android-13+ RPC client configuration, consumed by
/// [`RpcSession::setup_client_android13plus_with_config`].
///
/// The config owns what it connects with — a path, a host name, a
/// connect function — because the session it founds keeps its connector:
/// [`RpcSession::add_outgoing_connection`] and
/// [`RpcSession::add_incoming_connection`] open a further connection the
/// way the founding one was opened, with nothing to restate.
///
/// One constructor per transport: [`unix`](Self::unix),
/// `unix_abstract` and `unix_fd` (Linux/Android), `vsock` (`rpc-vsock`), `tls`
/// (`rpc-tls`), `tcp_debug` (`rpc-tcp-debug`), or [`new`](Self::new)
/// with a connect function of your own. The knobs are
/// the same whichever one built it, because the setup opens every
/// connection the same way: one connect per connection — the founding
/// one, each outgoing fan-out connection, each incoming (callback)
/// connection. That is AOSP's own shape (`RpcSession::setupClient` takes
/// a per-connection `connectAndInit`), which is why
/// `setMaxIncomingThreads` works over vsock and inet there, and why
/// incoming connections and
/// [`TransportCaps::CALLBACKS`](crate::TransportCaps::CALLBACKS) are
/// available on every transport here.
///
/// The defaults (`outgoing_connections = 1`, `incoming_connections = 0`,
/// `fd_mode` unset) reproduce a plain single-connection
/// [`RpcSession::connect_android13plus`] on the first transport, byte for
/// byte.
///
/// # Manual attach
///
/// The deprecated
/// [`add_outgoing_connection_with_config`](RpcSession::add_outgoing_connection_with_config)
/// and
/// [`add_incoming_connection_with_config`](RpcSession::add_incoming_connection_with_config)
/// open one connection on an [`RpcSession`] that already exists from a
/// config of its own. That config says how to reach the server —
/// `session_id`, which is required, and the per-connection knobs — and
/// nothing about the session, whose settings were fixed when it was
/// founded. A config that asks for more than one connection or sets
/// something session-wide is [`StatusCode::BadValue`]; `fd_mode` may
/// restate the mode the session negotiated and nothing else.
/// `session_id` must be that session's own server-minted id
/// ([`RpcSession::get_session_id`]); any other id is `BadValue` too,
/// because attaching a connection to another server session would split
/// one server session across two client states.
pub struct RpcClientConfig {
    source: ClientSource,
    max_version: u32,
    session_id: Vec<u8>,
    outgoing_connections: u32,
    incoming_connections: u32,
    fd_mode: Option<FileDescriptorTransportMode>,
    timeout: Option<Duration>,
}

impl RpcClientConfig {
    fn with_source(source: ClientSource, max_version: u32) -> Self {
        Self {
            source,
            max_version,
            session_id: Vec::new(),
            outgoing_connections: 1,
            incoming_connections: 0,
            fd_mode: None,
            timeout: None,
        }
    }

    /// Connect to a filesystem-path Unix socket, offering at most wire
    /// version `max_version` in the handshake.
    pub fn unix(path: impl AsRef<Path>, max_version: u32) -> Self {
        let path = path.as_ref().to_path_buf();
        Self::with_source(ClientSource::Unix(RpcUnixAddr::Path(path)), max_version)
    }

    /// Connect to a Linux/Android abstract Unix socket.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn unix_abstract(name: impl AsRef<[u8]>, max_version: u32) -> Self {
        let name = name.as_ref().to_vec();
        Self::with_source(ClientSource::Unix(RpcUnixAddr::Abstract(name)), max_version)
    }

    /// Connect to the Unix socket whose file `path_fd` is an `O_PATH` descriptor of, through
    /// `/proc/self/fd/<n>`. The socket file may have no name left: a server can unlink it right
    /// after it binds, and then only a process that was given a descriptor reaches the socket.
    ///
    /// The config, and then the session it founds, owns `path_fd`: every connection of the
    /// session goes through it and so to the same socket, and its number cannot name another
    /// file meanwhile, as a `/proc/self/fd` path given to [`unix`](Self::unix) can once its
    /// descriptor is closed. A connection fails with the transport's own code for what the kernel
    /// answers: `ECONNREFUSED` once the socket's listener has closed.
    ///
    /// The connect is judged by the socket file's mode bits against this process, however
    /// `path_fd` was obtained; the search permission of the directories above it is not
    /// looked at. It needs `/proc` mounted; without it the connect fails with `ENOENT`, which
    /// rsbinder reports as [`StatusCode::NameNotFound`].
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn unix_fd(path_fd: std::os::fd::OwnedFd, max_version: u32) -> Self {
        Self::with_source(ClientSource::UnixFd(path_fd), max_version)
    }

    /// Connect to a vsock server (cid 2 = the host from inside a guest;
    /// the VM's own cid from the host).
    #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
    pub fn vsock(cid: u32, port: u32, max_version: u32) -> Self {
        Self::with_source(ClientSource::Vsock { cid, port }, max_version)
    }

    /// Connect to a **plaintext** TCP server — the `rpc-tcp-debug`
    /// transport, which is for tests and bring-up, not for a network you
    /// do not control. `tls` (`rpc-tls`) is the TCP transport to ship.
    #[cfg(feature = "rpc-tcp-debug")]
    pub fn tcp_debug(addr: std::net::SocketAddr, max_version: u32) -> Self {
        Self::with_source(ClientSource::TcpDebug(addr), max_version)
    }

    /// Connect to a TCP server over TLS: `host` is a host name or an IP
    /// literal, `server_name` is the name verified against the server's
    /// certificate per `config` (a bad or untrusted certificate fails
    /// before any RPC byte), and every connection gets its own TLS
    /// session.
    ///
    /// `host` is resolved once per config: the first connection this
    /// config opens resolves it, and every further connection *it* opens
    /// goes to the address that one reached — the fan-out and incoming
    /// connections of the setup, and those
    /// [`RpcSession::add_outgoing_connection`] /
    /// [`RpcSession::add_incoming_connection`] open later. AOSP
    /// `setupInetClient` resolves once too. A second resolve could land a
    /// connection on another server behind the same name, which does not
    /// know the session id.
    #[cfg(feature = "rpc-tls")]
    pub fn tls(
        host: impl Into<String>,
        port: u16,
        server_name: impl Into<String>,
        config: std::sync::Arc<rustls::ClientConfig>,
        max_version: u32,
    ) -> Self {
        Self::with_source(
            ClientSource::Tls {
                host: host.into(),
                port,
                server_name: server_name.into(),
                config,
            },
            max_version,
        )
    }

    /// Connect with `connect`, for a transport the constructors above do
    /// not name — TLS over a Unix socket or vsock, a preconnected fd, a
    /// backend of your own.
    ///
    /// It is called once per connection and must return a transport ready
    /// for the android-13+ handshake: for TLS, connected *and*
    /// TLS-handshaken. It should bound its own blocking steps
    /// (`connect(2)`, a TLS handshake) if the peer may be silent —
    /// [`timeout`](Self::timeout) covers only the android-13+ handshake
    /// that follows here; its own doc says which constructors apply it to
    /// `connect(2)` and the TLS handshake.
    ///
    /// `connect` must be [`Send`]: the config can be built on one thread
    /// and consumed on another. A closure capturing an [`Rc`](std::rc::Rc) or
    /// another `!Send` handle does not compile here.
    ///
    /// `connect` must be `'static`: the session keeps it, so that
    /// [`RpcSession::add_outgoing_connection`] and
    /// [`RpcSession::add_incoming_connection`] can open more connections
    /// for as long as the session lives. Move what it needs into it, or
    /// share it through an [`Arc`](std::sync::Arc).
    ///
    /// ```no_run
    /// # #[cfg(all(feature = "rpc-vsock", target_os = "linux"))]
    /// # fn f() -> rsbinder::Result<()> {
    /// use rsbinder::rpc::{transport::VsockTransport, RpcClientConfig, RpcSession, RpcTransport};
    ///
    /// let session = RpcSession::setup_client_android13plus_with_config(
    ///     RpcClientConfig::new(2, || {
    ///         Ok(Box::new(VsockTransport::connect(3, 5000)?) as Box<dyn RpcTransport>)
    ///     })
    ///     .incoming_connections(1),
    /// )?;
    /// # session.close_session();
    /// # Ok(()) }
    /// ```
    pub fn new(
        max_version: u32,
        connect: impl FnMut() -> Result<Box<dyn RpcTransport>> + Send + 'static,
    ) -> Self {
        Self::with_source(ClientSource::Custom(Box::new(connect)), max_version)
    }

    /// The server-minted 32-byte session id a manual attach echoes
    /// ([`add_outgoing_connection_with_config`](RpcSession::add_outgoing_connection_with_config),
    /// [`add_incoming_connection_with_config`](RpcSession::add_incoming_connection_with_config),
    /// which require it): AOSP `RpcSession::setupClient` follow-up
    /// connections, read from
    /// [`get_session_id`](RpcSession::get_session_id) on the session they
    /// join. Default empty. The attach calls refuse any other id with
    /// [`StatusCode::BadValue`] before connecting, for the same reason as
    /// below.
    ///
    /// [`RpcSession::setup_client_android13plus_with_config`] refuses a
    /// non-empty id with [`StatusCode::BadValue`]: it builds a new
    /// `RpcSession`, which would then share one server session with the
    /// `RpcSession` that founded it while keeping its own oneway
    /// numbering, binder addresses and lifetime. The server would drop one
    /// side's oneway calls as stale, could dispatch a call meant for one
    /// side's binder to the other side's binder at the same address, and
    /// would end the shared session when either side's connection closes.
    /// AOSP has no public entry that does this: `RpcSession::setupClient`
    /// is private, and the follow-up connections that echo the id are
    /// opened by the same `RpcSession`.
    #[deprecated(
        since = "0.13.0",
        note = "only the deprecated manual attach reads it: `RpcSession::add_outgoing_connection`/`add_incoming_connection` echo the session's own id"
    )]
    pub fn session_id(mut self, session_id: &[u8]) -> Self {
        self.session_id = session_id.to_vec();
        self
    }

    /// Ask for an `n`-connection outgoing pool (AOSP `setupClient`
    /// fan-out): the setup call negotiates `min(n, server max)` and
    /// opens that many connections. Default 1 = founding connection
    /// only, skipping negotiation entirely.
    pub fn outgoing_connections(mut self, n: u32) -> Self {
        self.outgoing_connections = n;
        self
    }

    /// Open `n` **incoming (callback) connections** in addition to the
    /// outgoing pool — AOSP `RpcSession::setMaxIncomingThreads(n)`.
    /// Each one is a further connection to the same endpoint (over TLS,
    /// its own TLS session), attached with the `INCOMING` header bit,
    /// added to the server's session as a slot the server *sends* on,
    /// and served here by a dedicated thread. Without at least one, the
    /// server can reach this client's callbacks only with a **twoway**
    /// call from inside a handler that is answering one of this client's
    /// calls (a nested call); a oneway, even from inside that handler, and
    /// a call from any other server thread — a timer, a worker — fail at
    /// once with [`StatusCode::WouldBlock`] on the server (AOSP `WOULD_BLOCK`).
    ///
    /// Side effect: a session with an incoming connection detects the
    /// server's death as soon as the connection drops (obituaries fire
    /// from the serving thread), instead of on the next failed call.
    ///
    /// The loss of any one connection ends the whole session, on both ends
    /// (module doc "Session end"). A server whose
    /// [`RpcServer::set_reply_timeout`](super::RpcServer::set_reply_timeout)
    /// elapses on a slow callback handler therefore ends the session: the
    /// founding connection is closed too, every proxy gets `binder_died`,
    /// and every local object the peer held is released, although the
    /// handler was only slow. Size the server's reply timeout against the
    /// slowest legitimate handler. (AOSP has no "lost only its callback
    /// connections" state either: its client-side
    /// `WaitForShutdownListener::onSessionAllIncomingThreadsEnded` is a
    /// no-op, and an incoming thread that exits without a session
    /// shutdown aborts the process.)
    ///
    /// Requires the android-13+ profile (a session id). Bounded on the server by twice its
    /// `RpcServer::set_max_threads` value. Default 0. The threads end
    /// when the server closes the session or on
    /// [`RpcSession::close_session`]; dropping the `RpcSession` handle alone
    /// does not stop them.
    pub fn incoming_connections(mut self, n: u32) -> Self {
        self.incoming_connections = n;
        self
    }

    /// Request an fd transport mode in the connection header (AOSP
    /// `setFileDescriptorTransportMode`). Default is no fd support.
    ///
    /// Only a Unix-domain transport carries fds, so
    /// [`FileDescriptorTransportMode::Unix`] on any other is
    /// [`StatusCode::BadValue`] from the setup call. The founding
    /// connection decides it
    /// ([`RpcTransport::supports_fd_passing`]),
    /// so the rule holds for a [`new`](Self::new) connect function too.
    pub fn fd_mode(mut self, mode: FileDescriptorTransportMode) -> Self {
        self.fd_mode = Some(mode);
        self
    }

    /// How long the server may go without answering before this end counts
    /// it as broken — [`RpcSession::set_reply_timeout`] on the session this
    /// config founds, and a bound on every step that connects it (plan 2-24
    /// D4, D6). Default `None` (no deadline anywhere).
    ///
    /// - **Connecting**: each step of each connection — the `founding`
    ///   connect and every fan-out or incoming attach — is bounded by `d`:
    ///   `connect(2)` for `tcp_debug` and `tls`, the TLS handshake, and the
    ///   android-13+ handshake. For `tls`, `connect(2)` is one step across
    ///   every address `host` resolves to: each try gets an even share of the
    ///   time left but at least 2 s (all of it when less is left), so an
    ///   address refused at once leaves its share to the next and one slower
    ///   to accept than its share is given up for the next. Resolving `host`
    ///   is not bounded; it blocks in the platform's resolver. The bound is
    ///   on the step as a whole: a server that answers one byte at a time
    ///   does not stretch it, on a transport
    ///   with a [`RpcTransport::shutdown_handle`] (the bundled ones; another is
    ///   bounded per wait only). A server that accepts the socket and then
    ///   answers nothing (one at its connection cap leaves new connections
    ///   in its listen backlog, where `connect(2)` succeeds) fails the setup
    ///   call after `d` instead of hanging it. The r34 wire has no
    ///   handshake, so there it bounds `connect(2)` only. A `unix` or
    ///   `vsock` `connect(2)` is not bounded by `d`: on Linux and Android
    ///   the first waits while the listener's accept queue is full, and the
    ///   kernel bounds the second by its own connect timeout
    ///   (`SO_VM_SOCKETS_CONNECT_TIMEOUT`, 2 s by default).
    /// - **The session**: applied **as soon as it exists**, so it also
    ///   bounds the round trips this setup performs (`GET_MAX_THREADS` for a
    ///   fan-out, `GET_SESSION_ID` for any additional connection), and then
    ///   every reply wait, send and liveness check as
    ///   [`RpcSession::set_reply_timeout`] describes. An expired reply wait
    ///   ends the session.
    ///
    /// `Duration::ZERO` is not a deadline: the setup call refuses the
    /// config with [`StatusCode::BadValue`] before connecting (plan 17
    /// C-2). This is a session-wide setting, so it belongs to the call that
    /// *founds* a session and a [manual attach](Self#manual-attach) refuses
    /// it: an attach bounds its handshake by the session's own reply
    /// deadline.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// `BadValue` for a zero `timeout`.
    fn reject_zero_timeout(&self) -> Result<()> {
        reject_zero_deadline(self.timeout, "RpcClientConfig::timeout")
    }

    /// One connection, for a caller that drives the wire itself (the entry layer's r34 path).
    pub(crate) fn connect_once(self) -> Result<Box<dyn RpcTransport>> {
        self.reject_zero_timeout()?;
        (self.source.into_connector(self.timeout))()
    }
}

impl std::fmt::Debug for RpcClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClientConfig")
            .field("source", &self.source)
            .field("max_version", &self.max_version)
            .field("session_id_len", &self.session_id.len())
            .field("outgoing_connections", &self.outgoing_connections)
            .field("incoming_connections", &self.incoming_connections)
            .field("fd_mode", &self.fd_mode)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The wire a session speaks: r34 (no handshake) or android-13+; both use bare AOSP framing.
enum WireProfile {
    /// android-12 r34: the client's `int32` session-id preamble, then `R34Codec` messages.
    R34(R34Codec),
    /// android-13+: AOSP framing, codec negotiated (v0 = 13, v1 = 14/15, v2 = 16).
    Android13Plus(Android13PlusCodec),
}

impl WireProfile {
    fn codec(&self) -> &dyn WireCodec {
        match self {
            WireProfile::R34(c) => c,
            WireProfile::Android13Plus(c) => c,
        }
    }

    /// AOSP `transactInternal` `onBinderLeaving`: the peer pays a transaction's target back.
    fn counts_transaction_targets(&self) -> bool {
        matches!(self, WireProfile::Android13Plus(_))
    }

    /// A twoway's target receipt is paid just before its `REPLY` ("Deferred `DEC_STRONG`").
    fn pays_target_before_reply(&self) -> bool {
        matches!(self, WireProfile::Android13Plus(_))
    }

    /// Body size of a well-formed `DEC_STRONG` (AOSP `processDecStrong` refuses any other).
    fn dec_strong_body_len(&self) -> usize {
        match self {
            WireProfile::R34(_) => RPC_ADDR_LEN,
            WireProfile::Android13Plus(_) => A13_DEC_STRONG_LEN,
        }
    }

    /// The `int32` after every binder: r34's android-12 `Category`, android-13+'s bare level.
    fn binder_stability_repr(&self, stability: Option<Stability>) -> i32 {
        match self {
            WireProfile::R34(_) => {
                crate::binder::android12_category_repr(stability.map_or(0, Stability::level))
            }
            WireProfile::Android13Plus(_) => stability.map_or(0, Stability::level),
        }
    }

    /// AOSP `Stability::setRepr` on a received stability word, returning its level; module doc.
    fn check_binder_stability(&self, repr: i32, null: bool) -> Result<i32> {
        let level = match self {
            WireProfile::R34(_) => {
                // android-12 `kBinderWireFormatOldest` is 1: a `Category` without one is older.
                let version = repr & 0xff;
                if version < 1 {
                    log::error!("RPC: binder stability {repr:#x} has wire format version 0");
                    return Err(StatusCode::BadType);
                }
                (repr >> 24) & 0xff
            }
            // AOSP 13+ reads the word as an `int16_t`; the high 16 bits are dropped.
            WireProfile::Android13Plus(_) => i32::from(repr as i16),
        };
        // A null binder's stability must be `UNDECLARED`.
        if null && level != 0 {
            log::error!("RPC: null binder with stability {repr:#x}");
            return Err(StatusCode::BadType);
        }
        Ok(level)
    }

    /// The negotiated protocol version; `None` for R34, which has no object table.
    fn wire_version(&self) -> Option<u32> {
        match self {
            WireProfile::R34(_) => None,
            WireProfile::Android13Plus(c) => Some(c.version()),
        }
    }

    /// Binder positions at v2 (android-16) only, as AOSP `flattenBinder` records them.
    fn records_binder_positions(&self) -> bool {
        matches!(self.wire_version(), Some(v) if v >= PROTOCOL_V2)
    }

    /// FD positions at v1+: AOSP records them always, but `validateParcel` refuses objects at v0.
    fn records_fd_positions(&self) -> bool {
        matches!(self.wire_version(), Some(v) if v >= PROTOCOL_V1)
    }
}

/// AOSP `writeInterfaceToken` on an RPC parcel: only `writeString16(descriptor)` (module doc).
pub(crate) fn write_rpc_interface_token(p: &mut Parcel, descriptor: &str) -> Result<()> {
    p.write(&descriptor)?;
    Ok(())
}

/// Read and check the RPC interface token: a bare `String16` descriptor (module doc).
fn consume_rpc_interface_token(reader: &mut Parcel, expected: &str) -> Result<()> {
    crate::parcelable::read_string16_matches(reader, expected)?.map_err(|got| {
        log::error!("RPC interface token mismatch: expected '{expected}', got '{got}'");
        StatusCode::BadType
    })
}

fn write_addr(p: &mut Parcel, addr: &RpcAddress) -> Result<()> {
    // 32 bytes, 4-aligned: the r34 Parcel RPC binder encoding (caller writes the present flag).
    p.write_aligned_data(addr.as_wire_bytes().as_slice())
}

fn read_addr(p: &mut Parcel) -> Result<RpcAddress> {
    let slice = p.read_aligned_data(RPC_ADDR_LEN)?;
    let mut bytes = [0u8; RPC_ADDR_LEN];
    bytes.copy_from_slice(slice);
    Ok(RpcAddress::from_wire_bytes(bytes))
}

/// AOSP's 32-byte session id, typed because it is an attach capability; `Debug` masks it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RpcSessionId([u8; 32]);

impl RpcSessionId {
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// From wire bytes; `None` unless exactly 32 (AOSP `kSessionIdBytes`).
    pub(crate) fn try_from_slice(s: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(s).ok().map(Self)
    }
}

impl std::fmt::Debug for RpcSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Masked: the id is an attach capability, and a `{:?}` in a log line would leak it.
        f.write_str("RpcSessionId(...)")
    }
}

/// CSPRNG id, as AOSP: a guessable attach capability is a hijack; no global state is involved.
fn gen_rpc_session_id() -> RpcResult<RpcSessionId> {
    let mut id = [0u8; 32];
    // `getrandom(2)` can fail in early-boot containers: surface `RpcError::Io`, never panic.
    getrandom::fill(&mut id).map_err(|e| {
        RpcError::Io(std::io::Error::other(format!(
            "CSPRNG getrandom failed for RPC session id: {e}"
        )))
    })?;
    Ok(RpcSessionId::new(id))
}

/// C-2 rule 2 (plan 17): a zero reply, idle or handshake deadline is `BadValue`, never "none".
pub(crate) fn reject_zero_deadline(timeout: Option<Duration>, setter: &str) -> Result<()> {
    super::deadline::refuse_zero(timeout, setter).map_err(StatusCode::from)
}

thread_local! {
    /// Read deadlines this thread's guards hold, innermost last, by transport ("Reply deadlines").
    static ARMED_READ: RefCell<Vec<(usize, Option<Duration>)>> = const { RefCell::new(Vec::new()) };
}

/// `DRIVING` or `ARMED_READ`; helpers end their borrow (R1) and are `Err` during TLS teardown.
type Stack<T> = &'static LocalKey<RefCell<Vec<T>>>;
type StackResult<T> = std::result::Result<T, AccessError>;

fn stack_push<T>(key: Stack<T>, entry: T) -> StackResult<()> {
    key.try_with(|s| s.borrow_mut().push(entry))
}

/// The value of the innermost entry keyed `k`.
fn stack_find<K: PartialEq + Copy, V: Copy>(key: Stack<(K, V)>, k: K) -> StackResult<Option<V>> {
    key.try_with(|s| s.borrow().iter().rev().find(|e| e.0 == k).map(|e| e.1))
}

fn stack_any<T>(key: Stack<T>, pred: impl FnMut(&T) -> bool) -> StackResult<bool> {
    key.try_with(|s| s.borrow().iter().any(pred))
}

/// Remove the innermost entry `pred` matches; a destroyed stack has nothing to remove.
fn stack_remove_last<T>(key: Stack<T>, mut pred: impl FnMut(&T) -> bool) {
    let _ = key.try_with(|s| {
        let mut s = s.borrow_mut();
        if let Some(i) = s.iter().rposition(&mut pred) {
            s.remove(i);
        }
    });
}

fn transport_key(t: &dyn RpcTransport) -> usize {
    t as *const dyn RpcTransport as *const () as usize
}

/// The read deadline this thread's innermost live guard set on `t`, else `baseline`.
fn read_deadline_in_effect(t: &dyn RpcTransport, baseline: Option<Duration>) -> Option<Duration> {
    stack_find(&ARMED_READ, transport_key(t))
        .ok()
        .flatten()
        .unwrap_or(baseline)
}

/// Set `t`'s read deadline and record it; `false` if skipped on a closed peer ("Reply deadlines").
fn push_read_deadline(t: &dyn RpcTransport, d: Option<Duration>) -> RpcResult<bool> {
    if let Err(e) = t.set_read_timeout(d) {
        // XNU refuses `SO_RCVTIMEO` once both directions are shut; reads there end at EOF.
        return if t.peer_closed() == Some(true) {
            Ok(false)
        } else {
            Err(e)
        };
    }
    let _ = stack_push(&ARMED_READ, (transport_key(t), d));
    Ok(true)
}

/// Drop the innermost record for `t` and set `restore` back on it.
fn pop_read_deadline(t: &dyn RpcTransport, restore: Option<Duration>) {
    let key = transport_key(t);
    stack_remove_last(&ARMED_READ, |e| e.0 == key);
    // Best-effort: Drop cannot surface it, and the next caller re-arms or clears anyway.
    let _ = t.set_read_timeout(restore);
}

/// The reply read deadline, reset on every exit to what it replaced (module doc).
struct ReplyDeadlineGuard<'a> {
    transport: &'a dyn RpcTransport,
    armed: bool,
    restore: Option<Duration>,
}

impl<'a> ReplyDeadlineGuard<'a> {
    /// `baseline`: the slot's own read deadline, restored when no enclosing guard set one.
    fn arm(
        transport: &'a dyn RpcTransport,
        deadline: Option<Duration>,
        baseline: Option<Duration>,
    ) -> RpcResult<Self> {
        let restore = read_deadline_in_effect(transport, baseline);
        let armed = match deadline {
            Some(d) => push_read_deadline(transport, Some(d))?,
            None => false,
        };
        Ok(Self {
            transport,
            armed,
            restore,
        })
    }
}

impl Drop for ReplyDeadlineGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            pop_read_deadline(self.transport, self.restore);
        }
    }
}

/// Handshake socket deadlines, cleared on drop ("Reply deadlines"); `whole` bounds the step.
struct HandshakeDeadline<'a> {
    transport: &'a dyn RpcTransport,
    armed: bool,
    whole: PhaseDeadline,
}

impl<'a> HandshakeDeadline<'a> {
    fn arm(transport: &'a dyn RpcTransport, deadline: Option<Duration>) -> RpcResult<Self> {
        // Sole funnel: the `RpcTransport` trait does not promise to reject `Some(ZERO)`.
        super::deadline::refuse_zero(deadline, "a handshake deadline")?;
        let armed = match deadline {
            Some(_) => match transport
                .set_read_timeout(deadline)
                .and_then(|()| transport.set_write_timeout(deadline))
            {
                Ok(()) => true,
                // As `push_read_deadline`: XNU refuses it on a shut socket, whose reads end at EOF.
                Err(_) if transport.peer_closed() == Some(true) => false,
                Err(e) => return Err(e),
            },
            None => false,
        };
        let whole = PhaseDeadline::arm_with(deadline, || transport.shutdown_handle());
        Ok(Self {
            transport,
            armed,
            whole,
        })
    }

    /// End the step that succeeded: `Timeout` if the deadline cut the connection as it ended.
    fn finish(&mut self) -> RpcResult<()> {
        if self.whole.disarm() {
            Ok(())
        } else {
            Err(RpcError::Timeout)
        }
    }

    /// A step's failure as the deadline's when the deadline cut the connection under it.
    fn classify(&self, e: RpcError) -> RpcError {
        if self.whole.fired() {
            RpcError::Timeout
        } else {
            e
        }
    }
}

impl Drop for HandshakeDeadline<'_> {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort: `Drop` cannot report; the slot's next reader arms or clears explicitly.
            let _ = self.transport.set_read_timeout(None);
            let _ = self.transport.set_write_timeout(None);
        }
    }
}

/// Prove an attach was admitted: `GET_SESSION_ID` echoes our id; module doc "Attach confirmation".
fn confirm_attach(
    transport: &dyn RpcTransport,
    codec: &Android13PlusCodec,
    session_id: &[u8],
) -> RpcResult<()> {
    let txn = WireTransaction {
        address: RpcAddress::zero(),
        code: SpecialTransaction::GetSessionId.code(),
        flags: 0,
        async_number: 0,
        data: Vec::new(),
        object_positions: Vec::new(),
    };
    let frame = codec.encode_transact(&txn)?;
    let mut io = RawTransportIo(transport);
    write_aosp_message(&mut io, &frame)?;
    let reply = read_aosp_message(&mut io)?;
    let peer_id = match codec.decode_message(&reply)? {
        WireMessage::Reply(WireReply {
            status: 0, data, ..
        }) => {
            let mut p = Parcel::from_vec(data);
            p.set_data_position(0);
            p.read::<Vec<u8>>()
                .map_err(|_| RpcError::Protocol("malformed GET_SESSION_ID reply on an attach"))?
        }
        _ => {
            return Err(RpcError::Protocol(
                "peer did not answer GET_SESSION_ID on an attach",
            ))
        }
    };
    if peer_id == session_id {
        Ok(())
    } else {
        Err(RpcError::Protocol(
            "peer put this attach in a different session than the id it echoed",
        ))
    }
}

/// Explain a failed outgoing attach probe; module doc "Attach confirmation".
fn log_attach_refused(e: &RpcError) {
    if matches!(
        e,
        RpcError::Timeout | RpcError::Truncated | RpcError::DeadlineMidFrame
    ) {
        log::error!(
            "android-13+ RPC: the attach admission probe (GET_SESSION_ID) did not complete \
             ({e}) — a read deadline armed by this caller ends it this way too, so this is \
             not necessarily a refusal"
        );
        return;
    }
    log::error!(
        "android-13+ RPC: the peer refused this attach ({e}) — it no longer knows this \
         session, its outgoing-slot cap (`set_max_threads`) is spent, or it is shutting down. \
         Keep the connection count within `RpcSession::negotiate()`"
    );
}

/// Refuse an id on an entry that builds a new `RpcSession`; see `RpcClientConfig::session_id`.
fn refuse_id_on_new_session(session_id: &[u8], what: &str) -> Result<()> {
    if session_id.is_empty() {
        return Ok(());
    }
    log::error!(
        "{what}: a session id would build a second client RpcSession on a server session \
         another RpcSession founded, with its own oneway numbering, binder addresses and \
         lifetime; add the connection with `add_outgoing_connection()` on that \
         RpcSession instead"
    );
    Err(StatusCode::BadValue)
}

/// Map a new-session client handshake failure to `StatusCode`, first logging the likely r34 cause.
fn client_handshake_err(e: RpcError) -> StatusCode {
    match &e {
        RpcError::EndOfStream => log::error!(
            "rsbinder RPC: the android-13+ handshake failed at the transport ({e}) after \
                 the peer accepted the connection — it may be speaking the r34 (default) \
                 profile. Connect with `WireProfile::R34` (no `?profile=android13plus`), or \
                 enable the android-13+ wire on the server (`RpcServer::set_android13plus`)"
        ),
        RpcError::Truncated => log::error!(
            "rsbinder RPC: the android-13+ handshake failed part-way through a response \
                 ({e}) — the peer closed mid-frame; it may be speaking the r34 (default) \
                 profile. Connect with `WireProfile::R34` (no \
                 `?profile=android13plus`), or enable the android-13+ wire on the server \
                 (`RpcServer::set_android13plus`)"
        ),
        RpcError::Timeout | RpcError::DeadlineMidFrame => log::error!(
            "rsbinder RPC: the android-13+ handshake did not complete within a deadline \
                 armed on this connection — that deadline is the caller's own (the \
                 whole-step bound of `RpcClientConfig::timeout` / `ClientOptions::timeout`, \
                 the 10s whole-handshake bound \
                 `RpcSession::from_preconnected_fd` arms, or a read deadline set on the \
                 transport directly), so it may simply be shorter than this peer's \
                 legitimate response time, a peer answering a byte at a time included. A \
                 peer that should have answered well within it may be speaking the r34 \
                 (default) profile instead"
        ),
        // The returned status drops the reason string; this log is the only description.
        RpcError::Protocol(_) => log::error!(
            "rsbinder RPC: the android-13+ handshake failed ({e}) — either the peer's \
                 answer violated the wire or the caller offered a `max_version` this build \
                 does not implement"
        ),
        _ => {}
    }
    StatusCode::from(e)
}

/// Lifts the reply deadline for a nested dispatch, restored on drop; module doc "Reply deadlines".
struct NestedDeadlineGuard<'a> {
    transport: &'a dyn RpcTransport,
    restore: Option<Duration>,
}

impl<'a> NestedDeadlineGuard<'a> {
    fn lift(transport: &'a dyn RpcTransport, deadline: Option<Duration>) -> RpcResult<Self> {
        let lifted = match deadline {
            Some(_) => push_read_deadline(transport, None)?,
            None => false,
        };
        Ok(Self {
            transport,
            restore: deadline.filter(|_| lifted),
        })
    }
}

impl Drop for NestedDeadlineGuard<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.restore {
            // The reply loop's next `recv` surfaces a transport error anyway.
            pop_read_deadline(self.transport, Some(d));
        }
    }
}

thread_local! {
    /// This thread's `(session, slot)` pins, under R1; see module doc "The `DRIVING` marker".
    static DRIVING: RefCell<Vec<(usize, u64)>> = const { RefCell::new(Vec::new()) };
}

/// A call or reply on a thread whose `DRIVING` is destroyed (kernel `send_flushed`).
fn refuse_without_driving() -> StatusCode {
    log::error!("RPC: a call from a thread-local destructor, after `DRIVING` is gone; refused");
    StatusCode::InvalidOperation
}

/// AOSP `mOutgoing`/`mIncoming` as a per-slot tag (plan 2-20); module doc "Connection selection".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotRole {
    /// This end serves it (AOSP `mIncoming`): server founding slot and attaches, client callbacks.
    Incoming,
    /// This end sends on it (AOSP `mOutgoing`): client founding slot and fan-out, server callbacks.
    Outgoing,
}

/// AOSP `ConnectionUse`: decides whether a serve-driven `DRIVING` pin may be reused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConnUse {
    /// AOSP `CLIENT`, a twoway: nests on a serve-driven pin only under `allow_nested`.
    Client,
    /// A oneway (AOSP `CLIENT_ASYNC`): never nests on an `Incoming` pin (plan 10-7b §13.3).
    ClientAsync,
    /// A reply: always the pin it arrived on (AOSP replies on the served connection, not `find`).
    Reply,
}

impl ConnUse {
    /// Whether this opens a new exchange (not a reply), so pin rules bind it.
    fn is_new_transaction(self) -> bool {
        matches!(self, ConnUse::Client | ConnUse::ClientAsync)
    }
}

/// Where a `DEC_STRONG` goes; module doc "Deferred `DEC_STRONG`".
enum DecRoute<'a> {
    /// Written now on this free `Outgoing` slot.
    Conn(ConnGuard<'a>),
    /// Written now on this free `Outgoing` slot, for a thread on this `Incoming` pin.
    ConnFromPin(ConnGuard<'a>, u64),
    /// This thread's pin, which the peer reads: an `Outgoing` one, or a twoway's `Incoming` one.
    Pin(ConnGuard<'a>),
    /// Held on this `Incoming` pin until its next `REPLY` or nested request.
    Pending(u64),
    /// Handed to the reaper, which waits for an `Outgoing` slot.
    Reaper,
}

/// Addresses a thread reading a peer's frames leaves owed before it writes (module doc).
const HELD_DEC_STRONG_LIMIT: usize = 10_000;

/// What the reaper thread does; module doc "Deferred `DEC_STRONG`".
enum ReaperJob {
    /// Write `ConnState::reaper_owed` on the first free `Outgoing` slot, waiting for room.
    Dec,
    /// Wait for room on this `Outgoing` slot, and end the session if the send deadline passes.
    AwaitRoom(u64),
    /// Write what no-wait sends left of their frames, on each free slot that has any.
    Carried,
    /// Run the death sequence of the session `stop_for_the_reaper` stopped.
    End,
    /// A peer's close was seen on a connection nothing reads: module doc "A peer's close".
    PeerClosed,
    /// The last holder that `PeerClosed` recorded let its slot go.
    PeerClosedReady,
}

/// What `DEC_STRONG`s read during a send released, settled once the frame is whole.
#[derive(Default)]
struct DrainedReleases {
    /// Local objects whose last reference the peer gave back; their drop may run user code.
    released: Vec<SIBinder>,
    /// Proxy releases that waited for these payments (`pay_proxy_sends`).
    releases: Vec<(RpcAddress, u32)>,
}

/// Why a `REPLY` did not go out; `Refused` put no byte on the slot (module doc "Failed sends").
enum ReplyNotSent {
    Refused(StatusCode),
    Failed(StatusCode),
}

impl ReplyNotSent {
    /// `FailedTransaction` whatever refused it, as AOSP `processTransactInternal` and the kernel.
    fn refused(_: RpcError) -> Self {
        ReplyNotSent::Refused(StatusCode::FailedTransaction)
    }
}

/// AOSP `exclusiveTid`'s type: `std`'s `ThreadId`, needing no global or extra thread-local.
type Tid = std::thread::ThreadId;

#[inline]
fn current_tid() -> Tid {
    std::thread::current().id()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
use super::peer_watch::PeerWatch;
/// Only Linux and Android watch ("A peer's close"), so this is never made elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
enum PeerWatch {}

/// The pool's types; a child module, so its private items keep the slot vector unreachable.
mod slot_pool {
    use std::ops::{Index, IndexMut};
    use std::sync::Arc;

    use super::{ConnTraits, PeerWatch, RpcAddress, RpcTransport, SlotRole, Tid};

    /// One connection of the pool: AOSP `RpcSession::RpcConnection`.
    pub(super) struct ConnSlot {
        /// `Arc`, so a `ConnGuard` keeps the transport alive after the pool drops the slot.
        pub(super) transport: Arc<dyn RpcTransport>,
        /// AOSP `exclusiveTid`: the thread driving this slot, `None` if free.
        pub(super) exclusive_tid: Option<Tid>,
        /// Monotonic and never reused: the `DRIVING` key and a worker's handle.
        pub(super) id: u64,
        /// Direction of use; `setMaxIncomingThreads` caps `Incoming`, the callback cap `Outgoing`.
        pub(super) role: SlotRole,
        /// AOSP `allowNested`: set only while a twoway dispatched here runs; the peer reads then.
        pub(super) allow_nested: bool,
        /// The peer made a twoway call here, past any attach's probe, whose read takes one `REPLY`.
        pub(super) peer_called: bool,
        /// A serve loop runs on it, frame after frame, and keeps it while the peer is quiet.
        pub(super) serving: bool,
        /// `DEC_STRONG`s held for this slot's next frame; module doc "Deferred `DEC_STRONG`".
        pub(super) pending_dec: std::collections::HashMap<RpcAddress, u32>,
        /// Private: only `SlotPool::push` builds a slot, so none can overwrite a pooled one.
        _pooled: (),
    }

    /// The pool and its slot-id counter, behind the one session mutex (AOSP `mMutex`).
    pub(super) struct ConnState {
        pub(super) slots: SlotPool,
        pub(super) next_slot_id: u64,
        /// The founding connection's traits; every later slot must match.
        pub(super) traits: ConnTraits,
        /// `DEC_STRONG` amounts for the reaper, one entry per address; module doc step 4.
        pub(super) reaper_owed: std::collections::HashMap<RpcAddress, u32>,
        /// A `ReaperJob::Dec` is queued or running: one is enough for a session.
        pub(super) reaper_dec_queued: bool,
        /// The peer-close watcher, from the first link that needs it to the session's end.
        pub(super) watcher: Option<PeerWatch>,
        /// `Outgoing` holders when a peer's close was seen, until each lets go ("A peer's close").
        pub(super) holder_wait: Vec<u64>,
        /// A failed no-wait write per slot: past the session's end, which empties the pool.
        pub(super) no_wait_write_ends: std::collections::HashMap<u64, super::HeldWriteEnd>,
    }

    impl ConnState {
        /// A pool holding the founding slot, id 1; `with_shared` is its one caller.
        pub(super) fn new(
            founding: Arc<dyn RpcTransport>,
            role: SlotRole,
            traits: ConnTraits,
        ) -> Self {
            let mut slots = SlotPool(Vec::new());
            slots.push(founding, 1, role, None);
            ConnState {
                slots,
                next_slot_id: 2,
                traits,
                reaper_owed: std::collections::HashMap::new(),
                reaper_dec_queued: false,
                watcher: None,
                holder_wait: Vec::new(),
                no_wait_write_ends: std::collections::HashMap::new(),
            }
        }
    }

    /// The slot vector; its two removals are `SlotClaim::retire`'s and the session end's.
    pub(super) struct SlotPool(Vec<ConnSlot>);

    impl SlotPool {
        pub(super) fn push(
            &mut self,
            transport: Arc<dyn RpcTransport>,
            id: u64,
            role: SlotRole,
            exclusive_tid: Option<Tid>,
        ) {
            self.0.push(ConnSlot {
                transport,
                exclusive_tid,
                id,
                role,
                allow_nested: false,
                peer_called: false,
                serving: false,
                pending_dec: std::collections::HashMap::new(),
                _pooled: (),
            });
        }

        pub(super) fn iter(&self) -> std::slice::Iter<'_, ConnSlot> {
            self.0.iter()
        }

        pub(super) fn iter_mut(&mut self) -> std::slice::IterMut<'_, ConnSlot> {
            self.0.iter_mut()
        }

        #[cfg(any(test, feature = "test-util"))]
        pub(super) fn len(&self) -> usize {
            self.0.len()
        }

        pub(super) fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// The one un-push of a single slot, `SlotClaim::retire`'s (module doc "Leaving").
        pub(super) fn unpush_retired(&mut self, slot_id: u64) {
            self.0.retain(|s| s.id != slot_id);
        }

        /// `on_session_dead`'s: every slot leaves with the session.
        pub(super) fn clear_at_session_end(&mut self) {
            self.0.clear();
        }
    }

    impl Index<usize> for SlotPool {
        type Output = ConnSlot;

        fn index(&self, i: usize) -> &ConnSlot {
            &self.0[i]
        }
    }

    impl IndexMut<usize> for SlotPool {
        fn index_mut(&mut self, i: usize) -> &mut ConnSlot {
            &mut self.0[i]
        }
    }
}

impl ConnState {
    /// Whether any slot can carry what the reaper owes.
    fn has_outgoing(&self) -> bool {
        self.slots.iter().any(|s| s.role == SlotRole::Outgoing)
    }
}

use slot_pool::{ConnSlot, ConnState};

/// The peer a slot's transport reports, reduced to what a later slot must share (module doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Principal {
    /// A kernel-vouched uid.
    Local(u32),
    /// A TLS client or server certificate's SHA-256; its label is not compared.
    Cert([u8; 32]),
    /// A vsock context id: one VM, every process in it.
    Vsock(u32),
    /// No identity: tcp-debug, or a TLS server that asks no client certificate.
    Anonymous,
}

impl Principal {
    fn of(peer: &PeerIdentity) -> Self {
        match peer {
            PeerIdentity::Local { uid, .. } => Principal::Local(*uid),
            PeerIdentity::Certificate(cert) => Principal::Cert(*cert.fingerprint()),
            PeerIdentity::Vsock { cid } => Principal::Vsock(*cid),
            PeerIdentity::Anonymous => Principal::Anonymous,
        }
    }
}

/// `caps` and admission inputs, fixed by the founding slot for all (module doc "Slot pool").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ConnTraits {
    passes_fds: bool,
    principal: Principal,
}

impl ConnTraits {
    fn of(transport: &dyn RpcTransport) -> Self {
        ConnTraits {
            passes_fds: transport.supports_fd_passing(),
            principal: Principal::of(&transport.peer_identity()),
        }
    }
}

impl ConnSlot {
    /// Whether any thread may claim it to send on: one this thread holds is not free either.
    fn is_free_outgoing(&self) -> bool {
        self.exclusive_tid.is_none() && self.role == SlotRole::Outgoing
    }
}

impl ConnState {
    /// Whether `transport` may join this session's pool: the one admission check of every slot.
    fn admits(&self, transport: &dyn RpcTransport) -> bool {
        let traits = ConnTraits::of(transport);
        let admitted = traits == self.traits;
        if !admitted {
            log::error!(
                "RPC: connection refused — its transport or peer ({traits:?}) differs from \
                 the session's founding connection ({:?})",
                self.traits
            );
        }
        admitted
    }
}

/// A selected slot, held by `exclusive_tid` unless reentrant; its sends and reads take no lock.
struct ConnGuard<'a> {
    inner: &'a RpcSessionInner,
    slot_id: u64,
    /// Keeps the transport alive while the session's end clears the pool under this guard.
    transport: Arc<dyn RpcTransport>,
    /// Reused via `DRIVING`: drop must not release `exclusive_tid`, which the outer frame holds.
    reentrant: bool,
}

impl ConnGuard<'_> {
    /// The selected slot's transport, stable for the guard's life via the held `Arc`.
    #[inline]
    fn transport(&self) -> &dyn RpcTransport {
        &*self.transport
    }
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        if self.reentrant {
            // The outer frame owns this `(session, slot)` marker and `exclusive_tid`: keep both.
            return;
        }
        let key = (self.inner as *const _ as usize, self.slot_id);
        stack_remove_last(&DRIVING, |&k| k == key);
        let (held, last_awaited) = {
            // `notify_all`: `notify_one` may wake only a waiter pinned to another slot.
            let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
            let mut held = std::collections::HashMap::new();
            if let Some(s) = st.slots.iter_mut().find(|s| s.id == self.slot_id) {
                s.exclusive_tid = None;
                // With the release: the next holder's own hold is its own. Module doc step 1.
                if s.role == SlotRole::Outgoing && !s.serving {
                    held = std::mem::take(&mut s.pending_dec);
                }
            }
            let awaited = !st.holder_wait.is_empty();
            st.holder_wait.retain(|id| *id != self.slot_id);
            let last_awaited = awaited && st.holder_wait.is_empty();
            drop(st);
            self.inner.slot_cv.notify_all();
            (held, last_awaited)
        };
        for (addr, amount) in held {
            self.inner.send_dec_strong(addr, amount);
        }
        if last_awaited {
            let _ = self.inner.reaper_tx.send(ReaperJob::PeerClosedReady);
        }
        // After the release: `ReaperJob::Carried` skips a slot that is held and waits for no one.
        self.inner.queue_carried(self.transport());
    }
}

/// A slot this thread pushed already claimed; drop frees it, `retire` un-pushes it ("Leaving").
struct SlotClaim<'a> {
    inner: &'a RpcSessionInner,
    slot_id: u64,
    transport: Arc<dyn RpcTransport>,
}

impl SlotClaim<'_> {
    /// Out of the pool while still claimed, then shut down: no other thread could have picked it.
    fn retire(self) {
        {
            let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
            st.slots.unpush_retired(self.slot_id);
        }
        // Unpooled, so no later arm reaches it; the lock waits out an arm already running.
        let _serial = self
            .inner
            .shared
            .liveness
            .lock()
            .expect("liveness poisoned");
        if let Err(e) = self.transport.shutdown() {
            log::warn!("RPC: shutting a retired connection down failed: {e}");
        }
    }
}

impl Drop for SlotClaim<'_> {
    fn drop(&mut self) {
        {
            let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
            if let Some(s) = st.slots.iter_mut().find(|s| s.id == self.slot_id) {
                s.exclusive_tid = None;
            }
        }
        self.inner.slot_cv.notify_all();
    }
}

/// AOSP `allowNested = !oneway` as RAII; save-and-restore is AOSP `origAllowNested` (module doc).
struct AllowNestedGuard<'a> {
    inner: &'a RpcSessionInner,
    slot_id: u64,
    prev: bool,
}

impl<'a> AllowNestedGuard<'a> {
    /// Arm on this thread's `DRIVING` slot; `None` only if the session ended mid-dispatch.
    fn arm(inner: &'a RpcSessionInner, allow: bool) -> Option<Self> {
        let slot_id = inner.driving_slot().ok().flatten()?;
        let mut st = inner.conn_state.lock().expect("conn_state poisoned");
        let slot = st.slots.iter_mut().find(|s| s.id == slot_id)?;
        let prev = std::mem::replace(&mut slot.allow_nested, allow);
        slot.peer_called |= allow;
        Some(Self {
            inner,
            slot_id,
            prev,
        })
    }
}

impl Drop for AllowNestedGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.inner.conn_state.lock().expect("conn_state poisoned");
        if let Some(s) = st.slots.iter_mut().find(|s| s.id == self.slot_id) {
            s.allow_nested = self.prev;
        }
    }
}

/// One call in flight, or one frame being written, in `SharedSession::open`; module doc "Idle".
struct OpenCall<'a>(&'a SharedSession);

impl<'a> OpenCall<'a> {
    fn enter(shared: &'a SharedSession) -> Self {
        shared.open.fetch_add(1, Ordering::SeqCst);
        OpenCall(shared)
    }
}

impl Drop for OpenCall<'_> {
    fn drop(&mut self) {
        // Bump first: an idle check that reads the decrement must also see the call's end.
        self.0.io_gen.fetch_add(1, Ordering::Relaxed);
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Stream ring traffic the sockets do not see; counts only with an idle timeout (plan 10-7c B7).
#[derive(Clone)]
pub(crate) struct SessionActivity(Arc<SharedSession>);

impl SessionActivity {
    /// One lock-free load where no idle timeout is set (every client session, most servers).
    pub(crate) fn bump(&self) {
        if self.0.serve_read_deadline.load(Ordering::Relaxed) != 0 {
            self.0.io_gen.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `RawTransportIo` that bumps `io_gen` on each read that moved bytes (module doc "Idle").
struct CountedIo<'a>(RawTransportIo<'a>, &'a AtomicU64);

impl std::io::Read for CountedIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut self.0, buf)?;
        if n > 0 {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        Ok(n)
    }
}

/// State every connection of one session shares (AOSP `RpcSession`), behind `Arc`; never global.
pub(crate) struct SharedSession {
    state: Mutex<RpcState>,
    root: Mutex<Option<SIBinder>>,
    /// Max-threads value advertised on `GET_MAX_THREADS` (server side).
    max_threads: AtomicU32,
    /// `min(local, remote)` after the client handshake (0 until done).
    negotiated: AtomicU32,
    /// `set_timeout`: reply, slot and send waits, and the liveness check (module doc "Liveness").
    timeout: Mutex<Option<Duration>>,
    /// Server `set_idle_timeout` in ns (0: none): serve slots' read baseline, every send bound.
    serve_read_deadline: AtomicU64,
    /// Negotiated FD-over-RPC mode as its AOSP wire value (`fd_mode()`); `None` refuses every fd.
    fd_mode: AtomicU8,
    /// Server role: whether `GET_FD_MODE` advertises `Unix` fd support (default false).
    fd_unix_supported: AtomicBool,
    /// Server role, set before any read: reads leave the fds a peer sends for the kernel to close.
    discard_peer_fds: AtomicBool,
    /// `GET_SESSION_ID`'s random id: AOSP `kSessionIdBytes == 32`, libbinder refuses other sizes.
    rpc_session_id: RpcSessionId,
    /// r34 `GET_SESSION_ID`: the `int32` an `RpcServer` minted, `RPC_SESSION_ID_NEW` if none.
    r34_session_id: AtomicI32,
    /// Client role: the server-minted id from the first `get_session_id`; attaches must echo it.
    server_session_id: Mutex<Option<Vec<u8>>>,
    /// The founding config's connector; set once its setup succeeded, never by other builds.
    redial: Mutex<Option<Arc<Redial>>>,
    /// `Live(n)`/`Dying`/`Dead`; `Dying` reads as torn down before the obituaries (module doc).
    lifecycle: SessionLifecycle,
    /// Set by `close` before the shutdown, so every later loop end is `EndedBy::Local`; sticky.
    ended_locally: AtomicBool,
    /// Declared user serve loops; undone by a failed spawn or a loop that found nothing to serve.
    serve_declared: AtomicUsize,
    /// This end's side: fixes the founding slot's role.
    space: AddressSpace,
    /// Calls in flight either way and frames being written (`OpenCall`); module doc "Idle".
    open: AtomicUsize,
    /// Reads that moved bytes on any slot, `OpenCall` ends and joins; module doc "Idle".
    io_gen: AtomicU64,
    /// Serializes `arm_liveness`'s read of its two inputs with their syscalls (module doc).
    liveness: Mutex<()>,
    /// Test hook: `find_conn_pinned` signals here as it parks, with the pool lock held.
    #[cfg(test)]
    park_hook: Mutex<Option<mpsc::Sender<()>>>,
    /// Test hook: runs between a failed `"cci"` and the slot's retirement.
    #[cfg(test)]
    cci_failed_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Test hook: a serve loop signals here once it recorded `seen`, before its first read.
    #[cfg(test)]
    serve_wait_hook: Mutex<Option<mpsc::Sender<()>>>,
    /// Test hook: `spawn_thread` fails instead of spawning.
    #[cfg(test)]
    fail_spawn: AtomicBool,
}

impl SharedSession {
    /// The negotiated fd mode; read by every send and receive, so an atomic, not a lock.
    fn fd_mode(&self) -> FileDescriptorTransportMode {
        match self.fd_mode.load(Ordering::Acquire) {
            FD_MODE_UNIX => FileDescriptorTransportMode::Unix,
            _ => FileDescriptorTransportMode::None,
        }
    }

    fn set_fd_mode(&self, mode: FileDescriptorTransportMode) {
        let bits = match mode {
            FileDescriptorTransportMode::None => FD_MODE_NONE,
            FileDescriptorTransportMode::Unix => FD_MODE_UNIX,
        };
        self.fd_mode.store(bits, Ordering::Release);
    }

    /// Live local nodes; the `timesSent` books net to 0 once every proxy drops (leak check).
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn local_node_count(&self) -> usize {
        self.state
            .lock()
            .expect("rpc state poisoned")
            .local_node_count()
    }

    /// Live connection count (0 once `Dying`): a test's witness that a worker reaped a drop.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn live_conn_count(&self) -> usize {
        self.lifecycle.live_count()
    }

    /// Anti-resurrection gate: `SessionLifecycle::try_bump_live`, a CAS that never revives `Dying`.
    pub(crate) fn try_bump_live_conns(&self) -> bool {
        self.lifecycle.try_bump_live()
    }

    pub(crate) fn space(&self) -> AddressSpace {
        self.space
    }

    /// Server `set_idle_timeout`; a lock-free load, so a session without one pays nothing.
    fn serve_read_deadline(&self) -> Option<Duration> {
        match self.serve_read_deadline.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Duration::from_nanos(ns)),
        }
    }

    /// `Builder::spawn` for a serve thread or a watcher; `fail_spawn` fails the next one, in tests.
    fn spawn_thread<T: Send + 'static>(
        &self,
        builder: std::thread::Builder,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<std::thread::JoinHandle<T>> {
        #[cfg(test)]
        if self.fail_spawn.swap(false, Ordering::SeqCst) {
            return Err(std::io::ErrorKind::OutOfMemory.into());
        }
        builder.spawn(f)
    }

    #[cfg(test)]
    fn run_serve_wait_hook(&self) {
        if let Some(tx) = &*self.serve_wait_hook.lock().expect("serve hook") {
            let _ = tx.send(());
        }
    }

    #[cfg(test)]
    fn run_cci_failed_hook(&self) {
        let hook = self.cci_failed_hook.lock().expect("cci hook").take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// One per logical session: the slot pool; see module doc "One inner per session".
pub(crate) struct RpcSessionInner {
    /// AOSP `mMutex`: the pool's single lock, held to pick a slot, never across a send or recv.
    conn_state: Mutex<ConnState>,
    /// AOSP `mAvailableConnectionCv`: `notify_all` on slot release, add and removal; no busy loop.
    slot_cv: Condvar,
    /// The founding connection's peer, for [`RpcSession::peer_identity`].
    peer: PeerIdentity,
    /// Wire profile, fixed per session as in AOSP; an attach on another version is refused.
    profile: WireProfile,
    self_weak: Weak<RpcSessionInner>,
    /// What `parcel_ops` hands every parcel of this session, built once with the inner.
    parcel_ops: Arc<SessionParcelOps>,
    /// The reaper's queue; its drop ends the reaper. See module doc "Deferred `DEC_STRONG`".
    reaper_tx: mpsc::Sender<ReaperJob>,
    /// A `ReaperJob::AwaitRoom` is queued or running: one is enough for a session.
    awaiting_room: AtomicBool,
    /// A `ReaperJob::Carried` is queued and has not begun: it looks at every slot.
    carried_queued: AtomicBool,
    /// Cuts the socket the reaper waits on, which holds no strong reference: this inner's drop.
    reaper_wake: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Session-wide state (nodes, root, id, lifecycle): the leak and teardown books in one place.
    shared: Arc<SharedSession>,
    /// Threads serving this client's incoming connections, by slot id; `close_session` joins them.
    incoming_threads: Mutex<Vec<(u64, std::thread::JoinHandle<()>)>>,
    /// Running incoming threads: bumped before spawn, dropped by the thread as its last act.
    incoming_live: AtomicUsize,
    /// Threads `close_session` has `join()`ed; `incoming_live` reads 0 whether joined or not.
    incoming_joined: AtomicUsize,
    /// The founding slot's serve loop reads the r34 session-id preamble first (`RpcSession::new`).
    awaits_preamble: AtomicBool,
}

/// The `RpcParcelOps` implementation bound to one session.
struct SessionParcelOps(Weak<RpcSessionInner>);

impl RpcParcelOps for SessionParcelOps {
    fn write_binder(&self, binder: Option<&SIBinder>, parcel: &mut Parcel) -> Result<()> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.write_binder(binder, parcel)
    }
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<SIBinder>> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.read_binder(parcel)
    }
    fn cancel_leaving(&self, addrs: &[RpcAddress]) {
        // A dead inner already ran `clear_local`; nothing to give back.
        if let Some(inner) = self.0.upgrade() {
            inner.cancel_leaving(addrs);
        }
    }
    // The parcel's `Weak` keeps the allocation, so no later session can reuse this address.
    fn session_id(&self) -> *const () {
        self.0.as_ptr().cast()
    }
    fn records_binder_positions(&self) -> Result<bool> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        Ok(inner.profile.records_binder_positions())
    }
    fn acquire_copied(&self, objects: &[&[u8]]) -> Result<CopiedBinders> {
        let inner = self.0.upgrade().ok_or(StatusCode::DeadObject)?;
        inner.acquire_copied(objects)
    }
}

impl RpcSessionInner {
    /// The session's reply deadline as `set_timeout` left it (`None` = unbounded).
    pub(crate) fn timeout(&self) -> Option<Duration> {
        *self.shared.timeout.lock().expect("timeout poisoned")
    }

    /// AOSP `ExclusiveConnection::find` for a twoway; order in module doc "Connection selection".
    fn find_conn(&self) -> Result<ConnGuard<'_>> {
        self.find_conn_impl(ConnUse::Client)
    }

    /// [`find_conn`](Self::find_conn) for a oneway transaction ([`ConnUse::ClientAsync`]).
    fn find_conn_async(&self) -> Result<ConnGuard<'_>> {
        self.find_conn_impl(ConnUse::ClientAsync)
    }

    /// Shared body of the `find_conn` family; see module doc "Connection selection".
    fn find_conn_impl(&self, use_: ConnUse) -> Result<ConnGuard<'_>> {
        // The scan is `Outgoing`-only for every use: module doc "Connection selection".
        let mut wait_until: Option<Option<Instant>> = None;
        let tid = current_tid();
        // (1) Reentrant pin, innermost first; a serve-driven slot also needs `allow_nested`.
        let Ok(pinned) = self.driving_slot() else {
            return Err(refuse_without_driving());
        };
        if let Some(slot_id) = pinned {
            let reusable = {
                let st = self.conn_state.lock().expect("conn_state poisoned");
                match st.slots.iter().find(|s| s.id == slot_id) {
                    // The session ended since the `DRIVING` push: typed error, no panic.
                    None => return Err(StatusCode::DeadObject),
                    Some(s)
                        if s.role == SlotRole::Outgoing
                            || (s.allow_nested && use_ != ConnUse::ClientAsync)
                            || !use_.is_new_transaction() =>
                    {
                        Some(Arc::clone(&s.transport))
                    }
                    Some(_) => None,
                }
            };
            if let Some(transport) = reusable {
                return Ok(ConnGuard {
                    inner: self,
                    slot_id,
                    transport,
                    reentrant: true,
                });
            }
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            // Torn-down/drained recheck: a re-`wait` on a dead pool would park forever.
            if self.shared.lifecycle.is_torn_down() || st.slots.is_empty() {
                return Err(StatusCode::DeadObject);
            }
            // (2)/(3) First free `Outgoing`; `pinned.is_none()` guards the self-match (module doc).
            let free = |s: &ConnSlot| {
                s.exclusive_tid.is_none() || (pinned.is_none() && s.exclusive_tid == Some(tid))
            };
            let pick = st
                .slots
                .iter()
                .position(|s| free(s) && s.role == SlotRole::Outgoing);
            if let Some(idx) = pick {
                return Ok(self.claim_slot(&mut st.slots[idx], tid));
            }
            // (3b) No `Outgoing` slot, and only a peer attach creates one: AOSP `WOULD_BLOCK`.
            if !st.slots.iter().any(|s| s.role == SlotRole::Outgoing) {
                // A client's founding slot is `Outgoing` and leaves the pool only with the session.
                if self.profile.wire_version().is_none() {
                    // r34 has no attach mechanism, so no incoming-connection advice applies.
                    log::error!(
                        "RPC: r34 session has no outgoing connection — this endpoint \
                         accepted the connection, and the r34 profile cannot open or \
                         attach one. Only a nested twoway call (from inside a twoway \
                         handler) can transact here — a oneway never nests; use \
                         `WireProfile::Android13Plus` (`?profile=android13plus`) for oneway \
                         calls and for callbacks outside a handler"
                    );
                } else {
                    log::error!(
                        "RPC: session has no outgoing connection — a non-nested call (from \
                         another thread, or any oneway) needs the peer to open \
                         incoming connections (RpcClientConfig::incoming_connections / \
                         ClientOptions::incoming_connections / \
                         ARpcSession_setMaxIncomingThreads); refusing instead of waiting forever"
                    );
                }
                return Err(StatusCode::WouldBlock);
            }
            // (3) Serve loops hold every `Outgoing` slot until the peer speaks: module doc.
            if st
                .slots
                .iter()
                .filter(|s| s.role == SlotRole::Outgoing)
                .all(|s| s.serving)
            {
                log::error!(
                    "RPC: every outgoing connection of this session is held by a serve loop \
                     (RpcSession::serve_blocking / spawn_serve), so only a nested call from \
                     inside a handler can transact here; a client that keeps calling opens \
                     incoming connections (RpcClientConfig::incoming_connections / \
                     ClientOptions::incoming_connections) instead"
                );
                return Err(StatusCode::WouldBlock);
            }
            // (4) Pool exhausted: wait under the session deadline; only the peer frees serve slots.
            let deadline = *self.shared.timeout.lock().expect("timeout poisoned");
            // Absolute: unrelated `slot_cv` wakes would re-arm a per-wake deadline.
            let at = deadline.and_then(|d| {
                // `None` inside: past what an `Instant` holds, so wait without a deadline.
                *wait_until.get_or_insert_with(|| Instant::now().checked_add(d))
            });
            let waited = crate::signal::wait_until(&self.slot_cv, st, at);
            st = match waited.expect("slot_cv poisoned") {
                Ok(st) => st,
                // Only an `at` passes, and there is one only under a `deadline`.
                Err(_) => {
                    log::warn!(
                        "RPC: no connection slot became available within {:?} \
                         (every slot is driven by another thread — a session \
                         served on one thread and transacted on another needs \
                         more than one connection); nothing was sent",
                        deadline.unwrap_or_default()
                    );
                    return Err(StatusCode::WouldBlock);
                }
            };
        }
    }

    /// This thread's innermost `DRIVING` slot of this session; `Err` once `DRIVING` is destroyed.
    fn driving_slot(&self) -> StackResult<Option<u64>> {
        stack_find(&DRIVING, self as *const RpcSessionInner as usize)
    }

    /// Hold `slot` for `tid` and push it on `DRIVING`; dropping the guard undoes both.
    fn claim_slot(&self, slot: &mut ConnSlot, tid: Tid) -> ConnGuard<'_> {
        slot.exclusive_tid = Some(tid);
        let sess_ptr = self as *const RpcSessionInner as usize;
        // Without `DRIVING` only a `DEC_STRONG` or `try_oneway` gets here: neither nests on it.
        let _ = stack_push(&DRIVING, (sess_ptr, slot.id));
        ConnGuard {
            inner: self,
            slot_id: slot.id,
            transport: Arc::clone(&slot.transport),
            reentrant: false,
        }
    }

    /// Non-blocking choice of a `DEC_STRONG`'s connection; module doc "Deferred `DEC_STRONG`".
    fn dec_route(&self) -> DecRoute<'_> {
        let tid = current_tid();
        // A proxy dropped by a thread-local's destructor: no pin to write on, a free slot will do.
        let pinned = self.driving_slot().ok().flatten();
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let mut pending_on = None;
        if let Some(slot_id) = pinned {
            match st.slots.iter().find(|s| s.id == slot_id) {
                // Read by the peer: it serves an `Outgoing` slot, awaits a twoway's reply here.
                Some(s) if s.role == SlotRole::Outgoing || s.allow_nested => {
                    return DecRoute::Pin(ConnGuard {
                        inner: self,
                        slot_id,
                        transport: Arc::clone(&s.transport),
                        reentrant: true,
                    });
                }
                Some(_) => pending_on = Some(slot_id),
                // The session ended since the push: the send below finds nothing to write on.
                None => {}
            }
        }
        // A slot another frame of this thread holds is not free: its drop would release it.
        let free = st.slots.iter().position(ConnSlot::is_free_outgoing);
        // A failed write stops the session before its slot is free: it may hold a cut frame.
        let free = free.filter(|_| !self.shared.lifecycle.is_torn_down());
        if let Some(idx) = free {
            let conn = self.claim_slot(&mut st.slots[idx], tid);
            return match pending_on {
                Some(pin) => DecRoute::ConnFromPin(conn, pin),
                None => DecRoute::Conn(conn),
            };
        }
        match pending_on {
            Some(slot_id) => DecRoute::Pending(slot_id),
            None => DecRoute::Reaper,
        }
    }

    /// A free `Outgoing` slot now, or why not: a send that waits for nothing does not wait here.
    fn find_conn_no_wait(&self) -> Result<ConnGuard<'_>> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if self.shared.lifecycle.is_torn_down() || st.slots.is_empty() {
            return Err(StatusCode::DeadObject);
        }
        match st.slots.iter().position(ConnSlot::is_free_outgoing) {
            Some(idx) => Ok(self.claim_slot(&mut st.slots[idx], current_tid())),
            None if st.slots.iter().any(|s| s.role == SlotRole::Outgoing) => {
                Err(StatusCode::WouldBlock)
            }
            // Only a peer attach adds an `Outgoing` slot: no retry of this one can succeed.
            None => Err(StatusCode::InvalidOperation),
        }
    }

    /// Reaper `find_conn`: a claim with no guard, as its wait holds no reference to the session.
    fn find_conn_for_reaper(&self) -> Option<(u64, Arc<dyn RpcTransport>)> {
        let tid = current_tid();
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            if self.shared.lifecycle.is_torn_down() || st.slots.is_empty() {
                return None;
            }
            // `Outgoing`, unclaimed: the reaper gives each claim back, so none is this thread's.
            if let Some(slot) = st.slots.iter_mut().find(|s| s.is_free_outgoing()) {
                slot.exclusive_tid = Some(tid);
                return Some((slot.id, Arc::clone(&slot.transport)));
            }
            // Only a peer attach adds an `Outgoing` slot; parking would hold the strong `Arc`.
            if !st.slots.iter().any(|s| s.role == SlotRole::Outgoing) {
                return None;
            }
            st = self.slot_cv.wait(st).expect("slot_cv poisoned");
        }
    }

    /// `slot_id` claimed for a reaper job if free now: no job of the reaper's waits for its holder.
    fn claim_if_free(&self, slot_id: u64) -> Option<Arc<dyn RpcTransport>> {
        let tid = current_tid();
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if self.shared.lifecycle.is_torn_down() {
            return None;
        }
        let slot = st.slots.iter_mut().find(|s| s.id == slot_id)?;
        if slot.serving || slot.exclusive_tid.is_some() {
            return None;
        }
        slot.exclusive_tid = Some(tid);
        Some(Arc::clone(&slot.transport))
    }

    /// Give back a claim `find_conn_for_reaper` or `claim_if_free` made.
    fn release_reaper_claim(&self, slot_id: u64) {
        {
            let mut st = self.conn_state.lock().expect("conn_state poisoned");
            if let Some(s) = st.slots.iter_mut().find(|s| s.id == slot_id) {
                s.exclusive_tid = None;
            }
        }
        self.slot_cv.notify_all();
    }

    /// A serve loop starts or ends on `slot_id`.
    fn mark_slot_serving(&self, slot_id: u64, serving: bool) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if let Some(slot) = st.slots.iter_mut().find(|s| s.id == slot_id) {
            slot.serving = serving;
        }
    }

    /// A worker's own slot, reentrant via `DRIVING`; `DeadObject` once the session ended.
    fn find_conn_pinned(&self, want_slot_id: u64) -> Result<ConnGuard<'_>> {
        let tid = current_tid();
        let sess_ptr = self as *const RpcSessionInner as usize;
        let key = (sess_ptr, want_slot_id);
        // Reentrant on the same slot.
        let reentrant = stack_any(&DRIVING, |&k| k == key).map_err(|_| refuse_without_driving())?;
        if reentrant {
            let transport = {
                let st = self.conn_state.lock().expect("conn_state poisoned");
                match st.slots.iter().find(|s| s.id == want_slot_id) {
                    Some(s) => Arc::clone(&s.transport),
                    // The session ended since the `DRIVING` push: typed error, no panic.
                    None => return Err(StatusCode::DeadObject),
                }
            };
            return Ok(ConnGuard {
                inner: self,
                slot_id: want_slot_id,
                transport,
                reentrant: true,
            });
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        loop {
            let target = st.slots.iter_mut().find(|s| s.id == want_slot_id);
            let Some(slot) = target else {
                // The session ended (or the id is not this pool's): `SessionEnded` for a loop.
                return Err(StatusCode::DeadObject);
            };
            if slot.exclusive_tid.is_none() || slot.exclusive_tid == Some(tid) {
                return Ok(self.claim_slot(slot, tid));
            }
            #[cfg(test)]
            if let Some(tx) = &*self.shared.park_hook.lock().expect("park hook") {
                let _ = tx.send(());
            }
            st = self.slot_cv.wait(st).expect("slot_cv poisoned");
        }
    }

    /// Lift a slot's read deadline (best-effort): r34's admission deadline after the first frame.
    fn clear_slot_read_timeout(&self, slot_id: u64) {
        let transport = {
            let st = self.conn_state.lock().expect("conn_state poisoned");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| Arc::clone(&s.transport))
        };
        if let Some(t) = transport {
            if let Err(e) = t.set_read_timeout(None) {
                log::debug!("RPC: failed to clear first-frame read deadline: {e:?}");
            }
        }
    }

    /// The session's activity count, recorded as a serve slot's wait begins (module doc "Idle").
    fn activity(&self) -> u64 {
        self.shared.io_gen.load(Ordering::Relaxed)
    }

    /// Whether a call is open or the count moved since `seen`, which it advances; "Idle".
    fn active_since(&self, seen: &mut u64) -> bool {
        // `open` first: reading a call's decrement makes its end bump visible to the next load.
        let open = self.shared.open.load(Ordering::SeqCst) > 0;
        let now = self.activity();
        let moved = now != *seen;
        *seen = now;
        open || moved
    }

    /// Lift every slot's handshake deadlines: reads unbounded, sends back to the session's own.
    fn clear_handshake_timeouts(&self) {
        for t in self.slot_transports() {
            let _ = t.set_read_timeout(None);
            self.arm_liveness(&*t);
        }
    }

    /// Every pooled slot's transport, copied out so the syscalls run unlocked.
    fn slot_transports(&self) -> Vec<Arc<dyn RpcTransport>> {
        let st = self.conn_state.lock().expect("conn_state poisoned");
        st.slots.iter().map(|s| Arc::clone(&s.transport)).collect()
    }

    /// Arm `transport`'s send deadline and liveness check; see module doc "Liveness".
    fn arm_liveness(&self, transport: &dyn RpcTransport) {
        let _serial = self.shared.liveness.lock().expect("liveness poisoned");
        self.arm_liveness_locked(transport);
    }

    /// `arm_liveness` with `SharedSession::liveness` already held.
    fn arm_liveness_locked(&self, transport: &dyn RpcTransport) {
        let timeout = *self.shared.timeout.lock().expect("timeout poisoned");
        if let Err(e) = transport.set_write_timeout(self.send_deadline()) {
            log::warn!("RPC: failed to arm a connection's send deadline: {e:?}");
        }
        if let Err(e) = transport.set_liveness(timeout) {
            log::warn!("RPC: failed to arm a connection's liveness check: {e:?}");
        }
    }

    /// The send deadline: the smaller of `set_timeout` and the idle deadline ("Liveness").
    fn send_deadline(&self) -> Option<Duration> {
        let timeout = *self.shared.timeout.lock().expect("timeout poisoned");
        match (timeout, self.shared.serve_read_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Change an input of `arm_liveness` with `store`, then arm every slot, all under one lock.
    fn arm_liveness_all(&self, store: impl FnOnce()) {
        let _serial = self.shared.liveness.lock().expect("liveness poisoned");
        store();
        for t in self.slot_transports() {
            self.arm_liveness_locked(&*t);
        }
    }

    pub(crate) fn parcel_ops(&self) -> Arc<dyn RpcParcelOps> {
        self.parcel_ops.clone()
    }

    /// Push a slot; `live_conns` is the caller's; module doc "Slot pool" has the refusals.
    fn add_slot_inner(&self, transport: Box<dyn RpcTransport>, role: SlotRole) -> Result<u64> {
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Anti-resurrection gate: must share the push's critical section (module doc).
        if self.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        if !st.admits(&*transport) {
            return Err(StatusCode::BadType);
        }
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        let armed = Arc::clone(&transport);
        st.slots.push(transport, id, role, None);
        if role == SlotRole::Outgoing {
            Self::watch_added_slot(&st, &*armed);
        }
        self.wake_reaper_for_owed(st);
        self.arm_liveness(&*armed);
        self.slot_cv.notify_all();
        Ok(id)
    }

    /// Server attach: cap (AOSP `mIncoming.size()`), live bump and push in one critical section.
    fn add_incoming_slot_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Incoming)
            .count()
            >= cap
        {
            return Err(StatusCode::FailedTransaction);
        }
        // Before the bump, which a refusal would have to undo.
        if !st.admits(&*transport) {
            return Err(StatusCode::BadType);
        }
        if !self.shared.try_bump_live_conns() {
            return Err(StatusCode::DeadObject);
        }
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        let armed = Arc::clone(&transport);
        st.slots.push(transport, id, SlotRole::Incoming, None);
        drop(st);
        // Its handshake bytes crossed outside the funnel: the join is activity (module doc "Idle").
        self.shared.io_gen.fetch_add(1, Ordering::Relaxed);
        self.arm_liveness(&*armed);
        self.slot_cv.notify_all();
        Ok(id)
    }

    /// Client fan-out: an `Outgoing` slot, no `live_conns` bump; refusals as `add_slot_inner`.
    fn add_outgoing_slot(&self, transport: Box<dyn RpcTransport>) -> Result<u64> {
        self.add_slot_inner(transport, SlotRole::Outgoing)
    }

    /// Callback-slot push under the `Outgoing` cap, free for any sender at once.
    #[cfg(test)]
    fn add_slot_inner_capped(&self, transport: Box<dyn RpcTransport>, cap: usize) -> Result<u64> {
        self.push_outgoing_capped(transport, cap, false)
            .map(|(id, _)| id)
    }

    /// Callback-slot push held by this thread until the claim drops; module doc "Callback slots".
    fn add_claimed_slot_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<SlotClaim<'_>> {
        let (slot_id, transport) = self.push_outgoing_capped(transport, cap, true)?;
        Ok(SlotClaim {
            inner: self,
            slot_id,
            transport,
        })
    }

    /// Shared body of the two callback-slot pushes; `claimed` sets `exclusive_tid` to this thread.
    fn push_outgoing_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
        claimed: bool,
    ) -> Result<(u64, Arc<dyn RpcTransport>)> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Anti-resurrection gate: same critical section as the cap check (module doc).
        if self.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        if st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Outgoing)
            .count()
            >= cap
        {
            return Err(StatusCode::FailedTransaction);
        }
        if !st.admits(&*transport) {
            return Err(StatusCode::BadType);
        }
        let transport: Arc<dyn RpcTransport> = Arc::from(transport);
        let id = st.next_slot_id;
        st.next_slot_id += 1;
        let armed = Arc::clone(&transport);
        st.slots
            .push(transport, id, SlotRole::Outgoing, claimed.then(current_tid));
        self.wake_reaper_for_owed(st);
        self.arm_liveness(&*armed);
        self.slot_cv.notify_all();
        Ok((id, armed))
    }

    /// The one rule for a failed send; see module doc "Failed sends". Hold no lock.
    fn end_after_failed_send(&self, e: &RpcError) {
        if Self::failed_send_ends_session(e) {
            self.fail_session();
        }
    }

    /// Whether a failed send ends the session, logged if the peer's close did not cause it.
    fn failed_send_ends_session(e: &RpcError) -> bool {
        // Refused before any byte: the stream is untouched and the session goes on.
        if matches!(e, RpcError::FrameTooLarge { .. } | RpcError::Protocol(_)) {
            return false;
        }
        if !matches!(e, RpcError::EndOfStream) {
            log::warn!("RPC: a send failed ({e}); ending the session");
        }
        true
    }

    /// A failed send's status: a send deadline is `TimedOut` wherever it stopped the frame.
    fn send_error_status(e: RpcError) -> StatusCode {
        match e {
            // Mid-frame `EAGAIN`; as errno it would read `WouldBlock`, "nothing was sent".
            RpcError::Io(io) if super::transport::is_timeout(&io) => StatusCode::TimedOut,
            e => e.into(),
        }
    }

    /// End the session on a fault; `close` without `ended_locally`. Module doc "Session end".
    pub(crate) fn fail_session(&self) {
        if self.shared.lifecycle.force_dying() {
            self.on_session_dead();
        }
    }

    /// `fail_session` for a thread that runs no death sequence: the reaper does ("Failed sends").
    fn stop_for_the_reaper(&self) {
        if !self.shared.lifecycle.force_dying() {
            return;
        }
        // Not `shutdown`, which may write. This also wakes whatever the reaper waits on.
        for t in self.slot_transports() {
            if let Some(cut) = t.shutdown_handle() {
                cut();
            }
        }
        self.slot_cv.notify_all();
        let _ = self.reaper_tx.send(ReaperJob::End);
    }

    /// End the session whatever its connection count; exactly one caller runs `on_session_dead`.
    pub(crate) fn close(&self) {
        if self.shared.lifecycle.force_dying() {
            // Set before the transports go down, so a worker they wake already sees it.
            self.shared.ended_locally.store(true, Ordering::SeqCst);
            self.on_session_dead();
            return;
        }
        // A stop may have cut nothing, and its `End` waits behind the reaper's write: wake that.
        let _serial = self.shared.liveness.lock().expect("liveness poisoned");
        self.shutdown_all_transports();
    }

    /// The death sequence, entered at `Dying`: unblock, obituaries, `Dead`, release (module doc).
    pub(crate) fn on_session_dead(&self) {
        {
            // No arm between a `shutdown`'s own send bound and its closing writes ("Liveness").
            let _serial = self.shared.liveness.lock().expect("liveness poisoned");
            self.shutdown_all_transports();
        }
        {
            let mut st = self.conn_state.lock().expect("conn_state poisoned");
            st.slots.clear_at_session_end();
            st.holder_wait.clear();
            // A `shutdown` that does nothing leaves it no signal: this closes its duplicates.
            st.watcher = None;
        }
        self.slot_cv.notify_all();
        self.send_session_obituaries();
        self.shared.lifecycle.mark_dead();
        let root = self.shared.root.lock().expect("root poisoned").take();
        let locals = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            st.clear_remote_counts();
            st.clear_local()
        };
        drop(locals);
        drop(root);
    }

    pub(crate) fn fd_mode(&self) -> FileDescriptorTransportMode {
        self.shared.fd_mode()
    }

    /// Whether this session's parcels record fd positions: android-13+ v1+ only, never R34.
    pub(crate) fn records_fd_positions(&self) -> bool {
        self.profile.records_fd_positions()
    }

    /// Send one frame (fds in `Unix` mode only); `drain` reads while it waits ("Draining sends").
    fn send_msg(
        &self,
        transport: &dyn RpcTransport,
        frame: &[u8],
        fds: &[OwnedFd],
        drain: Option<&mut dyn FnMut() -> RpcResult<()>>,
    ) -> RpcResult<()> {
        // No frame follows one that was cut, which left `Live` before its slot went back.
        if self.shared.lifecycle.is_torn_down() {
            return Err(RpcError::EndOfStream);
        }
        // A frame on its way out is activity until its write returns, at any pace ("Idle").
        let _sending = OpenCall::enter(&self.shared);
        // AOSP wire: no length prefix; fds ride the first `sendmsg` (`RpcTransportRaw`).
        if self.fd_mode() == FileDescriptorTransportMode::Unix {
            let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
            return write_aosp_message_with_fds(transport, frame, &borrowed, drain);
        }
        // Not `RawTransportIo`: it folds a not-started send's `Timeout` into `Io`.
        debug_assert!(fds.is_empty(), "a non-Unix session must not carry fds");
        let _ = fds; // release: `debug_assert!` is compiled out, so `fds` is otherwise unused
        write_aosp_message_with_fds(transport, frame, &[], drain)
    }

    /// [`send_msg`](Self::send_msg) with no wait for room; `None` for a transport without one.
    fn send_msg_no_wait(
        &self,
        transport: &dyn RpcTransport,
        frame: &[u8],
        fds: &[OwnedFd],
    ) -> Option<RpcResult<bool>> {
        // As in `send_msg`: a transport may have this send and a `send_raw` that cut a frame.
        if self.shared.lifecycle.is_torn_down() {
            return Some(Err(RpcError::EndOfStream));
        }
        let _sending = OpenCall::enter(&self.shared);
        let sent = if self.fd_mode() == FileDescriptorTransportMode::Unix {
            let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
            transport.__send_no_wait(frame, &borrowed)
        } else {
            debug_assert!(fds.is_empty(), "a non-Unix session must not carry fds");
            transport.__send_no_wait(frame, &[])
        };
        self.queue_carried(transport);
        sent
    }

    /// One `ReaperJob::Carried` for part of a frame `transport` holds; it looks at every slot.
    fn queue_carried(&self, transport: &dyn RpcTransport) {
        if transport.__carrying() && !self.carried_queued.swap(true, Ordering::SeqCst) {
            let _ = self.reaper_tx.send(ReaperJob::Carried);
        }
    }

    /// The slots the reaper finishes a frame on; a serve loop does that itself before it reads.
    fn slots_carrying(&self) -> Vec<u64> {
        let st = self.conn_state.lock().expect("conn_state poisoned");
        st.slots
            .iter()
            .filter(|s| s.role == SlotRole::Outgoing && !s.serving && s.transport.__carrying())
            .map(|s| s.id)
            .collect()
    }

    /// AOSP `drainCommands(CONTROL_ONLY)` for one message; module doc "Draining sends".
    fn drain_one<'t>(
        &self,
        transport: &'t dyn RpcTransport,
        slot_id: u64,
        deadline: &mut Option<ReplyDeadlineGuard<'t>>,
        after: &mut DrainedReleases,
        fault: &std::cell::Cell<Option<StatusCode>>,
    ) -> RpcResult<()> {
        if deadline.is_none() {
            let baseline = self.slot_baseline_read_deadline(slot_id);
            *deadline = Some(ReplyDeadlineGuard::arm(
                transport,
                self.send_deadline(),
                baseline,
            )?);
        }
        let refuse = |status: StatusCode, what: &'static str| {
            log::error!(
                "RPC: {what} arrived while a transaction was being sent; ending the session"
            );
            fault.set(Some(status));
            Err(RpcError::Protocol(what))
        };
        // A `DEC_STRONG` carries no fds; any that came are closed here, as AOSP drops them.
        let (frame, _fds) =
            self.recv_msg_gated(transport, |header| {
                match control_only_refusal(header, self.profile.dec_strong_body_len()) {
                    None => Ok(()),
                    Some((status, what)) => refuse(status, what),
                }
            })?;
        match self.profile.codec().decode_message(&frame)? {
            WireMessage::DecStrong(addr, amount) => {
                let mut st = self.shared.state.lock().expect("rpc state poisoned");
                after.released.extend(st.dec_strong_local(&addr, amount));
                let held = st.pay_proxy_sends(&addr, amount);
                if held > 0 {
                    after.releases.push((addr, held));
                }
                Ok(())
            }
            // AOSP `processCommand`: `TRANSACT` under `CONTROL_ONLY` is `BAD_TYPE`.
            WireMessage::Transact(_) => refuse(StatusCode::BadType, "a TRANSACT"),
            // AOSP has no `REPLY` case there: an unknown command, `DEAD_OBJECT`.
            WireMessage::Reply(_) => refuse(StatusCode::DeadObject, "a REPLY"),
        }
    }

    /// Receive one frame (+ `SCM_RIGHTS` fds in `Unix` mode, fixed before any RPC traffic).
    fn recv_msg(&self, transport: &dyn RpcTransport) -> RpcResult<(Vec<u8>, Vec<OwnedFd>)> {
        self.recv_msg_gated(transport, |_| Ok(()))
    }

    /// `recv_msg` whose `header_gate` may refuse the `RpcWireHeader` before the body is read.
    fn recv_msg_gated(
        &self,
        transport: &dyn RpcTransport,
        header_gate: impl FnOnce(&[u8]) -> RpcResult<()>,
    ) -> RpcResult<(Vec<u8>, Vec<OwnedFd>)> {
        // Header, then `bodySize` bytes; each read that moved bytes bumps `io_gen` ("Idle").
        let io_gen = &self.shared.io_gen;
        let discard = self.shared.discard_peer_fds.load(Ordering::Relaxed);
        if self.fd_mode() == FileDescriptorTransportMode::Unix && !discard {
            return read_aosp_message_with_fds(
                |buf| {
                    let got = transport.recv_raw_with_fds(buf)?;
                    if got.0 > 0 {
                        io_gen.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(got)
                },
                header_gate,
            );
        }
        let mut io = CountedIo(RawTransportIo(transport), io_gen);
        Ok((read_aosp_message_gated(&mut io, header_gate)?, Vec::new()))
    }

    /// The `int32` session id a client writes before its first message on the r34 wire.
    fn read_session_preamble(&self, transport: &dyn RpcTransport) -> RpcResult<i32> {
        let mut io = CountedIo(RawTransportIo(transport), &self.shared.io_gen);
        read_r34_session_preamble(&mut io)
    }

    fn self_weak(&self) -> Weak<RpcSessionInner> {
        self.self_weak.clone()
    }

    /// `SharedSession::local_node_count`, for `RpcServer::live_session_node_count` (leak check).
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn local_node_count(&self) -> usize {
        self.shared.local_node_count()
    }

    /// `SharedSession::live_conn_count`, the teardown witness for `RpcServer::session_live_conns`.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn live_conn_count(&self) -> usize {
        self.shared.live_conn_count()
    }

    /// Slots in this session's pool, attaches included; for `RpcServer::session_slot_count`.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn slot_count(&self) -> usize {
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .len()
    }

    /// Readable callback connections, by this end's role; see module doc "Callback slots".
    pub(crate) fn callback_conn_count(&self) -> usize {
        let callback_role = match self.shared.space() {
            AddressSpace::Initiator => SlotRole::Incoming,
            AddressSpace::Acceptor => SlotRole::Outgoing,
        };
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .filter(|s| s.role == callback_role)
            .count()
    }

    /// A handle that counts a stream ring's traffic toward this session's idle check.
    pub(crate) fn ring_activity(&self) -> SessionActivity {
        SessionActivity(self.shared.clone())
    }

    /// This session's `TransportCaps` now; `RpcSession::caps` states what each bit means here.
    pub(crate) fn caps(&self) -> crate::TransportCaps {
        use crate::TransportCaps as C;
        let traits = self.conn_state.lock().expect("conn_state poisoned").traits;
        let mut caps = C::NONE;
        // `Unix` fd mode is agreed regardless of transport kind; vsock/TLS cannot send fds.
        if self.fd_mode() == FileDescriptorTransportMode::Unix && traits.passes_fds {
            caps |= C::FD_PASSING;
        }
        if matches!(traits.principal, Principal::Local(_)) {
            // A kernel-vouched uid, and the same kernel on both ends.
            caps |= C::TRUSTED_UID | C::SAME_HOST;
        }
        if self.callback_conn_count() > 0 {
            caps |= C::CALLBACKS;
        }
        caps
    }

    /// The slot's role while pooled; `None` once un-pushed or cleared by the session's end.
    fn slot_role(&self, slot_id: u64) -> Option<SlotRole> {
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .find(|s| s.id == slot_id)
            .map(|s| s.role)
    }

    /// Read deadline `slot_id` returns to after a reply deadline: serve-driven slots only.
    fn slot_baseline_read_deadline(&self, slot_id: u64) -> Option<Duration> {
        // No idle value (every client session): no pool lock.
        let idle = self.shared.serve_read_deadline()?;
        let serve_driven = self
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .any(|s| s.id == slot_id && s.role == SlotRole::Incoming);
        serve_driven.then_some(idle)
    }

    /// The baseline if this thread drives a serve slot of this session ("Reply deadlines").
    fn handler_read_deadline(&self) -> Option<Duration> {
        // No idle value (every client session): no pool lock and no stack scan.
        let idle = self.shared.serve_read_deadline()?;
        let sess_ptr = self as *const RpcSessionInner as usize;
        let st = self.conn_state.lock().expect("conn_state poisoned");
        // The scan runs no user code and enters no binder.
        let serving = stack_any(&DRIVING, |&(sp, sid)| {
            sp == sess_ptr
                && st
                    .slots
                    .iter()
                    .any(|s| s.id == sid && s.role == SlotRole::Incoming)
        });
        serving.unwrap_or(false).then_some(idle)
    }

    /// Whether something reads this session's connections (AOSP `linkToDeath`'s incoming test).
    pub(crate) fn notices_connection_loss(&self) -> bool {
        self.shared.space() == AddressSpace::Acceptor
            || self.shared.serve_declared.load(Ordering::SeqCst) > 0
            || self.incoming_slot_count() > 0
    }

    /// `ReaperJob::PeerClosed`: end the session once current holders have let go; waits for none.
    fn end_at_peers_close(&self) {
        // What reads a connection ends the session at its end of stream, with its value.
        if self.notices_connection_loss() {
            return;
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        if self.shared.lifecycle.is_torn_down() {
            return;
        }
        st.holder_wait = st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Outgoing && s.exclusive_tid.is_some())
            .map(|s| s.id)
            .collect();
        let none_held = st.holder_wait.is_empty();
        drop(st);
        if none_held {
            self.fail_session();
        }
    }

    /// What a `link_to_death` needs of this session, so that the link's recipient can ever run.
    pub(crate) fn watch_for_death_link(&self) -> Result<()> {
        // A torn-down session's own latch answers the link.
        if self.shared.lifecycle.is_torn_down() || self.notices_connection_loss() {
            return Ok(());
        }
        self.start_watcher()
    }

    /// One watcher per session, of every `Outgoing` slot; module doc "A peer's close".
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn start_watcher(&self) -> Result<()> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Under the lock: the session's end takes the watcher here, and would not take a later one.
        if st.watcher.is_some() || self.shared.lifecycle.is_torn_down() {
            return Ok(());
        }
        let mut sockets = Vec::new();
        for s in st.slots.iter().filter(|s| s.role == SlotRole::Outgoing) {
            let Some(socket) = s.transport.peer_close_fd() else {
                return Err(StatusCode::InvalidOperation);
            };
            sockets.push(socket.try_clone_to_owned().map_err(StatusCode::from)?);
        }
        let tell = self.reaper_tx.clone();
        let signal = move || {
            let _ = tell.send(ReaperJob::PeerClosed);
        };
        let (watch, body) = PeerWatch::new(sockets, signal).map_err(StatusCode::from)?;
        // Under the lock, so a link that finds the watcher here knows its thread began.
        let builder = std::thread::Builder::new().name("rsbinder-rpc-watch".into());
        drop(self.shared.spawn_thread(builder, body).map_err(|e| {
            log::error!("RPC: cannot start a session's watcher: {e}");
            StatusCode::from(e)
        })?);
        st.watcher = Some(watch);
        Ok(())
    }

    /// Only Linux and Android watch: module doc "A peer's close".
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn start_watcher(&self) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }

    /// A slot pushed under `st`, for the watcher if the session has one.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn watch_added_slot(st: &ConnState, transport: &dyn RpcTransport) {
        let Some(watch) = &st.watcher else {
            return;
        };
        match transport.peer_close_fd().map(|s| s.try_clone_to_owned()) {
            Some(Ok(socket)) => watch.add(socket),
            Some(Err(e)) => log::warn!("RPC: a new connection goes unwatched: {e}"),
            None => log::warn!("RPC: a new connection has no socket to watch"),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn watch_added_slot(_: &ConnState, _: &dyn RpcTransport) {}

    /// A `spawn_serve` that found no thread: its loop no longer reads, so nothing else may notice.
    fn after_a_failed_serve_spawn(&self) {
        if let Err(e) = self.watch_for_death_link() {
            log::error!("RPC: no watcher in place of a serve loop: {e:?}");
        }
        // Its signal may have gone while the loop still counted: the watcher ended on it.
        if self.peer_closed() {
            let _ = self.reaper_tx.send(ReaperJob::PeerClosed);
        }
    }

    fn incoming_slot_count(&self) -> usize {
        self.conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Incoming)
            .count()
    }

    /// `RpcSession::peer_closed`: the transports copied out, then polled outside the lock.
    pub(crate) fn peer_closed(&self) -> bool {
        let transports: Vec<Arc<dyn RpcTransport>> = self
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .map(|s| Arc::clone(&s.transport))
            .collect();
        transports.iter().any(|t| t.peer_closed() == Some(true))
    }

    /// Shut every slot down so a `recv`-blocked thread sees `EndOfStream`; unlocked syscalls.
    fn shutdown_all_transports(&self) {
        let transports: Vec<Arc<dyn RpcTransport>> = self
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .slots
            .iter()
            .map(|s| Arc::clone(&s.transport))
            .collect();
        for t in transports {
            if let Err(e) = t.shutdown() {
                log::warn!("RPC: shutting a connection's transport down failed: {e}");
            }
        }
    }

    fn take_incoming_threads(&self) -> Vec<(u64, std::thread::JoinHandle<()>)> {
        std::mem::take(
            &mut *self
                .incoming_threads
                .lock()
                .expect("incoming_threads poisoned"),
        )
    }

    /// The r34 id an `RpcServer` minted for this session, answered by `GET_SESSION_ID`.
    pub(crate) fn set_r34_session_id(&self, id: i32) {
        self.shared.r34_session_id.store(id, Ordering::SeqCst);
    }

    /// AOSP `setMaxIncomingThreads`: the `GET_MAX_THREADS` advertise and the `Incoming` slot cap.
    pub(crate) fn max_threads_value(&self) -> u32 {
        // `Relaxed`: one config cell set before any accept; `thread::spawn` orders the workers.
        self.shared.max_threads.load(Ordering::Relaxed)
    }

    /// Negotiated wire version (`None` for R34); an attach that settled on another one is refused.
    pub(crate) fn wire_protocol_version(&self) -> Option<u32> {
        self.profile.wire_version()
    }

    /// The binder address after the present flag, per profile (module doc, binders).
    fn wire_write_binder_addr(&self, p: &mut Parcel, addr: &RpcAddress) -> Result<()> {
        match &self.profile {
            WireProfile::R34(_) => write_addr(p, addr),
            WireProfile::Android13Plus(_) => {
                p.write_aligned_data(&Android13PlusCodec::encode_addr(addr))
            }
        }
    }

    fn wire_read_binder_addr(&self, p: &mut Parcel) -> Result<RpcAddress> {
        match &self.profile {
            WireProfile::R34(_) => read_addr(p),
            WireProfile::Android13Plus(_) => {
                let slice = p.read_aligned_data(A13_ADDR_LEN)?;
                Android13PlusCodec::decode_addr(slice, 0).map_err(StatusCode::from)
            }
        }
    }

    /// AOSP `flattenBinder` (RPC branch): `i32` present flag, then the profile's address if set.
    fn write_binder(&self, binder: Option<&SIBinder>, parcel: &mut Parcel) -> Result<()> {
        match binder {
            None => {
                parcel.write(&0i32)?;
                // AOSP `finishFlattenBinder` follows a null binder too, with `UNDECLARED`.
                parcel.write(&self.profile.binder_stability_repr(None))?;
                Ok(())
            }
            Some(b) => {
                let addr = if let Some(rp) = (**b).as_any().downcast_ref::<RpcProxy>() {
                    // Another session's address is meaningless here (AOSP `onBinderLeaving`).
                    if !std::ptr::eq(rp.session_ptr(), self) {
                        log::error!("RPC: cannot send a binder from an unrelated RPC session");
                        return Err(StatusCode::InvalidOperation);
                    }
                    // Read back from a received parcel it would pass as a receipt and owe a DEC.
                    if !parcel.rpc_is_unsent() {
                        log::error!("RPC: cannot write a binder into a sent or received parcel");
                        return Err(StatusCode::InvalidOperation);
                    }
                    let addr = rp.address();
                    // AOSP `onBinderLeaving` on every wire: the release waits for the payment.
                    self.shared
                        .state
                        .lock()
                        .expect("rpc state poisoned")
                        .on_proxy_leaving(addr);
                    // Recorded so an unsent parcel gives it back on drop (`cancel_leaving`).
                    parcel.rpc_record_leaving_addr(addr);
                    // Pinned past the send, as the frame names the address this proxy keeps open.
                    parcel.rpc_pin_binder(b.clone());
                    addr
                } else if (**b).is_remote() {
                    // Kernel proxy: AOSP `onBinderLeaving` "Cannot send binder proxy over sockets".
                    log::error!("RPC: cannot send a kernel binder proxy over sockets");
                    return Err(StatusCode::InvalidOperation);
                } else {
                    // A bump recorded on a sent or received parcel would have no one to settle it.
                    if !parcel.rpc_is_unsent() {
                        log::error!("RPC: cannot write a local binder into a sent parcel");
                        return Err(StatusCode::BadType);
                    }
                    // Recorded so an unsent parcel gives the bump back on drop (`cancel_leaving`).
                    let addr = {
                        let mut st = self.shared.state.lock().expect("rpc state poisoned");
                        // AOSP `mTerminated`; `clear_local` holds this lock, so no late insert.
                        if self.shared.lifecycle.is_torn_down() {
                            return Err(StatusCode::DeadObject);
                        }
                        st.on_binder_leaving(b)?
                    };
                    parcel.rpc_record_leaving_addr(addr);
                    addr
                };
                // AOSP `flattenBinder` positions: module doc "Binders in an RPC parcel".
                let obj_pos = parcel.data_position();
                parcel.write(&1i32)?;
                self.wire_write_binder_addr(parcel, &addr)?;
                if self.profile.records_binder_positions() {
                    parcel.rpc_record_object_position(obj_pos);
                }
                // libbinder requires the declared stability: module doc (binders).
                parcel.write(&self.profile.binder_stability_repr(Some(b.stability())))?;
                // Freeze stability mutation once it crosses IPC, as the kernel path does.
                b.set_parceled();
                Ok(())
            }
        }
    }

    /// android `unflattenBinder` (RPC branch).
    fn read_binder(&self, parcel: &mut Parcel) -> Result<Option<SIBinder>> {
        // AOSP `unflattenBinder`: `objectPos` is captured before the present/type int32.
        let obj_pos = parcel.data_position();
        let present: i32 = parcel.read()?;
        if present == 0 {
            // AOSP `finishUnflattenBinder` follows a null binder too.
            let stability: i32 = parcel.read()?;
            self.profile.check_binder_stability(stability, true)?;
            return Ok(None);
        }
        // v2 strict receive (`bindersInObjectPositions`): module doc "Binders in an RPC parcel".
        if self.profile.records_binder_positions() && !parcel.rpc_object_position_present(obj_pos) {
            return Err(StatusCode::BadValue);
        }
        let addr = self.wire_read_binder_addr(parcel)?;
        // AOSP android-16.0.0_r4 `unflattenBinder`: only a received position enters.
        let received = parcel.rpc_received_at(obj_pos);
        let binder = if received {
            self.entered_binder_at(parcel, obj_pos, addr)?
        } else {
            self.lookup_binder(addr)?
        };
        // After `onBinderEntering`, as AOSP: the parcel's drop pays a refused binder's receipt.
        let stability: i32 = parcel.read()?;
        let level = self.profile.check_binder_stability(stability, false)?;
        // A word this process wrote is the proxy's own `get()`, not a received level.
        if received {
            if let Some(rp) = (*binder).as_any().downcast_ref::<RpcProxy>() {
                rp.record_received_stability(level);
            }
        }
        Ok(Some(binder))
    }

    /// The binder a received non-null RPC binder at `obj_pos` names, entered once.
    fn entered_binder_at(
        &self,
        parcel: &mut Parcel,
        obj_pos: usize,
        addr: RpcAddress,
    ) -> Result<SIBinder> {
        if let Some((entered, binder)) = parcel.rpc_entered_at(obj_pos) {
            if entered != addr {
                log::error!("RPC: position {obj_pos} entered {entered:?}, now reads {addr:?}");
                return Err(StatusCode::BadValue);
            }
            return Ok(binder);
        }
        let binder = self.enter_binder(addr)?;
        parcel.rpc_record_entered(obj_pos, addr, binder.clone());
        Ok(binder)
    }

    /// AOSP `lookupAddress`, for a binder that did not arrive with its parcel: nothing is owed.
    fn lookup_binder(&self, addr: RpcAddress) -> Result<SIBinder> {
        let found = {
            let st = self.shared.state.lock().expect("rpc state poisoned");
            st.lookup_local(&addr).or_else(|| st.lookup_remote(&addr))
        };
        found.ok_or_else(|| {
            log::error!("RPC: {addr:?} names no live binder in a parcel that was not received");
            StatusCode::BadValue
        })
    }

    /// AOSP `onBinderEntering` + `flushExcessBinderRefs`: one receipt of `addr`, paid as owed.
    fn enter_binder(&self, addr: RpcAddress) -> Result<SIBinder> {
        // Cannot fail: `self` is reached through the `Arc` `with_shared` published.
        let strong = self.self_weak().upgrade().ok_or(StatusCode::DeadObject)?;
        // Explicit block: the state guard drops before the `DEC_STRONG` send (no I/O under it).
        let (binder, owed) = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            if let Some(local) = st.lookup_local(&addr) {
                // Coming home: the peer took a `timesSent` to send it, and nothing else pays it.
                (local, true)
            } else {
                // AOSP `onBinderEntering`; under this lock a proxy lands in the obituary snapshot.
                if self.shared.lifecycle.is_torn_down() {
                    return Err(StatusCode::DeadObject);
                }
                // `excess`: a live proxy's single drop DEC cannot pay this receipt too.
                st.remote_proxy(addr, || {
                    SIBinder::new(Arc::new(RpcProxy::new(addr, strong)))
                        .expect("SIBinder::new(RpcProxy)")
                })?
            }
        };
        if owed {
            self.send_dec_strong(addr, 1);
        }
        Ok(binder)
    }

    /// AOSP `rpcSetDataReference`; a failed entry still returns the parcel, to drop unlocked.
    fn receive_parcel(
        &self,
        data: Vec<u8>,
        object_positions: Vec<u32>,
        in_fds: Vec<OwnedFd>,
    ) -> (Parcel, Result<()>) {
        let mut p = Parcel::from_vec(data);
        // Without the profile a v1+ fd reads as the R34 `[present|idx]` shape.
        p.configure_rpc(
            self.parcel_ops(),
            self.fd_mode(),
            self.records_fd_positions(),
        );
        let received_fds = in_fds.len();
        p.rpc_set_in_fds(in_fds);
        // After `configure_rpc` (RPC mode), so binder/FD reads validate positions.
        p.rpc_set_object_positions(object_positions);
        p.rpc_mark_received();
        let entered = if self.profile.records_fd_positions() {
            self.validate_objects(&mut p, received_fds)
        } else {
            Ok(())
        };
        p.set_data_position(0);
        (p, entered)
    }

    /// AOSP android-17.0.0_r1 `rpcSetDataReference` object loop: a bad object ends the session.
    fn validate_objects(&self, p: &mut Parcel, received_fds: usize) -> Result<()> {
        use super::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR;
        const TYPE_BINDER: i32 = 1;
        let mut named_fds = 0;
        // By index: reading a binder never changes the table, and `read_binder` looks it up.
        for i in 0..p.rpc_object_positions().len() {
            let pos = p.rpc_object_positions()[i];
            p.set_data_position(pos as usize);
            let object = match p.read::<i32>() {
                Ok(TYPE_BINDER) if self.profile.records_binder_positions() => {
                    p.set_data_position(pos as usize);
                    self.read_binder(p).map(drop)
                }
                Ok(TYPE_NATIVE_FILE_DESCRIPTOR) => {
                    named_fds += 1;
                    Ok(())
                }
                Ok(other) => {
                    log::error!("RPC: object position {pos} holds type {other}, not on this wire");
                    Err(StatusCode::BadValue)
                }
                // AOSP's range check ahead of this loop, a position past the parcel: `BAD_VALUE`.
                Err(_) => Err(StatusCode::BadValue),
            };
            if let Err(e) = object {
                log::error!("RPC: a parcel's object at {pos} is not readable ({e}); session ends");
                self.stop_for_the_reaper();
                return Err(e);
            }
        }
        if named_fds != received_fds {
            log::error!("RPC: a parcel names {named_fds} fds and came with {received_fds}");
            return Err(StatusCode::BadValue);
        }
        Ok(())
    }

    /// AOSP `appendFrom` `TYPE_BINDER` arm (`lookupAddress`, `onBinderLeaving`), one lock per copy.
    fn acquire_copied(&self, objects: &[&[u8]]) -> Result<CopiedBinders> {
        let addrs = objects
            .iter()
            .map(|bytes| self.decode_copied_addr(bytes))
            .collect::<Result<Vec<_>>>()?;
        let mut took = CopiedBinders::default();
        // Clones and released nodes drop after the guard: a user `Drop` may re-enter the session.
        let mut release: Vec<SIBinder> = Vec::new();
        let mut releases: Vec<(RpcAddress, u32)> = Vec::new();
        let result = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            let result = self.acquire_copied_locked(&mut st, &addrs, &mut took, &mut release);
            if result.is_err() {
                for addr in took.leaving.drain(..) {
                    let (node, held) = st.cancel_leaving(&addr);
                    release.extend(node);
                    releases.push((addr, held));
                }
            }
            result
        };
        drop(release);
        self.send_held_releases(releases);
        result.map(|()| took)
    }

    /// [`Self::acquire_copied`] under the lock; on `Err` the caller rolls `took.leaving` back.
    fn acquire_copied_locked(
        &self,
        st: &mut RpcState,
        addrs: &[RpcAddress],
        took: &mut CopiedBinders,
        release: &mut Vec<SIBinder>,
    ) -> Result<()> {
        // AOSP `mTerminated`, as in `write_binder`: `clear_local` holds this lock.
        if self.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        for addr in addrs {
            if let Some(local) = st.lookup_local(addr) {
                let leaving = st.on_binder_leaving(&local);
                release.push(local);
                let leaving = leaving?;
                if leaving != *addr {
                    // AOSP shuts down here; rsbinder refuses the copy, as `remote_proxy` does.
                    log::error!("RPC: copied address {addr:?} names a node at {leaving:?}");
                    release.extend(st.cancel_binder_leaving(&leaving));
                    return Err(StatusCode::BadValue);
                }
                took.leaving.push(leaving);
            } else if addr.is_zero() || addr.minted_by(self.shared.space) {
                log::error!("RPC: a copied binder names no node of this session: {addr:?}");
                return Err(StatusCode::BadValue);
            } else if let Some(proxy) = st.lookup_remote(addr) {
                // AOSP `appendFrom` runs `onBinderLeaving` on proxies too, as in `write_binder`.
                st.on_proxy_leaving(*addr);
                took.leaving.push(*addr);
                took.pinned.push(proxy);
            }
            // No live proxy: a source that neither received nor wrote it (parcel.rs "append_from").
        }
        Ok(())
    }

    /// The address after a copied binder's type word, per profile (`wire_read_binder_addr`).
    fn decode_copied_addr(&self, bytes: &[u8]) -> Result<RpcAddress> {
        match &self.profile {
            WireProfile::R34(_) => {
                let raw = bytes
                    .get(..RPC_ADDR_LEN)
                    .and_then(|b| <[u8; RPC_ADDR_LEN]>::try_from(b).ok())
                    .ok_or(StatusCode::BadValue)?;
                Ok(RpcAddress::from_wire_bytes(raw))
            }
            WireProfile::Android13Plus(_) => {
                Android13PlusCodec::decode_addr(bytes, 0).map_err(StatusCode::from)
            }
        }
    }

    /// `Parcel::drop`'s half of AOSP `truncateRpcObjects`: one `cancel_binder_leaving` per address.
    fn cancel_leaving(&self, addrs: &[RpcAddress]) {
        if addrs.is_empty() {
            return;
        }
        // Released nodes drop after the guard: a user `Drop` may re-enter this session.
        let (released, releases): (Vec<SIBinder>, Vec<(RpcAddress, u32)>) = {
            // Reached from a `Drop`, maybe while unwinding: a poisoned table is still consistent.
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut released = Vec::new();
            let mut releases = Vec::new();
            for a in addrs {
                let (node, held) = st.cancel_leaving(a);
                released.extend(node);
                releases.push((*a, held));
            }
            (released, releases)
        };
        drop(released);
        self.send_held_releases(releases);
    }

    /// Send the proxy releases a rollback or a payment let go; the lock is already dropped.
    fn send_held_releases(&self, releases: Vec<(RpcAddress, u32)>) {
        for (addr, amount) in releases {
            self.send_dec_strong(addr, amount);
        }
    }

    /// An inbound `DEC_STRONG`: our node's release, or the peer paying back its address's sends.
    fn apply_dec_strong(&self, addr: RpcAddress, amount: u32) {
        // Bound: a statement temporary would drop the ref (user `Drop`) under the guard.
        let (released, held) = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            (
                st.dec_strong_local(&addr, amount),
                st.pay_proxy_sends(&addr, amount),
            )
        };
        drop(released);
        self.send_dec_strong(addr, held);
    }

    /// `RpcProxy::drop`: forget the proxy and send its `DEC_STRONG`, or hold it for unpaid sends.
    pub(crate) fn release_proxy(&self, addr: RpcAddress, who: *const ()) {
        let now = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .release_proxy(&addr, who);
        self.send_dec_strong(addr, now);
    }

    /// Undo a oneway attempt's `async_number` reservation; binder bumps belong to the parcel.
    fn cancel_oneway_number(&self, addr: RpcAddress, consumed: u64) -> bool {
        self.shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .cancel_send_async_number(addr, consumed)
    }

    /// `RpcProxy::__try_oneway`: a oneway on a free `Outgoing` slot that waits for nothing.
    ///
    /// `Ok` is taken: whole on the stream unless the session ends first (`__send_no_wait`).
    /// `WouldBlock` is not sent, with the session as it was; the reaper judges a
    /// slot with no room ("Deferred `DEC_STRONG`"). `InvalidOperation` is a session or transport
    /// with no such send, `BadValue` a frame past the transport's limit, and `DeadObject` a
    /// session ended or ending, which the reaper does. A caller sends one at a time per `addr`: a
    /// reservation it could not take back ends the session rather than leave a gap in the numbers.
    pub(crate) fn try_oneway(
        &self,
        addr: RpcAddress,
        code: u32,
        data: &Parcel,
        flags: u32,
    ) -> Result<()> {
        self.check_parcel_session(data)?;
        data.rpc_begin_send()?;
        let conn = match self.find_conn_no_wait() {
            Ok(conn) => conn,
            Err(e) => {
                data.rpc_end_send(false);
                return Err(e);
            }
        };
        let slot_id = conn.slot_id;
        let counts_target = !addr.is_zero() && self.profile.counts_transaction_targets();
        let async_number = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            if counts_target && st.past_unpaid_limit(&addr) {
                None
            } else {
                if counts_target {
                    st.on_proxy_leaving(addr);
                }
                Some(st.next_send_async_number(addr))
            }
        };
        let Some(async_number) = async_number else {
            log::warn!("RPC: a peer leaves too many addresses unpaid; ending the session");
            data.rpc_end_send(false);
            self.stop_for_the_reaper();
            return Err(StatusCode::DeadObject);
        };
        let txn = WireTransactionRef {
            address: &addr,
            code,
            flags: flags | FLAG_ONEWAY,
            async_number,
            data: data.rpc_data_bytes(),
            object_positions: data.rpc_object_positions(),
        };
        let sent = match self.profile.codec().encode_transact_ref(txn) {
            Ok(frame) => self.send_msg_no_wait(conn.transport(), &frame, data.rpc_out_fds()),
            Err(e) => Some(Err(e)),
        };
        // While this thread holds the slot: nothing may be written after a cut frame.
        if let Some(Err(e)) = &sent {
            self.end_on_the_reaper(e);
        }
        // Before the release a rollback lets go: it picks its slot, and this one may be free.
        drop(conn);
        let take_back = || {
            data.rpc_end_send(false);
            self.take_back_oneway(addr, async_number, counts_target)
        };
        match sent {
            Some(Ok(true)) => {
                data.rpc_end_send(true);
                Ok(())
            }
            None => {
                take_back();
                Err(StatusCode::InvalidOperation)
            }
            Some(Ok(false)) => {
                if !take_back() {
                    return Err(StatusCode::DeadObject);
                }
                if !self.awaiting_room.swap(true, Ordering::SeqCst) {
                    let _ = self.reaper_tx.send(ReaperJob::AwaitRoom(slot_id));
                }
                Err(StatusCode::WouldBlock)
            }
            Some(Err(e @ (RpcError::FrameTooLarge { .. } | RpcError::Protocol(_)))) => {
                take_back();
                Err(e.into())
            }
            Some(Err(_)) => {
                take_back();
                Err(StatusCode::DeadObject)
            }
        }
    }

    /// An unsent oneway's number and target send; `false` if the session has to end for a gap.
    fn take_back_oneway(&self, addr: RpcAddress, number: u64, counts_target: bool) -> bool {
        let numbered = self.cancel_oneway_number(addr, number);
        if !numbered {
            log::warn!("RPC: a oneway's number cannot be taken back; ending the session");
            self.stop_for_the_reaper();
        }
        if counts_target {
            let held = self
                .shared
                .state
                .lock()
                .expect("rpc state poisoned")
                .pay_proxy_sends(&addr, 1);
            self.send_dec_strong(addr, held);
        }
        numbered
    }

    /// Outbound transaction: the reply (`None` if oneway), applying interleaved `DEC_STRONG`s.
    pub(crate) fn client_transact(
        &self,
        addr: RpcAddress,
        code: u32,
        data: &Parcel,
        flags: u32,
    ) -> Result<Option<Parcel>> {
        let oneway = (flags & FLAG_ONEWAY) != 0;
        self.check_parcel_session(data)?;
        // Before the slot wait, so a second sender of the same parcel never enters it.
        data.rpc_begin_send()?;
        let found = if oneway {
            self.find_conn_async()
        } else {
            self.find_conn()
        };
        let conn = match found {
            Ok(conn) => conn,
            Err(e) => {
                // The parcel still owns its bumps; a retry or its drop settles them.
                data.rpc_end_send(false);
                return Err(e);
            }
        };
        let transport = conn.transport();
        // AOSP `transactInternal` `onBinderLeaving`: the peer pays the target back (r34 does not).
        let counts_target = !addr.is_zero() && self.profile.counts_transaction_targets();
        // AOSP `BinderNode::asyncNumber` (send side, per-remote-addr).
        let async_number = {
            let mut st = self.shared.state.lock().expect("rpc state poisoned");
            if counts_target {
                st.on_proxy_leaving(addr);
            }
            if oneway {
                st.next_send_async_number(addr)
            } else {
                0
            }
        };
        // Borrowed: the encoder copies the payload into the frame once.
        let txn = WireTransactionRef {
            address: &addr,
            code,
            flags,
            async_number,
            data: data.rpc_data_bytes(),
            // Binder (v2) / FD (v1+) positions from serialization; empty on R34 / v0.
            object_positions: data.rpc_object_positions(),
        };
        // The attempt owns the async number and target send; returns a release the send let go.
        let rollback = || {
            let numbered = !oneway || self.cancel_oneway_number(addr, async_number);
            data.rpc_end_send(false);
            let held = if counts_target {
                let mut st = self.shared.state.lock().expect("rpc state poisoned");
                st.pay_proxy_sends(&addr, 1)
            } else {
                0
            };
            if !numbered {
                log::warn!("RPC: a oneway's number cannot be taken back; ending the session");
                self.fail_session();
            }
            held
        };
        let frame = match self.profile.codec().encode_transact_ref(txn) {
            Ok(frame) => frame,
            Err(e) => {
                let held = rollback();
                self.send_dec_strong(addr, held);
                return Err(e.into());
            }
        };
        // A twoway is open from its send to its return, nested dispatches included ("Idle").
        let _open = (!oneway).then(|| OpenCall::enter(&self.shared));
        // Out-of-band fds (empty unless `Unix` fd-mode); the send reads while it waits.
        let mut drained = DrainedReleases::default();
        let mut drain_deadline = None;
        let fault = std::cell::Cell::new(None);
        let slot = conn.slot_id;
        let mut drain =
            || self.drain_one(transport, slot, &mut drain_deadline, &mut drained, &fault);
        // What this slot holds goes first, so a call sees the releases made before it.
        let sent = self
            .take_held_dec_strongs(slot)
            .into_iter()
            .try_for_each(|(a, amount)| self.send_dec_frames(&conn, a, amount, Some(&mut drain)))
            .and_then(|()| self.send_msg(transport, &frame, data.rpc_out_fds(), Some(&mut drain)));
        drop(drain_deadline);
        let DrainedReleases { released, releases } = drained;
        if let Err(e) = sent {
            let held = rollback();
            // First: a release written now on this slot would follow a cut frame.
            self.end_after_failed_send(&e);
            drop(released);
            self.send_held_releases(releases);
            self.send_dec_strong(addr, held);
            return Err(fault.get().unwrap_or_else(|| Self::send_error_status(e)));
        }
        drop(released);
        self.send_held_releases(releases);
        data.rpc_end_send(true);
        if oneway {
            return Ok(None);
        }
        // Post-send, any failure strands this REPLY (no id), so the session ends: "Session end".
        let fail = |status: StatusCode| {
            self.fail_session();
            Err(status)
        };
        // The reply deadline covers the reply wait only; the guard restores what it replaced.
        let baseline = self.slot_baseline_read_deadline(conn.slot_id);
        // Driving a serve slot of this session, the idle value stands in ("Reply deadlines").
        let deadline = self.timeout().or_else(|| self.handler_read_deadline());
        let _deadline_guard = match ReplyDeadlineGuard::arm(transport, deadline, baseline) {
            Ok(g) => g,
            Err(e) => return fail(e.into()),
        };
        loop {
            // A closed peer's `REPLY` may be queued: the read takes it, or reports the close.
            if let Err(HeldWriteEnd::Failed(status)) = self.write_held_past_limit(&conn) {
                return fail(status);
            }
            let (frame, in_fds) = match self.recv_msg(transport) {
                Ok(v) => v,
                Err(e) => return fail(e.into()),
            };
            let message = match self.profile.codec().decode_message(&frame) {
                Ok(m) => m,
                Err(e) => return fail(e.into()),
            };
            match message {
                WireMessage::Reply(WireReply {
                    status,
                    data,
                    object_positions,
                }) => {
                    if status != 0 {
                        return Err(StatusCode::from(status));
                    }
                    let (reply, entered) = self.receive_parcel(data, object_positions, in_fds);
                    // The frame is consumed, so the stream stays in step, if the session goes on.
                    entered?;
                    return Ok(Some(reply));
                }
                WireMessage::DecStrong(a, amount) => self.apply_dec_strong(a, amount),
                WireMessage::Transact(t) => {
                    // Inline nested callback; leaving the wait early strands our `REPLY`.
                    let _restore = match NestedDeadlineGuard::lift(transport, deadline) {
                        Ok(g) => g,
                        Err(e) => return fail(e.into()),
                    };
                    let peer = transport.peer_identity();
                    if let Err(e) = self.dispatch_transact(t, in_fds, peer) {
                        return fail(e);
                    }
                }
            }
        }
    }

    /// Pay `addr`'s receipts; waits only to finish a pin's frame, runs no death sequence ("Deferred").
    pub(crate) fn send_dec_strong(&self, addr: RpcAddress, amount: u32) {
        // Past `Live` the pool is draining; the best-effort send is skipped.
        if amount == 0 || self.shared.lifecycle.is_torn_down() {
            return;
        }
        match self.dec_route() {
            DecRoute::Conn(conn) => {
                let left = self.write_dec_no_wait(&conn, addr, amount);
                // First: the reaper may want this slot.
                drop(conn);
                self.reap_dec_strong(addr, left);
            }
            DecRoute::ConnFromPin(conn, pin) => {
                let left = self.write_dec_no_wait(&conn, addr, amount);
                drop(conn);
                self.owe_from_pin(pin, addr, left);
            }
            DecRoute::Pin(conn) => self.write_dec_on_pin(&conn, addr, amount),
            DecRoute::Pending(slot_id) => match self.held_past_limit(slot_id, &addr) {
                Some(conn) => self.write_dec_on_pin(&conn, addr, amount),
                None => self.hold_dec_strong(slot_id, addr, amount),
            },
            DecRoute::Reaper => self.reap_dec_strong(addr, amount),
        }
    }

    /// Write on this pin, whose holder reads next, leaving no part of a frame; hold what is left.
    fn write_dec_on_pin(&self, conn: &ConnGuard<'_>, addr: RpcAddress, amount: u32) {
        let transport = conn.transport();
        let left = if transport.__whole_small_frame_now() {
            self.write_dec_no_wait(conn, addr, amount)
        } else {
            amount
        };
        if transport.__carrying() && !self.shared.lifecycle.is_torn_down() {
            let _sending = OpenCall::enter(&self.shared);
            if let Err(e) = transport.__flush_carried(None) {
                self.end_no_wait_write(conn, e);
            }
        }
        if left > 0 {
            self.hold_dec_strong(conn.slot_id, addr, left);
        }
    }

    /// The `DEC_STRONG` frames for `addr` on `conn` with no wait for room: the amount left unsent.
    fn write_dec_no_wait(&self, conn: &ConnGuard<'_>, addr: RpcAddress, amount: u32) -> u32 {
        let Some((frame, times)) = self.profile.codec().encode_dec_strong(&addr, amount) else {
            return 0;
        };
        for written in 0..times {
            // A transport with no such send has no room to promise: the reaper's write waits.
            let sent = self
                .send_msg_no_wait(conn.transport(), &frame, &[])
                .unwrap_or(Ok(false));
            match sent {
                Ok(true) => {}
                // `times` is `amount` where a frame is one reference, else 1.
                Ok(false) => {
                    self.pay_receipts(&addr, written);
                    return amount - written;
                }
                Err(e) => {
                    self.end_no_wait_write(conn, e);
                    return 0;
                }
            }
        }
        self.pay_receipts(&addr, amount);
        0
    }

    /// `end_on_the_reaper` for a release on `conn`; its reader ends as on a hold's failed write.
    fn end_no_wait_write(&self, conn: &ConnGuard<'_>, e: RpcError) {
        // Past `Live`, what ended the session failed this write: it says nothing of this slot.
        if !Self::failed_send_ends_session(&e) || self.shared.lifecycle.is_torn_down() {
            return;
        }
        // Before the stop: a loop whose slot the reaper has emptied reads its end from this.
        if let Err(end) = Self::held_write_end(e) {
            let mut st = self.conn_state.lock().expect("conn_state poisoned");
            st.no_wait_write_ends.entry(conn.slot_id).or_insert(end);
        }
        self.stop_for_the_reaper();
    }

    /// What a failed no-wait write on `slot_id` left its reader; a nested wait reads it too.
    fn no_wait_write_end(&self, slot_id: u64) -> Option<HeldWriteEnd> {
        // Recorded before the failure takes the lifecycle out of `Live`: no lock for a live frame.
        if !self.shared.lifecycle.is_torn_down() {
            return None;
        }
        let st = self.conn_state.lock().expect("conn_state poisoned");
        st.no_wait_write_ends.get(&slot_id).copied()
    }

    /// `RpcState::pay_receipts`, after the write that paid them; the state lock is not held there.
    fn pay_receipts(&self, addr: &RpcAddress, amount: u32) {
        // Reached from a `Drop`, maybe while unwinding: a poisoned table is still consistent.
        let mut st = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.pay_receipts(addr, amount);
    }

    /// Add `amount` to what the reaper owes `addr`, and wake it if nothing else will (step 4).
    fn reap_dec_strong(&self, addr: RpcAddress, amount: u32) {
        if amount == 0 {
            return;
        }
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let owed = st.reaper_owed.entry(addr).or_insert(0);
        *owed = owed.saturating_add(amount);
        self.wake_reaper_for_owed(st);
    }

    /// Queue a `ReaperJob::Dec` if the session owes, none is queued and a slot could carry it.
    fn wake_reaper_for_owed(&self, mut st: std::sync::MutexGuard<'_, ConnState>) {
        let wake = !st.reaper_dec_queued && !st.reaper_owed.is_empty() && st.has_outgoing();
        st.reaper_dec_queued |= wake;
        drop(st);
        if wake {
            let _ = self.reaper_tx.send(ReaperJob::Dec);
        }
    }

    /// What the reaper is to write next, as a whole; `None` ends its `ReaperJob::Dec`.
    fn take_reaper_owed(&self) -> Option<std::collections::HashMap<RpcAddress, u32>> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        // Each is why `find_conn_for_reaper` finds nothing: asking again would spin.
        if st.reaper_owed.is_empty() || !st.has_outgoing() || self.shared.lifecycle.is_torn_down() {
            st.reaper_dec_queued = false;
            return None;
        }
        Some(std::mem::take(&mut st.reaper_owed))
    }

    /// What a free slot's write left for `pin`: its frames stop adding to the reaper at the limit.
    fn owe_from_pin(&self, pin: u64, addr: RpcAddress, left: u32) {
        if left == 0 {
            return;
        }
        let st = self.conn_state.lock().expect("conn_state poisoned");
        let past = st.reaper_owed.len() >= HELD_DEC_STRONG_LIMIT;
        drop(st);
        if past {
            self.hold_dec_strong(pin, addr, left);
        } else {
            self.reap_dec_strong(addr, left);
        }
    }

    /// `end_after_failed_send` for a thread that runs no death sequence, holding the slot.
    fn end_on_the_reaper(&self, e: &RpcError) {
        if Self::failed_send_ends_session(e) {
            self.stop_for_the_reaper();
        }
    }

    /// Write the `DEC_STRONG` frames for `addr` on `conn`; a failed send follows "Failed sends".
    fn write_dec_strong(
        &self,
        conn: &ConnGuard<'_>,
        addr: RpcAddress,
        amount: u32,
    ) -> RpcResult<()> {
        // No drain, as AOSP `sendDecStrongToTarget`.
        let written = self.send_dec_frames(conn, addr, amount, None);
        if let Err(e) = &written {
            self.end_after_failed_send(e);
        }
        written
    }

    /// The frames of what `conn` holds or the reader owes; the failure is the caller's to judge.
    fn send_dec_frames(
        &self,
        conn: &ConnGuard<'_>,
        addr: RpcAddress,
        amount: u32,
        mut drain: Option<&mut dyn FnMut() -> RpcResult<()>>,
    ) -> RpcResult<()> {
        let Some((frame, times)) = self.profile.codec().encode_dec_strong(&addr, amount) else {
            return Ok(());
        };
        for _ in 0..times {
            let reads = drain
                .as_mut()
                .map(|d| &mut **d as &mut dyn FnMut() -> RpcResult<()>);
            self.send_msg(conn.transport(), &frame, &[], reads)?;
        }
        self.pay_receipts(&addr, amount);
        Ok(())
    }

    /// The pin itself once it holds `HELD_DEC_STRONG_LIMIT` other addresses: AOSP's write there.
    fn held_past_limit(&self, slot_id: u64, addr: &RpcAddress) -> Option<ConnGuard<'_>> {
        let st = self.conn_state.lock().expect("conn_state poisoned");
        let s = st.slots.iter().find(|s| s.id == slot_id)?;
        if s.pending_dec.len() < HELD_DEC_STRONG_LIMIT || s.pending_dec.contains_key(addr) {
            return None;
        }
        // This thread drives the slot (`Pending` came from its pin), so the guard is reentrant.
        Some(ConnGuard {
            inner: self,
            slot_id,
            transport: Arc::clone(&s.transport),
            reentrant: true,
        })
    }

    /// Add to `slot_id`'s held `DEC_STRONG`s, one entry per address; lost if the slot is gone.
    fn hold_dec_strong(&self, slot_id: u64, addr: RpcAddress, amount: u32) {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let Some(slot) = st.slots.iter_mut().find(|s| s.id == slot_id) else {
            return;
        };
        // Keyed: a peer-sized backlog would make a linear search quadratic under `conn_state`.
        let owed = slot.pending_dec.entry(addr).or_insert(0);
        *owed = owed.saturating_add(amount);
    }

    /// What a reader's failed write of a hold ends it with; `Ok` reads on ("Failed sends").
    fn held_write_end(e: RpcError) -> std::result::Result<(), HeldWriteEnd> {
        match e {
            RpcError::EndOfStream => Err(HeldWriteEnd::PeerClosed),
            RpcError::FrameTooLarge { .. } | RpcError::Protocol(_) => Ok(()),
            e => Err(HeldWriteEnd::Failed(Self::send_error_status(e))),
        }
    }

    /// Write what `conn`'s reader leaves owed past `HELD_DEC_STRONG_LIMIT`, between frames.
    fn write_held_past_limit(&self, conn: &ConnGuard<'_>) -> std::result::Result<(), HeldWriteEnd> {
        fn past_limit(
            owed: &mut std::collections::HashMap<RpcAddress, u32>,
        ) -> Vec<(RpcAddress, u32)> {
            let excess = owed.len().saturating_sub(HELD_DEC_STRONG_LIMIT);
            let addrs: Vec<RpcAddress> = owed.keys().take(excess).copied().collect();
            addrs.iter().filter_map(|a| owed.remove_entry(a)).collect()
        }
        // Each reader's last step ahead of its read: a release's failure counts as this write's.
        if let Some(end) = self.no_wait_write_end(conn.slot_id) {
            return Err(end);
        }
        let past: Vec<(RpcAddress, u32)> = {
            let mut guard = self.conn_state.lock().expect("conn_state poisoned");
            let st = &mut *guard;
            let Some(slot) = st.slots.iter_mut().find(|s| s.id == conn.slot_id) else {
                return Ok(());
            };
            let mut past = past_limit(&mut slot.pending_dec);
            let (outgoing, called) = (slot.role == SlotRole::Outgoing, slot.peer_called);
            // The reaper may be waiting for this very slot, or have no slot to write on at all.
            if outgoing || (called && !st.has_outgoing()) {
                past.extend(past_limit(&mut st.reaper_owed));
            }
            past
        };
        for (addr, amount) in past {
            if let Err(e) = self.write_dec_strong(conn, addr, amount) {
                return Self::held_write_end(e);
            }
        }
        Ok(())
    }

    /// Before a serve loop's read on an `Outgoing` slot: what no one else writes while it reads.
    fn write_before_serve_read(
        &self,
        conn: &ConnGuard<'_>,
    ) -> std::result::Result<(), HeldWriteEnd> {
        let transport = conn.transport();
        let slot = conn.slot_id;
        // android-12's oneway handler nests a `TRANSACT` here, which a drain would refuse.
        let drains = self.profile.wire_version().is_some();
        loop {
            let carrying = transport.__carrying();
            let held = self.take_held_for_serve_read(slot);
            let idle = !carrying && held.is_empty();
            if idle || self.shared.lifecycle.is_torn_down() {
                return Ok(());
            }
            let mut drained = DrainedReleases::default();
            let mut deadline = None;
            let fault = std::cell::Cell::new(None);
            let mut drain = || self.drain_one(transport, slot, &mut deadline, &mut drained, &fault);
            let mut written = Ok(());
            if carrying {
                // A frame on its way out is activity until its write returns ("Idle").
                let _sending = OpenCall::enter(&self.shared);
                written = transport.__flush_carried(drains.then_some(&mut drain));
            }
            if written.is_ok() {
                written = held.into_iter().try_for_each(|(addr, amount)| {
                    self.send_dec_frames(conn, addr, amount, drains.then_some(&mut drain))
                });
            }
            drop(deadline);
            let DrainedReleases { released, releases } = drained;
            // First: a release written now on this slot would follow a cut frame.
            if let Err(e) = &written {
                self.end_after_failed_send(e);
            }
            drop(released);
            self.send_held_releases(releases);
            // With no failure the next round looks again: a release may add a hold.
            if let Err(e) = written {
                return match fault.get() {
                    Some(status) => Err(HeldWriteEnd::Failed(status)),
                    None => Self::held_write_end(e),
                };
            }
        }
    }

    /// How a serve loop ends once a write on its slot failed: module doc "Session end".
    fn end_after_failed_write(&self, end: HeldWriteEnd, transport: &dyn RpcTransport) -> EndReason {
        match end {
            HeldWriteEnd::Failed(status) => EndReason::Dispatch(status),
            HeldWriteEnd::PeerClosed => self.read_to_peers_close(transport),
        }
    }

    /// A serve loop's read once its write found the peer closed: nothing it read may run.
    fn read_to_peers_close(&self, transport: &dyn RpcTransport) -> EndReason {
        loop {
            match self.recv_msg(transport) {
                Ok(_) if self.shared.ended_locally.load(Ordering::SeqCst) => {
                    return EndReason::Interrupted;
                }
                Ok(_) => {}
                Err(e) => return recv_end_reason(e),
            }
        }
    }

    /// What this `Outgoing` slot holds, emptied; an `Incoming` slot's waits for its next frame.
    fn take_held_for_serve_read(&self, slot_id: u64) -> std::collections::HashMap<RpcAddress, u32> {
        let mut st = self.conn_state.lock().expect("conn_state poisoned");
        let slot = st
            .slots
            .iter_mut()
            .find(|s| s.id == slot_id && s.role == SlotRole::Outgoing);
        slot.map(|s| std::mem::take(&mut s.pending_dec))
            .unwrap_or_default()
    }

    /// Before a serve loop's read: no reply need follow on this slot, so the reaper takes the hold.
    fn hand_held_to_reaper(&self, slot_id: u64) {
        let mut guard = self.conn_state.lock().expect("conn_state poisoned");
        let st = &mut *guard;
        // A serve loop's `Outgoing` slot waits for the peer's frame: the hold is no later here.
        let writable = |s: &ConnSlot| s.role == SlotRole::Outgoing && !s.serving;
        if !st.slots.iter().any(writable) {
            return;
        }
        let Some(slot) = st
            .slots
            .iter_mut()
            .find(|s| s.id == slot_id && s.role == SlotRole::Incoming)
        else {
            return;
        };
        let room = HELD_DEC_STRONG_LIMIT.saturating_sub(st.reaper_owed.len());
        let addrs: Vec<RpcAddress> = slot.pending_dec.keys().take(room).copied().collect();
        if addrs.is_empty() {
            return;
        }
        for addr in addrs {
            if let Some(amount) = slot.pending_dec.remove(&addr) {
                let owed = st.reaper_owed.entry(addr).or_insert(0);
                *owed = owed.saturating_add(amount);
            }
        }
        self.wake_reaper_for_owed(guard);
    }

    /// The `DEC_STRONG`s to write before `slot_id`'s next frame, emptied: the peer reads them.
    fn take_held_dec_strongs(&self, slot_id: u64) -> std::collections::HashMap<RpcAddress, u32> {
        let mut guard = self.conn_state.lock().expect("conn_state poisoned");
        let st = &mut *guard;
        let Some(slot) = st.slots.iter_mut().find(|s| s.id == slot_id) else {
            return std::collections::HashMap::new();
        };
        let mut held = std::mem::take(&mut slot.pending_dec);
        let called = slot.peer_called;
        // With an `Outgoing` slot they are the reaper's; without one this is their way out.
        if called && !st.has_outgoing() {
            for (addr, amount) in std::mem::take(&mut st.reaper_owed) {
                let owed = held.entry(addr).or_insert(0);
                *owed = owed.saturating_add(amount);
            }
        }
        held
    }

    /// Fire each cached proxy's `binder_died` (AOSP `sendObituaries`) unlocked; idempotent.
    pub(crate) fn send_session_obituaries(&self) {
        let snapshot = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .remote_proxy_snapshot();
        for arc in snapshot {
            let Some(proxy) = arc.as_any().downcast_ref::<RpcProxy>() else {
                continue;
            };
            // `who` = the dying proxy's weak binder (kernel `send_obituary(&WIBinder)` parity).
            let sib = SIBinder::from_arc(arc.clone());
            let who = SIBinder::downgrade(&sib);
            proxy.send_obituary(&who);
        }
    }

    /// Send a `REPLY`; `object_positions` is empty for error/no-payload replies and on R34 / v0.
    fn send_reply(
        &self,
        status: i32,
        data: &[u8],
        object_positions: &[u32],
        fds: &[OwnedFd],
    ) -> Result<()> {
        self.write_reply(status, data, object_positions, fds)
            .map_err(|e| match e {
                ReplyNotSent::Refused(s) | ReplyNotSent::Failed(s) => s,
            })
    }

    /// `send_reply`, telling a refusal before any byte (module doc "Failed sends") from a failure.
    fn write_reply(
        &self,
        status: i32,
        data: &[u8],
        object_positions: &[u32],
        fds: &[OwnedFd],
    ) -> std::result::Result<(), ReplyNotSent> {
        let frame = self
            .profile
            .codec()
            .encode_reply_ref(WireReplyRef {
                status,
                data,
                object_positions,
            })
            .map_err(ReplyNotSent::refused)?;
        // `ConnUse::Reply` pins the request's slot whatever its role: the peer waits only there.
        let conn = self
            .find_conn_impl(ConnUse::Reply)
            .map_err(ReplyNotSent::Failed)?;
        // Held `DEC_STRONG`s go first, each its own frame: module doc "Deferred `DEC_STRONG`".
        for (addr, amount) in self.take_held_dec_strongs(conn.slot_id) {
            if let Err(e) = self.write_dec_strong(&conn, addr, amount) {
                // Only a codec error leaves the session up; any other already ended it.
                if !matches!(e, RpcError::FrameTooLarge { .. } | RpcError::Protocol(_)) {
                    return Err(ReplyNotSent::Failed(Self::send_error_status(e)));
                }
            }
        }
        // No drain, as AOSP's reply `rpcSend`: the peer is reading for this `REPLY`.
        if let Err(e) = self.send_msg(conn.transport(), &frame, fds, None) {
            self.end_after_failed_send(&e);
            return Err(match e {
                RpcError::FrameTooLarge { .. } | RpcError::Protocol(_) => ReplyNotSent::refused(e),
                e => ReplyNotSent::Failed(Self::send_error_status(e)),
            });
        }
        Ok(())
    }

    /// AOSP `RpcState::validateParcel`: a session parcel's addresses are this session's only.
    fn check_parcel_session(&self, parcel: &Parcel) -> Result<()> {
        match parcel.rpc_session_id() {
            Some(id) if id != (self as *const Self).cast() => {
                log::error!("RPC: the parcel was built for another session");
                Err(StatusCode::BadType)
            }
            _ => Ok(()),
        }
    }

    /// AOSP `validateParcel` on a reply; the fd cap is the transport's 64, not AOSP's 253.
    fn check_reply_parcel(&self, reply: &Parcel) -> Result<()> {
        if reply.rpc_session_id().is_none() {
            log::error!("RPC: the reply was built for no session; write into the one handed in");
            return Err(StatusCode::BadType);
        }
        self.check_parcel_session(reply)?;
        let fds = reply.rpc_out_fds().len();
        if fds > super::transport::unix::MAX_FDS_PER_FRAME {
            log::error!("RPC: a reply carries {fds} fds, over the per-frame limit");
            return Err(StatusCode::BadValue);
        }
        Ok(())
    }

    /// `send_reply` for a payload parcel; marked `Sent` after the write (module doc: binders).
    fn send_reply_parcel(&self, reply: &Parcel) -> Result<()> {
        if let Err(e) = self
            .check_reply_parcel(reply)
            .and_then(|()| reply.rpc_begin_send())
        {
            // AOSP `processTransactInternal`: a reply failing `validateParcel` goes out as status.
            return self.send_reply(e.into(), &[], &[], &[]);
        }
        let r = self.write_reply(
            0,
            reply.rpc_data_bytes(),
            reply.rpc_object_positions(),
            reply.rpc_out_fds(),
        );
        reply.rpc_end_send(r.is_ok());
        match r {
            Ok(()) => Ok(()),
            // Nothing reached the slot, so the caller gets the status and the loop serves on.
            Err(ReplyNotSent::Refused(s)) => self.send_reply(s.into(), &[], &[], &[]),
            Err(ReplyNotSent::Failed(s)) => Err(s),
        }
    }

    /// Dispatch one inbound `TRANSACT` and reply: from `serve_once_on_slot` or a reply wait's nest.
    fn dispatch_transact(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        peer: PeerIdentity,
    ) -> Result<()> {
        // Past `Live` a frame read here does not run: its target may be cleared ("Session end").
        if self.shared.lifecycle.is_torn_down() {
            return Ok(());
        }
        // A call is open here until its reply is out, nested calls included: module doc "Idle".
        let _open = OpenCall::enter(&self.shared);
        let oneway = (t.flags & FLAG_ONEWAY) != 0;
        if t.address.is_zero() {
            // Zero-address specials (GET_ROOT etc.): no caller identity, no user handler.
            return self.serve_special(t, oneway);
        }
        // One `Arc` (Plan 2-16): each oneway drain entry installs it with a refcount bump.
        let peer = Arc::new(peer);
        if oneway {
            self.dispatch_oneway_ordered(t, in_fds, peer)
        } else {
            self.execute_dispatched(t, in_fds, false, peer)
        }
    }

    /// Order a oneway by its node's `asyncTodo` (AOSP `processTransactInternal`), then dispatch.
    fn dispatch_oneway_ordered(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        peer: Arc<PeerIdentity>,
    ) -> Result<()> {
        let addr = t.address;
        let wire_async = t.async_number;
        // Target receipts this drain resolved (module doc "Deferred `DEC_STRONG`"); not r34's.
        let counts_targets = self.profile.counts_transaction_targets();
        let mut owed: u32 = 0;
        let decision = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .dispatch_async_or_enqueue(addr, wire_async, t, in_fds);
        let mut next = match decision {
            super::state::AsyncDecision::Dispatch(t, fds) => {
                owed = 1;
                Some((t, fds))
            }
            // Owed when it is popped, purged or flushed, by whichever drain does that.
            super::state::AsyncDecision::Enqueued => {
                log::trace!(
                    "RPC oneway parked: addr={:?} async#={} (out of order)",
                    addr,
                    wire_async
                );
                None
            }
            super::state::AsyncDecision::Drop(reason) => {
                log::debug!(
                    "RPC oneway dropped: addr={:?} async#={} reason={:?}",
                    addr,
                    wire_async,
                    reason
                );
                if matches!(reason, super::state::DropReason::StaleAsyncNumber) {
                    owed = 1;
                }
                None
            }
            super::state::AsyncDecision::Terminate(num_pending) => {
                // Watermark hit, backlog flushed: the dispatch error ends the session, as AOSP.
                log::error!(
                    "RPC: {num_pending} pending oneway transactions on {addr:?}; \
                         flushing backlog and ending the session"
                );
                if counts_targets {
                    // Best effort: the session ends right after, and a held entry with it.
                    let flushed = u32::try_from(num_pending).unwrap_or(u32::MAX);
                    self.send_dec_strong(addr, flushed);
                }
                return Err(StatusCode::FailedTransaction);
            }
        };
        let mut result = Ok(());
        while let Some((t, fds)) = next {
            // Every drained oneway is from this session: same caller identity.
            if let Err(e) = self.execute_dispatched(t, fds, true, Arc::clone(&peer)) {
                result = Err(e);
                break;
            }
            let advance = {
                let mut state = self.shared.state.lock().expect("rpc state poisoned");
                state.advance_and_pop_async(addr)
            };
            owed = owed.saturating_add(advance.purged);
            if advance.next.is_some() {
                owed = owed.saturating_add(1);
            }
            next = advance.next;
        }
        // AOSP flushes a oneway target's refs after the drain (android-17.0.0_r1 `:1284`).
        if counts_targets {
            self.send_dec_strong(addr, owed);
        }
        result
    }

    /// Local dispatch and reply for both paths; `dispatch_oneway_ordered` gates oneways above it.
    fn execute_dispatched(
        &self,
        t: WireTransaction,
        in_fds: Vec<OwnedFd>,
        oneway: bool,
        peer: Arc<PeerIdentity>,
    ) -> Result<()> {
        let target = self
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .lookup_local(&t.address);
        let Some(target) = target else {
            if oneway {
                // Seen only if `dec_strong_local` ran between the asyncTodo gate and now.
                log::debug!(
                    "RPC oneway to unknown/released address {:?} dropped",
                    t.address
                );
            } else {
                self.send_reply(StatusCode::DeadObject.into(), &[], &[], &[])?;
            }
            return Ok(());
        };

        // The target receipt, on every exit that replies to a found target, whatever the status.
        let address = t.address;
        let pay_target = || {
            if self.profile.pays_target_before_reply() {
                if let Ok(Some(slot_id)) = self.driving_slot() {
                    self.hold_dec_strong(slot_id, address, 1);
                }
            }
        };
        if !oneway {
            // No token: libbinder answers PING in `BBinder::transact`, INTERFACE in `onTransact`.
            match t.code {
                INTERFACE_TRANSACTION => {
                    let mut reply = Parcel::new();
                    reply.attach_rpc_ops(self.parcel_ops());
                    reply.write(&target.descriptor())?;
                    pay_target();
                    return self.send_reply_parcel(&reply);
                }
                PING_TRANSACTION => {
                    pay_target();
                    return self.send_reply(0, &[], &[], &[]);
                }
                _ => {}
            }
        }

        // AOSP `allowNested = !oneway`; armed first, so `reader`/`reply` proxies drop under it.
        let _nested = AllowNestedGuard::arm(self, !oneway);
        let (mut reader, entered) = self.receive_parcel(t.data, t.object_positions, in_fds);
        if let Err(e) = entered {
            log::error!("RPC: a transaction's objects failed validation ({e:?}); refused");
            // The session ended on an unreadable object: this loop ends on the fault, not its read.
            if self.shared.lifecycle.is_torn_down() {
                return Err(e);
            }
            if oneway {
                return Ok(());
            }
            pay_target();
            return self.send_reply(StatusCode::BadValue.into(), &[], &[], &[]);
        }
        // A oneway reply is never sent: no buffer unless the handler writes into it.
        let mut reply = if oneway {
            Parcel::with_capacity(0)
        } else {
            Parcel::new()
        };
        reply.configure_rpc(
            self.parcel_ops(),
            self.fd_mode(),
            self.records_fd_positions(),
        );

        // Before the guard: `caps()` locks the pool, which no one may hold across the callback.
        let caps = self.caps();
        // Plan 2-16: the handler's calling identity; the guard restores it, so re-entry nests.
        let code = t.code;
        let observed_ctx = || {
            let (calling_uid, calling_pid) = crate::thread_state::peer_uid_pid(&peer);
            crate::observe::TxnContext {
                descriptor: target.descriptor(),
                code,
                method: target
                    .as_transactable()
                    .and_then(|transactable| transactable.transaction_name(code)),
                is_oneway: oneway,
                calling_uid,
                calling_pid,
                transport: caps,
            }
        };
        // Guards outside `observed`: the observer sees the handler's thread state, as on kernel.
        let result = {
            let _calling = crate::thread_state::RpcCallingGuard::install(Arc::clone(&peer), caps);
            // AOSP RPC leaves the work source alone; reset it so a handler's `set` cannot leak.
            let _work_source = crate::thread_state::WorkSourceDispatchGuard::enter();
            crate::observe::observed(observed_ctx, || {
                // AOSP `RpcState` checks no token; the stub does, for call codes (kernel path too).
                let token = if crate::binder::code_takes_token(code) {
                    consume_rpc_interface_token(&mut reader, target.descriptor())
                } else {
                    Ok(())
                };
                // Caught here, so a handler panic is an `Unknown` reply and the session serves on.
                token.and_then(|()| {
                    crate::binder::run_handler(code, &mut reply, |reply| {
                        let token = crate::binder::sealed::Token::new();
                        target.rpc_transact(token, code, &mut reader, reply)
                    })
                })
            })
        };

        // AOSP destroys `data` before the reply, so an argument proxy's `DEC_STRONG` precedes it.
        drop(reader);
        if oneway {
            if let Err(e) = result {
                log::error!("oneway RPC transaction failed (dropped): {e:?}");
            }
            // Never sent: `reply` drops unsent and gives its bumps back (on a panic, already did).
            return Ok(());
        }
        pay_target();
        match result {
            Ok(()) => self.send_reply_parcel(&reply),
            // `reply` then drops unsent and gives its bumps back; a panic's was already dropped.
            Err(e) => self.send_reply(crate::binder::handler_reply_status(e).into(), &[], &[], &[]),
        }
    }

    /// Serve one frame on the pinned slot.
    fn serve_once_on_slot(&self, slot_id: u64) -> ServeStep {
        // Only the death sequence empties the pool of a served slot: module doc "Session end".
        let Ok(conn) = self.find_conn_pinned(slot_id) else {
            return ServeStep::Ended(match self.no_wait_write_end(slot_id) {
                Some(HeldWriteEnd::Failed(status)) => EndReason::Dispatch(status),
                // No transport left to read it on: a frame the peer cut reads as its close.
                Some(HeldWriteEnd::PeerClosed) => EndReason::EndOfStream,
                None => EndReason::SessionEnded,
            });
        };
        let transport = conn.transport();
        // Only the founding slot's first read takes it: no other read can come before it.
        if slot_id == RpcSession::FOUNDING_SLOT_ID
            && self.awaits_preamble.swap(false, Ordering::SeqCst)
        {
            match self.read_session_preamble(transport) {
                Ok(RPC_SESSION_ID_NEW) => {}
                Ok(id) => {
                    log::error!(
                        "RPC r34: the client asked to join session id {id}; joining an existing \
                         session needs an `RpcServer`, so this connection is closed"
                    );
                    return ServeStep::Ended(EndReason::Frame(
                        RpcError::Protocol("r34 preamble names an existing session").into(),
                    ));
                }
                Err(e) => return ServeStep::Ended(recv_end_reason(e)),
            }
        }
        let written = self.write_before_serve_read(&conn).and_then(|()| {
            self.hand_held_to_reaper(slot_id);
            self.write_held_past_limit(&conn)
        });
        if let Err(end) = written {
            return ServeStep::Ended(self.end_after_failed_write(end, transport));
        }
        let (frame, in_fds) = match self.recv_msg(transport) {
            Ok(f) => f,
            Err(e) => return ServeStep::Ended(recv_end_reason(e)),
        };
        // Ended locally with this frame in flight (a kernel may keep its queue past shutdown).
        if self.shared.ended_locally.load(Ordering::SeqCst) {
            return ServeStep::Ended(EndReason::Interrupted);
        }
        let msg = match self.profile.codec().decode_message(&frame) {
            Ok(m) => m,
            Err(e) => return ServeStep::Ended(EndReason::Frame(e.into())),
        };
        let step = match msg {
            WireMessage::Transact(t) => {
                // Plan 2-16 Phase B: the dispatch stamps the calling uid/pid from this.
                let peer = transport.peer_identity();
                match self.dispatch_transact(t, in_fds, peer) {
                    Ok(()) => ServeStep::Continue,
                    Err(e) => ServeStep::Ended(EndReason::Dispatch(e)),
                }
            }
            WireMessage::DecStrong(a, amount) => {
                self.apply_dec_strong(a, amount);
                ServeStep::Continue
            }
            WireMessage::Reply(_) => {
                // AOSP `processCommand` ends the session on an unsolicited command (no log flood).
                log::warn!("RPC server received an unexpected REPLY; ending the session");
                ServeStep::Ended(EndReason::Frame(StatusCode::BadType))
            }
        };
        // Here, with the transport in hand: by the next frame the reaper may have emptied the pool.
        match self.no_wait_write_end(slot_id) {
            Some(end) if matches!(step, ServeStep::Continue) => {
                ServeStep::Ended(self.end_after_failed_write(end, transport))
            }
            _ => step,
        }
    }

    /// Zero-address specials: AOSP `GET_ROOT`, `GET_MAX_THREADS`, `GET_SESSION_ID`; `GET_FD_MODE`.
    fn serve_special(&self, t: WireTransaction, oneway: bool) -> Result<()> {
        if oneway {
            // Special transactions are never oneway.
            return Ok(());
        }
        match SpecialTransaction::from_code(t.code) {
            Some(SpecialTransaction::GetRoot) => {
                let root = self.shared.root.lock().expect("root poisoned").clone();
                let mut reply = Parcel::new();
                reply.attach_rpc_ops(self.parcel_ops());
                // On any failure below `reply` drops unsent and gives the root's bump back.
                reply.write(&root)?;
                // At v2 the root binder's position is in the object table.
                self.send_reply_parcel(&reply)
            }
            Some(SpecialTransaction::GetMaxThreads) => {
                let n = self.shared.max_threads.load(Ordering::SeqCst) as i32;
                let mut reply = Parcel::new();
                reply.write(&n)?;
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])
            }
            Some(SpecialTransaction::GetSessionId) => {
                let mut reply = Parcel::new();
                if self.profile.wire_version().is_some() {
                    // AOSP `writeByteVector(mId)` = the `&[u8]` path; a bare `i32` is BAD_VALUE.
                    reply.write(&self.shared.rpc_session_id.as_bytes()[..])?;
                } else {
                    // android-12 `writeInt32(id)`, an id only an `RpcServer` mints.
                    let id = self.shared.r34_session_id.load(Ordering::SeqCst);
                    if id == RPC_SESSION_ID_NEW {
                        let status = StatusCode::UnknownTransaction.into();
                        return self.send_reply(status, &[], &[], &[]);
                    }
                    reply.write(&id)?;
                }
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])
            }
            // android-13+ fixes the mode in the header (AOSP); a flip races other slots' reads.
            Some(SpecialTransaction::GetFdMode) if self.profile.wire_version().is_some() => {
                self.send_reply(StatusCode::UnknownTransaction.into(), &[], &[], &[])
            }
            Some(SpecialTransaction::GetFdMode) => {
                // `Unix` is never renegotiated: a flip to `None` drops in-flight fds, desyncs R34.
                if self.fd_mode() == FileDescriptorTransportMode::Unix {
                    let mut reply = Parcel::new();
                    reply.write(&1i32)?;
                    return self.send_reply(0, reply.rpc_data_bytes(), &[], &[]);
                }
                let mut req = Parcel::from_vec(t.data);
                req.set_data_position(0);
                // A malformed body defaults to "no FD support", but is logged.
                let want_unix = match req.read::<i32>() {
                    Ok(v) => v == 1,
                    Err(e) => {
                        log::debug!("RPC GET_FD_MODE: malformed body ({e:?}); defaulting to None");
                        false
                    }
                };
                let agreed = if want_unix && self.shared.fd_unix_supported.load(Ordering::SeqCst) {
                    FileDescriptorTransportMode::Unix
                } else {
                    FileDescriptorTransportMode::None
                };
                let mut reply = Parcel::new();
                reply.write(
                    &(if agreed == FileDescriptorTransportMode::Unix {
                        1i32
                    } else {
                        0i32
                    }),
                )?;
                self.send_reply(0, reply.rpc_data_bytes(), &[], &[])?;
                // Switch AFTER the reply is on the wire (None-mode).
                self.shared.set_fd_mode(agreed);
                Ok(())
            }
            None => self.send_reply(StatusCode::UnknownTransaction.into(), &[], &[], &[]),
        }
    }
}

/// A failed write on a slot its reader holds, which ended the session; the read is the caller's.
#[derive(Clone, Copy)]
enum HeldWriteEnd {
    /// `RpcError::EndOfStream`: the read reports the close.
    PeerClosed,
    /// Any other failure, with the status it ends the reader with.
    Failed(StatusCode),
}

/// How a serve loop's failed read ends it.
fn recv_end_reason(e: RpcError) -> EndReason {
    match e {
        RpcError::EndOfStream => EndReason::EndOfStream,
        RpcError::UncleanEndOfStream => EndReason::UncleanEndOfStream,
        RpcError::DeadlineMidFrame => EndReason::DeadlineMidFrame,
        // The kernel's `ETIMEDOUT`: a lost connection, not an idle eviction.
        RpcError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            EndReason::Frame(StatusCode::DeadObject)
        }
        e => EndReason::Frame(e.into()),
    }
}

/// An RPC session over a pool of connections, one by default (client and/or server role).
///
/// # Lifetime
///
/// Proxies obtained over this session (`get_root`, binders read from
/// replies) hold the session **strongly** — AOSP `BpBinder` ↔
/// `sp<RpcSession>`. Dropping this handle does not invalidate them; the
/// connection closes when the last proxy and handle are gone. The
/// converse also holds: while the *peer* still holds one of this
/// endpoint's local objects (a callback it was handed), the session
/// stays alive until the peer releases it (`DEC_STRONG`) or the
/// connection ends — so dropping every proxy is not a guaranteed
/// disconnect. On connection loss every local object the peer held is
/// released (AOSP `RpcState::clear`); for a session without a serve
/// thread that loss is detected on the next failed transaction, or at the
/// peer's close once a [`link_to_death`](crate::IBinder::link_to_death) has
/// started a watcher.
///
/// That leaves one case the runtime cannot notice: a local object handed
/// to the peer may itself hold a proxy back into this session, and if the
/// session neither serves nor transacts again, and no watcher sees a close,
/// nothing runs the release. [`RpcSession::close_session`] is the explicit
/// break for it.
///
/// A client session with incoming (callback) connections
/// ([`RpcClientConfig::incoming_connections`]) owns the threads that
/// serve them, and those threads keep the session alive: dropping every
/// handle and proxy does not stop them. They end when the server closes
/// the session or on [`RpcSession::close_session`] — call it when you are done
/// with such a session.
#[derive(Clone)]
pub struct RpcSession {
    inner: Arc<RpcSessionInner>,
}

impl std::fmt::Debug for RpcSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcSession")
            .field("wire_protocol_version", &self.wire_protocol_version())
            .field("space", &self.inner.shared.space)
            .field("ended", &self.is_ended())
            .finish_non_exhaustive()
    }
}

impl RpcSession {
    /// Wrap a connected transport in a session. `space` is this
    /// endpoint's address subspace — [`AddressSpace::Initiator`] for
    /// the side that connected, [`AddressSpace::Acceptor`] for the
    /// side that accepted (so the two peers never mint colliding
    /// addresses on the shared connection).
    /// Fails when the `Initiator`'s preamble cannot be sent (the
    /// transport's error, projected as `From<RpcError> for StatusCode`
    /// does), when `getrandom` cannot mint the session id — an
    /// early-boot container without a working CSPRNG — and when the
    /// process cannot spawn the session's thread.
    ///
    /// # Direction
    ///
    /// `space` also fixes the direction the founding connection is used
    /// in (AOSP `RpcSession::mConnections.{mOutgoing, mIncoming}`):
    /// `Initiator` sends on it, `Acceptor` serves it. An `Acceptor`
    /// session therefore **cannot open a transaction of its own** —
    /// `get_root`, `RpcProxy::transact` and `ping_binder` called on it
    /// from outside a dispatch fail with
    /// [`StatusCode::WouldBlock`], because writing a request into
    /// a connection the peer only reads inside its own reply wait would
    /// sit unread. Twoway callbacks *from inside a twoway handler* are
    /// unaffected (they re-enter the dispatching connection); a oneway
    /// never re-enters it. To call
    /// out of an acceptor otherwise, the peer must open incoming
    /// connections, which needs the android-13+ profile
    /// (`?profile=android13plus`,
    /// `RpcClientConfig::incoming_connections`); the r34 profile has
    /// no such mechanism.
    ///
    /// # Preamble
    ///
    /// The session speaks the android-12 r34 wire, which opens every
    /// connection with the client's `int32` session id (AOSP
    /// `RpcSession::setupOneSocketConnection`). An `Initiator` writes `-1`
    /// (a new session) here, before the session exists. An
    /// `Acceptor` does not block here: its first
    /// [`serve_blocking`](Self::serve_blocking) read takes the id and accepts
    /// only `-1`. Any other id asks to join a session this endpoint does not
    /// have — joining needs an [`RpcServer`](super::RpcServer) — and ends the
    /// session.
    pub fn new(transport: Box<dyn RpcTransport>, space: AddressSpace) -> Result<RpcSession> {
        Ok(Self::new_rpc(transport, space)?)
    }

    /// [`new`](Self::new) keeping the transport layer's error, for the crate's own callers.
    pub(crate) fn new_rpc(
        transport: Box<dyn RpcTransport>,
        space: AddressSpace,
    ) -> RpcResult<RpcSession> {
        match space {
            AddressSpace::Initiator => {
                transport.send_raw(&R34Codec.encode_session_preamble(RPC_SESSION_ID_NEW))?;
                Self::new_accepted(transport, space)
            }
            AddressSpace::Acceptor => {
                let session = Self::new_accepted(transport, space)?;
                session.inner.awaits_preamble.store(true, Ordering::SeqCst);
                Ok(session)
            }
        }
    }

    /// An r34 session whose preamble is already past: written by `new`, or read by `RpcServer`.
    pub(crate) fn new_accepted(
        transport: Box<dyn RpcTransport>,
        space: AddressSpace,
    ) -> RpcResult<RpcSession> {
        RpcSession::with_profile(transport, space, WireProfile::R34(R34Codec))
    }

    /// A session with a fixed profile: the handshake finalizes the android-13+ codec before this.
    fn with_profile(
        transport: Box<dyn RpcTransport>,
        space: AddressSpace,
        profile: WireProfile,
    ) -> RpcResult<RpcSession> {
        Self::with_shared(transport, profile, Self::fresh_shared(space)?)
    }

    /// A new session's shared state; `lifecycle` starts at `Live(1)` for the founding connection.
    fn fresh_shared(space: AddressSpace) -> RpcResult<Arc<SharedSession>> {
        Ok(Arc::new(SharedSession {
            state: Mutex::new(RpcState::new(space)),
            root: Mutex::new(None),
            max_threads: AtomicU32::new(1),
            negotiated: AtomicU32::new(0),
            timeout: Mutex::new(None),
            serve_read_deadline: AtomicU64::new(0),
            fd_mode: AtomicU8::new(FD_MODE_NONE),
            fd_unix_supported: AtomicBool::new(false),
            discard_peer_fds: AtomicBool::new(false),
            rpc_session_id: gen_rpc_session_id()?,
            r34_session_id: AtomicI32::new(RPC_SESSION_ID_NEW),
            server_session_id: Mutex::new(None),
            redial: Mutex::new(None),
            lifecycle: SessionLifecycle::new(),
            ended_locally: AtomicBool::new(false),
            serve_declared: AtomicUsize::new(0),
            space,
            open: AtomicUsize::new(0),
            io_gen: AtomicU64::new(0),
            liveness: Mutex::new(()),
            #[cfg(test)]
            park_hook: Mutex::new(None),
            #[cfg(test)]
            cci_failed_hook: Mutex::new(None),
            #[cfg(test)]
            serve_wait_hook: Mutex::new(None),
            #[cfg(test)]
            fail_spawn: AtomicBool::new(false),
        }))
    }

    /// The inner with `transport` as a fresh `shared`'s founding slot, and its reaper, or `Err`.
    fn with_shared(
        transport: Box<dyn RpcTransport>,
        profile: WireProfile,
        shared: Arc<SharedSession>,
    ) -> RpcResult<RpcSession> {
        // Founding role: AOSP `setupClient` (sends) vs `RpcServer::establishConnection` (serves).
        let founding_role = match shared.space() {
            AddressSpace::Initiator => SlotRole::Outgoing,
            AddressSpace::Acceptor => SlotRole::Incoming,
        };
        let traits = ConnTraits::of(&*transport);
        let peer = transport.peer_identity();
        let founding: Arc<dyn RpcTransport> = Arc::from(transport);
        let armed = Arc::clone(&founding);
        let (reaper_tx, reaper_rx) = mpsc::channel();
        let inner = Arc::new_cyclic(|weak: &Weak<RpcSessionInner>| RpcSessionInner {
            conn_state: Mutex::new(ConnState::new(founding, founding_role, traits)),
            slot_cv: Condvar::new(),
            peer,
            profile,
            self_weak: weak.clone(),
            parcel_ops: Arc::new(SessionParcelOps(weak.clone())),
            shared,
            reaper_tx,
            awaiting_room: AtomicBool::new(false),
            carried_queued: AtomicBool::new(false),
            reaper_wake: Mutex::new(None),
            incoming_threads: Mutex::new(Vec::new()),
            incoming_live: AtomicUsize::new(0),
            incoming_joined: AtomicUsize::new(0),
            awaits_preamble: AtomicBool::new(false),
        });
        inner.arm_liveness(&*armed);
        // Detached reaper for deferred DEC_STRONG; exits when the inner drops its sender.
        let weak_for_reaper = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("rsbinder-rpc-reaper".into())
            .spawn(move || reaper_loop(weak_for_reaper, reaper_rx))?;
        Ok(RpcSession { inner })
    }

    /// Test-only leak probe: a `Weak` on the inner, to assert the session graph is reclaimed.
    #[cfg(test)]
    pub(crate) fn inner_weak(&self) -> Weak<RpcSessionInner> {
        Arc::downgrade(&self.inner)
    }

    /// The founding slot's id, which every non-attach `serve_blocking` caller drives.
    pub(crate) const FOUNDING_SLOT_ID: u64 = 1;

    /// Attach a serve-driven slot to the founding inner; see module doc "One inner per session".
    pub(crate) fn add_incoming_slot_capped(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        self.inner.add_incoming_slot_capped(transport, cap)
    }

    /// Admit a server-side callback slot, then send its `"cci"`; see module doc "Callback slots".
    pub(crate) fn add_callback_slot_and_init(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
        codec: &Android13PlusCodec,
    ) -> Result<u64> {
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        let claim = self.inner.add_claimed_slot_capped(transport, cap)?;
        let sent = {
            let mut io = RawTransportIo(&*claim.transport);
            super::wire_android13::server_write_connection_init(&mut io, codec)
        };
        match sent {
            // The claim's drop frees the slot for callbacks; the join is activity ("Idle").
            Ok(()) => {
                self.inner.shared.io_gen.fetch_add(1, Ordering::Relaxed);
                Ok(claim.slot_id)
            }
            Err(e) => {
                #[cfg(test)]
                self.inner.shared.run_cci_failed_hook();
                // A corpse slot eats the budget and is picked first (module doc "Callback slots").
                claim.retire();
                Err(StatusCode::from(e))
            }
        }
    }

    /// Test form of `add_callback_slot_and_init` without the wire init (no peer to read it).
    #[cfg(test)]
    pub(crate) fn add_callback_slot(
        &self,
        transport: Box<dyn RpcTransport>,
        cap: usize,
    ) -> Result<u64> {
        // Fast pre-check; `add_slot_inner_capped` re-checks under the lock.
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        self.inner.add_slot_inner_capped(transport, cap)
    }

    /// The inner (slot pool included) `RpcServer.sessions` keeps a `Weak` of for later attaches.
    pub(crate) fn inner_arc(&self) -> Arc<RpcSessionInner> {
        Arc::clone(&self.inner)
    }

    /// Wrap a founding inner from `RpcServer.sessions` so an attach worker can `serve_blocking_on`.
    pub(crate) fn wrap_inner(inner: Arc<RpcSessionInner>) -> Self {
        RpcSession { inner }
    }

    /// Accept handshake only, so the id decides new vs attach; `RpcError` keeps the log's reason.
    pub(crate) fn android13plus_accept_handshake(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> RpcResult<Android13PlusAccept> {
        let (codec, client_fd_mode, client_id, incoming) = {
            let mut io = RawTransportIo(transport.as_ref());
            server_accept_deferred_init(&mut io, server_max_version)?
        };
        Ok((transport, codec, client_fd_mode, client_id, incoming))
    }

    /// Server: a brand-new session from a completed accept; see module doc "One inner per session".
    pub(crate) fn from_android13plus(
        transport: Box<dyn RpcTransport>,
        codec: Android13PlusCodec,
        client_fd_mode: u8,
        server_fd_unix: bool,
    ) -> RpcResult<RpcSession> {
        // AOSP `RpcServer.cpp` "Rejecting connection": a mode the server does not support.
        if !(client_fd_mode == FD_MODE_NONE || (client_fd_mode == FD_MODE_UNIX && server_fd_unix)) {
            log::error!(
                "android-13+ RPC: rejecting connection: FileDescriptorTransportMode \
                 {client_fd_mode} is not supported (Unix needs \
                 `RpcServer::set_supported_fd_modes`)"
            );
            return Err(RpcError::Protocol(
                "client requested an unsupported FileDescriptorTransportMode",
            ));
        }
        let negotiated = codec.version();
        let shared = Self::fresh_shared(AddressSpace::Acceptor)?;
        let session = Self::with_shared(transport, WireProfile::Android13Plus(codec), shared)?;
        if server_fd_unix && client_fd_mode == FD_MODE_UNIX && negotiated >= PROTOCOL_V1 {
            session
                .inner
                .shared
                .set_fd_mode(FileDescriptorTransportMode::Unix);
        }
        Ok(session)
    }

    /// Client role, **opt-in android-13+ versioned wire**.
    /// Runs the AOSP connection handshake on `transport`
    /// (`RpcConnectionHeader → RpcNewSessionResponse → "cci"`,
    /// negotiating `min(max_version, server_max)`), then returns a
    /// session that speaks the negotiated version with AOSP-faithful
    /// framing, over the same per-session `RpcState` and
    /// `client_transact`/dispatch as the r34 profile. `max_version` is the
    /// highest `RPC_WIRE_PROTOCOL_VERSION` to offer (0 = android-13,
    /// 1 = android-14/15, 2 = android-16).
    ///
    /// The default [`RpcSession::new`] /
    /// [`RpcSession::setup_unix_client`] keep the r34 wire — this never
    /// changes the R34 path (AOSP android-12 layout).
    pub fn connect_android13plus(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
    ) -> Result<RpcSession> {
        Self::connect_android13plus_fd(transport, max_version, FileDescriptorTransportMode::None)
    }

    /// Client role, opt-in android-13+ wire **with FD-over-RPC**.
    /// Requests `fd_mode` in the
    /// `RpcConnectionHeader.fileDescriptorTransportMode` byte (byte-exact
    /// to AOSP `setFileDescriptorTransportMode`/`setupClient`, **not**
    /// the R34 `GET_FD_MODE` special-transact) and, on a successful
    /// handshake at **v1+** (android-14/15/16; v0 category-forbids fd,
    /// AOSP-faithful), switches the session to `Unix`.
    /// `FileDescriptorTransportMode::None` is exactly
    /// [`RpcSession::connect_android13plus`] (byte-identical no-FD path).
    ///
    /// A server that does not support the requested mode closes the
    /// connection after the handshake (AOSP `RpcServer.cpp` "Rejecting
    /// connection: FileDescriptorTransportMode is not supported"; an
    /// rsbinder server supports `Unix` only after
    /// [`RpcServer::set_supported_fd_modes`](super::RpcServer::set_supported_fd_modes)),
    /// so the first call on the returned session fails.
    pub fn connect_android13plus_fd(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
    ) -> Result<RpcSession> {
        Self::connect_android13plus_fd_hs(transport, max_version, fd_mode, None)
    }

    /// `connect_android13plus_fd` with a whole-handshake deadline (`HandshakeDeadline`).
    pub(crate) fn connect_android13plus_fd_hs(
        transport: Box<dyn RpcTransport>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
        handshake_timeout: Option<Duration>,
    ) -> Result<RpcSession> {
        let want_unix = fd_mode == FileDescriptorTransportMode::Unix;
        let hdr_fd_mode = if want_unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let codec = {
            // Scoped: the deadline is cleared before `transport` moves into the session.
            let mut hs = HandshakeDeadline::arm(transport.as_ref(), handshake_timeout)
                .map_err(StatusCode::from)?;
            let mut io = RawTransportIo(transport.as_ref());
            // An empty id requests a new session.
            let codec = client_connect_with_id(&mut io, max_version, false, hdr_fd_mode, &[])
                .map_err(|e| client_handshake_err(hs.classify(e)))?;
            hs.finish().map_err(client_handshake_err)?;
            codec
        };
        let negotiated = codec.version();
        let session = RpcSession::with_profile(
            transport,
            AddressSpace::Initiator,
            WireProfile::Android13Plus(codec),
        )
        .map_err(StatusCode::from)?;
        // v0 forbids fd-over-RPC: stay `None` below v1 (a fd write is AOSP's `BAD_TYPE`).
        if want_unix && negotiated >= PROTOCOL_V1 {
            session
                .inner
                .shared
                .set_fd_mode(FileDescriptorTransportMode::Unix);
        }
        Ok(session)
    }

    /// Server role, **opt-in android-13+ versioned wire**. Runs
    /// the AOSP accept handshake on an already-accepted `transport`
    /// (negotiates `min(server_max_version, client_max)`), then returns
    /// an [`AddressSpace::Acceptor`] session speaking the negotiated
    /// version. Supports no fd
    /// mode: a client that requests one is refused, as in
    /// [`RpcSession::accept_android13plus_fd`] with `server_fd_unix == false`.
    ///
    /// A test entry (`test-util`): a server is an [`RpcServer`](super::RpcServer),
    /// which runs this handshake on its own workers and demultiplexes
    /// session ids, which this does not.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn accept_android13plus(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
    ) -> Result<RpcSession> {
        Self::accept_android13plus_fd(transport, server_max_version, false)
    }

    /// Server role, opt-in android-13+ wire **with FD-over-RPC**,
    /// accepting one connection as a
    /// **brand-new session** (no id-demux). Reads the client's
    /// requested FD mode from the `RpcConnectionHeader` and, when the
    /// client asked for `Unix`, this server opted in (`server_fd_unix`,
    /// [`super::RpcServer::set_supported_fd_modes`]), **and** the
    /// negotiated wire is v1+ (v0 forbids fd), switches the session to
    /// `Unix`. A client that requests a mode this server does not support
    /// (`Unix` without `server_fd_unix`, or `Trusty`) is refused with an
    /// error after the handshake response has gone out, so the client
    /// sees the close on its first call — AOSP `RpcServer.cpp` "Rejecting
    /// connection: FileDescriptorTransportMode is not supported".
    /// `server_fd_unix == false` is exactly
    /// [`RpcSession::accept_android13plus`].
    ///
    /// This is a thin wrapper over `android13plus_accept_handshake` then
    /// `from_android13plus`, which always builds a fresh session: a 32-byte
    /// client id is ignored, and an id of any other non-empty size is
    /// refused before it is read, as AOSP `RpcServer::establishConnection`
    /// does. The id-demux (new vs. attach) lives in
    /// [`super::RpcServer::serve_connection`].
    ///
    /// A test entry (`test-util`), as [`accept_android13plus`](Self::accept_android13plus) is.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn accept_android13plus_fd(
        transport: Box<dyn RpcTransport>,
        server_max_version: u32,
        server_fd_unix: bool,
    ) -> Result<RpcSession> {
        let (transport, codec, client_fd_mode, _client_id, incoming) =
            Self::android13plus_accept_handshake(transport, server_max_version)
                .map_err(StatusCode::from)?;
        // No callback-slot path here; incoming attaches go through `RpcServer::serve_connection`.
        if incoming {
            return Err(StatusCode::BadType);
        }
        Self::from_android13plus(transport, codec, client_fd_mode, server_fd_unix)
            .map_err(StatusCode::from)
    }

    /// The negotiated android-13+ wire protocol version
    /// (`0` = android-13, `1` = android-14/15, `2` = android-16), or `None` for the
    /// default android-12 r34 profile. Lets a caller assert the
    /// `min(client_max, server_max)` handshake outcome.
    pub fn wire_protocol_version(&self) -> Option<u32> {
        match &self.inner.profile {
            WireProfile::Android13Plus(c) => Some(c.version()),
            WireProfile::R34(_) => None,
        }
    }

    /// This session's 32-byte id (AOSP `mId`); a client's is local, not the server's (module doc).
    pub(crate) fn session_id(&self) -> [u8; 32] {
        *self.inner.shared.rpc_session_id.as_bytes()
    }

    /// Client: fetch the server-minted 32-byte
    /// session id via the `GET_SESSION_ID` special transact. AOSP
    /// `RpcSession::setupClient` reads this on the first connection and
    /// echoes it on the remaining ones
    /// ([`add_outgoing_connection`](Self::add_outgoing_connection)). The
    /// server already replies it (real-peer-validated:
    /// `writeByteVector(mId)` == the AIDL `byte[]` path); this is the
    /// missing *client* half.
    ///
    /// A client session keeps the first id it reads: an attach on it
    /// must echo exactly that id (AOSP attaches with its own `mId`), and
    /// one that has not read it yet makes this round trip itself.
    ///
    /// On the r34 wire the id is android-12's `int32` (`writeInt32(id)`),
    /// returned as its 4 little-endian bytes. Only a session an
    /// [`RpcServer`](super::RpcServer) accepted has one; any other r34 peer
    /// answers [`StatusCode::UnknownTransaction`].
    pub fn get_session_id(&self) -> Result<Vec<u8>> {
        let data = Parcel::new();
        let mut reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        // Keep the read error: BadValue/BadType on a malformed vector differs from a null reply.
        let id = if self.inner.profile.wire_version().is_some() {
            reply.read::<Vec<u8>>()?
        } else {
            reply.read::<i32>()?.to_le_bytes().to_vec()
        };
        if self.inner.shared.space() == AddressSpace::Initiator {
            let mut kept = self.server_id_lock();
            if kept.is_none() {
                *kept = Some(id.clone());
            }
        }
        Ok(id)
    }

    fn server_id_lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.inner
            .shared
            .server_session_id
            .lock()
            .expect("server_session_id poisoned")
    }

    /// Whether `session_id` is the id an attach must echo: the server-minted one for a client.
    fn is_own_attach_id(&self, session_id: &[u8]) -> Result<bool> {
        if self.inner.shared.space() != AddressSpace::Initiator {
            return Ok(session_id == self.session_id().as_slice());
        }
        // One statement, so the guard is gone before `get_session_id` locks again.
        let kept = self.server_id_lock().as_deref().map(|id| id == session_id);
        match kept {
            Some(same) => Ok(same),
            None => Ok(session_id == self.get_session_id()?.as_slice()),
        }
    }

    /// Refuse an attach id other than this session's own: it would join another server session.
    fn refuse_foreign_attach_id(&self, session_id: &[u8]) -> Result<()> {
        if self.is_own_attach_id(session_id)? {
            return Ok(());
        }
        if session_id == self.session_id().as_slice() {
            log::error!(
                "android-13+ RPC: this attach echoes the client-local `RpcSession::session_id()`; \
                 echo the server-minted id from `RpcSession::get_session_id()`"
            );
        } else {
            log::error!(
                "android-13+ RPC: this attach names a server session other than this \
                 RpcSession's own; two client states on one server session would keep separate \
                 oneway numbering, binder addresses and lifetimes. Echo this session's \
                 `RpcSession::get_session_id()`"
            );
        }
        Err(StatusCode::BadValue)
    }

    /// Server role: advertise that this endpoint will accept the
    /// `Unix` FD-over-RPC mode on `GET_FD_MODE`. Default
    /// is *not* advertised, so the categorical FD reject is the default
    /// everywhere. On a non-UDS transport `Unix` may still be agreed, but
    /// every fd send then fails (the transport fd methods reject by type).
    ///
    /// No effect on an android-13+ session: its mode is fixed by the
    /// connection header at the handshake
    /// ([`RpcServer::set_supported_fd_modes`](super::server::RpcServer::set_supported_fd_modes)).
    pub fn set_supported_fd_modes(&self, modes: &[FileDescriptorTransportMode]) {
        let unix = modes.contains(&FileDescriptorTransportMode::Unix);
        self.inner
            .shared
            .fd_unix_supported
            .store(unix, Ordering::SeqCst);
    }

    /// `RpcServer::__limit_unpaid_addresses` for this session.
    pub(crate) fn limit_unpaid_addresses(&self, limit: usize) {
        self.inner
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .limit_unpaid_addresses(limit);
    }

    /// `RpcServer::set_receive_fds(false)` for this session, before its first read.
    pub(crate) fn discard_peer_fds(&self) {
        self.inner
            .shared
            .discard_peer_fds
            .store(true, Ordering::Relaxed);
    }

    /// Client role: negotiate the FD-over-RPC mode.
    /// On r34, sends exactly one `GET_FD_MODE` packet; the agreed mode is
    /// `Unix` iff *both* peers opted in, else `None` (never an error).
    /// Must be called before any FD-bearing call, like
    /// [`RpcSession::negotiate`].
    ///
    /// r34 only. An android-13+ session fixes the mode in its connection
    /// header ([`RpcClientConfig::fd_mode`]), as AOSP does
    /// (`RpcSession::setFileDescriptorTransportMode` aborts once setup has
    /// started), and other connections of the session may already be
    /// reading under that mode. There this sends nothing: it returns the
    /// current mode, or [`StatusCode::InvalidOperation`] when `want` is
    /// `Unix` and the header did not agree `Unix` — request it with
    /// [`RpcClientConfig::fd_mode`] before connecting, at wire v1+ (a v0
    /// session carries no fd mode, so offer `max_version >= 1` to a v1+
    /// server). An android-13+ server answers `GET_FD_MODE` with
    /// `UNKNOWN_TRANSACTION`, as AOSP libbinder does.
    pub fn negotiate_fd_transport(
        &self,
        want: FileDescriptorTransportMode,
    ) -> Result<FileDescriptorTransportMode> {
        if let Some(version) = self.inner.profile.wire_version() {
            let current = self.inner.fd_mode();
            if want == FileDescriptorTransportMode::Unix && current != want {
                if version < PROTOCOL_V1 {
                    log::error!(
                        "RpcSession::negotiate_fd_transport: wire v0 (android-13) carries no fd \
                         mode; offer `max_version >= 1` to a v1+ server and request Unix with \
                         `RpcClientConfig::fd_mode`"
                    );
                } else {
                    log::error!(
                        "RpcSession::negotiate_fd_transport: an android-13+ session fixes the \
                         fd mode in its connection header ({current:?}); request Unix with \
                         `RpcClientConfig::fd_mode` before connecting"
                    );
                }
                return Err(StatusCode::InvalidOperation);
            }
            return Ok(current);
        }
        let want_unix = want == FileDescriptorTransportMode::Unix;
        let mut req = Parcel::new();
        req.write(&(if want_unix { 1i32 } else { 0i32 }))?;
        let mut reply = match self.inner.client_transact(
            RpcAddress::zero(),
            SpecialTransaction::GetFdMode.code(),
            &req,
            0,
        ) {
            // AOSP r34 libbinder has no `GET_FD_MODE` and carries no fds.
            Err(StatusCode::UnknownTransaction) => return Ok(FileDescriptorTransportMode::None),
            r => r?.ok_or(StatusCode::UnexpectedNull)?,
        };
        let agreed = if reply.read::<i32>()? == 1 {
            FileDescriptorTransportMode::Unix
        } else {
            FileDescriptorTransportMode::None
        };
        // Switch AFTER the reply has been fully read in None-mode.
        self.inner.shared.set_fd_mode(agreed);
        Ok(agreed)
    }

    /// The negotiated FD-over-RPC mode (default `None`).
    pub fn fd_transport_mode(&self) -> FileDescriptorTransportMode {
        self.inner.fd_mode()
    }

    /// What this session can do, as a
    /// [`TransportCaps`](crate::TransportCaps) set.
    ///
    /// **A snapshot.** Connections come and go, so the answer can change:
    /// [`CALLBACKS`](crate::TransportCaps::CALLBACKS) is lost when the
    /// last callback connection dies, and
    /// [`FD_PASSING`](crate::TransportCaps::FD_PASSING) appears only once
    /// [`negotiate_fd_transport`](Self::negotiate_fd_transport) (or the
    /// android-13+ handshake) has agreed the `Unix` mode. Read it when
    /// you are about to act, not once at setup.
    ///
    /// Where each bit comes from:
    ///
    /// - `FD_PASSING` — the **negotiated** mode is `Unix` *and* the
    ///   transport underneath can carry fds (`SCM_RIGHTS`, i.e. a
    ///   Unix-domain socket). Both are required: a vsock or TLS session
    ///   that negotiated `Unix` reports the bit as absent, because the
    ///   send would fail in the transport, and so does a session that
    ///   never negotiated.
    ///   [`Endpoint::static_caps`](crate::Endpoint::static_caps) is the
    ///   one that answers "could it".
    /// - `TRUSTED_UID` and `SAME_HOST` — the peer is
    ///   [`PeerIdentity::Local`], the
    ///   same test [`get_calling_uid`](crate::get_calling_uid) applies.
    /// - `CALLBACKS` — this session holds at least one callback
    ///   connection, so calls cross in both directions. A default
    ///   one-connection session reports it on neither end.
    /// - `KERNEL_KNOBS` — never; this is a socket.
    ///
    /// The transport-derived bits are the same for every connection of
    /// the session, so [`calling_caps`](crate::calling_caps) in a handler
    /// gives this same answer. A session refuses a further connection
    /// whose transport differs from its founding one in fd capability or
    /// peer locality.
    pub fn caps(&self) -> crate::TransportCaps {
        self.inner.caps()
    }

    /// Who is at the other end of this session's founding connection, as its transport
    /// established it. It is fixed when the connection is made and stays after the session ends.
    /// A further connection of the session is admitted only with the same principal (the uid
    /// over a Unix socket), so it speaks for all of them as far as the principal goes.
    ///
    /// On a client over a Unix socket this is [`PeerIdentity::Local`] from `SO_PEERCRED`: the
    /// credentials of the process that called `listen(2)` on the socket, which is not always the
    /// one that made the socket. Across user namespaces a uid with no mapping reads 65534, and a
    /// pid not visible in this pid namespace reads -1 ([`PeerIdentity::pid`] returns `None`).
    pub fn peer_identity(&self) -> PeerIdentity {
        self.inner.peer.clone()
    }

    /// Publish the server's root object (returned by `get_root`).
    ///
    /// Refuses a **remote** binder with [`StatusCode::InvalidOperation`] — see
    /// [`RpcServer::add_service`](crate::rpc::RpcServer::add_service).
    pub fn set_root(&self, binder: SIBinder) -> Result<()> {
        super::refuse_remote(&binder, "RpcSession::set_root")?;
        *self.inner.shared.root.lock().expect("root poisoned") = Some(binder);
        Ok(())
    }

    /// Client: fetch the peer's root object as an [`RpcProxy`]-backed
    /// `SIBinder`.
    pub fn get_root(&self) -> Result<SIBinder> {
        let data = Parcel::new();
        let reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetRoot.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        let mut reply = reply;
        // Keep the read error (v2 `BadValue`, stability short read), not `UnexpectedNull`.
        reply.read::<SIBinder>()
    }

    /// Server: process inbound messages until this connection's read
    /// reaches end of stream or the loop fails.
    ///
    /// When the loop ends — for either reason — the session ends with it:
    /// every remote object reachable over it is dead, so registered death
    /// recipients are fired here (AOSP `RpcState::sendObituaries` when a
    /// session's incoming threads end). Any loop's end ends the whole
    /// session, whichever of its connections the loop served, as a fault on
    /// any one connection does (AOSP `RpcState::handleRpcError`); a loop
    /// that finds the session already ended stops with
    /// [`EndReason::SessionEnded`], or with what a failed release on its
    /// connection left it.
    ///
    /// The returned [`SessionEnd`] says how the loop ended, on three
    /// axes: whether the stream was still intact
    /// ([`stream`](SessionEnd::stream)), whether this end decided to end
    /// it ([`by`](SessionEnd::by)), and what happened
    /// ([`reason`](SessionEnd::reason)). A caller that wants only
    /// "did it end well" calls [`into_result`](SessionEnd::into_result):
    /// `Ok(())` for an intact stream — a peer that closed, or this end's
    /// own [`close_session`](RpcSession::close_session), read the same — and
    /// `Err(`[`StatusCode::DeadObject`]`)` for a lost one, whatever the
    /// cause; the cause is in the value, not in the code. A write of the loop
    /// that fails is a lost stream ([`EndReason::Dispatch`]) unless it found
    /// the peer closed.
    pub fn serve_blocking(&self) -> SessionEnd {
        self.serve_blocking_on(Self::FOUNDING_SLOT_ID)
    }

    /// [`serve_blocking`](Self::serve_blocking) on a new thread, with the
    /// serve loop recorded on the session **before** the thread starts.
    ///
    /// This makes a session without incoming connections read its founding
    /// connection, so that it notices a connection loss by reading it.
    /// [`link_to_death`](crate::IBinder::link_to_death) needs this only where
    /// the session cannot be watched without reading: off Linux and Android,
    /// and on a transport with no
    /// [`peer_close_fd`](super::transport::RpcTransport::peer_close_fd).
    /// There a `link_to_death` made after this returns never races the
    /// thread's start. A thread spawned by hand that calls `serve_blocking`
    /// records the loop only once it runs, so a `link_to_death` issued right
    /// after the spawn can still be refused.
    ///
    /// The loop holds the founding connection while it waits, so a
    /// single-connection session makes no further calls of its own once it
    /// is served — only nested ones from inside a handler; a call from any
    /// other thread fails at once with [`StatusCode::WouldBlock`]. Where the
    /// session cannot be watched, a client that keeps calling and also wants
    /// death notification opens incoming connections instead
    /// ([`RpcClientConfig::incoming_connections`]).
    ///
    /// The thread ends when the session does; join the handle for its
    /// [`SessionEnd`]. Fails only if the thread cannot be created.
    ///
    /// A session that had already ended when the thread starts, or when its
    /// loop goes to take its connection between frames, ends the thread
    /// with [`EndReason::SessionEnded`], or with what a failed release on
    /// that connection left it. One that ends while the loop is
    /// reading stops it with what the connection's shutdown gives that
    /// read ([`EndReason::EndOfStream`] on a Unix socket).
    pub fn spawn_serve(&self) -> Result<std::thread::JoinHandle<SessionEnd>> {
        let declared = &self.inner.shared.serve_declared;
        // Nothing to serve: module doc "Session end".
        let serves = self.inner.slot_role(Self::FOUNDING_SLOT_ID).is_some();
        if serves {
            declared.fetch_add(1, Ordering::SeqCst);
        }
        let session = self.clone();
        let builder = std::thread::Builder::new().name("rsbinder-rpc-serve".into());
        self.inner
            .shared
            .spawn_thread(builder, move || {
                let none = PhaseDeadline::none();
                session.serve_blocking_on_inner(Self::FOUNDING_SLOT_ID, false, false, none, serves)
            })
            .map_err(|e| {
                log::error!("RpcSession::spawn_serve: cannot start the serve thread: {e}");
                if serves {
                    declared.fetch_sub(1, Ordering::SeqCst);
                    self.inner.after_a_failed_serve_spawn();
                }
                StatusCode::from(e)
            })
    }

    /// `serve_blocking` on slot `slot_id`; a slot this session lacks ends at once, `SessionEnded`.
    pub(crate) fn serve_blocking_on(&self, slot_id: u64) -> SessionEnd {
        self.serve_blocking_on_inner(slot_id, false, false, PhaseDeadline::none(), false)
    }

    /// `serve_blocking` with an armed admission deadline lifted after the first frame (r34 server).
    #[cfg(test)]
    pub(crate) fn serve_blocking_clearing_deadline_after_first(&self) -> SessionEnd {
        let none = PhaseDeadline::none();
        self.serve_blocking_on_inner(Self::FOUNDING_SLOT_ID, true, true, none, false)
    }

    /// Server entry; `armed`: a handshake deadline is set; the first frame ends `admission`.
    pub(crate) fn serve_blocking_clearing_admission_deadline(
        &self,
        armed: bool,
        admission: PhaseDeadline,
    ) -> SessionEnd {
        self.serve_blocking_on_inner(Self::FOUNDING_SLOT_ID, true, armed, admission, false)
    }

    /// `spawn_declared`: `spawn_serve` already counted this loop in `serve_declared`.
    fn serve_blocking_on_inner(
        &self,
        slot_id: u64,
        clear_deadline_after_first: bool,
        admission_deadline_armed: bool,
        mut admission: PhaseDeadline,
        spawn_declared: bool,
    ) -> SessionEnd {
        let declared = &self.inner.shared.serve_declared;
        // Nothing to serve or settle: module doc "Session end".
        if self.inner.slot_role(slot_id).is_none() {
            if spawn_declared && !self.inner.shared.lifecycle.is_torn_down() {
                declared.fetch_sub(1, Ordering::SeqCst);
            }
            let ended_locally = self.inner.shared.ended_locally.load(Ordering::SeqCst);
            return SessionEnd::new(EndReason::SessionEnded, ended_locally, false);
        }
        declared.fetch_add(1, Ordering::SeqCst);
        self.inner.mark_slot_serving(slot_id, true);
        // Only an armed deadline of ours makes a `TimedOut` an idle eviction (`SessionEnd::new`).
        let baseline_deadline = self.inner.slot_baseline_read_deadline(slot_id).is_some();
        let (reason, deadline_armed) = {
            let mut first = clear_deadline_after_first;
            let mut deadline_armed = baseline_deadline || admission_deadline_armed;
            // The session's activity count as this wait began: module doc "Idle".
            let mut seen = self.inner.activity();
            #[cfg(test)]
            self.inner.shared.run_serve_wait_hook();
            loop {
                // An unwind would skip the session end below: module doc "Session end".
                let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.inner.serve_once_on_slot(slot_id)
                }));
                let step = match step {
                    Ok(step) => step,
                    Err(payload) => {
                        let msg = crate::error::panic_message(&payload);
                        log::error!("RPC: serving slot {slot_id} panicked ({msg}); session ends");
                        break (EndReason::Dispatch(StatusCode::Unknown), deadline_armed);
                    }
                };
                match step {
                    ServeStep::Continue => {
                        // Lift the admission deadline: later idle waits are unbounded.
                        if first {
                            self.inner.clear_slot_read_timeout(slot_id);
                            first = false;
                            // The deadline cut the connection as the first frame ended.
                            if !admission.disarm() {
                                break (EndReason::Frame(StatusCode::TimedOut), deadline_armed);
                            }
                            deadline_armed = false;
                        }
                        seen = self.inner.activity();
                    }
                    // The whole-phase deadline cut the first frame: the admission deadline's end.
                    ServeStep::Ended(_) if first && admission.fired() => {
                        break (EndReason::Frame(StatusCode::TimedOut), deadline_armed);
                    }
                    // The pool emptied under the expiry: another connection ended the session.
                    ServeStep::Ended(EndReason::Frame(StatusCode::TimedOut))
                        if baseline_deadline
                            && !first
                            && self.inner.slot_role(slot_id).is_none() =>
                    {
                        break (EndReason::SessionEnded, deadline_armed);
                    }
                    // This wait was quiet; the session is idle only if no connection was busy.
                    ServeStep::Ended(EndReason::Frame(StatusCode::TimedOut))
                        if baseline_deadline && !first && self.inner.active_since(&mut seen) => {}
                    ServeStep::Ended(reason) => break (reason, deadline_armed),
                }
            }
        };
        self.inner.mark_slot_serving(slot_id, false);
        // Read before this worker's own teardown below, which is never a local decision.
        let end = SessionEnd::new(
            reason,
            self.inner.shared.ended_locally.load(Ordering::SeqCst),
            deadline_armed,
        );
        // Every loop's end ends the session, as this end's decision or not: "Session end".
        match (reason, end.by) {
            (EndReason::SessionEnded, _) => {}
            (_, EndedBy::Local) => self.inner.close(),
            _ => self.inner.fail_session(),
        }
        end
    }

    /// Server-only `GET_MAX_THREADS` value, set per connection by `RpcServer::configure_session`.
    pub(crate) fn set_max_threads(&self, n: u32) {
        self.inner
            .shared
            .max_threads
            .store(n.max(1), Ordering::SeqCst);
    }

    /// Declare this session dead now: fire every cached proxy's
    /// `binder_died` and release every local object the peer held (AOSP
    /// `RpcState::clear`). Idempotent; subsequent transactions on proxies
    /// of this session fail with [`StatusCode::DeadObject`]. The
    /// connection count does not matter: a session several workers drive
    /// (a server session with attached connections) is ended the same
    /// way — every slot's transport is shut down and its workers exit.
    ///
    /// Normally death is detected on its own — a serve loop ending, the
    /// watcher a [`link_to_death`](crate::IBinder::link_to_death) starts, or a
    /// transaction failing on a lost connection. This is the explicit
    /// form, and with a peer that has not closed it is the **only** way to
    /// break the `session → local object → stored proxy → session` reference
    /// cycle for a session that has no serve loop and will never transact
    /// again: a service this endpoint handed to the peer may hold a proxy
    /// back into the same session, and proxies keep the session alive (see
    /// the type-level `# Lifetime` note). Call it when abandoning such a
    /// session.
    ///
    /// The threads serving this client's incoming (callback) connections
    /// (`RpcClientConfig::incoming_connections`) are stopped and
    /// joined here — every slot's transport is shut down, which ends
    /// their serve loops — except a thread that calls `close_session` from
    /// inside its own callback handler, which is left to finish on its
    /// own (joining it would deadlock). Unlike AOSP
    /// `RpcSession::shutdownAndWait`, a user-driven `serve_blocking` on
    /// the founding slot is not joined; it exits on its own once the
    /// transport is shut down.
    pub fn close_session(&self) {
        self.inner.close();
        let me = std::thread::current().id();
        for (slot_id, handle) in self.inner.take_incoming_threads() {
            if handle.thread().id() == me {
                // Called from this connection's own dispatch: dropping the handle detaches it.
                log::debug!(
                    "RPC: close_session from incoming connection {slot_id}'s own thread; not joined"
                );
                continue;
            }
            if handle.join().is_err() {
                log::warn!("RPC: incoming connection {slot_id} thread panicked");
            }
            self.inner.incoming_joined.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Whether this session has ended: every connection is shut, its
    /// proxies return [`StatusCode::DeadObject`] without I/O, and its death
    /// recipients have fired or are firing. Once `true` it stays `true`.
    ///
    /// A session ends as a whole on any connection's failure, an expired
    /// reply deadline, the peer closing, or
    /// [`close_session`](Self::close_session).
    /// The call that ended it may itself have returned `TimedOut` or a
    /// decode error rather than `DeadObject`, so after a failed call this —
    /// not the status code — tells whether to reconnect. A handler can also
    /// return `DeadObject` from a live session, which this tells apart.
    ///
    /// This reads state; it does not probe the connection. A session whose
    /// peer has gone while this end has not read from or written to it since
    /// (no incoming connection, no serve loop, no call, and no
    /// [`link_to_death`](crate::IBinder::link_to_death), whose thread watches
    /// on Linux and Android) still reads `false`.
    /// It reads `true` from the moment the session starts ending, before its
    /// recipients have all run.
    pub fn is_ended(&self) -> bool {
        self.inner.shared.lifecycle.is_torn_down()
    }

    /// Whether something reads this session's connections (module doc "A peer's close").
    pub(crate) fn notices_connection_loss(&self) -> bool {
        self.inner.notices_connection_loss()
    }

    /// Whether any connection's peer has closed, by `RpcTransport::peer_closed`; reads nothing.
    pub(crate) fn peer_closed(&self) -> bool {
        self.inner.peer_closed()
    }

    /// End the session as a connection fault does ("Session end"), from outside the module.
    pub(crate) fn fail(&self) {
        self.inner.fail_session();
    }

    /// Set the reply deadline: how long the peer may go without answering
    /// before this end counts it as broken. `None` (default) blocks
    /// forever. The same deadline as
    /// [`RpcServer::set_reply_timeout`](super::RpcServer::set_reply_timeout)
    /// sets on every session a server builds, and as
    /// [`RpcClientConfig::timeout`] sets on the session it founds.
    ///
    /// The same deadline bounds how long a call waits for a free
    /// connection slot when every slot is driven by another thread — a
    /// session served on one thread and transacted on another needs more
    /// than one connection. The two expiries are told apart by their
    /// code: the slot wait fails with [`StatusCode::WouldBlock`] — the
    /// request was never sent, so the call is safe to retry with the same
    /// parcel and the session goes on — where the reply wait fails with
    /// [`StatusCode::TimedOut`], after the peer may already have executed it.
    ///
    /// **An expired reply wait ends the session** (module doc "Session
    /// end"): every call in flight on its other connections fails with
    /// [`StatusCode::DeadObject`], every death recipient fires, and every
    /// local object the peer held is released. The value is how long the
    /// peer may go without answering before it counts as broken, not a
    /// per-call budget: set it above the slowest legitimate handler. A
    /// caller that wants to give up on one call and keep the session runs
    /// the call on another thread (or as a future) and stops waiting for
    /// it instead.
    ///
    /// The same value bounds how long the peer may leave this end's sends
    /// stuck and its host unanswering (plan 2-24 D4), on every connection
    /// of the session, the ones added later included, and from the moment
    /// this is called:
    ///
    /// - **Sends**: `SO_SNDTIMEO` on the bundled socket transports, and on a
    ///   transport of your own only if it implements
    ///   [`RpcTransport::set_write_timeout`] (with the no-op default a
    ///   stalled send has no bound), so a send that makes no progress for `d`
    ///   because the peer stopped reading fails and ends the session. A
    ///   peer that reads slowly but steadily is not cut: on the bundled
    ///   socket transports the deadline counts from the peer's last progress,
    ///   and on Linux and Android a reply wait does not expire while the peer
    ///   is still taking the request either
    ///   ([`transport` doc "A slow peer"](super::transport#a-slow-peer)). On a
    ///   Unix socket progress is a whole frame read: the bytes of a part of one
    ///   do not move what the socket reports.
    /// - **The peer's host**, on TCP (`tcp_debug`, `tls` over TCP): the
    ///   kernel's check that the host still answers, sized to `d` and on
    ///   with `None` too, ends the session once the host goes silent even
    ///   when no call is waiting; [`RpcTransport::set_liveness`] has the
    ///   values, the platform differences and the relay limit.
    ///
    /// A server's [`set_idle_timeout`](super::RpcServer::set_idle_timeout)
    /// also bounds its sends; with both set the smaller one applies.
    ///
    /// `Some(Duration::ZERO)` is **not** a deadline: every transaction
    /// would fail before its reply could arrive. It is refused with
    /// [`StatusCode::BadValue`] and the deadline in force is kept (plan 17
    /// C-2).
    ///
    /// [`RpcTransport::set_liveness`]: super::transport::RpcTransport::set_liveness
    pub fn set_reply_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        reject_zero_deadline(timeout, "RpcSession::set_reply_timeout")?;
        self.store_reply_timeout(timeout);
        Ok(())
    }

    /// The former name of [`set_reply_timeout`](Self::set_reply_timeout).
    #[deprecated(
        since = "0.13.0",
        note = "renamed `set_reply_timeout`, the name `RpcServer` uses for the same deadline"
    )]
    pub fn set_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.set_reply_timeout(timeout)
    }

    /// `set_reply_timeout` past its zero check, for callers that checked already.
    pub(crate) fn store_reply_timeout(&self, timeout: Option<Duration>) {
        self.inner.arm_liveness_all(|| {
            *self.inner.shared.timeout.lock().expect("timeout poisoned") = timeout;
        });
    }

    /// The server's idle deadline: serve slots' read baseline and every slot's send bound.
    pub(crate) fn set_serve_read_deadline(&self, deadline: Option<Duration>) {
        // A duration past `u64::MAX` ns (584 years) saturates; a zero one reads back as none.
        let ns = deadline.map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        self.inner.arm_liveness_all(|| {
            self.inner
                .shared
                .serve_read_deadline
                .store(ns, Ordering::Relaxed);
        });
    }

    /// `min(local, remote)` worker count established by
    /// [`RpcSession::negotiate`] (0 if not negotiated).
    pub fn negotiated_max_threads(&self) -> u32 {
        self.inner.shared.negotiated.load(Ordering::SeqCst)
    }

    /// Client role: exchange `GET_MAX_THREADS` with the server and
    /// record `min(local_max, remote_max)` (android
    /// `getRemoteMaxThreads`). Exactly one negotiation packet.
    pub fn negotiate(&self, local_max: u32) -> Result<u32> {
        let data = Parcel::new();
        let mut reply = self
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetMaxThreads.code(),
                &data,
                0,
            )?
            .ok_or(StatusCode::UnexpectedNull)?;
        let remote: i32 = reply.read()?;
        if remote < 1 {
            return Err(StatusCode::BadValue);
        }
        let negotiated = local_max.min(remote as u32).max(1);
        self.inner
            .shared
            .negotiated
            .store(negotiated, Ordering::SeqCst);
        Ok(negotiated)
    }

    /// Client: connect to a Unix-domain RPC server. Thread negotiation
    /// is a separate, explicit step ([`RpcSession::negotiate`]) so a
    /// caller that negotiates does so with exactly one packet.
    pub fn setup_unix_client(path: impl AsRef<std::path::Path>) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::new(Box::new(t), AddressSpace::Initiator)
    }

    /// Client: connect to a Linux/Android abstract Unix-domain RPC server.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn setup_unix_client_abstract(name: &[u8]) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect_abstract(name)?;
        RpcSession::new(Box::new(t), AddressSpace::Initiator)
    }

    /// Client: connect to a Unix-domain RPC server speaking the
    /// **android-13+ versioned wire**. Connects
    /// the UDS, then runs the AOSP handshake via
    /// [`RpcSession::connect_android13plus`] negotiating
    /// `min(max_version, server_max)`. The r34 client is
    /// [`RpcSession::setup_unix_client`].
    pub fn setup_unix_client_android13plus(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::connect_android13plus(Box::new(t), max_version)
    }

    /// Client: connect to a Linux/Android abstract Unix-domain RPC
    /// server speaking the **android-13+ versioned wire**.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn setup_unix_client_android13plus_abstract(
        name: &[u8],
        max_version: u32,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect_abstract(name)?;
        RpcSession::connect_android13plus(Box::new(t), max_version)
    }

    /// Client: connect to a **TCP** RPC server over **TLS**,
    /// R34 wire. Establishes the TCP connection, completes the
    /// TLS handshake to `server_name` (verified per `config` — a
    /// bad/untrusted server certificate fails **here**, before any RPC
    /// payload byte is exchanged), then builds an R34 session. A refused
    /// certificate is `StatusCode::Errno(-EKEYREJECTED)`, `-EKEYREVOKED` or
    /// `-EKEYEXPIRED` on Linux and Android (`From<RpcError> for StatusCode`
    /// has the table). Over TLS 1.3 a server's refusal of *this* client's
    /// certificate arrives only with the first reply, so this returns `Ok`
    /// and the first call returns the refusal. The
    /// android-13+ variant is
    /// [`setup_tcp_client_tls_android13plus`](RpcSession::setup_tcp_client_tls_android13plus).
    ///
    /// `config` is the caller's `rustls::ClientConfig` (roots / client
    /// cert / verification policy) — rsbinder never invents crypto.
    /// For a non-TCP stream (a preconnected `unix`/`vsock`
    /// fd) build the transport directly with
    /// [`TlsTransport::connect_stream`](super::transport::TlsTransport::connect_stream)
    /// and pass it to [`RpcSession::new`].
    ///
    /// No deadline: neither `connect(2)` nor the TLS handshake is bounded,
    /// so a server that accepts the connection and then sends nothing
    /// hangs this call. [`Client::open_with`](crate::Client::open_with) on
    /// [`Endpoint::tls`](crate::Endpoint::tls) with
    /// [`ClientOptions::timeout`](crate::ClientOptions::timeout) set opens
    /// the same r34 TLS session with each connecting step bounded. That
    /// `timeout` then stays the session's reply deadline
    /// ([`RpcSession::set_reply_timeout`]), and an expired reply wait ends the
    /// session; call `set_reply_timeout(None)` on the session afterwards to
    /// keep reply waits unbounded.
    #[cfg(feature = "rpc-tls")]
    pub fn setup_tcp_client_tls(
        addr: impl std::net::ToSocketAddrs,
        server_name: &str,
        config: std::sync::Arc<rustls::ClientConfig>,
    ) -> Result<RpcSession> {
        let tcp = std::net::TcpStream::connect(addr)?;
        let t = super::transport::TlsTransport::connect(tcp, server_name, config)?;
        RpcSession::new(Box::new(t), AddressSpace::Initiator)
    }

    /// Client: connect to a **TCP** RPC server over **TLS** speaking the
    /// **android-13+ versioned wire**. TCP-connects,
    /// TLS-handshakes to `server_name` per `config` (a bad cert fails
    /// before any RPC byte), then runs the AOSP android-13+ handshake via
    /// [`RpcSession::connect_android13plus`] negotiating
    /// `min(max_version, server_max)`. The R34 variant is
    /// [`setup_tcp_client_tls`](RpcSession::setup_tcp_client_tls).
    ///
    /// No deadline: `connect(2)`, the TLS handshake and the android-13+
    /// handshake are unbounded, so a server that accepts the connection and
    /// then sends nothing hangs this call.
    /// [`setup_client_android13plus_with_config`](Self::setup_client_android13plus_with_config)
    /// with [`RpcClientConfig::tls`] and [`RpcClientConfig::timeout`] set
    /// opens the same session with each of those steps bounded. That
    /// `timeout` then stays the session's reply deadline
    /// ([`RpcSession::set_reply_timeout`]), and an expired reply wait ends the
    /// session; call `set_reply_timeout(None)` on the session afterwards to
    /// keep reply waits unbounded.
    #[cfg(feature = "rpc-tls")]
    pub fn setup_tcp_client_tls_android13plus(
        addr: impl std::net::ToSocketAddrs,
        server_name: &str,
        config: std::sync::Arc<rustls::ClientConfig>,
        max_version: u32,
    ) -> Result<RpcSession> {
        let tcp = std::net::TcpStream::connect(addr)?;
        let t = super::transport::TlsTransport::connect(tcp, server_name, config)?;
        RpcSession::connect_android13plus(Box::new(t), max_version)
    }

    /// Client: connect to an android-13+ server over any transport — the
    /// founding connection, the outgoing fan-out and the incoming
    /// (callback) connections each come from one call to the config's
    /// `connect`. See [`RpcClientConfig`].
    pub fn setup_client_android13plus_with_config(config: RpcClientConfig) -> Result<RpcSession> {
        config.reject_zero_timeout()?;
        let RpcClientConfig {
            source,
            max_version,
            session_id: requested_id,
            outgoing_connections,
            incoming_connections: incoming,
            fd_mode: requested_fd_mode,
            timeout,
        } = config;
        // `timeout` bounds every connecting step as well as the session's reply waits.
        let handshake_timeout = timeout;
        let local = outgoing_connections.max(1);
        refuse_id_on_new_session(
            &requested_id,
            "RpcSession::setup_client_android13plus_with_config",
        )?;

        let mut connect = source.into_connector(handshake_timeout);
        let founding = connect()?;
        // Checked on the transport (custom ones too), or `Unix` is agreed and fd sends fail later.
        if requested_fd_mode == Some(FileDescriptorTransportMode::Unix)
            && !founding.supports_fd_passing()
        {
            log::error!(
                "RPC client: FileDescriptorTransportMode::Unix needs a Unix-domain transport"
            );
            return Err(StatusCode::BadValue);
        }
        let session = RpcSession::connect_android13plus_fd_hs(
            founding,
            max_version,
            requested_fd_mode.unwrap_or(FileDescriptorTransportMode::None),
            handshake_timeout,
        )?;
        // Before `negotiate`/`get_session_id` below: their round trips read it.
        if timeout.is_some() {
            session.store_reply_timeout(timeout);
        }
        if local == 1 && incoming == 0 {
            // Single connection: byte-identical to `connect_android13plus_fd`.
            session.keep_redial(connect, handshake_timeout);
            return Ok(session);
        }

        // AOSP `setupClient` order: outgoing fan-out first, then incoming connections.
        let mut build = || -> Result<()> {
            let negotiated = if local > 1 {
                session.negotiate(local)?
            } else {
                1
            };
            let session_id = session.get_session_id()?;
            let fd_mode = session.fd_transport_mode();
            for _ in 1..negotiated {
                session.add_outgoing_connection_android13plus_transport(
                    &mut connect,
                    max_version,
                    &session_id,
                    fd_mode,
                    handshake_timeout,
                )?;
            }
            for _ in 0..incoming {
                session.add_incoming_connection_android13plus_transport(
                    &mut connect,
                    max_version,
                    &session_id,
                    fd_mode,
                    handshake_timeout,
                )?;
            }
            Ok(())
        };
        if let Err(e) = build() {
            // No degradation: tear the partial session down; the caller never gets a handle.
            session.close_session();
            return Err(e);
        }
        session.keep_redial(connect, handshake_timeout);
        Ok(session)
    }

    /// Keep the founding config's connector for the `add_*_connection()` calls.
    fn keep_redial(&self, connect: Connector, handshake_timeout: Option<Duration>) {
        let redial = Redial {
            connect: Mutex::new(connect),
            handshake_timeout,
        };
        *self.inner.shared.redial.lock().expect("redial poisoned") = Some(Arc::new(redial));
    }

    /// The founding connector and the id and wire an attach echoes; `InvalidOperation` if none.
    fn redial_parts(&self, entry: &str) -> Result<(Arc<Redial>, Vec<u8>, u32)> {
        let redial = self
            .inner
            .shared
            .redial
            .lock()
            .expect("redial poisoned")
            .clone();
        let Some(redial) = redial else {
            log::error!(
                "{entry}: this session was not set up from an `RpcClientConfig` \
                 (`RpcSession::setup_client_android13plus_with_config`, or `Client::open` on an \
                 android-13+ endpoint), so it has no connector to open another connection with"
            );
            return Err(StatusCode::InvalidOperation);
        };
        // Only an android-13+ setup keeps a connector, and its session has a version.
        let version = self.wire_protocol_version().ok_or(StatusCode::BadType)?;
        // One statement, so the guard is gone before `get_session_id` locks again.
        let kept = self.server_id_lock().clone();
        let session_id = match kept {
            Some(id) => id,
            None => self.get_session_id()?,
        };
        Ok((redial, session_id, version))
    }

    /// Open one more **outgoing** connection to this session's server, the
    /// way [`setup_client_android13plus_with_config`](Self::setup_client_android13plus_with_config)
    /// opened the founding one, and add it to the pool (AOSP
    /// `RpcSession::setupClient` opens N outgoing; `findConnection`
    /// distributes calls across them). Returns the new slot id.
    ///
    /// It reuses the founding [`RpcClientConfig`]'s connector, so the
    /// connection goes where the founding one went (for
    /// `RpcClientConfig::tls`, the address its host name resolved to),
    /// under the same handshake deadline, echoing this session's
    /// server-minted id, at its wire version and fd mode. Nothing about the
    /// session is asked again.
    ///
    /// A session that has no such connector — one built from a transport
    /// ([`connect_android13plus`](Self::connect_android13plus),
    /// [`from_preconnected_fd`](Self::from_preconnected_fd), an r34
    /// session, a server's session) — is [`StatusCode::InvalidOperation`].
    ///
    /// The server's admission is confirmed before the slot joins the pool,
    /// and the failures that end the session are those of the deprecated
    /// [`add_outgoing_connection_with_config`](Self::add_outgoing_connection_with_config),
    /// which documents them: a libbinder server pools the connection once
    /// it has its header, so a failure past that point ends the session.
    /// A server's `set_max_threads` caps the outgoing connections; stay
    /// within [`negotiate()`](Self::negotiate).
    pub fn add_outgoing_connection(&self) -> Result<u64> {
        let (redial, session_id, version) =
            self.redial_parts("RpcSession::add_outgoing_connection")?;
        let fd_mode = self.fd_transport_mode();
        let connect = || (redial.connect.lock().expect("redial poisoned"))();
        self.add_outgoing_connection_android13plus_transport(
            connect,
            version,
            &session_id,
            fd_mode,
            redial.handshake_timeout,
        )
    }

    /// Open one more **incoming (callback) connection** to this session's
    /// server and serve it on a thread this session owns — AOSP
    /// `RpcSession::addIncomingConnection`, the one-at-a-time form of
    /// [`RpcClientConfig::incoming_connections`]. Returns the new slot id.
    ///
    /// The connection is opened as
    /// [`add_outgoing_connection`](Self::add_outgoing_connection) opens
    /// one, from the founding config's connector, and a session without
    /// one is [`StatusCode::InvalidOperation`]. The server bounds a
    /// session's callback connections at twice its `set_max_threads`.
    /// Which failures end the session is as the deprecated
    /// [`add_incoming_connection_with_config`](Self::add_incoming_connection_with_config)
    /// documents.
    pub fn add_incoming_connection(&self) -> Result<u64> {
        let (redial, session_id, version) =
            self.redial_parts("RpcSession::add_incoming_connection")?;
        let fd_mode = self.fd_transport_mode();
        let connect = || (redial.connect.lock().expect("redial poisoned"))();
        self.add_incoming_connection_android13plus_transport(
            connect,
            version,
            &session_id,
            fd_mode,
            redial.handshake_timeout,
        )
    }

    /// Client multi-outgoing using a [`RpcClientConfig`], which is held
    /// to the [manual attach](RpcClientConfig#manual-attach) rule.
    ///
    /// A libbinder server adds the connection to the session as soon as
    /// it has read the whole connection header, before the rest of the
    /// handshake and the `GET_SESSION_ID` probe that confirms the attach,
    /// and ends the session when that connection fails. From
    /// `android-16.0.0_r3` it also ends the session when it refuses the
    /// attach at its `setMaxThreads` cap, which this end sees only as a
    /// close; rsbinder and older libbinder refuse the same way and keep
    /// theirs. So a failure ends the whole session, and returns its
    /// error, unless the attach never got its whole header out: a check
    /// before the header refused it (a `max_version` below the session's,
    /// a transport or server peer unlike the founding connection's, or an
    /// id other than this session's [`get_session_id()`](Self::get_session_id), such as
    /// the client-local id this session minted for itself or another
    /// session's id, which is [`StatusCode::BadValue`]), the
    /// connect or the handshake deadline's setup failed, or the header
    /// write failed.
    ///
    /// Every other failure ends the session: a close or reset before the
    /// probe's reply, as a refused attach does (an id the server does not
    /// know, its cap spent, its shutdown), an expired deadline or any
    /// other error after the header, a reply cut part-way, and a reply
    /// that does not confirm the id. Stay within
    /// [`negotiate()`](Self::negotiate) connections and echo
    /// [`get_session_id()`](Self::get_session_id) to keep an attach from
    /// costing the session.
    #[deprecated(
        since = "0.13.0",
        note = "use `add_outgoing_connection()`, which reuses the founding config's connector and echoes the session's own id, version and fd mode"
    )]
    pub fn add_outgoing_connection_with_config(&self, config: RpcClientConfig) -> Result<u64> {
        let AttachParts {
            mut connect,
            max_version,
            session_id,
            fd_mode,
            handshake_timeout,
        } = self.attach_parts(config)?;
        self.add_outgoing_connection_android13plus_transport(
            &mut connect,
            max_version,
            &session_id,
            fd_mode,
            handshake_timeout,
        )
    }

    /// The connection and knobs of a manual attach, refusing what an attach cannot express.
    fn attach_parts(&self, config: RpcClientConfig) -> Result<AttachParts> {
        if config.outgoing_connections.max(1) != 1
            || config.incoming_connections != 0
            || config.session_id.len() != 32
            // The session already has its deadline; accepting one here would silently drop it.
            || config.timeout.is_some()
        {
            return Err(StatusCode::BadValue);
        }
        let fd_mode = self.fd_transport_mode();
        if config.fd_mode.is_some_and(|mode| mode != fd_mode) {
            return Err(StatusCode::BadValue);
        }
        // `timeout` is refused above: the session's own bounds the attach's handshake.
        let handshake_timeout = *self.inner.shared.timeout.lock().expect("timeout poisoned");
        Ok(AttachParts {
            connect: config.source.into_connector(handshake_timeout),
            max_version: config.max_version,
            session_id: config.session_id,
            fd_mode,
            handshake_timeout,
        })
    }

    fn add_outgoing_connection_android13plus_transport(
        &self,
        connect: impl FnOnce() -> Result<Box<dyn RpcTransport>>,
        max_version: u32,
        session_id: &[u8],
        fd_mode: FileDescriptorTransportMode,
        handshake_timeout: Option<Duration>,
    ) -> Result<u64> {
        // AOSP: one version per session, 32-byte ids; checked before a handshake burns an attach.
        if session_id.len() != 32 {
            return Err(StatusCode::BadValue);
        }
        let session_version = match &self.inner.profile {
            WireProfile::Android13Plus(c) => c.version(),
            WireProfile::R34(_) => return Err(StatusCode::BadType),
        };
        // Checked early: reaching `add_outgoing_slot` costs a connect, handshake and a server slot.
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        let effective_max = max_version.min(session_version);
        if effective_max != session_version {
            // Refused before the header: libbinder pools an attach at any header version.
            log::error!(
                "android-13+ RPC: this attach asks for wire v{effective_max} but the session \
                 runs v{session_version} — a caller-supplied `max_version` below the session's \
                 negotiated version can never attach; pass `RpcSession::wire_protocol_version()`"
            );
            return Err(StatusCode::BadType);
        }
        // Refused before the header, where a server's close would end the session.
        self.refuse_foreign_attach_id(session_id)?;
        let hdr_fd_mode = if fd_mode == FileDescriptorTransportMode::Unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let t = connect()?;
        // The pool would refuse it past the header, where a refusal ends the session.
        if !self
            .inner
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .admits(&*t)
        {
            return Err(StatusCode::BadType);
        }
        let codec = {
            let mut hs =
                HandshakeDeadline::arm(&*t, handshake_timeout).map_err(StatusCode::from)?;
            let mut io = RawTransportIo(&*t);
            // A failed header write leaves the server short of it, so none admitted.
            let codec = client_write_connection_header(
                &mut io,
                effective_max,
                false,
                hdr_fd_mode,
                session_id,
            )
            .map_err(|e| StatusCode::from(hs.classify(e)))?;
            // libbinder pools the connection once it has the header, before it reads `"cci"`.
            let init = std::io::Write::write_all(&mut io, &codec.encode_connection_init());
            if let Err(e) = init {
                let e = hs.classify(RpcError::from(e));
                return Err(self.end_past_outgoing_header(StatusCode::from(e)));
            }
            if let Err(e) = hs.finish() {
                return Err(self.end_past_outgoing_header(StatusCode::from(e)));
            }
            codec
        };
        {
            // Confirm admission before the slot joins; the probe falls back to `set_timeout`.
            let probe_deadline =
                handshake_timeout.or(*self.inner.shared.timeout.lock().expect("timeout poisoned"));
            let mut hs = match HandshakeDeadline::arm(&*t, probe_deadline) {
                Ok(hs) => hs,
                Err(e) => return Err(self.end_past_outgoing_header(StatusCode::from(e))),
            };
            if let Err(e) = confirm_attach(&*t, &codec, session_id).and_then(|()| hs.finish()) {
                let e = hs.classify(e);
                log_attach_refused(&e);
                return Err(self.end_past_outgoing_header(StatusCode::from(e)));
            }
        }
        match self.inner.add_outgoing_slot(t) {
            Ok(id) => Ok(id),
            Err(e) => Err(self.end_past_outgoing_header(e)),
        }
    }

    /// Past its header the server may hold the attach: only a session end tells it ("Leaving").
    fn end_past_outgoing_header(&self, status: StatusCode) -> StatusCode {
        log::error!("RPC: outgoing attach failed past its header ({status:?}); ending the session");
        self.inner.fail_session();
        status
    }

    /// Open one *additional* incoming (callback) connection using a
    /// [`RpcClientConfig`] — on any transport, not only a Unix socket.
    /// The config is held to the
    /// [manual attach](RpcClientConfig#manual-attach) rule.
    ///
    /// The server adds the connection as a callback slot before it sends
    /// its `"cci"` (the handshake's last message), and its first callback
    /// on a connection this end dropped would fail and end the session
    /// anyway (plan 2-24 D1). So a failure ends the whole session, and
    /// returns its error, unless the server cannot hold the connection.
    /// That is the case in exactly two ways:
    ///
    /// - the attach never got its whole header out: a check before the
    ///   header refused it (a `max_version` below the session's, a
    ///   transport or server peer unlike the founding connection's, an id other than this
    ///   session's [`get_session_id()`](Self::get_session_id), which is
    ///   [`StatusCode::BadValue`]), the connect or the handshake deadline's
    ///   setup failed, or the header write failed (a server admits
    ///   nothing before the whole header);
    /// - the connection closed, a reset included, before any `"cci"` byte
    ///   arrived, as a refused attach does.
    ///
    /// Every other failure ends the session: a cut inside `"cci"`, an
    /// expired handshake deadline or any other error while awaiting it,
    /// bytes that are not `"cci"`, and every failure after it. A reset on
    /// the path before the first `"cci"` byte cannot be told from a
    /// refusal and leaves the session up; a server that did pool the
    /// connection ends the session at its first callback there.
    #[deprecated(
        since = "0.13.0",
        note = "use `add_incoming_connection()`, which reuses the founding config's connector and echoes the session's own id, version and fd mode"
    )]
    pub fn add_incoming_connection_with_config(&self, config: RpcClientConfig) -> Result<u64> {
        let AttachParts {
            mut connect,
            max_version,
            session_id,
            fd_mode,
            handshake_timeout,
        } = self.attach_parts(config)?;
        self.add_incoming_connection_android13plus_transport(
            &mut connect,
            max_version,
            &session_id,
            fd_mode,
            handshake_timeout,
        )
    }

    fn add_incoming_connection_android13plus_transport(
        &self,
        connect: impl FnOnce() -> Result<Box<dyn RpcTransport>>,
        max_version: u32,
        session_id: &[u8],
        fd_mode: FileDescriptorTransportMode,
        handshake_timeout: Option<Duration>,
    ) -> Result<u64> {
        // Only the side that connected owns incoming threads.
        if self.inner.shared.space() != AddressSpace::Initiator {
            return Err(StatusCode::InvalidOperation);
        }
        if session_id.len() != 32 {
            return Err(StatusCode::BadValue);
        }
        let session_version = match &self.inner.profile {
            WireProfile::Android13Plus(c) => c.version(),
            WireProfile::R34(_) => return Err(StatusCode::BadType),
        };
        if self.inner.shared.lifecycle.is_torn_down() {
            return Err(StatusCode::DeadObject);
        }
        let effective_max = max_version.min(session_version);
        // An attach sends `effective_max` unnegotiated; refused here, as past `"cci"` it would end.
        if effective_max != session_version {
            log::error!(
                "android-13+ RPC: this incoming attach asks for wire v{effective_max} but the \
                 session runs v{session_version}; pass `RpcSession::wire_protocol_version()`"
            );
            return Err(StatusCode::BadType);
        }
        self.refuse_foreign_attach_id(session_id)?;
        let hdr_fd_mode = if fd_mode == FileDescriptorTransportMode::Unix {
            FD_MODE_UNIX
        } else {
            FD_MODE_NONE
        };
        let t = connect()?;
        // The pool would refuse it past `"cci"`, where a refusal ends the session.
        if !self
            .inner
            .conn_state
            .lock()
            .expect("conn_state poisoned")
            .admits(&*t)
        {
            return Err(StatusCode::BadType);
        }
        {
            // Cleared before the push: a lingering read deadline would break the serve loop.
            let mut hs =
                HandshakeDeadline::arm(&*t, handshake_timeout).map_err(StatusCode::from)?;
            let mut io = RawTransportIo(&*t);
            // An INCOMING header; a failed write leaves the server short of it, so none admitted.
            let codec = client_write_connection_header(
                &mut io,
                effective_max,
                true,
                hdr_fd_mode,
                session_id,
            )
            .map_err(|e| StatusCode::from(hs.classify(e)))?;
            // The deadline's cut reads as an end of stream: classified, it ends the session.
            if let Err((e, received)) = client_read_connection_init(&mut io, &codec) {
                return Err(self.fail_awaiting_cci(hs.classify(e), received));
            }
            // `"cci"` came, but the deadline cut the connection as it did: the server holds it.
            if let Err(e) = hs.finish() {
                let received = super::wire_android13::A13_CONN_INIT_LEN;
                return Err(self.fail_awaiting_cci(e, received));
            }
        }
        // Dropping `t` on refusal closes the socket, but the server already pooled it.
        let slot_id = match self.inner.add_slot_inner(t, SlotRole::Incoming) {
            Ok(id) => id,
            Err(e) => return Err(self.fail_after_cci(e)),
        };
        let inner = Arc::clone(&self.inner);
        // Bumped before the spawn, so the counter is never observed low.
        self.inner.incoming_live.fetch_add(1, Ordering::SeqCst);
        let builder = std::thread::Builder::new().name(format!("rsbinder-rpc-in-{slot_id}"));
        let spawned = self.inner.shared.spawn_thread(builder, move || {
            let session = RpcSession::wrap_inner(inner);
            session
                .serve_blocking_on(slot_id)
                .log(&format!("RPC: incoming connection {slot_id} ended"));
            // Last act of the thread — see `incoming_live`.
            session.inner.incoming_live.fetch_sub(1, Ordering::SeqCst);
        });
        match spawned {
            Ok(handle) => {
                self.inner
                    .incoming_threads
                    .lock()
                    .expect("incoming_threads poisoned")
                    .push((slot_id, handle));
                if !self.inner.shared.lifecycle.is_torn_down() {
                    return Ok(slot_id);
                }
                // Ended meanwhile: a `close_session` that took the list first never joins this one.
                let mine = {
                    let mut threads = self
                        .inner
                        .incoming_threads
                        .lock()
                        .expect("incoming_threads poisoned");
                    let at = threads.iter().position(|(id, _)| *id == slot_id);
                    at.map(|i| threads.remove(i).1)
                };
                if let Some(handle) = mine {
                    if handle.join().is_err() {
                        log::warn!("RPC: incoming connection {slot_id} thread panicked");
                    }
                    self.inner.incoming_joined.fetch_add(1, Ordering::SeqCst);
                }
                Err(StatusCode::DeadObject)
            }
            Err(e) => {
                log::error!("RPC: incoming connection thread spawn failed ({e})");
                self.inner.incoming_live.fetch_sub(1, Ordering::SeqCst);
                Err(self.fail_after_cci(StatusCode::from(e)))
            }
        }
    }

    /// The server pooled the connection when it sent `"cci"`: only a session end tells it.
    fn fail_after_cci(&self, status: StatusCode) -> StatusCode {
        log::error!(
            "RPC: incoming attach failed past the server's \"cci\" ({status:?}); ending the session"
        );
        self.inner.fail_session();
        status
    }

    /// Only a close before any `"cci"` byte means the server holds no slot ("Leaving").
    fn fail_awaiting_cci(&self, e: RpcError, received: usize) -> StatusCode {
        // A reset is a close too: a refusal ahead of the header closes with it unread ("Leaving").
        if received == 0 && matches!(e, RpcError::EndOfStream | RpcError::UncleanEndOfStream) {
            return StatusCode::from(e);
        }
        log::error!(
            "RPC: incoming attach failed awaiting the server's \"cci\" ({e}); ending the session"
        );
        self.inner.fail_session();
        StatusCode::from(e)
    }

    /// Client: connect to a Unix-domain android-13+ RPC server **with
    /// FD-over-RPC** opt-in. UDS connect + the
    /// AOSP handshake requesting `fd_mode` in the connection header
    /// (see [`RpcSession::connect_android13plus_fd`]).
    /// `FileDescriptorTransportMode::None` ==
    /// [`RpcSession::setup_unix_client_android13plus`] (byte-identical).
    pub fn setup_unix_client_android13plus_fd(
        path: impl AsRef<std::path::Path>,
        max_version: u32,
        fd_mode: FileDescriptorTransportMode,
    ) -> Result<RpcSession> {
        let t = super::transport::UnixTransport::connect(path)?;
        RpcSession::connect_android13plus_fd(Box::new(t), max_version, fd_mode)
    }

    /// Adopt a **preconnected** RPC socket fd handed
    /// to us by an out-of-band channel (the AOSP `IAccessor::addConnection`
    /// path: `BackendUnifiedServiceManager` receives a `unique_fd` and
    /// hands it to `RpcSession::setupPreconnectedClient(fd, request)`).
    ///
    /// The fd's address family (`SO_DOMAIN`) selects the rsbinder
    /// transport — `AF_UNIX` → [`super::transport::UnixTransport`],
    /// `AF_VSOCK` → `VsockTransport` (feature `rpc-vsock`, Linux only),
    /// `AF_INET`/`AF_INET6` → `TcpDebugTransport`
    /// (feature `rpc-tcp-debug`, which Cargo enables for the whole build when
    /// any crate in the graph asks for it — release builds too). This call
    /// reads no run-time opt-in: its caller picked the socket. The accessor
    /// path, where a peer picks it, refuses an inet socket unless
    /// `rpc::set_accessor_plaintext_inet(true)` was called. Any other family is rejected as
    /// [`StatusCode::BadType`], paralleling AOSP's
    /// `IAccessor::ERROR_UNSUPPORTED_SOCKET_FAMILY`. The handshake then
    /// runs through [`RpcSession::connect_android13plus_fd`] with
    /// `FileDescriptorTransportMode::None` (the fd carries no FD-mode
    /// metadata of its own — re-using the versioned wire bytes, neither
    /// a new codec nor a new framing path). `max_version` is the highest
    /// `RPC_WIRE_PROTOCOL_VERSION` to offer (`2` for android-16, `1` for
    /// android-14/15, `0` for android-13). The peer's `RpcServer`
    /// negotiates `min(max_version, server_max)` exactly as for the
    /// path-based client.
    ///
    /// rsbinder uses a single-connection session here, so no
    /// AOSP `request` reconnect closure is needed.
    ///
    /// The fd is switched to blocking mode first. AOSP
    /// `singleSocketConnection` (`frameworks/native/libs/binder/RpcSession.cpp`,
    /// `android-16.0.0_r4`) opens its preconnected socket with
    /// `SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK`, and
    /// `LocalAccessor::addConnection` returns that same fd to a client, so an
    /// Accessor-supplied fd arrives non-blocking. rsbinder's RPC I/O is
    /// blocking: an `EAGAIN` mid-handshake would surface as `Io(WouldBlock)`
    /// and end the connection.
    ///
    /// The handshake as a whole is bounded by 10 s, as
    /// [`RpcClientConfig::timeout`] bounds a handshake step: a peer that
    /// answers one byte at a time does not stretch it, and an expired
    /// handshake returns [`StatusCode::TimedOut`]. The fd comes from an
    /// Accessor the service manager returned, a peer that may accept the
    /// connection and then never send or never read; without the deadline
    /// `getService`/`get_root` would block forever, because the per-call
    /// session timeout applies only inside a transaction. The deadline is
    /// cleared on the established session.
    pub fn from_preconnected_fd(fd: OwnedFd, max_version: u32) -> Result<RpcSession> {
        // (a) `getsockname`, not `SO_DOMAIN` (absent on macOS); a connected fd always has a name.
        let local = rustix::net::getsockname(fd.as_fd())
            .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        let family = local.address_family();

        // (a') Clear `O_NONBLOCK`: an Accessor fd arrives non-blocking (rustdoc).
        let flags = rustix::fs::fcntl_getfl(fd.as_fd())
            .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        if flags.contains(rustix::fs::OFlags::NONBLOCK) {
            rustix::fs::fcntl_setfl(fd.as_fd(), flags - rustix::fs::OFlags::NONBLOCK)
                .map_err(|e| RpcError::from(std::io::Error::from(e)))?;
        }

        // (b) family → backend; a compiled-out backend is AOSP `UNSUPPORTED_SOCKET_FAMILY`.
        let transport: Box<dyn RpcTransport> = match family {
            rustix::net::AddressFamily::UNIX => {
                Box::new(super::transport::UnixTransport::from_owned_fd(fd)?)
            }
            #[cfg(all(feature = "rpc-vsock", any(target_os = "linux", target_os = "android")))]
            rustix::net::AddressFamily::VSOCK => {
                Box::new(super::transport::VsockTransport::from_owned_fd(fd)?)
            }
            #[cfg(feature = "rpc-tcp-debug")]
            rustix::net::AddressFamily::INET | rustix::net::AddressFamily::INET6 => {
                Box::new(super::transport::TcpDebugTransport::from_owned_fd(fd)?)
            }
            _ => {
                log::warn!(
                    "RPC preconnected fd has unsupported socket family ({:?}); \
                     rejecting (AOSP IAccessor::ERROR_UNSUPPORTED_SOCKET_FAMILY)",
                    family.as_raw()
                );
                return Err(StatusCode::BadType);
            }
        };

        // (c) android-13+ handshake without FD mode, bounded as a whole (rustdoc).
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
        let session = RpcSession::connect_android13plus_fd_hs(
            transport,
            max_version,
            FileDescriptorTransportMode::None,
            Some(HANDSHAKE_TIMEOUT),
        )?;
        session.inner.clear_handshake_timeouts();
        Ok(session)
    }

    /// Test/diagnostic: number of connection slots in this session's pool
    /// (founding + fan-out + incoming, or founding + attaches + callback
    /// slots on a server session). Not a stable API.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn __slot_count(&self) -> usize {
        self.inner.slot_count()
    }

    /// Test/diagnostic, not a stable API: `HELD_DEC_STRONG_LIMIT` ("Deferred `DEC_STRONG`").
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn __held_dec_strong_limit() -> usize {
        HELD_DEC_STRONG_LIMIT
    }

    /// Test/diagnostic: incoming-connection threads whose `JoinHandle`
    /// this session still holds. `close_session` takes the whole set before
    /// joining any of it, so this drops to zero the moment `close_session`
    /// starts — use [`__incoming_thread_live_count`](Self::__incoming_thread_live_count)
    /// to observe the join itself.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn __incoming_thread_count(&self) -> usize {
        self.inner
            .incoming_threads
            .lock()
            .expect("incoming_threads poisoned")
            .len()
    }

    /// Test/diagnostic: incoming-connection threads still running.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn __incoming_thread_live_count(&self) -> usize {
        self.inner.incoming_live.load(Ordering::SeqCst)
    }

    /// Test/diagnostic: incoming-connection threads `close_session` has
    /// joined. Stays 0 if they are detached instead.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn __incoming_thread_joined_count(&self) -> usize {
        self.inner.incoming_joined.load(Ordering::SeqCst)
    }

    /// Test/diagnostic: live local-node count (leak check).
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn local_node_count(&self) -> usize {
        self.inner
            .shared
            .state
            .lock()
            .expect("rpc state poisoned")
            .local_node_count()
    }
}

impl Drop for RpcSessionInner {
    fn drop(&mut self) {
        let wake = self.reaper_wake.get_mut();
        let wake = wake.unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cut) = wake.take() {
            cut();
        }
    }
}

/// The thread that waits and ends sessions for those that must not; module doc.
fn reaper_loop(weak: Weak<RpcSessionInner>, rx: mpsc::Receiver<ReaperJob>) {
    while let Ok(job) = rx.recv() {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        match job {
            ReaperJob::Dec => reap_owed(&weak, inner),
            ReaperJob::AwaitRoom(slot_id) => await_room(&weak, inner, slot_id),
            ReaperJob::Carried => flush_carried(&weak, inner),
            // Whoever queued this took the session to `Dying` and left the sequence to us.
            ReaperJob::End => inner.on_session_dead(),
            ReaperJob::PeerClosed => inner.end_at_peers_close(),
            ReaperJob::PeerClosedReady => {
                if !inner.notices_connection_loss() {
                    inner.fail_session();
                }
            }
        }
    }
}

/// Run `wait` with no strong reference to the session; `None` if the session went meanwhile.
fn wait_without_the_session<T>(
    weak: &Weak<RpcSessionInner>,
    inner: Arc<RpcSessionInner>,
    transport: &dyn RpcTransport,
    wait: impl FnOnce(&SharedSession) -> T,
) -> Option<(Arc<RpcSessionInner>, T)> {
    let shared = Arc::clone(&inner.shared);
    *inner.reaper_wake.lock().expect("reaper_wake poisoned") = transport.shutdown_handle();
    drop(inner);
    let waited = wait(&shared);
    let inner = weak.upgrade()?;
    let wake = inner
        .reaper_wake
        .lock()
        .expect("reaper_wake poisoned")
        .take();
    drop(wake);
    Some((inner, waited))
}

/// `ReaperJob::Dec`: what the session owes, until it owes nothing; the session may go meanwhile.
fn reap_owed(weak: &Weak<RpcSessionInner>, mut inner: Arc<RpcSessionInner>) {
    while let Some(owed) = inner.take_reaper_owed() {
        for (addr, amount) in owed {
            inner = match reap_dec(weak, inner, addr, amount) {
                Some(inner) => inner,
                None => return,
            };
        }
    }
}

/// One address's amount: best-effort, but a transport failure ends the session ("Failed sends").
fn reap_dec(
    weak: &Weak<RpcSessionInner>,
    inner: Arc<RpcSessionInner>,
    addr: RpcAddress,
    amount: u32,
) -> Option<Arc<RpcSessionInner>> {
    let Some((frame, times)) = inner.profile.codec().encode_dec_strong(&addr, amount) else {
        return Some(inner);
    };
    // Waits only on this thread, and gives up on teardown or drain.
    let Some((slot_id, transport)) = inner.find_conn_for_reaper() else {
        // Owed still, as this job ends: an attach, a `REPLY` or a nested request writes it.
        let mut st = inner.conn_state.lock().expect("conn_state poisoned");
        let owed = st.reaper_owed.entry(addr).or_insert(0);
        *owed = owed.saturating_add(amount);
        drop(st);
        return Some(inner);
    };
    let sent = wait_without_the_session(weak, inner, &*transport, |shared| {
        // A frame on its way out is activity ("Idle"); no drain, as AOSP `sendDecStrongToTarget`.
        let _sending = OpenCall::enter(shared);
        (0..times).try_for_each(|_| write_aosp_message_with_fds(&*transport, &frame, &[], None))
    });
    let (inner, sent) = sent?;
    // Before the slot goes back: a failed write may have left a cut frame on it.
    match sent {
        Ok(()) => inner.pay_receipts(&addr, amount),
        Err(e) => inner.end_after_failed_send(&e),
    }
    inner.release_reaper_claim(slot_id);
    Some(inner)
}

/// `ReaperJob::Carried`: a claim on each free slot, whose next writer may have written it already.
fn flush_carried(weak: &Weak<RpcSessionInner>, mut inner: Arc<RpcSessionInner>) {
    // Before the look: a frame left after it, or on a slot held during it, queues this job again.
    inner.carried_queued.store(false, Ordering::SeqCst);
    for slot_id in inner.slots_carrying() {
        // A slot's holder queues this job again as its guard drops.
        let Some(transport) = inner.claim_if_free(slot_id) else {
            continue;
        };
        let flushed = wait_without_the_session(weak, inner, &*transport, |shared| {
            let _sending = OpenCall::enter(shared);
            transport.__flush_carried(None)
        });
        let Some((back, flushed)) = flushed else {
            return;
        };
        inner = back;
        if let Err(e) = flushed {
            inner.end_after_failed_send(&e);
        }
        inner.release_reaper_claim(slot_id);
    }
}

/// `ReaperJob::AwaitRoom`: a free slot held until there is room, so no writer fills it meanwhile.
fn await_room(weak: &Weak<RpcSessionInner>, inner: Arc<RpcSessionInner>, slot_id: u64) {
    // A slot's holder has a send deadline of its own, and the next no-room queues this job again.
    let Some(transport) = inner.claim_if_free(slot_id) else {
        inner.awaiting_room.store(false, Ordering::SeqCst);
        return;
    };
    let room = wait_without_the_session(weak, inner, &*transport, |_| transport.__wait_for_room());
    let Some((inner, room)) = room else {
        return;
    };
    match room {
        Ok(true) => {}
        Ok(false) => {
            log::warn!("RPC: a peer took nothing for the send deadline; ending the session");
            inner.fail_session();
        }
        Err(e) => inner.end_after_failed_send(&e),
    }
    inner.release_reaper_claim(slot_id);
    inner.awaiting_room.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    //! Unit gate for the session as a whole, without standing up an
    //! `RpcServer` (the end-to-end handshake against a peer is the
    //! `tests/rpc_accessor.rs` integration suite's job).
    //!
    //! Cross-platform host (Linux + macOS): tests run over `AF_UNIX`
    //! socketpairs or `MemTransport`, a few over TCP loopback or on
    //! `/dev/null`, and some judge elapsed time.
    //!
    //! # Mutation gates
    //!
    //! * `zero_handshake_deadline_is_refused_before_it_reaches_a_transport`: dropping the guard in
    //!   `HandshakeDeadline::arm` makes the zero arm `Ok` for any transport whose
    //!   `set_read_timeout` tolerates zero, turning the caller's bound into no bound.
    //! * `a_nested_call_that_loses_the_stream_ends_the_session`: drop the `fail_session` call
    //!   on the reply wait's decode failure and the session stays live, and the serve loop reads
    //!   on, the state in which the frame that owns the slot takes the peer's next frame for its
    //!   own. `shutdown` alone does not gate it: what it does to already-received bytes is
    //!   platform- and backend-dependent (Linux keeps the kernel queue, a transport may hold a
    //!   buffered leftover, the default impl is a no-op), which is why the loop's stop comes from
    //!   the emptied pool.
    //! * `a_real_nested_call_that_loses_the_stream_fails_the_outer_call`: with the same mutant
    //!   the handler's reply goes out, and the outer takes the first queued `REPLY` as its own:
    //!   `Ok(Some(_))`, the wire one reply out of step. Two `REPLY`s are queued so that the
    //!   mutant does not hit end of stream and fail for the wrong reason. The gate is
    //!   platform-independent because the mutant never shuts the socket down, so the queue is
    //!   intact on both; the platform split (macOS discards the queue on shutdown, Linux keeps
    //!   it) is on the *correct* path, which is why no end-of-stream reason is asserted.
    //! * `a_failed_send_ends_the_whole_session`: make `end_after_failed_send` return before
    //!   `fail_session` and the fan-out slot keeps the session up after its founding
    //!   connection's send failed.
    //! * `liveness_follows_the_session_values_on_every_slot` and
    //!   `a_send_the_peer_stops_reading_ends_the_session`: drop `arm_liveness_all` from
    //!   `set_timeout` and the recorder sees no new value, and the stalled send never gives up
    //!   (the test's bounded wait fails it); drop `arm_liveness` from a slot push and the added
    //!   slot is never armed.
    //! * `the_kernels_etimedout_under_an_armed_deadline_is_not_an_eviction` gates both halves of
    //!   the `ETIMEDOUT` split. Count `TimedOut` in `transport::is_timeout`, or drop the
    //!   `Io(TimedOut)` arm of `serve_once_on_slot`'s receive, and the loop ends on
    //!   `Frame(TimedOut)` with a deadline armed: an idle eviction, `Local` and `InSync`.
    //! * `a_callback_on_a_serve_slot_waits_under_the_idle_value_by_default`: drop the
    //!   `.or_else(..)` from `client_transact`'s reply deadline and the wait inherits the socket's
    //!   empty deadline (as inside a nested dispatch that lifted it) and never ends; the bounded
    //!   receive fails the test. `a_oneway_handlers_callback_waits_under_the_idle_value_by_default`
    //!   fails the same way when the default reads only the slot the call picked (`restore`), a
    //!   callback slot with no baseline.
    //! * `a_slot_armed_across_a_set_timeout_ends_with_the_stored_value`: drop the lock from
    //!   `arm_liveness` and `set_timeout` finishes while the push still holds its older value.
    //! * `a_set_timeout_during_teardown_leaves_the_shutdown_send_bound`: drop the lock from
    //!   `on_session_dead` and the store re-arms the transport between `shutdown`'s bound and
    //!   its closing write, which then runs with no deadline.
    //! * `a_session_ended_under_a_parked_serve_loop_stops_it`: drop `on_session_dead`'s
    //!   `slot_cv.notify_all` and the parked loop never wakes; the bounded join fails the test.
    //!   `park_hook` fires under the pool lock, so `fail_session` runs only once the loop waits.
    //! * `a_frame_being_sent_is_activity`: drop the `OpenCall` from `send_msg` and the serve
    //!   loop evicts while the oneway's write is parked for three periods; `open` also reads 0.
    //!   The write is parked before the serve loop starts its wait (`serve_wait_hook`), so each
    //!   expiry finds it open, and the parked transport holds the frame: the only timing is the
    //!   stall's length, a period longer than the mutant needs.
    //! * `a_frame_being_received_is_activity`: drop the `io_gen` bump from either android-13+
    //!   reader in `recv_msg` (`CountedIo`, the fd closure) and `active_since` finds nothing
    //!   while the transport is parked after the header's first byte with no call open. The
    //!   parked transport holds the frame there, so no timing decides the verdict.
    //! * `an_idle_session_ends_between_d_and_2d_after_its_last_activity`: drop the `io_gen` bump
    //!   from `OpenCall::drop` ("call end"), from `add_incoming_slot_capped` ("serve attach") or
    //!   after `add_callback_slot_and_init`'s `"cci"` ("callback attach"), or have
    //!   `active_since` ignore the count, and the serve slot's first expiry evicts half a period
    //!   after the activity; drop the send's `OpenCall` and "callback frame" does the same.
    //!   Drop `*seen = now` from `active_since` and one move reads as moved at every
    //!   later expiry: the session is never evicted and the bounded receive fails. The half
    //!   period is timed from `serve_wait_hook`, so a late serve thread moves neither bound.
    //! * `a_frame_trickled_across_the_idle_period_is_not_idle` and
    //!   `frames_on_another_slot_keep_a_quiet_one_up`: drop the bump from `CountedIo` and the
    //!   quiet slot evicts at its first expiry, while the bytes are still arriving (a
    //!   `DEC_STRONG` opens no call, so bytes are the only activity). Their last assertion is the
    //!   other half: once the peer is quiet the session ends.
    //! * `a_sender_racing_a_failed_callback_init_never_picks_its_slot`: release the claim and
    //!   notify before `SlotClaim::retire` (in `add_callback_slot_and_init`'s failure arm, ahead
    //!   of `cci_failed_hook`) and the racer the hook runs picks the dying slot, its oneway fails
    //!   with `EPIPE` on the gone peer (a Unix socketpair), and the session ends. The hook joins
    //!   the racer inside the window, so no timing decides the verdict.
    //! * `an_incoming_connection_whose_thread_fails_to_spawn_ends_the_session`: un-push the slot
    //!   instead of `fail_session` on the spawn failure and the session stays up while the
    //!   server holds that connection as a callback slot.
    //! * `an_incoming_attach_below_the_session_version_never_connects`: move the version check
    //!   after the connect and the connector panics.
    //!   `an_incoming_attach_on_another_transport_kind_is_refused_before_its_header`: drop the
    //!   `admits` check ahead of the header and the header goes out.
    //!   `an_incoming_attach_whose_deadline_expires_before_cci_ends_the_session`:
    //!   count `Timeout` as a close in `fail_awaiting_cci` and it stays up.
    //!   `an_incoming_attach_cut_inside_cci_ends_the_session`: exempt `Truncated` outside the
    //!   `received == 0` test and its EOF case stays up; drop the `received == 0` test and its
    //!   no-`close_notify` case stays up.
    //!   `an_incoming_attach_closed_before_cci_leaves_the_session_up` and
    //!   `an_incoming_attach_reset_before_cci_leaves_the_session_up` are the other half: drop
    //!   `EndOfStream` from the close arm and the session ends;
    //!   `an_incoming_attach_uncleanly_closed_before_cci_leaves_the_session_up` does the same for
    //!   `UncleanEndOfStream`.
    //! * The outgoing attach, one test per class.
    //!   `an_outgoing_attach_failing_past_its_header_ends_the_session`:
    //!   return the error without `end_past_outgoing_header` on the `"cci"` write or on the
    //!   probe's deadline arm and its case of that name stays up; so does its expiry case once
    //!   the probe's failure skips it.
    //!   `an_outgoing_attach_closed_before_the_reply_ends_the_session`:
    //!   exempt a close (`EndOfStream`, `UncleanEndOfStream`) before any reply byte, at the read
    //!   (EOF, no-`close_notify` cases), at the `"cci"` write or at the probe's write, and that
    //!   case stays up. `an_outgoing_attach_cut_inside_the_reply_ends_the_session` and
    //!   `an_outgoing_attach_refused_after_the_reply_ends_the_session`: return the probe's error
    //!   as is and the session stays up. The other half:
    //!   `an_outgoing_attach_whose_header_write_fails_leaves_the_session_up` ends the session
    //!   once the header write goes through `end_past_outgoing_header`;
    //!   `an_outgoing_attach_below_the_session_version_never_connects` and
    //!   `an_outgoing_attach_echoing_the_client_local_id_never_connects` connect once their
    //!   check moves after the connect;
    //!   `an_outgoing_attach_on_another_transport_kind_is_refused_before_its_header`
    //!   sends its header once the `admits` check ahead of it is dropped.
    //! * `a_proxy_drop_on_a_full_socket_waits_for_nothing` and
    //!   `a_release_read_in_a_reply_wait_on_a_full_socket_waits_for_nothing`: have
    //!   `write_dec_no_wait` fall back to `send_msg` and the drop waits for the send deadline.
    //! * `the_reaper_finishes_what_a_no_wait_send_left`: queue no `ReaperJob::Carried` and the rest
    //!   of the frame stays in the transport.
    //! * `a_serve_loop_finishes_what_a_no_wait_send_left_before_it_reads`: drop the flush from
    //!   `write_before_serve_read` and the read has part of a frame left; drop its `OpenCall` and
    //!   the write to a slow reader is not activity.
    //! * `a_serve_loop_writes_what_its_outgoing_slot_holds_before_it_reads`: drop `!s.serving` from
    //!   `ConnGuard::drop` and the hold goes to the reaper, which a loop in its read keeps waiting.
    //!   `a_serve_loops_write_reads_while_it_waits_for_room`: write with no drain and both ends
    //!   only write until the send deadline.
    //! * `a_serve_loops_failed_write_ends_it_as_a_failed_reply_does`: ignore what
    //!   `write_before_serve_read` returns and the read reports the session's own shutdown as a
    //!   clean end. `a_serve_loops_failed_write_past_the_limit_ends_it_too`: the same for
    //!   `write_held_past_limit`. `a_reply_wait_fails_on_its_failed_write_past_the_limit`: the same
    //!   in `client_transact`, and the call fails `DeadObject`.
    //!   `a_serve_loops_write_to_a_closed_peer_leaves_the_end_to_its_read`: drop `EndOfStream` from
    //!   `held_write_end` and a plain close is a lost stream.
    //!   `a_serve_loops_write_to_a_closed_peer_runs_nothing_the_peer_left`: read on as usual, and a
    //!   left frame runs on the ended session. `a_serve_loops_write_to_a_closed_peer_sees_a_cut`:
    //!   end with no read, and a frame the peer cut reads as a clean close.
    //! * `a_no_wait_write_to_a_closed_peer_runs_nothing_the_peer_left` and
    //!   `a_no_wait_writes_failure_ends_a_serve_loop_as_a_holds_does`: record nothing in
    //!   `end_no_wait_write` and the loop reads on.
    //!   `a_no_wait_write_to_a_closed_peer_ends_its_frames_step`: drop the check at the end of
    //!   `serve_once_on_slot` and the next step may find no slot.
    //!   `a_no_wait_writes_failure_fails_a_reply_wait_with_its_status`: drop the check of
    //!   `write_held_past_limit` and the wait reads the shutdown, `DeadObject`.
    //!   `a_close_found_between_frames_ends_a_loop_the_same_past_the_reaper` and
    //!   `a_failure_found_between_frames_ends_a_loop_the_same_past_the_reaper`: drop the check of
    //!   `serve_once_on_slot`'s `find_conn_pinned` and the loop reads `SessionEnded`.
    //!   `a_release_failed_by_an_end_under_way_leaves_a_loop_session_ended`: record past `Live` in
    //!   `end_no_wait_write` and another connection's fault reads as this peer's clean close.
    //! * `a_reply_wait_runs_nothing_it_reads_past_live` and
    //!   `a_serve_loop_runs_nothing_it_reads_past_live`: drop the lifecycle check from
    //!   `dispatch_transact` and the handler runs on the ended session.
    //! * `a_reply_whose_release_finds_the_peer_closed_fails_dead_object`: have `validate_objects`
    //!   return `BadValue` for every failure, and a session's end reads as a malformed parcel.
    //!   `a_serve_loops_drain_refusal_keeps_its_status`: drop `fault` from the failure's value.
    //! * `the_reaper_waits_for_no_slot_to_finish_a_frame`: have `flush_carried` wait for a slot's
    //!   holder. `the_reaper_waits_for_no_slot_to_judge_its_room`: the same in `await_room`.
    //!   `a_slot_given_back_with_a_frame_left_queues_the_reaper`: drop the `queue_carried` from
    //!   `ConnGuard::drop` and the frame stays in the transport.
    //! * `a_failed_release_runs_no_recipient_on_the_dropping_thread_*`: have `end_on_the_reaper`
    //!   call `fail_session` and the recipient runs on the dropping thread.
    //! * `a_slot_with_no_room_is_judged_as_a_waiting_send_is`: drop the `AwaitRoom` send and no
    //!   judgement comes; replace `__wait_for_room`'s `SendWait` with a sleep of the deadline and a
    //!   peer that takes frames loses its session.
    //! * `a_pin_past_its_limit_with_no_room_holds_and_the_serve_loop_writes_the_excess`: drop
    //!   `write_held_past_limit` from `serve_once_on_slot` and the hold stays past its limit.
    //!   `a_reply_wait_writes_what_its_slot_holds_past_the_limit`: the same in `client_transact`.
    //! * `nothing_is_written_after_a_frame_cut_with_no_death_sequence_on_this_thread`: have
    //!   `stop_for_the_reaper` only queue `End` and the job ahead of it writes after the cut.
    //! * `a_reaper_waiting_to_write_keeps_no_session_alive`,
    //!   `a_reaper_judging_a_full_socket_keeps_no_session_alive`: keep a clone of `inner` across
    //!   `wait_without_the_session`'s wait and the session outlives its handle; drop
    //!   `RpcSessionInner`'s `Drop` and the reaper keeps the socket, so the peer sees no end.
    //! * `a_thread_that_cut_a_frame_writes_nothing_more_on_its_pin`: drop the lifecycle check of
    //!   `send_msg` and a reply follows the frame this thread cut, on a transport with nothing to
    //!   cut it. `a_stopped_session_sends_nothing_that_does_not_wait`: the same in
    //!   `send_msg_no_wait`.
    //! * `close_session_ends_what_a_stop_with_nothing_to_cut_left_waiting`: return from `close`
    //!   when it lost the edge, and `close_session` stays in its join.
    //! * `a_burst_of_releases_ends_no_session_and_each_address_gets_its_sum`: end the session past
    //!   a length of `reaper_owed` and the burst fails; `insert` over `entry` loses a sum.
    //! * `a_reply_wait_on_an_outgoing_pin_writes_what_the_reaper_owes_past_the_limit`: drop the
    //!   `reaper_owed` arm of `write_held_past_limit`.
    //!   `an_incoming_pin_holds_what_the_reaper_owes_no_more_of`: have `owe_from_pin` always add to
    //!   the reaper. Either way a peer's frames grow it for good.
    //! * `a_oneway_refused_past_a_later_number_ends_the_session`: drop the `fail_session` of
    //!   `client_transact`'s `rollback` and the gap stays.
    //! * `a_reply_past_the_fds_in_flight_fails_the_call_and_leaves_the_session_up`: drop the
    //!   `TOOMANYREFS` arm of `send_stream` and the victim's session ends.
    //! * `an_idle_expiry_on_a_slot_the_session_dropped_is_not_an_eviction`: drop the loop's
    //!   `slot_role` arm ahead of the idle check and the loop reports `Frame(TimedOut)`, `Local`:
    //!   an idle eviction of a session another connection's fault ended. The transport's read
    //!   runs that fault inside the window, between the wait and the expiry.
    //!
    //! # Test notes
    //!
    //! * Nested-loss tests set the `DRIVING` marker (what an outer frame's `find_conn` leaves
    //!   behind) directly and queue the undecodable frame (command 7, none of AOSP's three)
    //!   before the call: no second thread, no timing assumption. Reaching it through a real
    //!   dispatch needs a peer that both calls back and violates the wire; that is AC-21.24's
    //!   scripted peer.
    //! * AC-21.24's peer answers the outer `TRANSACT` with one write of four frames: a nested
    //!   `TRANSACT` into a local object, an undecodable frame, and two `REPLY`s. The handler's
    //!   nested call ends the session and the handler returns `Ok`, the worst case for the
    //!   slot's owner. What stops the owner is slot selection for the reply to the nested
    //!   `TRANSACT` (`find_conn_impl` with `ConnUse::Reply` finds the pool empty).
    //! * `dec_strong_never_scans_a_serve_driven_slot`: a serve loop holds its slot across `recv`,
    //!   so the free-slot window exists only between two of the worker's messages; the unit form
    //!   (no worker running) makes it deterministic.
    //! * `from_preconnected_fd_clears_o_nonblock_before_dispatch` does not observe a stuck read
    //!   (any synthetic EOF hides the EAGAIN). It dups the fd before the call: the dup shares the
    //!   open file description and so its status flags (fcntl(2)).
    use super::*;
    use std::os::fd::{AsFd, OwnedFd};

    /// Auto traits are public API (`api/rsbinder-rpc.txt`); a non-`RefUnwindSafe` field drops two.
    #[test]
    fn rpc_session_keeps_its_auto_traits() {
        fn assert_traits<
            T: Send + Sync + Unpin + std::panic::RefUnwindSafe + std::panic::UnwindSafe,
        >() {
        }
        assert_traits::<RpcSession>();
    }

    /// The peer drops every connection, so both TLS handshakes fail; only TCP connects are tested.
    #[cfg(feature = "rpc-tls")]
    #[test]
    fn tls_connections_stay_on_the_first_resolved_address() {
        use rustls::{ClientConfig, RootCertStore};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for s in listener.incoming().take(2) {
                drop(s);
                let _ = tx.send(());
            }
        });
        let cfg = std::sync::Arc::new(
            ClientConfig::builder()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth(),
        );
        let mut pinned = None;
        let timeout = Some(Duration::from_secs(2));
        let wait = Duration::from_secs(2);

        let first = connect_tls(
            "127.0.0.1",
            addr.port(),
            &mut pinned,
            "localhost",
            &cfg,
            timeout,
        );
        assert!(first.is_err());
        rx.recv_timeout(wait).expect("first TCP connect");
        assert_eq!(pinned, Some(addr));

        let second = connect_tls(
            "rsbinder-pin-test.invalid",
            addr.port(),
            &mut pinned,
            "localhost",
            &cfg,
            timeout,
        );
        assert!(second.is_err());
        rx.recv_timeout(wait)
            .expect("second TCP connect went to the pinned address");
    }

    /// A listener with a full accept queue: Linux drops the SYN, so a connect to it never ends.
    #[cfg(all(feature = "rpc-tls", any(target_os = "linux", target_os = "android")))]
    fn tcp_silent_listener() -> (std::net::TcpListener, Vec<std::net::TcpStream>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        rustix::net::listen(&listener, 0).expect("listen");
        let addr = listener.local_addr().expect("addr");
        let mut queued = Vec::new();
        loop {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                Ok(s) => queued.push(s),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => panic!("filling the accept queue: {e}"),
            }
        }
        (listener, queued)
    }

    /// The connect deadline bounds every resolved address together, not each one.
    #[cfg(all(feature = "rpc-tls", any(target_os = "linux", target_os = "android")))]
    #[test]
    fn a_tls_connect_bounds_all_addresses_by_one_deadline() {
        let (silent, _queued) = tcp_silent_listener();
        let silent = silent.local_addr().expect("addr");
        let d = Duration::from_secs(1);

        let start = Instant::now();
        let e = tcp_connect_any(&[silent, silent], Some(d)).expect_err("nothing answers");
        let took = start.elapsed();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            took < d + d / 2,
            "two silent addresses took {took:?}, deadline {d:?}"
        );

        let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let live = live.local_addr().expect("addr");
        let d = CONNECT_SHARE_FLOOR + CONNECT_SHARE_FLOOR / 2;
        let start = Instant::now();
        let (_, reached) = tcp_connect_any(&[silent, live], Some(d)).expect("the live address");
        let took = start.elapsed();
        assert_eq!(reached, live);
        assert!(
            took >= CONNECT_SHARE_FLOOR * 9 / 10,
            "the silent address got {took:?}, under the floor"
        );
    }

    /// Each try waits at least the floor, or all that is left when that is less.
    #[cfg(feature = "rpc-tls")]
    #[test]
    fn a_connect_share_is_even_above_the_floor() {
        let s = Duration::from_secs;
        assert_eq!(connect_share(s(10), 2), s(5));
        assert_eq!(connect_share(s(10), 10), CONNECT_SHARE_FLOOR);
        assert_eq!(connect_share(s(1), 2), s(1));
        assert_eq!(connect_share(s(3), 1), s(3));
    }

    /// An address refused at once leaves its share of the deadline to the next one.
    #[cfg(all(feature = "rpc-tls", any(target_os = "linux", target_os = "android")))]
    #[test]
    fn a_refused_address_leaves_its_share_to_the_next() {
        let (silent, _queued) = tcp_silent_listener();
        let silent = silent.local_addr().expect("addr");
        let refused = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        // Above the floor, so the silent address's own even share is half of `d`.
        let d = CONNECT_SHARE_FLOOR * 2;

        let start = Instant::now();
        let e = tcp_connect_any(&[refused, silent], Some(d)).expect_err("nothing answers");
        let took = start.elapsed();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            took >= d * 9 / 10,
            "the silent address got {took:?} of {d:?}"
        );
    }

    /// Build a Unix socketpair and return one half as `OwnedFd`.
    fn unix_socketpair_fd() -> (OwnedFd, OwnedFd) {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .expect("socketpair")
    }

    /// A zero handshake deadline is refused in `HandshakeDeadline::arm`, before any transport.
    #[test]
    fn zero_handshake_deadline_is_refused_before_it_reaches_a_transport() {
        let (a, _b) = super::super::transport::MemTransport::pair();
        let refused = HandshakeDeadline::arm(&a, Some(Duration::ZERO)).err();
        assert_eq!(
            refused.map(StatusCode::from),
            Some(StatusCode::BadValue),
            "a zero duration is not a deadline, and every setter says so alike"
        );
        // The two shapes that are deadlines still arm.
        assert!(HandshakeDeadline::arm(&a, None).is_ok());
        assert!(HandshakeDeadline::arm(&a, Some(Duration::from_millis(50))).is_ok());
    }

    /// A transport that refuses socket deadlines, as XNU does once both directions are shut.
    struct RefusesDeadlines {
        closed: bool,
    }

    impl RpcTransport for RefusesDeadlines {
        fn send_raw(&self, _buf: &[u8]) -> RpcResult<()> {
            Err(RpcError::EndOfStream)
        }
        fn recv_raw(&self, _buf: &mut [u8]) -> RpcResult<usize> {
            Err(RpcError::EndOfStream)
        }
        fn peer_identity(&self) -> super::super::transport::PeerIdentity {
            super::super::transport::PeerIdentity::Anonymous
        }
        fn describe(&self) -> &str {
            "refuses-deadlines"
        }
        fn shutdown(&self) -> RpcResult<()> {
            Ok(())
        }
        fn set_read_timeout(&self, _timeout: Option<Duration>) -> RpcResult<()> {
            Err(RpcError::Io(std::io::ErrorKind::InvalidInput.into()))
        }
        fn peer_closed(&self) -> Option<bool> {
            Some(self.closed)
        }
    }

    /// A deadline refused on a closed peer is skipped, as `push_read_deadline` skips it.
    #[test]
    fn a_handshake_deadline_refused_on_a_closed_peer_is_skipped() {
        let d = Some(Duration::from_secs(1));
        assert!(HandshakeDeadline::arm(&RefusesDeadlines { closed: true }, d).is_ok());
        assert!(HandshakeDeadline::arm(&RefusesDeadlines { closed: false }, d).is_err());
    }

    /// A connect's own deadline is `TimedOut` though std's error for it carries no errno.
    #[cfg(any(feature = "rpc-tcp-debug", feature = "rpc-tls"))]
    #[test]
    fn a_connect_deadline_is_timed_out_not_unknown() {
        let own_deadline = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert_eq!(own_deadline.raw_os_error(), None, "the case under test");
        assert_eq!(connect_status(own_deadline), StatusCode::TimedOut);
        let etimedout = rustix::io::Errno::TIMEDOUT.raw_os_error();
        assert_eq!(
            connect_status(std::io::Error::from_raw_os_error(etimedout)),
            StatusCode::TimedOut
        );
        let refused = rustix::io::Errno::CONNREFUSED.raw_os_error();
        assert_eq!(
            connect_status(std::io::Error::from_raw_os_error(refused)),
            StatusCode::from(std::io::Error::from_raw_os_error(refused)),
            "every other failure keeps its errno"
        );
    }

    /// The config entry reports a zero `timeout` as `BadValue`, before any connect.
    #[test]
    fn zero_timeout_is_bad_value_at_the_config_entry() {
        let path = std::path::Path::new("/nonexistent/rsb-zero-timeout.sock");
        assert_eq!(
            RpcSession::setup_client_android13plus_with_config(
                RpcClientConfig::unix(path, 2).timeout(Duration::ZERO),
            )
            .err(),
            Some(StatusCode::BadValue),
            "rejected before the connect — the path is never touched"
        );
    }

    /// A nested call that cannot decode a frame ends the session under the slot's owning frame.
    #[test]
    fn a_nested_call_that_loses_the_stream_ends_the_session() {
        use crate::rpc::wire_android13::write_aosp_message;
        use std::os::unix::net::UnixStream;

        let (client_fd, peer_fd) = unix_socketpair_fd();
        let mut peer = UnixStream::from(peer_fd);
        let session = RpcSession::with_profile(
            Box::new(
                super::super::transport::UnixTransport::from_stream(UnixStream::from(client_fd))
                    .expect("transport"),
            ),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");

        // Queued first; command 7 is none of AOSP's three: well-framed but undecodable.
        let mut undecodable = [0u8; 16];
        undecodable[0..4].copy_from_slice(&7u32.to_le_bytes());
        write_aosp_message(&mut peer, &undecodable).expect("queue the undecodable frame");

        // Reentrant: this thread already drives the only slot, as an outer call leaves it.
        let (slot_id, slot_transport) = {
            let st = session.inner.conn_state.lock().expect("conn_state");
            (st.slots[0].id, Arc::clone(&st.slots[0].transport))
        };
        let sess_ptr = &*session.inner as *const RpcSessionInner as usize;
        // RAII: a failed assertion must not leak a `DRIVING` entry to the next inline test.
        struct DrivingMark;
        impl Drop for DrivingMark {
            fn drop(&mut self) {
                DRIVING.with(|d| {
                    d.borrow_mut().pop();
                });
            }
        }
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let _mark = DrivingMark;

        let err = session
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &Parcel::new(),
                0,
            )
            .expect_err("an undecodable frame fails the nested call");
        assert_eq!(
            err,
            StatusCode::RpcError,
            "the nested call gets its own failure"
        );

        let lifecycle = &session.inner.shared.lifecycle;
        assert!(lifecycle.is_torn_down(), "a lost stream ends the session");
        assert!(
            !session.inner.shared.ended_locally.load(Ordering::SeqCst),
            "a fault, not this end's decision"
        );
        assert!(
            slot_transport.send_raw(b"x").is_err(),
            "the borrowed connection must be shut down, not left usable"
        );
        // The owning frame's next read finds the pool empty instead of the peer's next frame.
        assert!(
            matches!(
                session.inner.serve_once_on_slot(slot_id),
                ServeStep::Ended(EndReason::SessionEnded)
            ),
            "the serve loop must stop, not read another frame on the slot"
        );
    }

    /// A frame read after a local end ends the loop `Interrupted`; a live session decodes it.
    #[test]
    fn a_frame_read_after_a_local_end_is_not_dispatched() {
        use crate::rpc::wire_android13::write_aosp_message;
        use std::os::unix::net::UnixStream;

        let build = || {
            let (client_fd, peer_fd) = unix_socketpair_fd();
            let mut peer = UnixStream::from(peer_fd);
            let session = RpcSession::with_profile(
                Box::new(
                    super::super::transport::UnixTransport::from_stream(UnixStream::from(
                        client_fd,
                    ))
                    .expect("transport"),
                ),
                AddressSpace::Acceptor,
                WireProfile::Android13Plus(
                    Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2"),
                ),
            )
            .expect("session");
            let mut undecodable = [0u8; 16];
            undecodable[0..4].copy_from_slice(&7u32.to_le_bytes());
            write_aosp_message(&mut peer, &undecodable).expect("queue a frame");
            (session, peer)
        };

        let (ended, _peer) = build();
        ended
            .inner
            .shared
            .ended_locally
            .store(true, Ordering::SeqCst);
        assert!(
            matches!(
                ended.inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
                ServeStep::Ended(EndReason::Interrupted)
            ),
            "a frame read after a local end must end the loop, not be dispatched"
        );

        let (live, _peer) = build();
        assert!(
            matches!(
                live.inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
                ServeStep::Ended(EndReason::Frame(_))
            ),
            "the same frame on a live session reaches the decoder"
        );
    }

    /// AC-21.24: a real nested call that loses the stream fails the outer call with `DeadObject`.
    #[test]
    fn a_real_nested_call_that_loses_the_stream_fails_the_outer_call() {
        use crate::rpc::wire_android13::read_aosp_message;
        use crate::{Binder, Interface, Remotable, TransactionCode};
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        const DESC: &str = "rsbinder.test.INestedLoser";
        const TX_OUTER: TransactionCode = crate::FIRST_CALL_TRANSACTION;
        const TX_CALLBACK: TransactionCode = crate::FIRST_CALL_TRANSACTION + 1;
        const TX_NESTED: TransactionCode = crate::FIRST_CALL_TRANSACTION + 2;

        /// The peer's callback target: its handler makes the nested call and swallows its failure.
        struct Loser {
            session: Arc<Mutex<Option<Arc<RpcSessionInner>>>>,
            nested: Arc<Mutex<Option<Result<()>>>>,
        }
        impl Interface for Loser {}
        impl Remotable for Loser {
            fn descriptor() -> &'static str {
                DESC
            }
            fn on_transact(
                &self,
                code: TransactionCode,
                _reader: &mut Parcel,
                _reply: &mut Parcel,
            ) -> Result<()> {
                assert_eq!(code, TX_CALLBACK, "the peer's nested TRANSACT");
                let session = self
                    .session
                    .lock()
                    .expect("session cell")
                    .clone()
                    .expect("session installed before the outer call");
                let r = session
                    .client_transact(RpcAddress::zero(), TX_NESTED, &Parcel::new(), 0)
                    .map(|_| ());
                *self.nested.lock().expect("nested cell") = Some(r);
                Ok(())
            }
            fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
                Ok(())
            }
        }

        let (client_fd, peer_fd) = unix_socketpair_fd();
        let mut peer = UnixStream::from(peer_fd);
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer read timeout");
        // Ends the peer's final read: whether `shutdown` reaches it is the platform's business.
        let peer_dup = peer.try_clone().expect("dup the peer socket");
        let session = RpcSession::with_profile(
            Box::new(
                super::super::transport::UnixTransport::from_stream(UnixStream::from(client_fd))
                    .expect("transport"),
            ),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        // A hang is a failure, not a wait: bound every reply wait.
        session
            .set_reply_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let session_cell = Arc::new(Mutex::new(Some(Arc::clone(&session.inner))));
        let nested_cell: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let cb: SIBinder = Interface::as_binder(&Binder::new(Loser {
            session: Arc::clone(&session_cell),
            nested: Arc::clone(&nested_cell),
        }));
        // An address for `cb`, as sending it in a parcel would mint; the peer scripts it.
        let cb_addr = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&cb)
            .expect("address for the callback");
        let (slot_id, slot_transport) = {
            let st = session.inner.conn_state.lock().expect("conn_state");
            (st.slots[0].id, Arc::clone(&st.slots[0].transport))
        };

        let peer_thread = std::thread::spawn(move || {
            let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
            let frame = read_aosp_message(&mut peer).expect("the outer TRANSACT");
            match codec
                .decode_message(&frame)
                .expect("decode the outer TRANSACT")
            {
                WireMessage::Transact(t) => assert_eq!(t.code, TX_OUTER),
                other => panic!("expected the outer TRANSACT, got {other:?}"),
            }
            let mut token = Parcel::new();
            write_rpc_interface_token(&mut token, DESC).expect("interface token");
            let nested = codec
                .encode_transact(&WireTransaction {
                    address: cb_addr,
                    code: TX_CALLBACK,
                    flags: 0,
                    async_number: 0,
                    data: token.rpc_data_bytes().to_vec(),
                    object_positions: Vec::new(),
                })
                .expect("encode the nested TRANSACT");
            // Command 7 is none of AOSP's three: well-framed, undecodable to the nested wait.
            let mut undecodable = [0u8; 16];
            undecodable[0..4].copy_from_slice(&7u32.to_le_bytes());
            let reply = codec
                .encode_reply(&WireReply::default())
                .expect("encode a REPLY");
            let mut burst = nested;
            burst.extend_from_slice(&undecodable);
            burst.extend_from_slice(&reply);
            burst.extend_from_slice(&reply);
            peer.write_all(&burst).expect("queue the burst");
            peer.flush().expect("flush the burst");
            // Stay connected until the client ends it; the nested request is drained unread.
            let mut sink = Vec::new();
            let _ = peer.read_to_end(&mut sink);
        });

        let outer = session
            .inner
            .client_transact(RpcAddress::zero(), TX_OUTER, &Parcel::new(), 0);
        assert!(
            matches!(outer, Err(StatusCode::DeadObject)),
            "the outer call must fail as DeadObject, got {outer:?}"
        );
        assert!(
            matches!(
                *nested_cell.lock().expect("nested cell"),
                Some(Err(StatusCode::RpcError))
            ),
            "the nested call must have run and failed on the undecodable frame"
        );
        assert!(
            session
                .inner
                .conn_state
                .lock()
                .expect("conn_state")
                .slots
                .is_empty(),
            "the session's end empties the pool, slot {slot_id} with it"
        );
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "the nested call's lost stream ended the session"
        );
        assert!(
            slot_transport.send_raw(b"x").is_err(),
            "the lost connection must be shut down, not left usable"
        );

        session.close_session();
        drop(slot_transport);
        let _ = peer_dup.shutdown(std::net::Shutdown::Both);
        peer_thread.join().expect("peer thread");
    }

    /// A call from another thread while a serve loop holds the only `Outgoing` slot fails at once.
    #[test]
    fn a_call_beside_the_serve_loop_on_the_only_outgoing_slot_does_not_wait() {
        let (a, _b) = super::super::transport::UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        session.set_reply_timeout(None).unwrap();
        let server_tid = std::thread::spawn(current_tid).join().expect("tid thread");
        {
            let mut st = session.inner.conn_state.lock().expect("conn_state");
            let slot = &mut st.slots[0];
            // As `serve_blocking` leaves a client's founding slot while the peer is quiet.
            slot.role = SlotRole::Outgoing;
            slot.exclusive_tid = Some(server_tid);
            slot.serving = true;
        }
        let started = Instant::now();
        let refused = session.inner.find_conn().err();
        assert_eq!(refused, Some(StatusCode::WouldBlock));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no wait on slot_cv"
        );

        // A slot held by a call, not a serve loop, is still waited for (here: bounded).
        session.inner.conn_state.lock().expect("conn_state").slots[0].serving = false;
        session
            .set_reply_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert_eq!(
            session.inner.find_conn().err(),
            Some(StatusCode::WouldBlock)
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the pool-exhausted wait ran"
        );
    }

    /// plans/10-7b-streaming-over-fmq.md §13.3: a twoway still nests, a oneway is `WouldBlock`.
    #[test]
    fn a_oneway_from_a_twoway_dispatch_never_nests_on_the_serving_slot() {
        let (a, _b) = super::super::transport::UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("session");
        let slot_id = {
            let mut st = session.inner.conn_state.lock().expect("conn_state");
            let slot = &mut st.slots[0];
            // As a serve loop dispatching a twoway leaves it.
            slot.role = SlotRole::Incoming;
            slot.allow_nested = true;
            slot.exclusive_tid = Some(current_tid());
            slot.id
        };
        let sess_ptr = &*session.inner as *const RpcSessionInner as usize;
        struct DrivingMark;
        impl Drop for DrivingMark {
            fn drop(&mut self) {
                DRIVING.with(|d| {
                    d.borrow_mut().pop();
                });
            }
        }
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let _mark = DrivingMark;

        let nested = session
            .inner
            .find_conn()
            .expect("a twoway nests on the serving slot");
        assert_eq!(nested.slot_id, slot_id);
        drop(nested);
        assert!(
            matches!(session.inner.find_conn_async(), Err(StatusCode::WouldBlock)),
            "a oneway needs an outgoing slot, and this session has none"
        );
    }

    /// An `AF_UNIX` fd takes the Unix arm; a closed peer fails the handshake cleanly, never hangs.
    #[test]
    fn from_preconnected_fd_unix_dispatches_then_fails_cleanly_on_eof() {
        let (a, b) = unix_socketpair_fd();
        // Closed peer: the handshake's first read of `RpcNewSessionResponse` hits EOF.
        drop(b);
        let err = match RpcSession::from_preconnected_fd(a, 2) {
            Ok(_) => panic!("expected Err on closed peer"),
            Err(e) => e,
        };
        // A peer/io-class status, never a panic or a hang.
        assert!(
            matches!(
                err,
                StatusCode::DeadObject | StatusCode::NotEnoughData | StatusCode::Unknown
            ),
            "unexpected status for closed peer: {err}"
        );
    }

    /// `from_preconnected_fd` clears `O_NONBLOCK` before its first read; a dup'd fd observes it.
    #[test]
    fn from_preconnected_fd_clears_o_nonblock_before_dispatch() {
        use std::os::fd::IntoRawFd;
        let (a, _b) = unix_socketpair_fd();
        // O_NONBLOCK, as AOSP `singleSocketConnection` creates it (SOCK_NONBLOCK).
        let flags = rustix::fs::fcntl_getfl(a.as_fd()).expect("getfl");
        rustix::fs::fcntl_setfl(a.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)
            .expect("setfl NONBLOCK");
        assert!(
            rustix::fs::fcntl_getfl(a.as_fd())
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK),
            "test setup: O_NONBLOCK must be set"
        );
        // Dup first: the shared open-file description makes the bridge's `fcntl_setfl` visible.
        let observer = rustix::io::fcntl_dupfd_cloexec(a.as_fd(), 0).expect("dup");
        // `_b` stays alive, so the bridge blocks in its first read: run it on a thread.
        let bridge_t = std::thread::spawn(move || {
            // Blocks reading the response until `_b` is dropped below.
            let _ = RpcSession::from_preconnected_fd(a, 2);
        });
        // The flag clears before the family dispatch and the read; poll, don't sleep a fixed time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let observed = loop {
            let fl = rustix::fs::fcntl_getfl(observer.as_fd()).expect("getfl observer");
            if !fl.contains(rustix::fs::OFlags::NONBLOCK) || std::time::Instant::now() > deadline {
                break fl;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            !observed.contains(rustix::fs::OFlags::NONBLOCK),
            "from_preconnected_fd did NOT clear O_NONBLOCK — handshake will trip EAGAIN \
             against a non-blocking peer fd from libbinder"
        );
        // Drop _b to unblock the bridge thread, then join.
        drop(_b);
        bridge_t.join().expect("bridge thread");
        // Explicit, so the `IntoRawFd` import stays in use.
        let _ = observer.into_raw_fd();
    }

    /// A non-socket fd fails the `getsockname` family probe cleanly, before any handshake I/O.
    #[test]
    fn from_preconnected_fd_rejects_non_socket_fd() {
        // `/dev/null` is never a socket: `getsockname()` returns ENOTSOCK.
        let fd = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .expect("open /dev/null");
        // Whatever ENOTSOCK maps to, the call must fail cleanly with no wire I/O.
        assert!(
            RpcSession::from_preconnected_fd(fd, 2).is_err(),
            "non-socket fd must reject before any handshake I/O"
        );
    }

    /// A `DEC_STRONG` never scans a free serve-driven slot; see module doc "Connection selection".
    #[test]
    fn dec_strong_never_scans_a_serve_driven_slot() {
        use crate::rpc::transport::MemTransport;
        // Server-side: the founding slot is `Incoming` and, with no worker running, free.
        let (t0, _p0) = MemTransport::pair();
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");
        assert_eq!(session.inner.slot_count(), 1);

        assert!(
            matches!(session.inner.dec_route(), DecRoute::Reaper),
            "a DEC_STRONG must not claim the free serve-driven slot"
        );
        assert!(
            session.inner.find_conn_for_reaper().is_none(),
            "the reaper must skip rather than take it (or park holding the session)"
        );

        // An `Outgoing` (callback) slot is what makes the send possible.
        let (t, _p) = MemTransport::pair();
        session
            .add_callback_slot(Box::new(t), 2)
            .expect("callback slot");
        assert!(
            matches!(session.inner.dec_route(), DecRoute::Conn(_)),
            "with an outgoing slot the DEC_STRONG goes out normally"
        );
    }

    /// A oneway pin holds a bounded number of addresses; past it the `DEC_STRONG` goes out there.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_oneway_pin_holds_a_bounded_number_of_addresses() {
        use super::super::transport::UnixTransport;
        // Server side with no callback connection: a oneway handler's `DEC_STRONG`s are held.
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");
        let inner = &*session.inner;
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let sess_ptr = inner as *const RpcSessionInner as usize;
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let held = || {
            let st = inner.conn_state.lock().expect("conn_state");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| s.pending_dec.len())
        };

        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT {
            inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Initiator), 1);
        }
        assert_eq!(held(), Some(HELD_DEC_STRONG_LIMIT));
        inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Initiator), 1);
        DRIVING.with(|d| d.borrow_mut().pop());

        assert_eq!(
            held(),
            Some(HELD_DEC_STRONG_LIMIT),
            "the extra address is not held"
        );
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("peer timeout");
        let mut buf = [0u8; 64];
        assert!(
            peer.recv_raw(&mut buf).expect("the DEC_STRONG frame") > 0,
            "it went out on the pin"
        );
        assert!(
            !inner.shared.lifecycle.is_torn_down(),
            "the session stays up"
        );
    }

    /// A no-wait send with no promise of a whole frame, as vsock and Apple's `AF_UNIX` have.
    struct NoWholeFrame(super::super::transport::UnixTransport);

    impl RpcTransport for NoWholeFrame {
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            self.0.recv_raw(buf)
        }
        fn __send_no_wait(
            &self,
            buf: &[u8],
            fds: &[std::os::fd::BorrowedFd<'_>],
        ) -> Option<RpcResult<bool>> {
            self.0.__send_no_wait(buf, fds)
        }
        fn shutdown_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
            self.0.shutdown_handle()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            self.0.describe()
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.0.shutdown()
        }
    }

    /// With no whole frame promised, the pin holds past its limit and its reader writes the excess.
    #[test]
    fn a_pin_with_no_whole_frame_holds_past_its_limit_for_its_reader() {
        const EXCESS: usize = 3;
        let (t0, peer) = super::super::transport::UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let session = RpcSession::from_android13plus(
            Box::new(NoWholeFrame(t0)),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");
        let inner = &*session.inner;
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let held = || {
            let st = inner.conn_state.lock().expect("conn_state");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| s.pending_dec.len())
        };
        let sess_ptr = inner as *const RpcSessionInner as usize;
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT + EXCESS {
            inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Initiator), 1);
        }
        DRIVING.with(|d| d.borrow_mut().pop());
        assert_eq!(
            held(),
            Some(HELD_DEC_STRONG_LIMIT + EXCESS),
            "none on the pin"
        );

        let codec = Android13PlusCodec::android14_15();
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let (frame, _) = codec.encode_dec_strong(&addr, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        assert_eq!(held(), Some(HELD_DEC_STRONG_LIMIT), "only the excess");
        let mut got = vec![0u8; EXCESS * frame.len()];
        let mut filled = 0;
        while filled < got.len() {
            let n = peer.recv_raw(&mut got[filled..]).expect("the excess");
            assert!(n > 0, "the session closed the socket");
            filled += n;
        }
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("peer timeout");
        let mut more = [0u8; 1];
        assert!(peer.recv_raw(&mut more).is_err(), "more than the excess");
    }

    /// What fills a socket: frames this size go in until a send that does not wait finds no room.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const FILLER: usize = 256;

    /// Fill the session's one socket; the bytes the peer has to read before anything else.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn fill_socket(session: &RpcSession) -> usize {
        let transports = session.inner.slot_transports();
        let filler = [0u8; FILLER];
        let mut filled = 0;
        while transports[0]
            .__send_no_wait(&filler, &[])
            .expect("a Unix socket")
            .expect("a send")
        {
            filled += FILLER;
        }
        filled
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn full_r34_client() -> (RpcSession, crate::rpc::transport::UnixTransport, usize) {
        let (a, peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        // The session-id preamble went first.
        let filled = 4 + fill_socket(&session);
        (session, peer, filled)
    }

    /// `len` bytes from the peer's end, whatever the reads they take.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read_bytes(peer: &dyn RpcTransport, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        let mut read = 0;
        while read < len {
            let n = peer.recv_raw(&mut bytes[read..]).expect("the peer's read");
            assert!(n > 0, "the end of stream after {read} of {len} bytes");
            read += n;
        }
        bytes
    }

    /// Past the filler, the next frame the peer reads is this `DEC_STRONG`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn expect_r34_dec_strong(peer: &dyn RpcTransport, filled: usize, addr: RpcAddress) {
        read_bytes(peer, filled);
        let (frame, _) = R34Codec.encode_dec_strong(&addr, 1).expect("a frame");
        let read = read_bytes(peer, frame.len());
        assert_eq!(read, frame, "the DEC_STRONG");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_address(serial: u64) -> RpcAddress {
        let mut counter = serial;
        RpcAddress::unique(&mut counter, AddressSpace::Acceptor)
    }

    /// A last drop writes with no wait; on no room the reaper does, once the peer reads.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_proxy_drop_on_a_full_socket_waits_for_nothing() {
        let (session, peer, filled) = full_r34_client();
        session
            .set_reply_timeout(Some(Duration::from_secs(60)))
            .expect("deadline");
        let addr = peer_address(1);
        let proxy = RpcProxy::new(addr, session.inner.clone());
        let (dropped, returned) = mpsc::channel();
        std::thread::spawn(move || {
            drop(proxy);
            let _ = dropped.send(());
        });
        let returned = returned.recv_timeout(Duration::from_secs(1));
        if returned.is_err() {
            session.close_session();
        }
        assert!(returned.is_ok(), "the drop waited for the peer");
        assert!(!session.inner.shared.lifecycle.is_torn_down());
        expect_r34_dec_strong(&peer, filled, addr);
    }

    /// The same inside a reply wait, whose slot only the reply gives back.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_release_read_in_a_reply_wait_on_a_full_socket_waits_for_nothing() {
        let (session, peer, filled) = full_r34_client();
        session
            .set_reply_timeout(Some(Duration::from_secs(60)))
            .expect("deadline");
        let addr = peer_address(2);
        {
            let mut st = session.inner.shared.state.lock().expect("state");
            st.on_proxy_leaving(addr);
            assert_eq!(st.release_proxy(&addr, std::ptr::null()), 0, "it is held");
        }
        let inner = session.inner.clone();
        let (entered, returned) = mpsc::channel();
        let (reply, replied) = mpsc::channel::<()>();
        let waiting = std::thread::spawn(move || {
            let conn = inner.find_conn().expect("the slot");
            inner.apply_dec_strong(addr, 1);
            let _ = entered.send(());
            let _ = replied.recv();
            drop(conn);
        });
        let returned = returned.recv_timeout(Duration::from_secs(1));
        if returned.is_err() {
            session.close_session();
        }
        assert!(returned.is_ok(), "the release waited in the reply wait");
        drop(reply);
        waiting.join().expect("the wait");
        expect_r34_dec_strong(&peer, filled, addr);
    }

    /// Records the thread a death recipient runs on.
    #[derive(Default)]
    struct RanOn(Mutex<Option<std::thread::ThreadId>>);

    impl crate::binder::DeathRecipient for RanOn {
        fn binder_died(&self, _who: &crate::binder::WIBinder) {
            *self.0.lock().expect("lock") = Some(std::thread::current().id());
        }
    }

    /// `peer` closed, a drop's write fails: the recipients of `transport`'s session run elsewhere.
    fn a_failed_release_runs_no_recipient_here(
        transport: Box<dyn RpcTransport>,
        peer: Box<dyn RpcTransport>,
    ) {
        let session = RpcSession::new(transport, AddressSpace::Initiator).expect("session");
        session
            .inner
            .shared
            .serve_declared
            .store(1, Ordering::SeqCst);
        let mut counter = 7;
        let watched = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let inner = session.inner.clone();
        let (kept, _) = session
            .inner
            .shared
            .state
            .lock()
            .expect("state")
            .remote_proxy(watched, || {
                SIBinder::new(Arc::new(RpcProxy::new(watched, inner))).expect("proxy")
            })
            .expect("a proxy");
        let ran = Arc::new(RanOn::default());
        let weak: Weak<RanOn> = Arc::downgrade(&ran);
        kept.link_to_death(weak).expect("a link");

        drop(peer);
        let released = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        drop(RpcProxy::new(released, session.inner.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while ran.0.lock().expect("lock").is_none() {
            assert!(Instant::now() < deadline, "the session did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
        let ran_on = ran.0.lock().expect("lock").take();
        assert_ne!(ran_on, Some(std::thread::current().id()));
    }

    #[test]
    fn a_failed_release_runs_no_recipient_on_the_dropping_thread_unix() {
        use crate::rpc::transport::UnixTransport;
        let (a, peer) = UnixTransport::pair().expect("socketpair");
        a_failed_release_runs_no_recipient_here(Box::new(a), Box::new(peer));
    }

    #[test]
    fn a_failed_release_runs_no_recipient_on_the_dropping_thread_mem_transport() {
        use crate::rpc::transport::MemTransport;
        let (a, peer) = MemTransport::pair();
        a_failed_release_runs_no_recipient_here(Box::new(a), Box::new(peer));
    }

    /// A transport with no send that does not wait leaves its write to the reaper thread.
    #[test]
    fn a_drop_on_a_transport_with_no_such_send_leaves_the_write_to_the_reaper() {
        struct WroteOn(Arc<Mutex<Option<std::thread::ThreadId>>>);
        impl RpcTransport for WroteOn {
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                *self.0.lock().expect("lock") = Some(std::thread::current().id());
                Ok(())
            }
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "wrote-on"
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
        }
        let wrote_on = Arc::new(Mutex::new(None));
        let transport = Box::new(WroteOn(wrote_on.clone()));
        let session = RpcSession::new(transport, AddressSpace::Initiator).expect("session");
        *wrote_on.lock().expect("lock") = None;
        let mut counter = 13;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        drop(RpcProxy::new(addr, session.inner.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while wrote_on.lock().expect("lock").is_none() {
            assert!(Instant::now() < deadline, "no write");
            std::thread::sleep(Duration::from_millis(5));
        }
        let wrote_on = wrote_on.lock().expect("lock").take();
        assert_ne!(wrote_on, Some(std::thread::current().id()));
    }

    /// A transport whose no-wait send leaves bytes, and that says who wrote them.
    #[derive(Clone, Default)]
    struct Carries {
        carrying: Arc<AtomicBool>,
        flushed_on: Arc<Mutex<Option<std::thread::ThreadId>>>,
        /// The session, for what its `open` says during a flush.
        shared: Arc<Mutex<Option<Arc<SharedSession>>>>,
        open_in_flush: Arc<AtomicBool>,
        carrying_in_read: Arc<AtomicBool>,
    }

    impl RpcTransport for Carries {
        fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
            Ok(())
        }
        fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
            let carrying = self.carrying.load(Ordering::SeqCst);
            self.carrying_in_read.store(carrying, Ordering::SeqCst);
            Err(RpcError::EndOfStream)
        }
        fn __send_no_wait(
            &self,
            _: &[u8],
            _: &[std::os::fd::BorrowedFd<'_>],
        ) -> Option<RpcResult<bool>> {
            self.carrying.store(true, Ordering::SeqCst);
            Some(Ok(true))
        }
        fn __carrying(&self) -> bool {
            self.carrying.load(Ordering::SeqCst)
        }
        fn __whole_small_frame_now(&self) -> bool {
            true
        }
        fn __flush_carried(&self, _: Option<&mut dyn FnMut() -> RpcResult<()>>) -> RpcResult<()> {
            if let Some(shared) = &*self.shared.lock().expect("lock") {
                let open = shared.open.load(Ordering::SeqCst) > 0;
                self.open_in_flush.store(open, Ordering::SeqCst);
            }
            self.carrying.store(false, Ordering::SeqCst);
            *self.flushed_on.lock().expect("lock") = Some(std::thread::current().id());
            Ok(())
        }
        fn peer_identity(&self) -> PeerIdentity {
            PeerIdentity::Anonymous
        }
        fn describe(&self) -> &str {
            "carries"
        }
        fn shutdown(&self) -> RpcResult<()> {
            Ok(())
        }
    }

    /// With nothing to write next, the reaper finishes a frame a no-wait send left.
    #[test]
    fn the_reaper_finishes_what_a_no_wait_send_left() {
        let transport = Carries::default();
        let session =
            RpcSession::new(Box::new(transport.clone()), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        let mut counter = 37;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        inner.send_dec_strong(addr, 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while transport.flushed_on.lock().expect("lock").is_none() {
            assert!(Instant::now() < deadline, "no flush");
            std::thread::sleep(Duration::from_millis(1));
        }
        let flushed_on = transport.flushed_on.lock().expect("lock").take();
        assert_ne!(flushed_on, Some(std::thread::current().id()));
        assert!(!inner.shared.lifecycle.is_torn_down());
    }

    /// A serve loop reads with no part of a frame left, and the write to a slow reader is activity.
    #[test]
    fn a_serve_loop_finishes_what_a_no_wait_send_left_before_it_reads() {
        let transport = Carries::default();
        let session =
            RpcSession::new(Box::new(transport.clone()), AddressSpace::Initiator).expect("session");
        *transport.shared.lock().expect("lock") = Some(session.inner.shared.clone());
        transport.carrying.store(true, Ordering::SeqCst);
        let _ = session.serve_blocking();
        assert!(transport.open_in_flush.load(Ordering::SeqCst));
        assert!(!transport.carrying_in_read.load(Ordering::SeqCst));
    }

    /// A transport with no send that does not wait, as a custom one starts out; tells of its reads.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    struct WaitingSendsOnly(crate::rpc::transport::UnixTransport, mpsc::Sender<()>);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl RpcTransport for WaitingSendsOnly {
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            let _ = self.1.send(());
            self.0.recv_raw(buf)
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            self.0.describe()
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.0.shutdown()
        }
    }

    /// A hold on the `Outgoing` slot a serve loop keeps goes out before the loop's next read.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loop_writes_what_its_outgoing_slot_holds_before_it_reads() {
        let (a, peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(3)))
            .expect("peer timeout");
        let (read_began, reading) = mpsc::channel();
        let session = RpcSession::with_profile(
            Box::new(WaitingSendsOnly(a, read_began)),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let inner = session.inner.clone();
        let serving = session.spawn_serve().expect("the loop");
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        // Past this round's write: only the next round has it to write.
        reading
            .recv_timeout(Duration::from_secs(10))
            .expect("the loop's read");
        inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the loop");
        let (paid, _) = codec.encode_dec_strong(&held, 1).expect("a frame");
        assert_eq!(read_bytes(&peer, paid.len()), paid);
        session.close_session();
        let _ = serving.join().expect("the loop");
    }

    /// A serve loop's write of its slot's hold reads meanwhile: the peer writes before it reads.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_write_reads_while_it_waits_for_room() {
        use std::io::{Read, Write};
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        session
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .expect("deadline");
        let inner = session.inner.clone();
        let filled = fill_socket(&session);
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        // Past every buffer: this write ends only if the loop reads.
        let flood = frame.repeat(4 * 1024 * 1024 / frame.len());
        let serving = session.spawn_serve().expect("the loop");
        peer.write_all(&flood).expect("the flood");
        let mut filler = vec![0u8; filled];
        peer.read_exact(&mut filler)
            .expect("what filled the socket");
        let first = raw_peer_reads(&mut peer);
        assert!(matches!(first, WireMessage::DecStrong(a, 1) if a == held));
        assert!(!inner.shared.lifecycle.is_torn_down());
        session.close_session();
        let _ = serving.join().expect("the loop");
    }

    /// A failed write of a hold ends a serve loop as a failed `REPLY` does: a lost stream.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_failed_write_ends_it_as_a_failed_reply_does() {
        use crate::rpc::StreamState;
        let (session, _peer) = a13_client_with_raw_peer();
        session
            .set_reply_timeout(Some(Duration::from_millis(300)))
            .expect("deadline");
        fill_socket(&session);
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        session
            .inner
            .hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::Dispatch(StatusCode::TimedOut));
        assert_eq!(end.stream, StreamState::Lost);
        assert_eq!(end.into_result(), Err(StatusCode::DeadObject));
    }

    /// The write of what is past the limit, on a slot whose hold waits for a reply, ends it too.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_failed_write_past_the_limit_ends_it_too() {
        let (t0, _peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        session
            .set_reply_timeout(Some(Duration::from_millis(300)))
            .expect("deadline");
        fill_socket(&session);
        let inner = session.inner.clone();
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT + 1 {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.hold_dec_strong(slot_id, addr, 1);
        }
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Ended(EndReason::Dispatch(StatusCode::TimedOut))
        ));
    }

    /// The peer's close fails a hold's write and the read reports it: a plain end, as with no hold.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_write_to_a_closed_peer_leaves_the_end_to_its_read() {
        let (session, peer) = a13_client_with_raw_peer();
        drop(peer);
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        session
            .inner
            .hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::EndOfStream);
        assert_eq!(end.into_result(), Ok(()));
    }

    /// What the peer wrote before that close does not change the end: nothing of it runs.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_write_to_a_closed_peer_runs_nothing_the_peer_left() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let (release, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        let call = codec
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: SpecialTransaction::GetMaxThreads.code(),
                flags: 0,
                async_number: 0,
                data: Vec::new(),
                object_positions: Vec::new(),
            })
            .expect("encode");
        std::io::Write::write_all(&mut peer, &release).expect("a release");
        std::io::Write::write_all(&mut peer, &call).expect("a call");
        drop(peer);
        session
            .inner
            .hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::EndOfStream);
        assert_eq!(end.into_result(), Ok(()));
    }

    /// Past those frames the read still tells a cut from a close: that is why it reads at all.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_write_to_a_closed_peer_sees_a_cut() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        std::io::Write::write_all(&mut peer, &a13_header(0, 1000)).expect("a cut frame");
        drop(peer);
        session
            .inner
            .hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let end = session.serve_blocking();
        assert_eq!(end.into_result(), Err(StatusCode::DeadObject));
    }

    /// A oneway to a local node: this end writes its receipt, with no wait, as the frame ends.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_oneway_to_a_local_node(session: &RpcSession) -> Vec<u8> {
        use crate::{Binder, Interface, Remotable};
        let target: SIBinder = Binder::new(TargetOfACall(Mutex::new(None))).as_binder();
        let address = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&target)
            .expect("an address");
        let mut token = Parcel::new();
        write_rpc_interface_token(&mut token, TargetOfACall::descriptor()).expect("token");
        Android13PlusCodec::with_version(PROTOCOL_V2)
            .expect("v2")
            .encode_transact(&WireTransaction {
                address,
                code: crate::FIRST_CALL_TRANSACTION,
                flags: FLAG_ONEWAY,
                async_number: 0,
                data: token.rpc_data_bytes().to_vec(),
                object_positions: Vec::new(),
            })
            .expect("encode")
    }

    /// The step that made the write ends the loop, so no later one finds the reaper's empty pool.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_no_wait_write_to_a_closed_peer_ends_its_frames_step() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let oneway = a_oneway_to_a_local_node(&session);
        std::io::Write::write_all(&mut peer, &oneway).expect("a oneway");
        drop(peer);
        assert!(matches!(
            session
                .inner
                .serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Ended(EndReason::EndOfStream)
        ));
    }

    /// As on a hold's write: a plain end, and what the peer left past the oneway does not run.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_no_wait_write_to_a_closed_peer_runs_nothing_the_peer_left() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let oneway = a_oneway_to_a_local_node(&session);
        let call = codec
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: SpecialTransaction::GetMaxThreads.code(),
                flags: 0,
                async_number: 0,
                data: Vec::new(),
                object_positions: Vec::new(),
            })
            .expect("encode");
        std::io::Write::write_all(&mut peer, &oneway).expect("a oneway");
        std::io::Write::write_all(&mut peer, &call).expect("a call");
        drop(peer);
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::EndOfStream);
        assert_eq!(end.into_result(), Ok(()));
    }

    /// A `UnixTransport` whose send that does not wait fails as one past a send deadline does.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    struct NoWaitExpires(crate::rpc::transport::UnixTransport);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl RpcTransport for NoWaitExpires {
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            self.0.recv_raw(buf)
        }
        fn __send_no_wait(
            &self,
            _: &[u8],
            _: &[std::os::fd::BorrowedFd<'_>],
        ) -> Option<RpcResult<bool>> {
            Some(Err(RpcError::Io(std::io::ErrorKind::WouldBlock.into())))
        }
        fn __whole_small_frame_now(&self) -> bool {
            true
        }
        fn shutdown_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
            self.0.shutdown_handle()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            self.0.describe()
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.0.shutdown()
        }
    }

    /// An android-13+ client on `NoWaitExpires` and the peer's end of its socket.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a13_client_whose_no_wait_send_expires() -> (RpcSession, crate::rpc::transport::UnixTransport)
    {
        let (a, peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let session = RpcSession::with_profile(
            Box::new(NoWaitExpires(a)),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        (session, peer)
    }

    /// Any other failure of that write ends the loop with its status and no read, as a hold's does.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_no_wait_writes_failure_ends_a_serve_loop_as_a_holds_does() {
        use crate::rpc::StreamState;
        let (session, peer) = a13_client_whose_no_wait_send_expires();
        peer.send_raw(&a_oneway_to_a_local_node(&session))
            .expect("a oneway");
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::Dispatch(StatusCode::TimedOut));
        assert_eq!(end.stream, StreamState::Lost);
    }

    /// A reply wait's nested frame whose release fails fails the call with that write's status.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_no_wait_writes_failure_fails_a_reply_wait_with_its_status() {
        let (session, peer) = a13_client_whose_no_wait_send_expires();
        let inner = session.inner.clone();
        let oneway = a_oneway_to_a_local_node(&session);
        let caller = std::thread::spawn(move || {
            let code = SpecialTransaction::GetSessionId.code();
            inner
                .client_transact(RpcAddress::zero(), code, &Parcel::new(), 0)
                .map(|_| ())
        });
        // Its header: the call has gone out, and its reply wait is what reads next.
        read_bytes(&peer, 16);
        peer.send_raw(&oneway).expect("a oneway");
        assert_eq!(
            caller.join().expect("the caller"),
            Err(StatusCode::TimedOut)
        );
    }

    /// Until the reaper's death sequence has emptied the pool.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn await_empty_pool(inner: &RpcSessionInner) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !inner
            .conn_state
            .lock()
            .expect("conn_state")
            .slots
            .is_empty()
        {
            assert!(std::time::Instant::now() < deadline, "the session went on");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Another thread's release finds the close between a loop's frames: no order with the reaper.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_close_found_between_frames_ends_a_loop_the_same_past_the_reaper() {
        let (session, peer) = a13_client_with_raw_peer();
        drop(peer);
        let inner = &session.inner;
        let mut counter = 0;
        inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1);
        await_empty_pool(inner);
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Ended(EndReason::EndOfStream)
        ));
    }

    /// Any other failure of that release: the loop ends with its status, as on its own write.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_failure_found_between_frames_ends_a_loop_the_same_past_the_reaper() {
        let (session, _peer) = a13_client_whose_no_wait_send_expires();
        let inner = &session.inner;
        let mut counter = 0;
        inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1);
        await_empty_pool(inner);
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Ended(EndReason::Dispatch(StatusCode::TimedOut))
        ));
    }

    /// A release that an end already under way failed is no close of the peer's on that slot.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_release_failed_by_an_end_under_way_leaves_a_loop_session_ended() {
        let (session, _peer) = a13_client_with_raw_peer();
        let inner = &session.inner;
        let conn = inner
            .find_conn_pinned(RpcSession::FOUNDING_SLOT_ID)
            .expect("the slot");
        assert!(inner.shared.lifecycle.force_dying());
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        assert_eq!(inner.write_dec_no_wait(&conn, addr, 1), 0);
        drop(conn);
        inner.on_session_dead();
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Ended(EndReason::SessionEnded)
        ));
    }

    /// A node that counts its handler's runs.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    struct CountsRuns(Arc<std::sync::atomic::AtomicUsize>);

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl crate::Interface for CountsRuns {}

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl crate::Remotable for CountsRuns {
        fn descriptor() -> &'static str {
            "rsbinder.test.ICountsRuns"
        }
        fn on_transact(
            &self,
            _code: crate::TransactionCode,
            _reader: &mut Parcel,
            _reply: &mut Parcel,
        ) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
            Ok(())
        }
    }

    /// A twoway call to a `CountsRuns` of `session`, and that node's count.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_call_to_a_counting_node(
        session: &RpcSession,
    ) -> (Vec<u8>, Arc<std::sync::atomic::AtomicUsize>) {
        use crate::{Binder, Interface, Remotable};
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let target: SIBinder = Binder::new(CountsRuns(Arc::clone(&runs))).as_binder();
        let address = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&target)
            .expect("an address");
        let mut token = Parcel::new();
        write_rpc_interface_token(&mut token, CountsRuns::descriptor()).expect("token");
        let call = Android13PlusCodec::with_version(PROTOCOL_V2)
            .expect("v2")
            .encode_transact(&WireTransaction {
                address,
                code: crate::FIRST_CALL_TRANSACTION,
                flags: 0,
                async_number: 0,
                data: token.rpc_data_bytes().to_vec(),
                object_positions: Vec::new(),
            })
            .expect("encode");
        (call, runs)
    }

    /// A frame read past `Live` in a reply wait: stopped by hand, so no reaper can clear its node.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_reply_wait_runs_nothing_it_reads_past_live() {
        use crate::rpc::wire_android13::read_aosp_message;
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let (call, runs) = a_call_to_a_counting_node(&session);
        let inner = session.inner.clone();
        let caller = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let code = SpecialTransaction::GetSessionId.code();
                inner
                    .client_transact(RpcAddress::zero(), code, &Parcel::new(), 0)
                    .map(|_| ())
            })
        };
        read_aosp_message(&mut peer).expect("the request");
        assert!(inner.shared.lifecycle.force_dying());
        std::io::Write::write_all(&mut peer, &call).expect("a call");
        drop(peer);
        assert_eq!(
            caller.join().expect("the caller"),
            Err(StatusCode::DeadObject)
        );
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        inner.on_session_dead();
    }

    /// The same in a serve loop whose read was under way as the session ended.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loop_runs_nothing_it_reads_past_live() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let (call, runs) = a_call_to_a_counting_node(&session);
        let inner = &session.inner;
        assert!(inner.shared.lifecycle.force_dying());
        std::io::Write::write_all(&mut peer, &call).expect("a call");
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Continue
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        inner.on_session_dead();
    }

    /// A failed release ends the session mid-table: the next binder reads what ended it, as AOSP.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_reply_whose_release_finds_the_peer_closed_fails_dead_object() {
        use crate::rpc::wire_android13::write_aosp_message;
        use crate::{Binder, Interface};
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let local: SIBinder = Binder::new(TargetOfACall(Mutex::new(None))).as_binder();
        let home = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&local)
            .expect("an address");
        let mut counter = 0;
        let remote = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let mut data = Parcel::new();
        data.write(&1i32).expect("present");
        session
            .inner
            .wire_write_binder_addr(&mut data, &home)
            .expect("address");
        data.write(&0i32).expect("stability");
        let second = data.data_size() as u32;
        data.write(&1i32).expect("present");
        session
            .inner
            .wire_write_binder_addr(&mut data, &remote)
            .expect("address");
        data.write(&0i32).expect("stability");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let reply = codec
            .encode_reply(&WireReply {
                status: 0,
                data: data.rpc_data_bytes().to_vec(),
                object_positions: vec![0, second],
            })
            .expect("encode");
        let inner = session.inner.clone();
        let caller = std::thread::spawn(move || {
            let code = SpecialTransaction::GetSessionId.code();
            inner
                .client_transact(RpcAddress::zero(), code, &Parcel::new(), 0)
                .map(|_| ())
        });
        read_aosp_message(&mut peer).expect("the request");
        // Before the `REPLY`: whenever the caller reads it, its release finds the peer closed.
        peer.shutdown(std::net::Shutdown::Read).expect("shutdown");
        write_aosp_message(&mut peer, &reply).expect("the reply");
        assert_eq!(
            caller.join().expect("the caller"),
            Err(StatusCode::DeadObject)
        );
    }

    /// What a serve loop's drain refused is what it ends with, as on a send that drains.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loops_drain_refusal_keeps_its_status() {
        let (session, mut peer) = a13_client_with_raw_peer();
        session
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .expect("deadline");
        fill_socket(&session);
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        session
            .inner
            .hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        std::io::Write::write_all(&mut peer, &a13_header(0, 1000)).expect("a TRANSACT's header");
        let end = session.serve_blocking();
        assert_eq!(end.reason, EndReason::Dispatch(StatusCode::BadType));
    }

    /// A reply wait's failed write of the excess fails the call with that write's status.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_reply_wait_fails_on_its_failed_write_past_the_limit() {
        use crate::rpc::wire_android13::{read_aosp_message, write_aosp_message};
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let inner = session.inner.clone();
        let caller = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let code = SpecialTransaction::GetSessionId.code();
                inner
                    .client_transact(RpcAddress::zero(), code, &Parcel::new(), 0)
                    .map(|_| ())
            })
        };
        read_aosp_message(&mut peer).expect("the request");
        fill_socket(&session);
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, addr, 1);
        }
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        // Nothing writes below the limit: any write has this deadline, and the frame follows it.
        session
            .set_reply_timeout(Some(Duration::from_secs(1)))
            .expect("deadline");
        let last = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, last, 1);
        write_aosp_message(&mut peer, &frame).expect("a frame the wait reads on after");
        assert_eq!(
            caller.join().expect("the caller"),
            Err(StatusCode::TimedOut)
        );
        assert!(inner.shared.lifecycle.is_torn_down());
    }

    /// A slot held through a long call holds up neither this job nor the reaper's jobs behind it.
    #[test]
    fn the_reaper_waits_for_no_slot_to_finish_a_frame() {
        let transport = Carries::default();
        let session =
            RpcSession::new(Box::new(transport.clone()), AddressSpace::Initiator).expect("session");
        let inner = session.inner.clone();
        let held = inner
            .find_conn_pinned(RpcSession::FOUNDING_SLOT_ID)
            .expect("the slot");
        transport.carrying.store(true, Ordering::SeqCst);
        let (done, finished) = std::sync::mpsc::channel();
        let job = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                flush_carried(&Arc::downgrade(&inner), inner.clone());
                let _ = done.send(());
            })
        };
        let returned = finished.recv_timeout(Duration::from_secs(2));
        drop(held);
        job.join().expect("the job");
        assert!(returned.is_ok(), "the job waited for the slot's holder");
    }

    /// Judging a slot's room waits for no holder either: the jobs behind it would wait with it.
    #[test]
    fn the_reaper_waits_for_no_slot_to_judge_its_room() {
        let session = RpcSession::new(Box::new(Carries::default()), AddressSpace::Initiator)
            .expect("session");
        let inner = session.inner.clone();
        let held = inner
            .find_conn_pinned(RpcSession::FOUNDING_SLOT_ID)
            .expect("the slot");
        inner.awaiting_room.store(true, Ordering::SeqCst);
        let (done, finished) = std::sync::mpsc::channel();
        let job = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let slot = RpcSession::FOUNDING_SLOT_ID;
                await_room(&Arc::downgrade(&inner), inner.clone(), slot);
                let _ = done.send(());
            })
        };
        let returned = finished.recv_timeout(Duration::from_secs(2));
        drop(held);
        job.join().expect("the job");
        assert!(returned.is_ok(), "the job waited for the slot's holder");
        // Or no later no-room queues the job.
        assert!(!inner.awaiting_room.load(Ordering::SeqCst));
    }

    /// The job skipped a held slot, so the guard that lets it go queues the job again.
    #[test]
    fn a_slot_given_back_with_a_frame_left_queues_the_reaper() {
        let transport = Carries::default();
        let session =
            RpcSession::new(Box::new(transport.clone()), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        let held = inner
            .find_conn_pinned(RpcSession::FOUNDING_SLOT_ID)
            .expect("the slot");
        transport.carrying.store(true, Ordering::SeqCst);
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(10);
        while transport.flushed_on.lock().expect("lock").is_none() {
            assert!(Instant::now() < deadline, "no flush");
            std::thread::sleep(Duration::from_millis(1));
        }
        let flushed_on = transport.flushed_on.lock().expect("lock").take();
        assert_ne!(flushed_on, Some(std::thread::current().id()));
    }

    /// The reaper never waits for a slot a serve loop keeps while the peer is quiet.
    #[test]
    fn the_reaper_claims_no_slot_a_serve_loop_keeps() {
        let session = RpcSession::new(Box::new(Carries::default()), AddressSpace::Initiator)
            .expect("session");
        let inner = &*session.inner;
        let slot = RpcSession::FOUNDING_SLOT_ID;
        inner.mark_slot_serving(slot, true);
        assert!(inner.claim_if_free(slot).is_none());
        inner.mark_slot_serving(slot, false);
        let claimed = inner.claim_if_free(slot);
        assert!(claimed.is_some());
        inner.release_reaper_claim(slot);
    }

    /// A last drop on a full TCP socket returns, and the frame it left arrives whole and in order.
    #[cfg(all(
        feature = "rpc-tcp-debug",
        any(target_os = "linux", target_os = "android")
    ))]
    #[test]
    fn a_last_drop_on_a_full_tcp_socket_waits_for_nothing_and_cuts_no_frame() {
        use crate::rpc::transport::TcpDebugTransport;

        let (a, peer) = TcpDebugTransport::pair_loopback().expect("a TCP pair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        let transport = inner.slot_transports()[0].clone();
        let filler = [7u8; 2048];
        let mut filled = 4;
        while inner
            .send_msg_no_wait(&*transport, &filler, &[])
            .expect("a TCP socket")
            .expect("a send")
        {
            filled += filler.len();
        }
        let mut counter = 41;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let started = Instant::now();
        drop(RpcProxy::new(addr, session.inner.clone()));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the drop waited"
        );
        // The session-id preamble, every byte of every frame, then the release: nothing is cut.
        let read = read_bytes(&peer, filled);
        assert!(
            read[4..].iter().all(|b| *b == 7),
            "another frame among them"
        );
        let (frame, _) = R34Codec.encode_dec_strong(&addr, 1).expect("a frame");
        assert_eq!(read_bytes(&peer, frame.len()), frame, "the DEC_STRONG");
    }

    /// What found no room leaves no number, no session's end, and the next send goes on from it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_oneway_with_no_room_is_not_sent_and_the_session_goes_on() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let proxy = RpcProxy::new(peer_address(3), session.inner.clone());
        let send = || {
            let data = proxy.request_parcel("t").expect("a parcel");
            proxy.__try_oneway(1, &data, FLAG_ONEWAY)
        };
        let read_number = |peer: &mut std::os::unix::net::UnixStream| {
            let frame = read_aosp_message(peer).expect("a frame");
            let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
            match codec.decode_message(&frame).expect("a message") {
                WireMessage::Transact(t) => t.async_number,
                _ => panic!("not a TRANSACT"),
            }
        };
        assert_eq!(send(), Ok(()));
        let filled = fill_socket(&session);
        assert_eq!(send(), Err(StatusCode::WouldBlock));
        assert!(!session.inner.shared.lifecycle.is_torn_down());
        assert_eq!(read_number(&mut peer), 0);
        let mut filler = vec![0u8; filled];
        std::io::Read::read_exact(&mut peer, &mut filler).expect("the filler");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match send() {
                Ok(()) => break,
                // The reaper still holds the slot.
                Err(StatusCode::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("after the peer read: {e:?}"),
            }
            assert!(Instant::now() < deadline, "no room after the peer read");
        }
        assert_eq!(read_number(&mut peer), 1, "the unsent one left no number");
    }

    /// What found no room leaves no send of its target unpaid: the proxy's release is not held.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_oneway_with_no_room_leaves_no_send_to_pay_back() {
        let (session, _peer) = a13_client_with_raw_peer();
        let addr = peer_address(24);
        let proxy = RpcProxy::new(addr, session.inner.clone());
        fill_socket(&session);
        let data = proxy.request_parcel("t").expect("a parcel");
        assert_eq!(
            proxy.__try_oneway(1, &data, FLAG_ONEWAY),
            Err(StatusCode::WouldBlock)
        );
        let mut st = session.inner.shared.state.lock().expect("state");
        assert_eq!(
            st.release_proxy(&addr, std::ptr::null()),
            1,
            "a send unpaid"
        );
    }

    /// A send past the transport's limit is refused before any byte, and the session stays up.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_oneway_too_long_for_a_send_that_does_not_wait_is_refused() {
        let (session, _peer) = a13_client_with_raw_peer();
        let proxy = RpcProxy::new(peer_address(4), session.inner.clone());
        let mut data = proxy.request_parcel("t").expect("a parcel");
        data.write(&vec![0u8; 4096]).expect("payload");
        assert_eq!(
            proxy.__try_oneway(1, &data, FLAG_ONEWAY),
            Err(StatusCode::BadValue)
        );
        assert!(!session.inner.shared.lifecycle.is_torn_down());
    }

    /// A number another send reserved past it cannot come back: a gap would park the rest.
    #[test]
    fn a_oneway_number_that_cannot_be_taken_back_ends_the_session() {
        use crate::rpc::transport::MemTransport;
        let (a, _peer) = MemTransport::pair();
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let mut counter = 11;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let (first, _next) = {
            let mut st = session.inner.shared.state.lock().expect("state");
            (
                st.next_send_async_number(addr),
                st.next_send_async_number(addr),
            )
        };
        assert!(!session.inner.take_back_oneway(addr, first, false));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !session.inner.shared.lifecycle.is_torn_down() {
            assert!(Instant::now() < deadline, "the session did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// No room and no frame taken loses the session at the send deadline; a frame taken does not.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_slot_with_no_room_is_judged_as_a_waiting_send_is() {
        let deadline = Duration::from_secs(1);
        let (session, _unread, _) = full_r34_client();
        session.set_reply_timeout(Some(deadline)).expect("deadline");
        let proxy = RpcProxy::new(peer_address(5), session.inner.clone());
        let send = |proxy: &RpcProxy| {
            let data = proxy.request_parcel("t");
            proxy.__try_oneway(1, &data.expect("a parcel"), FLAG_ONEWAY)
        };
        let started = Instant::now();
        while !session.inner.shared.lifecycle.is_torn_down() {
            // Sends in the meantime change nothing of the judgement.
            assert_ne!(send(&proxy), Ok(()));
            assert!(started.elapsed() < Duration::from_secs(5), "no judgement");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            started.elapsed() >= deadline * 3 / 4,
            "{:?}",
            started.elapsed()
        );

        let (session, peer, _) = full_r34_client();
        session.set_reply_timeout(Some(deadline)).expect("deadline");
        let proxy = RpcProxy::new(peer_address(6), session.inner.clone());
        assert_eq!(send(&proxy), Err(StatusCode::WouldBlock));
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(200));
            read_bytes(&peer, FILLER);
            let _ = send(&proxy);
            assert!(!session.inner.shared.lifecycle.is_torn_down());
        }
    }

    /// Past the limit with no room a pin holds on, and the serve loop writes only the excess.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_pin_past_its_limit_with_no_room_holds_and_the_serve_loop_writes_the_excess() {
        const EXCESS: usize = 3;
        let (t0, peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");
        let filled = fill_socket(&session);
        let inner = session.inner.clone();
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let held = || {
            let st = inner.conn_state.lock().expect("conn_state");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| s.pending_dec.len())
        };
        let sess_ptr = &*inner as *const RpcSessionInner as usize;
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT + EXCESS {
            inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Initiator), 1);
        }
        DRIVING.with(|d| d.borrow_mut().pop());
        assert_eq!(held(), Some(HELD_DEC_STRONG_LIMIT + EXCESS));

        let codec = Android13PlusCodec::android14_15();
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let (frame, _) = codec.encode_dec_strong(&addr, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the serve loop");
        let read = std::thread::spawn(move || {
            read_bytes(&peer, filled + EXCESS * frame.len());
            peer
        });
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        assert_eq!(held(), Some(HELD_DEC_STRONG_LIMIT), "only the excess");
        let peer = read.join().expect("the peer");
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("peer timeout");
        let mut more = [0u8; 1];
        assert!(peer.recv_raw(&mut more).is_err(), "more than the excess");
    }

    /// How many addresses the reaper owes and has not taken yet.
    fn reaper_owed_len(inner: &RpcSessionInner) -> usize {
        inner
            .conn_state
            .lock()
            .expect("conn_state")
            .reaper_owed
            .len()
    }

    /// Wait until the reaper has taken what it owes: it now writes or waits for a slot with that.
    fn reaper_takes_what_it_owes(inner: &RpcSessionInner) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reaper_owed_len(inner) > 0 {
            assert!(Instant::now() < deadline, "the reaper took nothing");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A burst of drops past any limit, with the only slot busy: no end, and every amount goes.
    #[test]
    fn a_burst_of_releases_ends_no_session_and_each_address_gets_its_sum() {
        use crate::rpc::wire_android13::read_aosp_message;
        const BURST: usize = 2 * HELD_DEC_STRONG_LIMIT;
        let (session, mut peer) = a13_client_with_raw_peer();
        let inner = &*session.inner;
        let taken = inner.find_conn().expect("the slot");
        let mut counter = 0;
        let first = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        inner.reap_dec_strong(first, 1);
        reaper_takes_what_it_owes(inner);
        let repeated = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        for _ in 0..3 {
            inner.reap_dec_strong(repeated, 1);
        }
        for _ in 0..BURST {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
            inner.reap_dec_strong(addr, 1);
        }
        assert_eq!(reaper_owed_len(inner), BURST + 1, "one entry per address");
        assert!(!inner.shared.lifecycle.is_torn_down(), "a burst ended it");
        drop(taken);

        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let mut frames = 0;
        let mut total = 0u64;
        let mut of_repeated = 0;
        while frames < BURST + 2 {
            let frame = read_aosp_message(&mut peer).expect("a frame");
            let WireMessage::DecStrong(addr, amount) = codec.decode_message(&frame).expect("frame")
            else {
                panic!("not a DEC_STRONG");
            };
            frames += 1;
            total += u64::from(amount);
            if addr == repeated {
                of_repeated = amount;
            }
        }
        assert_eq!(of_repeated, 3, "the sum of one address");
        assert_eq!(total, BURST as u64 + 4, "the sum of them all");
        assert!(!inner.shared.lifecycle.is_torn_down());
    }

    /// With no `Outgoing` slot what the reaper owes waits for a way out, and its book with it.
    #[test]
    fn a_session_with_nothing_to_write_on_keeps_what_the_reaper_owes() {
        use crate::rpc::transport::MemTransport;
        let (a, _peer) = MemTransport::pair();
        let session = RpcSession::from_android13plus(
            Box::new(a),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = &*session.inner;
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        drop(inner.enter_binder(addr).expect("a receipt"));
        assert_eq!(reaper_owed_len(inner), 1);
        let st = inner.conn_state.lock().expect("conn_state");
        assert!(!st.reaper_dec_queued, "no job: no slot could carry it");
        drop(st);
        assert!(!inner.shared.lifecycle.is_torn_down());
        let books = inner
            .shared
            .state
            .lock()
            .expect("state")
            .remote_book_count();
        assert_eq!(books, 1, "the amount is not on the wire");
    }

    /// With no `Outgoing` slot a `REPLY` is the way out of what the reaper owes: the peer reads it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn what_the_reaper_owes_with_no_slot_goes_out_before_a_reply() {
        use crate::rpc::transport::UnixTransport;
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        inner.conn_state.lock().expect("conn_state").slots[0].peer_called = true;
        let codec = Android13PlusCodec::android14_15();
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        inner.reap_dec_strong(addr, 2);

        let key = (
            &*inner as *const RpcSessionInner as usize,
            RpcSession::FOUNDING_SLOT_ID,
        );
        DRIVING.with(|d| d.borrow_mut().push(key));
        assert!(inner.write_reply(0, &[], &[], &[]).is_ok());
        DRIVING.with(|d| d.borrow_mut().pop());

        let (paid, _) = codec.encode_dec_strong(&addr, 2).expect("a frame");
        assert_eq!(read_bytes(&peer, paid.len()), paid);
        assert_eq!(reaper_owed_len(&inner), 0);
        session.close_session();
    }

    /// With no `Outgoing` slot the loop reading an `Incoming` one writes what is past the limit.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loop_with_no_slot_for_the_reaper_writes_what_is_past_the_limit() {
        use crate::rpc::transport::UnixTransport;
        const EXCESS: usize = 3;
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        inner.conn_state.lock().expect("conn_state").slots[0].peer_called = true;
        let codec = Android13PlusCodec::android14_15();
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT + EXCESS {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.reap_dec_strong(addr, 1);
        }
        let other = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Continue
        ));
        assert_eq!(reaper_owed_len(&inner), HELD_DEC_STRONG_LIMIT);
        session.close_session();
    }

    /// An attach's probe reads one `REPLY`: what the reaper owes waits for a call on the slot.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_attach_reads_only_its_reply_however_much_the_reaper_owes() {
        use crate::rpc::transport::UnixTransport;
        const OWED: usize = HELD_DEC_STRONG_LIMIT + 3;
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        // Writing what is owed would fill the socket: a deadline makes that a failure, not a hang.
        session
            .set_reply_timeout(Some(Duration::from_secs(2)))
            .expect("deadline");
        let inner = session.inner.clone();
        let codec = Android13PlusCodec::android14_15();
        let mut counter = 0;
        for _ in 0..OWED {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.reap_dec_strong(addr, 1);
        }
        let probe = codec
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: SpecialTransaction::GetSessionId.code(),
                flags: 0,
                async_number: 0,
                data: Vec::new(),
                object_positions: Vec::new(),
            })
            .expect("a probe");
        peer.send_raw(&probe).expect("the probe");
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Continue
        ));
        let mut io = RawTransportIo(&peer);
        let first = read_aosp_message(&mut io).expect("a frame");
        let first = codec.decode_message(&first).expect("decode");
        assert!(matches!(first, WireMessage::Reply(_)), "{first:?}");
        assert_eq!(reaper_owed_len(&inner), OWED);
        session.close_session();
    }

    /// An `Outgoing` pin's frames add to what the reaper owes: its reader writes what is past it.
    #[test]
    fn a_reply_wait_on_an_outgoing_pin_writes_what_the_reaper_owes_past_the_limit() {
        use crate::rpc::wire_android13::write_aosp_message;
        const EXCESS: usize = 3;
        let (session, mut peer) = a13_client_with_raw_peer();
        let inner = session.inner.clone();
        let conn = inner.find_conn().expect("the slot");
        let mut counter = 0;
        let first = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        inner.reap_dec_strong(first, 1);
        reaper_takes_what_it_owes(&inner);
        for _ in 0..HELD_DEC_STRONG_LIMIT + EXCESS {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
            inner.reap_dec_strong(addr, 1);
        }
        let peer_side = std::thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("peer timeout");
            read_aosp_message(&mut peer).expect("the request");
            peer.set_read_timeout(Some(Duration::from_millis(500)))
                .expect("peer timeout");
            for _ in 0..EXCESS {
                let _ = read_aosp_message(&mut peer);
            }
            let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
            let reply = codec
                .encode_reply_ref(WireReplyRef {
                    status: 0,
                    data: &[],
                    object_positions: &[],
                })
                .expect("a reply");
            write_aosp_message(&mut peer, &reply).expect("the reply");
            peer
        });
        let replied = inner.client_transact(
            RpcAddress::zero(),
            SpecialTransaction::GetSessionId.code(),
            &Parcel::new(),
            0,
        );
        let _peer = peer_side.join().expect("the peer");
        assert!(replied.is_ok(), "{replied:?}");
        assert_eq!(reaper_owed_len(&inner), HELD_DEC_STRONG_LIMIT, "the excess");
        drop(conn);
    }

    /// An `Incoming` pin's frames stop adding to what the reaper owes at the limit: it holds.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_incoming_pin_holds_what_the_reaper_owes_no_more_of() {
        use crate::rpc::transport::UnixTransport;
        let (t0, _p0) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        // Two: the reaper waits in one's full socket, and the other is free for the route.
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (t, peer) = UnixTransport::pair().expect("socketpair");
            session.add_callback_slot(Box::new(t), 2).expect("a slot");
            peers.push(peer);
        }
        for t in &inner.slot_transports()[1..] {
            while t
                .__send_no_wait(&[0u8; FILLER], &[])
                .expect("a Unix socket")
                .expect("a send")
            {}
        }
        let mut counter = 0;
        let first = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        inner.reap_dec_strong(first, 1);
        reaper_takes_what_it_owes(&inner);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = inner.conn_state.lock().expect("conn_state");
            if st.slots.iter().any(|s| s.exclusive_tid.is_some()) {
                break;
            }
            drop(st);
            assert!(Instant::now() < deadline, "the reaper took no slot");
            std::thread::sleep(Duration::from_millis(1));
        }
        for _ in 0..HELD_DEC_STRONG_LIMIT {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.reap_dec_strong(addr, 1);
        }
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let sess_ptr = &*inner as *const RpcSessionInner as usize;
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        inner.send_dec_strong(addr, 1);
        DRIVING.with(|d| d.borrow_mut().pop());
        assert_eq!(
            reaper_owed_len(&inner),
            HELD_DEC_STRONG_LIMIT,
            "added past it"
        );
        let st = inner.conn_state.lock().expect("conn_state");
        let held = st
            .slots
            .iter()
            .find(|s| s.id == slot_id)
            .map(|s| s.pending_dec.len());
        drop(st);
        assert_eq!(held, Some(1), "not held on the pin");
        session.close_session();
    }

    /// A frame served does not end the mark: between two frames the loop is still on the slot.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loop_stays_marked_between_its_frames() {
        use crate::rpc::transport::UnixTransport;
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        inner.mark_slot_serving(slot_id, true);
        let codec = Android13PlusCodec::android14_15();
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let (frame, _) = codec.encode_dec_strong(&addr, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        let st = inner.conn_state.lock().expect("conn_state");
        assert!(st.slots.iter().all(|s| s.id != slot_id || s.serving));
        drop(st);
        session.close_session();
    }

    /// A oneway handler's hold leaves through the reaper, with no reply to come on its connection.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_serve_loop_hands_what_its_slot_holds_to_the_reaper() {
        use crate::rpc::transport::UnixTransport;
        let (t0, peer) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let held = || {
            let st = inner.conn_state.lock().expect("conn_state");
            let slot = st.slots.iter().find(|s| s.id == slot_id);
            slot.map(|s| s.pending_dec.len())
        };
        let codec = Android13PlusCodec::android14_15();
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let sess_ptr = &*inner as *const RpcSessionInner as usize;
        DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
        inner.send_dec_strong(addr, 2);
        assert_eq!(held(), Some(1), "no `Outgoing` slot to write on");

        let other = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        assert_eq!(held(), Some(1), "the reaper has no slot to write it on");

        let (t, callback) = UnixTransport::pair().expect("socketpair");
        let added = session.add_callback_slot(Box::new(t), 2).expect("a slot");
        let mark_serving = |serving: bool| {
            let mut st = inner.conn_state.lock().expect("conn_state");
            let slot = st.slots.iter_mut().find(|s| s.id == added).expect("slot");
            slot.serving = serving;
        };
        mark_serving(true);
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        assert_eq!(
            held(),
            Some(1),
            "the reaper would wait for the peer's frame"
        );

        mark_serving(false);
        peer.send_raw(&frame).expect("a frame for the serve loop");
        assert!(matches!(
            inner.serve_once_on_slot(slot_id),
            ServeStep::Continue
        ));
        DRIVING.with(|d| d.borrow_mut().pop());
        assert_eq!(held(), Some(0), "still held for a reply that may not come");

        callback
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let (paid, _) = codec.encode_dec_strong(&addr, 2).expect("a frame");
        assert_eq!(read_bytes(&callback, paid.len()), paid);
        session.close_session();
    }

    /// Fails its first frame part-way, as a send deadline does, and counts what is written after.
    struct CutsAFrame {
        cut_next: Arc<AtomicBool>,
        writes_after: Arc<AtomicUsize>,
    }

    impl RpcTransport for CutsAFrame {
        fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
            if self.cut_next.swap(false, Ordering::SeqCst) {
                return Err(RpcError::Io(std::io::ErrorKind::WouldBlock.into()));
            }
            self.writes_after.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn __send_no_wait(
            &self,
            buf: &[u8],
            _: &[std::os::fd::BorrowedFd<'_>],
        ) -> Option<RpcResult<bool>> {
            Some(self.send_raw(buf).map(|()| true))
        }
        fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
            Err(RpcError::EndOfStream)
        }
        fn peer_identity(&self) -> PeerIdentity {
            PeerIdentity::Anonymous
        }
        fn describe(&self) -> &str {
            "cuts-a-frame"
        }
        fn shutdown(&self) -> RpcResult<()> {
            Ok(())
        }
    }

    /// A release the reaper already waits to write on this slot never follows the frame cut there.
    #[test]
    fn nothing_is_written_after_a_frame_cut_with_no_death_sequence_on_this_thread() {
        let cut_next = Arc::new(AtomicBool::new(false));
        let writes_after = Arc::new(AtomicUsize::new(0));
        let transport = CutsAFrame {
            cut_next: cut_next.clone(),
            writes_after: writes_after.clone(),
        };
        let session =
            RpcSession::new(Box::new(transport), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        writes_after.store(0, Ordering::SeqCst);
        cut_next.store(true, Ordering::SeqCst);
        let mut counter = 17;
        let conn = inner.find_conn().expect("the slot");
        inner.reap_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1);
        reaper_takes_what_it_owes(inner);
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        assert_eq!(inner.write_dec_no_wait(&conn, addr, 1), 0);
        assert!(
            inner.shared.lifecycle.is_torn_down(),
            "before the slot goes back"
        );
        // With the slot still held: the stop's own wake is what the reaper needs.
        let deadline = Instant::now() + Duration::from_secs(10);
        while inner.slot_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "the reaper ran no death sequence"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(conn);
        assert_eq!(
            writes_after.load(Ordering::SeqCst),
            0,
            "a frame after the cut"
        );
    }

    /// The thread that cut a frame goes on through its pin: nothing it sends reaches the socket.
    #[test]
    fn a_thread_that_cut_a_frame_writes_nothing_more_on_its_pin() {
        /// Stays in `shutdown` until told, so the slot stays pooled while the test writes.
        struct HoldsItsEnd {
            frames: CutsAFrame,
            ending: Mutex<mpsc::Receiver<()>>,
        }
        impl RpcTransport for HoldsItsEnd {
            fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
                self.frames.send_raw(buf)
            }
            fn __send_no_wait(
                &self,
                buf: &[u8],
                fds: &[std::os::fd::BorrowedFd<'_>],
            ) -> Option<RpcResult<bool>> {
                self.frames.__send_no_wait(buf, fds)
            }
            fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
                self.frames.recv_raw(buf)
            }
            fn peer_identity(&self) -> PeerIdentity {
                self.frames.peer_identity()
            }
            fn describe(&self) -> &str {
                self.frames.describe()
            }
            fn shutdown(&self) -> RpcResult<()> {
                let ending = self.ending.lock().expect("ending");
                let _ = ending.recv_timeout(Duration::from_secs(10));
                Ok(())
            }
        }
        let cut_next = Arc::new(AtomicBool::new(false));
        let writes_after = Arc::new(AtomicUsize::new(0));
        let (end, ending) = mpsc::channel();
        let transport = HoldsItsEnd {
            frames: CutsAFrame {
                cut_next: cut_next.clone(),
                writes_after: writes_after.clone(),
            },
            ending: Mutex::new(ending),
        };
        let session =
            RpcSession::new(Box::new(transport), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        writes_after.store(0, Ordering::SeqCst);
        cut_next.store(true, Ordering::SeqCst);
        let conn = inner.find_conn().expect("the slot");
        let mut counter = 29;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        assert_eq!(inner.write_dec_no_wait(&conn, addr, 1), 0);
        assert!(inner.shared.lifecycle.is_torn_down());
        assert!(
            inner.slot_count() > 0,
            "the reaper's sequence is in shutdown"
        );
        assert!(matches!(
            inner.write_reply(0, &[], &[], &[]),
            Err(ReplyNotSent::Failed(StatusCode::DeadObject))
        ));
        assert!(inner.write_dec_strong(&conn, addr, 1).is_err());
        let called = inner.client_transact(
            RpcAddress::zero(),
            SpecialTransaction::GetSessionId.code(),
            &Parcel::new(),
            0,
        );
        assert_eq!(called.err(), Some(StatusCode::DeadObject));
        assert_eq!(
            writes_after.load(Ordering::SeqCst),
            0,
            "a frame after the cut"
        );
        drop(conn);
        let _ = end.send(());
    }

    /// A stopped session sends nothing on a send that does not wait, whatever room there is.
    #[test]
    fn a_stopped_session_sends_nothing_that_does_not_wait() {
        let (session, peer) = a13_client_with_raw_peer();
        let inner = &*session.inner;
        assert!(inner.shared.lifecycle.force_dying());
        let transports = inner.slot_transports();
        let sent = inner.send_msg_no_wait(&*transports[0], b"frame", &[]);
        assert!(matches!(sent, Some(Err(RpcError::EndOfStream))), "{sent:?}");
        peer.set_nonblocking(true).expect("nonblocking");
        let mut byte = [0u8; 1];
        assert_eq!(
            std::io::Read::read(&mut &peer, &mut byte).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::WouldBlock),
            "a frame reached the peer"
        );
    }

    /// A send only `shutdown` ends, on a transport with no `shutdown_handle`: `close_session` ends.
    #[test]
    fn close_session_ends_what_a_stop_with_nothing_to_cut_left_waiting() {
        #[derive(Default)]
        struct Down {
            down: Mutex<bool>,
            changed: Condvar,
        }
        impl Down {
            /// What a blocked send or recv does: up to a limit, so that a failure ends the test.
            fn wait(&self) -> RpcResult<usize> {
                let down = self.down.lock().expect("down");
                let limit = Duration::from_secs(30);
                let woke = self.changed.wait_timeout_while(down, limit, |d| !*d);
                drop(woke.expect("down"));
                Err(RpcError::EndOfStream)
            }
        }
        struct WaitsForShutdown {
            blocks: Arc<AtomicBool>,
            down: Arc<Down>,
        }
        impl RpcTransport for WaitsForShutdown {
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                if self.blocks.load(Ordering::SeqCst) {
                    self.down.wait()?;
                }
                Ok(())
            }
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                self.down.wait()
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "waits-for-shutdown"
            }
            fn shutdown(&self) -> RpcResult<()> {
                *self.down.down.lock().expect("down") = true;
                self.down.changed.notify_all();
                Ok(())
            }
        }
        let blocks = Arc::new(AtomicBool::new(false));
        let down = Arc::new(Down::default());
        let transport = WaitsForShutdown {
            blocks: blocks.clone(),
            down: down.clone(),
        };
        let session =
            RpcSession::new(Box::new(transport), AddressSpace::Initiator).expect("session");
        let inner = session.inner.clone();
        // What `close_session` joins: a thread in `recv`, as a connection's serve loop is.
        let serving = std::thread::spawn({
            let down = down.clone();
            move || {
                let _ = down.wait();
            }
        });
        inner
            .incoming_threads
            .lock()
            .expect("incoming_threads")
            .push((2, serving));
        blocks.store(true, Ordering::SeqCst);
        let mut counter = 31;
        inner.reap_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1);
        reaper_takes_what_it_owes(&inner);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = inner.conn_state.lock().expect("conn_state");
            if st.slots.iter().any(|s| s.exclusive_tid.is_some()) {
                break;
            }
            drop(st);
            assert!(Instant::now() < deadline, "the reaper took no slot");
            std::thread::sleep(Duration::from_millis(1));
        }
        inner.stop_for_the_reaper();
        assert!(inner.shared.lifecycle.is_torn_down());
        assert!(
            inner.slot_count() > 0,
            "the stop cut nothing, so it runs behind"
        );
        let (closed, returned) = mpsc::channel();
        std::thread::spawn(move || {
            session.close_session();
            let _ = closed.send(());
        });
        assert!(
            returned.recv_timeout(Duration::from_secs(10)).is_ok(),
            "close_session stayed in its join"
        );
        while inner.slot_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "the reaper ran no death sequence"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A slot free in a stopped session may hold a cut frame: a release is not written there.
    #[test]
    fn a_stopped_session_routes_no_release_to_its_free_slot() {
        use crate::rpc::transport::MemTransport;
        let (a, _peer) = MemTransport::pair();
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        assert!(matches!(session.inner.dec_route(), DecRoute::Conn(_)));
        assert!(session.inner.shared.lifecycle.force_dying());
        assert!(matches!(session.inner.dec_route(), DecRoute::Reaper));
    }

    /// A oneway refused after another send reserved the next number would leave a gap: it ends.
    #[test]
    fn a_oneway_refused_past_a_later_number_ends_the_session() {
        struct ReservesTheNext {
            session: Arc<Mutex<Weak<RpcSessionInner>>>,
            addr: RpcAddress,
        }
        impl RpcTransport for ReservesTheNext {
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                let session = self.session.lock().expect("lock").upgrade();
                let Some(inner) = session else {
                    return Ok(());
                };
                let mut st = inner.shared.state.lock().expect("state");
                st.next_send_async_number(self.addr);
                Err(RpcError::Protocol("refused before any byte"))
            }
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "reserves-the-next"
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
        }
        let mut counter = 19;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let cell = Arc::new(Mutex::new(Weak::new()));
        let transport = ReservesTheNext {
            session: cell.clone(),
            addr,
        };
        let session =
            RpcSession::new(Box::new(transport), AddressSpace::Initiator).expect("session");
        *cell.lock().expect("lock") = Arc::downgrade(&session.inner);
        assert!(session
            .inner
            .client_transact(addr, 1, &Parcel::new(), FLAG_ONEWAY)
            .is_err());
        assert!(session.inner.shared.lifecycle.is_torn_down());
    }

    /// What a pin holds past its limit goes out before a reply wait reads on, as in a serve loop.
    #[test]
    fn a_reply_wait_writes_what_its_slot_holds_past_the_limit() {
        use crate::rpc::wire_android13::write_aosp_message;
        const EXCESS: usize = 3;
        let (session, mut peer) = a13_client_with_raw_peer();
        let inner = session.inner.clone();
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let held = || {
            let st = inner.conn_state.lock().expect("conn_state");
            st.slots
                .iter()
                .find(|s| s.id == slot_id)
                .map(|s| s.pending_dec.len())
        };
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let caller = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let code = SpecialTransaction::GetSessionId.code();
                let replied = inner.client_transact(RpcAddress::zero(), code, &Parcel::new(), 0);
                replied.map(|_| ())
            })
        };
        read_aosp_message(&mut peer).expect("the request");
        // Made in the wait, as a nested handler's drops are: the request took what came before.
        let mut counter = 0;
        for _ in 0..HELD_DEC_STRONG_LIMIT + EXCESS {
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            inner.hold_dec_strong(slot_id, addr, 1);
        }
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
        write_aosp_message(&mut peer, &frame).expect("a frame the wait reads on after");
        for _ in 0..EXCESS {
            let frame = read_aosp_message(&mut peer).expect("what is past the limit");
            let message = codec.decode_message(&frame).expect("decode");
            assert!(matches!(message, WireMessage::DecStrong(..)), "{message:?}");
        }
        assert_eq!(held(), Some(HELD_DEC_STRONG_LIMIT), "only the excess");
        let reply = codec
            .encode_reply_ref(WireReplyRef {
                status: 0,
                data: &[],
                object_positions: &[],
            })
            .expect("a reply");
        write_aosp_message(&mut peer, &reply).expect("the reply");
        let replied = caller.join().expect("the caller");
        assert!(replied.is_ok(), "{replied:?}");
    }

    /// What a slot holds goes before a request, reading meanwhile: the peer writes before it reads.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn what_a_slot_holds_goes_before_a_request_reading_while_it_waits_for_room() {
        use std::io::{Read, Write};
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        session
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .expect("deadline");
        let inner = session.inner.clone();
        let filled = fill_socket(&session);
        let mut counter = 0;
        let held = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, held, 1);
        let peer_side = std::thread::spawn(move || {
            let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
            let (frame, _) = codec.encode_dec_strong(&other, 1).expect("a frame");
            // Past every buffer: this write ends only if the sender reads.
            let flood = frame.repeat(4 * 1024 * 1024 / frame.len());
            peer.write_all(&flood).expect("the flood");
            let mut filler = vec![0u8; filled];
            peer.read_exact(&mut filler)
                .expect("what filled the socket");
            let first = raw_peer_reads(&mut peer);
            assert!(matches!(first, WireMessage::DecStrong(a, 1) if a == held));
            let second = raw_peer_reads(&mut peer);
            assert!(matches!(second, WireMessage::Transact(_)));
            let reply = codec.encode_reply(&WireReply::default()).expect("a reply");
            peer.write_all(&reply).expect("the reply");
            peer
        });
        let code = SpecialTransaction::GetSessionId.code();
        let replied = inner.client_transact(RpcAddress::zero(), code, &Parcel::new(), 0);
        assert!(replied.is_ok(), "{:?}", replied.map(|_| ()));
        let _peer = peer_side.join().expect("the peer");
        session.close_session();
    }

    /// Part of a frame a pin's write left never stays: its holder reads next.
    #[test]
    fn a_pin_finishes_what_its_send_left() {
        let transport = Carries::default();
        let session =
            RpcSession::new(Box::new(transport.clone()), AddressSpace::Initiator).expect("session");
        let inner = &*session.inner;
        let conn = inner.find_conn().expect("the slot");
        let mut counter = 37;
        inner.send_dec_strong(RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1);
        assert!(!transport.__carrying());
        let flushed_on = transport.flushed_on.lock().expect("lock").take();
        assert_eq!(flushed_on, Some(std::thread::current().id()));
        drop(conn);
        assert!(!inner.shared.lifecycle.is_torn_down());
    }

    /// A hold made in a reply wait on an `Outgoing` pin goes out when the wait's slot goes back.
    #[test]
    fn what_a_reply_wait_holds_goes_out_as_its_slot_goes_back() {
        use crate::rpc::wire_android13::write_aosp_message;
        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let inner = session.inner.clone();
        let mut counter = 0;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let caller = {
            let inner = inner.clone();
            std::thread::spawn(move || {
                let code = SpecialTransaction::GetSessionId.code();
                let replied = inner.client_transact(RpcAddress::zero(), code, &Parcel::new(), 0);
                replied.map(|_| ())
            })
        };
        read_aosp_message(&mut peer).expect("the request");
        inner.hold_dec_strong(RpcSession::FOUNDING_SLOT_ID, addr, 2);
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let reply = codec
            .encode_reply_ref(WireReplyRef {
                status: 0,
                data: &[],
                object_positions: &[],
            })
            .expect("a reply");
        write_aosp_message(&mut peer, &reply).expect("the reply");
        assert!(caller.join().expect("the caller").is_ok());
        match raw_peer_reads(&mut peer) {
            WireMessage::DecStrong(paid, 2) => assert_eq!(paid, addr),
            other => panic!("the hold goes out behind the reply, got {other:?}"),
        }
        session.close_session();
    }

    /// A oneway handler's hold stays on its `Incoming` slot past the frame: the limit counts it.
    #[test]
    fn a_hold_on_an_incoming_slot_outlives_its_frame() {
        use crate::{Binder, Interface};
        use std::io::Write;

        let (t0, mut peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let t0 = crate::rpc::transport::UnixTransport::from_stream(t0).expect("transport");
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2"),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let inner = session.inner.clone();
        let target: SIBinder = Binder::new(TargetOfACall(Mutex::new(None))).as_binder();
        let addr = inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&target)
            .expect("an address");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let frame = codec
            .encode_transact(&WireTransaction {
                address: addr,
                code: crate::FIRST_CALL_TRANSACTION,
                flags: FLAG_ONEWAY,
                async_number: 0,
                data: Vec::new(),
                object_positions: Vec::new(),
            })
            .expect("encode");
        peer.write_all(&frame).expect("the oneway");
        assert!(matches!(
            inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
            ServeStep::Continue
        ));
        assert_eq!(
            reaper_owed_len(&inner),
            0,
            "not handed on as the frame ended"
        );
        let held = inner.conn_state.lock().expect("conn_state").slots[0]
            .pending_dec
            .len();
        assert_eq!(held, 1, "held for the slot's next REPLY");
        session.close_session();
    }

    /// `session`'s reaper waits for room: its last handle and proxy end it all the same.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn the_last_handle_ends_a_session_with_its_reaper_waiting(
        session: RpcSession,
        peer: &dyn RpcTransport,
        filled: usize,
    ) {
        let slot_id = RpcSession::FOUNDING_SLOT_ID;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let st = session.inner.conn_state.lock().expect("conn_state");
            let claimed = st
                .slots
                .iter()
                .any(|s| s.id == slot_id && s.exclusive_tid.is_some());
            drop(st);
            if claimed {
                break;
            }
            assert!(Instant::now() < deadline, "the reaper took no slot");
            std::thread::sleep(Duration::from_millis(1));
        }
        let probe = session.inner_weak();
        drop(session);
        while probe.strong_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "the reaper's wait keeps the session"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // With nothing read: a read would give the reaper the room it waits for.
        while peer.peer_closed() != Some(true) {
            assert!(Instant::now() < deadline, "the peer sees no end of stream");
            std::thread::sleep(Duration::from_millis(1));
        }
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("peer timeout");
        read_bytes(peer, filled);
        let mut more = [0u8; 1];
        assert!(
            matches!(peer.recv_raw(&mut more), Ok(0) | Err(RpcError::EndOfStream)),
            "a frame after what was in the socket",
        );
    }

    /// `ReaperJob::Dec` in a socket's wait, on a session with no deadline.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_reaper_waiting_to_write_keeps_no_session_alive() {
        let (session, peer, filled) = full_r34_client();
        drop(RpcProxy::new(peer_address(21), session.inner.clone()));
        the_last_handle_ends_a_session_with_its_reaper_waiting(session, &peer, filled);
    }

    /// `ReaperJob::AwaitRoom` in a socket's wait, on a session with no deadline.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_reaper_judging_a_full_socket_keeps_no_session_alive() {
        let (session, peer, filled) = full_r34_client();
        let proxy = RpcProxy::new(peer_address(22), session.inner.clone());
        let data = proxy.request_parcel("t").expect("a parcel");
        assert_eq!(
            proxy.__try_oneway(1, &data, FLAG_ONEWAY),
            Err(StatusCode::WouldBlock)
        );
        drop(data);
        drop(proxy);
        the_last_handle_ends_a_session_with_its_reaper_waiting(session, &peer, filled);
    }

    /// `close_session` wakes the same wait, and the reaper keeps nothing after it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn close_session_ends_a_reapers_wait_for_room() {
        let (session, _peer, _) = full_r34_client();
        drop(RpcProxy::new(peer_address(23), session.inner.clone()));
        session.close_session();
        let probe = session.inner_weak();
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(5);
        while probe.strong_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "the reaper's wait keeps the session"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A reply with fds past the uid's fds in flight fails the call; its session stays up.
    #[test]
    // Not on s390x: CI runs it under qemu-user with no binfmt entry, so the child cannot start.
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(target_arch = "s390x")
    ))]
    fn a_reply_past_the_fds_in_flight_fails_the_call_and_leaves_the_session_up() {
        // Root has the capability that passes this check.
        if rustix::process::geteuid().is_root() {
            return;
        }
        crate::tests::in_child_process(
            "rpc::session::tests::\
             a_reply_past_the_fds_in_flight_fails_the_call_and_leaves_the_session_up",
            || {
                use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
                let held = Rlimit {
                    current: Some(16),
                    maximum: getrlimit(Resource::Nofile).maximum,
                };
                setrlimit(Resource::Nofile, held).expect("RLIMIT_NOFILE");
                let (t0, _peer) = crate::rpc::transport::UnixTransport::pair().expect("socketpair");
                let session = RpcSession::from_android13plus(
                    Box::new(t0),
                    Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2"),
                    FD_MODE_UNIX,
                    true,
                )
                .expect("session");
                let inner = &*session.inner;
                let file = std::fs::File::open("/dev/null").expect("/dev/null");
                let fd = OwnedFd::from(file);
                let transport = &inner.slot_transports()[0];
                let slot_id = RpcSession::FOUNDING_SLOT_ID;
                let sess_ptr = inner as *const RpcSessionInner as usize;
                DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
                let mut sent = 0;
                let replied = loop {
                    while transport
                        .__send_no_wait(b"frame", &[fd.as_fd()])
                        .expect("a Unix socket")
                        .expect("a send")
                    {
                        sent += 1;
                        assert!(sent < 64, "no limit on what is in flight");
                    }
                    // The count is the uid's: another process may have let some go since the fill.
                    let replied = inner.write_reply(0, &[0; 8], &[], std::slice::from_ref(&fd));
                    if replied.is_err() {
                        break replied;
                    }
                    sent += 1;
                    assert!(sent < 64, "no limit on what is in flight");
                };
                DRIVING.with(|d| d.borrow_mut().pop());
                assert!(matches!(
                    replied,
                    Err(ReplyNotSent::Refused(StatusCode::FailedTransaction))
                ));
                assert!(!inner.shared.lifecycle.is_torn_down());
            },
        );
    }

    /// The callback-slot cap (checked and pushed under one `conn_state` lock) refuses past it.
    #[test]
    fn callback_slot_cap_is_enforced() {
        use crate::rpc::transport::MemTransport;
        // Server-side post-handshake session form (no wire I/O).
        let (t0, _p0) = MemTransport::pair();
        let session = RpcSession::from_android13plus(
            Box::new(t0),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("build session");

        // The cap counts callback (`Outgoing`) slots only, not the served founding slot.
        let base = session.inner.slot_count();
        let cap = 2;
        let mut peers = Vec::new();
        let mut admitted = 0usize;
        let mut refused = 0usize;
        for _ in 0..5 {
            let (t, p) = MemTransport::pair();
            peers.push(p); // keep peer halves alive
            match session.add_callback_slot(Box::new(t), cap) {
                Ok(_) => admitted += 1,
                Err(_) => refused += 1,
            }
        }
        assert_eq!(
            session.inner.slot_count(),
            base + cap,
            "pool never exceeds founding + cap"
        );
        assert_eq!(admitted, cap, "exactly cap callback slots admitted");
        assert!(refused >= 1, "attaches past the cap are refused");
    }

    /// A failed callback connection-init leaves no slot to eat the cap or be picked first.
    #[test]
    fn callback_slot_init_failure_retires_the_slot() {
        use crate::rpc::transport::MemTransport;
        let (t0, _p0) = MemTransport::pair();
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");
        let base = session.inner.slot_count();

        // The peer is gone, so the `"cci"` write fails: the case under test.
        let (t, p) = MemTransport::pair();
        drop(p);
        assert!(
            session
                .add_callback_slot_and_init(Box::new(t), 2, &codec)
                .is_err(),
            "a failed connection-init must be reported"
        );
        assert_eq!(
            session.inner.slot_count(),
            base,
            "a failed connection-init must leave no slot behind"
        );

        // …and the callback budget is intact: two slots still fit under the same cap.
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (t, p) = MemTransport::pair();
            peers.push(p);
            session
                .add_callback_slot(Box::new(t), 2)
                .expect("callback budget still has room");
        }
        assert_eq!(session.inner.slot_count(), base + 2);
    }

    /// A slot whose `"cci"` failed stays claimed until it has left the pool: no sender picks it.
    #[test]
    fn a_sender_racing_a_failed_callback_init_never_picks_its_slot() {
        use crate::rpc::transport::UnixTransport;
        let (t0, _p0) = UnixTransport::pair().expect("socketpair");
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");
        let base = session.inner.slot_count();
        // Bounds the racer's wait: inside the window the pool has no free `Outgoing` slot.
        session
            .set_reply_timeout(Some(Duration::from_millis(300)))
            .unwrap();

        // The peer is gone: `"cci"` fails with `EPIPE`, and so would a frame sent on this slot.
        let (t, p) = UnixTransport::pair().expect("socketpair");
        drop(p);
        let racer_inner = Arc::clone(&session.inner);
        let (raced_tx, raced_rx) = mpsc::channel();
        // Runs inside the window, between the failed `"cci"` and the slot's retirement.
        let hook = move || {
            let raced = std::thread::spawn(move || {
                racer_inner
                    .client_transact(RpcAddress::zero(), 1, &Parcel::new(), FLAG_ONEWAY)
                    .map(|_| ())
            })
            .join()
            .expect("racer");
            let _ = raced_tx.send(raced);
        };
        *session
            .inner
            .shared
            .cci_failed_hook
            .lock()
            .expect("cci hook") = Some(Box::new(hook));

        assert!(
            session
                .add_callback_slot_and_init(Box::new(t), 2, &codec)
                .is_err(),
            "a failed connection-init must be reported"
        );
        let raced = raced_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the hook ran");
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "a retired callback slot must not end the session"
        );
        assert_eq!(
            raced,
            Err(StatusCode::WouldBlock),
            "the racer must find no free slot, not the one being retired"
        );
        assert_eq!(session.inner.slot_count(), base, "the slot left the pool");
    }

    /// A client's incoming connection whose serve thread fails to spawn ends the session.
    #[test]
    fn an_incoming_connection_whose_thread_fails_to_spawn_ends_the_session() {
        use super::super::transport::UnixTransport;
        use crate::rpc::wire_android13::server_accept;
        use std::os::unix::net::UnixStream;

        let (client_fd, founding_peer_fd) = unix_socketpair_fd();
        let session = RpcSession::with_profile(
            Box::new(UnixTransport::from_stream(UnixStream::from(client_fd)).expect("transport")),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        seed_server_id(&session);
        // Armed before the session can end: macOS refuses `SO_RCVTIMEO` once the peer closed.
        let mut founding_peer = UnixStream::from(founding_peer_fd);
        founding_peer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");

        // The server's half of an incoming attach: read the header, admit, write `"cci"`.
        let (attach_fd, server_fd) = unix_socketpair_fd();
        let server = std::thread::spawn(move || {
            let mut s = UnixStream::from(server_fd);
            server_accept(&mut s, PROTOCOL_V2).map(|_| s)
        });
        let attach = UnixTransport::from_stream(UnixStream::from(attach_fd)).expect("transport");
        session
            .inner
            .shared
            .fail_spawn
            .store(true, Ordering::SeqCst);
        let added = session.add_incoming_connection_android13plus_transport(
            move || Ok(Box::new(attach) as Box<dyn RpcTransport>),
            PROTOCOL_V2,
            &[7u8; 32],
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        let _server_side = server.join().expect("server half").expect("handshake");

        assert!(added.is_err(), "the spawn failure is reported");
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "the server pooled the connection as a callback slot, so the session must end"
        );
        assert_eq!(session.inner.slot_count(), 0, "the dead pool is empty");
        assert_eq!(session.inner.incoming_live.load(Ordering::SeqCst), 0);
        // The founding connection went down with the session: its peer reads end of stream.
        let mut byte = [0u8; 1];
        assert_eq!(
            std::io::Read::read(&mut founding_peer, &mut byte).expect("read"),
            0,
            "the founding connection's peer sees the session end"
        );
    }

    /// A v2 initiator session over a socketpair, with its founding connection's peer.
    fn v2_initiator() -> (RpcSession, std::os::unix::net::UnixStream) {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        let (client_fd, founding_peer_fd) = unix_socketpair_fd();
        let session = RpcSession::with_profile(
            Box::new(UnixTransport::from_stream(UnixStream::from(client_fd)).expect("transport")),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        seed_server_id(&session);
        (session, UnixStream::from(founding_peer_fd))
    }

    /// Stands in for `get_session_id`: these tests' attaches echo `[7; 32]`.
    fn seed_server_id(session: &RpcSession) {
        *session.server_id_lock() = Some(vec![7u8; 32]);
    }

    /// Runs an incoming attach whose server half `server` drives by hand over a socketpair.
    fn incoming_attach_against(
        session: &RpcSession,
        max_version: u32,
        deadline: Duration,
        wrap: impl FnOnce(super::super::transport::UnixTransport) -> Box<dyn RpcTransport>,
        server: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
    ) -> Result<u64> {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        let (attach_fd, server_fd) = unix_socketpair_fd();
        let server_side = armed_server_side(server_fd);
        let server = std::thread::spawn(move || server(server_side));
        let attach = wrap(UnixTransport::from_stream(UnixStream::from(attach_fd)).expect("unix"));
        let added = session.add_incoming_connection_android13plus_transport(
            move || Ok(attach),
            max_version,
            &[7u8; 32],
            FileDescriptorTransportMode::None,
            Some(deadline),
        );
        server.join().expect("server half");
        added
    }

    /// The server half with a 10 s read timeout, armed while the client cannot have closed yet.
    fn armed_server_side(server_fd: OwnedFd) -> std::os::unix::net::UnixStream {
        let s = std::os::unix::net::UnixStream::from(server_fd);
        // macOS refuses `SO_RCVTIMEO` (EINVAL) on a socket whose peer already closed.
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        s
    }

    /// Blocks until the attaching client drops its end; panics if it keeps it for 10 s.
    fn hold_until_client_closes(mut s: std::os::unix::net::UnixStream) {
        let mut rest = Vec::new();
        if let Err(e) = std::io::Read::read_to_end(&mut s, &mut rest) {
            assert!(
                !matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
                "the client kept the connection for 10 s: its attach must have pooled it"
            );
        }
    }

    /// Reads the client's header, writes the first byte of `"cci"`, then closes.
    fn cut_inside_cci(mut s: std::os::unix::net::UnixStream) {
        use crate::rpc::wire_android13::server_accept_deferred_init;
        let (codec, ..) = server_accept_deferred_init(&mut s, PROTOCOL_V2).expect("server header");
        std::io::Write::write_all(&mut s, &codec.encode_connection_init()[..1]).expect("cci byte");
    }

    /// A Unix connection that reports a close as a TLS end without `close_notify` does.
    struct NoCloseNotify(super::super::transport::UnixTransport);
    impl RpcTransport for NoCloseNotify {
        fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_frame(buf)
        }
        fn recv_frame(&self) -> RpcResult<Vec<u8>> {
            self.0.recv_frame()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            "no-close-notify"
        }
        fn supports_fd_passing(&self) -> bool {
            self.0.supports_fd_passing()
        }
        fn set_read_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            self.0.set_read_timeout(timeout)
        }
        fn set_write_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            self.0.set_write_timeout(timeout)
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.0.shutdown()
        }
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            match self.0.recv_raw(buf) {
                Ok(0) | Err(RpcError::EndOfStream) => Err(RpcError::UncleanEndOfStream),
                other => other,
            }
        }
    }

    /// A Unix connection that claims no fd passing: not the founding connection's kind.
    struct NoFds(super::super::transport::UnixTransport);
    impl RpcTransport for NoFds {
        fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_frame(buf)
        }
        fn recv_frame(&self) -> RpcResult<Vec<u8>> {
            self.0.recv_frame()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            "no-fds"
        }
        fn set_read_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            self.0.set_read_timeout(timeout)
        }
        fn set_write_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            self.0.set_write_timeout(timeout)
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.0.shutdown()
        }
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            self.0.recv_raw(buf)
        }
    }

    /// Reads nothing and asserts the client closed with no byte sent: no header, nothing held.
    fn expect_no_header(mut s: std::os::unix::net::UnixStream) {
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut s, &mut rest).expect("the client closes");
        assert!(rest.is_empty(), "the client sent {} bytes", rest.len());
    }

    /// A `max_version` below the session's is refused before connecting, so nothing is held.
    #[test]
    fn an_incoming_attach_below_the_session_version_never_connects() {
        let (session, _founding_peer) = v2_initiator();
        let added = session.add_incoming_connection_android13plus_transport(
            || -> Result<Box<dyn RpcTransport>> { panic!("the attach must not connect") },
            PROTOCOL_V1,
            &[7u8; 32],
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(added, Err(StatusCode::BadType));
        assert!(!session.inner.shared.lifecycle.is_torn_down());
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");
    }

    /// A transport unlike the founding one is refused before its header, so nothing is held.
    #[test]
    fn an_incoming_attach_on_another_transport_kind_is_refused_before_its_header() {
        let (session, _founding_peer) = v2_initiator();
        let added = incoming_attach_against(
            &session,
            PROTOCOL_V2,
            Duration::from_secs(5),
            |t| Box::new(NoFds(t)),
            expect_no_header,
        );
        assert_eq!(added, Err(StatusCode::BadType));
        assert!(!session.inner.shared.lifecycle.is_torn_down());
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");
    }

    /// No `"cci"` within the handshake deadline: the server may have pooled the connection.
    #[test]
    fn an_incoming_attach_whose_deadline_expires_before_cci_ends_the_session() {
        use crate::rpc::wire_android13::server_accept_deferred_init;
        let (session, _founding_peer) = v2_initiator();
        let added = incoming_attach_against(
            &session,
            PROTOCOL_V2,
            Duration::from_millis(200),
            |t| Box::new(t),
            |mut s| {
                // Reads the header and never answers, holding the connection open.
                server_accept_deferred_init(&mut s, PROTOCOL_V2).expect("server header");
                hold_until_client_closes(s);
            },
        );
        assert_eq!(added, Err(StatusCode::TimedOut));
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "the client cannot tell whether the server pooled it, so the session must end"
        );
        assert_eq!(session.inner.slot_count(), 0, "the dead pool is empty");
    }

    /// The server closes before `"cci"` (a refused attach): it holds nothing, the session goes on.
    #[test]
    fn an_incoming_attach_closed_before_cci_leaves_the_session_up() {
        use super::super::transport::UnixTransport;
        use crate::rpc::wire_android13::server_accept_deferred_init;
        let (session, founding_peer) = v2_initiator();
        let added = incoming_attach_against(
            &session,
            PROTOCOL_V2,
            Duration::from_secs(5),
            |t| Box::new(t),
            |mut s| {
                server_accept_deferred_init(&mut s, PROTOCOL_V2).expect("server header");
            },
        );
        assert_eq!(added, Err(StatusCode::DeadObject));
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "a server that closed before \"cci\" holds no slot, so the session goes on"
        );
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");

        // A call on the founding connection still gets its reply.
        let server = RpcSession::with_profile(
            Box::new(UnixTransport::from_stream(founding_peer).expect("transport")),
            AddressSpace::Acceptor,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("server session");
        let serving = std::thread::spawn(move || server.serve_blocking());
        session
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let reply = session.inner.client_transact(
            RpcAddress::zero(),
            SpecialTransaction::GetSessionId.code(),
            &Parcel::new(),
            0,
        );
        assert!(
            reply.is_ok(),
            "the founding connection still serves: {reply:?}"
        );
        session.close_session();
        let _end = serving.join().expect("server loop");
    }

    /// android-13+ fixes the fd mode in the header: `GET_FD_MODE` is refused, the mode unchanged.
    #[test]
    fn an_android13plus_server_refuses_get_fd_mode() {
        use super::super::transport::UnixTransport;
        let (a, b) = UnixTransport::pair().expect("socketpair");
        let v1 = || WireProfile::Android13Plus(Android13PlusCodec::android14_15());
        let server = RpcSession::with_profile(Box::new(a), AddressSpace::Acceptor, v1())
            .expect("server session");
        server.set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
        let server_inner = Arc::clone(&server.inner);
        let serving = std::thread::spawn(move || server.serve_blocking());
        let client = RpcSession::with_profile(Box::new(b), AddressSpace::Initiator, v1())
            .expect("client session");
        client
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut req = Parcel::new();
        req.write(&1i32).expect("want Unix");
        let reply = client.inner.client_transact(
            RpcAddress::zero(),
            SpecialTransaction::GetFdMode.code(),
            &req,
            0,
        );
        assert_eq!(reply.err(), Some(StatusCode::UnknownTransaction));
        assert_eq!(server_inner.fd_mode(), FileDescriptorTransportMode::None);
        // `Unix` the header did not agree is refused, not reported as a `None` fallback.
        assert_eq!(
            client.negotiate_fd_transport(FileDescriptorTransportMode::Unix),
            Err(StatusCode::InvalidOperation)
        );
        client.close_session();
        let _end = serving.join().expect("server loop");
    }

    /// android-13+ `negotiate_fd_transport` sends nothing: a peer that never answers sees no byte.
    #[test]
    fn android13plus_negotiate_fd_transport_sends_nothing() {
        use super::super::transport::UnixTransport;
        use std::io::Read;
        let (ours, mut peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let v1 = WireProfile::Android13Plus(Android13PlusCodec::android14_15());
        let t = UnixTransport::from_stream(ours).expect("transport");
        let client = RpcSession::with_profile(Box::new(t), AddressSpace::Initiator, v1)
            .expect("client session");
        client
            .set_reply_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(
            client.negotiate_fd_transport(FileDescriptorTransportMode::None),
            Ok(FileDescriptorTransportMode::None)
        );
        assert_eq!(
            client.negotiate_fd_transport(FileDescriptorTransportMode::Unix),
            Err(StatusCode::InvalidOperation)
        );
        assert_eq!(
            client.fd_transport_mode(),
            FileDescriptorTransportMode::None
        );
        peer.set_nonblocking(true).expect("nonblocking");
        let pending = peer.read(&mut [0u8; 1]);
        assert!(
            matches!(&pending, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "{pending:?}"
        );
        client.close_session();
    }

    /// An r34 client opens with the `-1` preamble, then `GET_ROOT` with no length prefix.
    #[test]
    fn r34_client_opens_with_the_preamble_and_an_unprefixed_get_root() {
        let (session, mut peer) = r34_client_with_raw_peer();
        session
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let call = std::thread::spawn(move || session.get_root().map(|_| ()));
        let mut got = [0u8; 4 + 16 + 64];
        std::io::Read::read_exact(&mut peer, &mut got).expect("the client's first bytes");
        let mut want = vec![0xff; 4]; // RPC_SESSION_ID_NEW
        want.extend_from_slice(&[0, 0, 0, 0]); // RpcWireHeader.command = TRANSACT
        want.extend_from_slice(&[64, 0, 0, 0]); // bodySize
        want.extend_from_slice(&[0; 8]); // reserved
        want.extend_from_slice(&[0; 32]); // RpcWireTransaction.address (special)
        want.extend_from_slice(&[0, 0, 0, 0]); // code = RPC_SPECIAL_TRANSACT_GET_ROOT
        want.extend_from_slice(&[0; 4 + 8 + 16]); // flags, asyncNumber, reserved
        assert_eq!(got.to_vec(), want);
        drop(peer);
        assert!(call.join().expect("caller").is_err(), "the peer closed");
    }

    /// An r34 acceptor takes the preamble, then AOSP frames; `GET_ROOT` with no root is null.
    #[test]
    fn r34_acceptor_takes_the_preamble_then_unprefixed_messages() {
        use super::super::transport::UnixTransport;
        let (a, peer) = UnixTransport::pair().expect("socketpair");
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("session");
        let serving = std::thread::spawn(move || server.serve_blocking());
        let get_root = R34Codec
            .encode_transact(&WireTransaction {
                address: RpcAddress::zero(),
                code: SpecialTransaction::GetRoot.code(),
                ..WireTransaction::default()
            })
            .expect("GET_ROOT");
        peer.send_raw(&(-1i32).to_le_bytes()).expect("preamble");
        peer.send_raw(&get_root).expect("GET_ROOT");
        let reply = read_aosp_message(&mut RawTransportIo(&peer)).expect("reply");
        match R34Codec.decode_message(&reply).expect("decode") {
            WireMessage::Reply(r) => {
                assert_eq!(r.status, 0);
                // Null, then `UNDECLARED` as an android-12 `Category`.
                assert_eq!(r.data, [0, 0, 0, 0, 1, 0, 0, 0]);
            }
            other => panic!("expected a REPLY, got {other:?}"),
        }
        drop(peer);
        assert_eq!(
            serving.join().expect("serve").reason,
            EndReason::EndOfStream
        );
    }

    /// An android-12 root reply becomes a proxy; a version-0 `Category` is refused, receipt paid.
    #[test]
    fn r34_client_reads_an_aosp12_root_reply() {
        let addr = [0x5a_u8; RPC_ADDR_LEN];
        for (stability, accepted) in [(0x0c00_0001_i32, true), (0x0c00_0000, false)] {
            let (session, mut peer) = r34_client_with_raw_peer();
            session
                .set_reply_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("peer read timeout");
            let inner = Arc::clone(&session.inner);
            let call = std::thread::spawn(move || {
                let mut reply = inner
                    .client_transact(
                        RpcAddress::zero(),
                        SpecialTransaction::GetRoot.code(),
                        &Parcel::new(),
                        0,
                    )?
                    .ok_or(StatusCode::UnexpectedNull)?;
                let root = reply.read::<SIBinder>();
                let at_end = reply.data_position() == reply.data_size();
                root.map(|_| at_end)
            });
            assert_eq!(
                read_r34_session_preamble(&mut peer).expect("preamble"),
                RPC_SESSION_ID_NEW
            );
            read_aosp_message(&mut peer).expect("GET_ROOT");
            let mut data = 1i32.to_le_bytes().to_vec();
            data.extend_from_slice(&addr);
            data.extend_from_slice(&stability.to_le_bytes());
            let reply = R34Codec
                .encode_reply(&WireReply {
                    status: 0,
                    data,
                    ..WireReply::default()
                })
                .expect("reply");
            std::io::Write::write_all(&mut peer, &reply).expect("reply");
            let got = call.join().expect("caller");
            if accepted {
                assert_eq!(got, Ok(true), "a proxy, and the cursor past the stability");
            } else {
                assert_eq!(got, Err(StatusCode::BadType), "stability {stability:#x}");
                let release = read_aosp_message(&mut peer).expect("the refused root's release");
                match R34Codec.decode_message(&release).expect("decode") {
                    WireMessage::DecStrong(a, 1) => assert_eq!(*a.as_wire_bytes(), addr),
                    other => panic!("expected a DEC_STRONG, got {other:?}"),
                }
            }
        }
    }

    /// A bare r34 acceptor answers `GET_SESSION_ID` as unknown: only an `RpcServer` mints an id.
    #[test]
    fn a_bare_r34_acceptor_has_no_session_id() {
        use super::super::transport::MemTransport;
        let (a, b) = MemTransport::pair();
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("server");
        let serving = std::thread::spawn(move || server.serve_blocking());
        let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("client");
        client
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(client.get_session_id(), Err(StatusCode::UnknownTransaction));
        client.close_session();
        assert!(
            serving.join().expect("serve").is_clean(),
            "the client closed"
        );
    }

    /// A bare r34 acceptor refuses every preamble but `-1`, a length prefix's first word included.
    #[test]
    fn r34_acceptor_refuses_any_preamble_but_new() {
        use super::super::transport::UnixTransport;
        for first in [5i32.to_le_bytes(), 80u32.to_le_bytes()] {
            let (a, peer) = UnixTransport::pair().expect("socketpair");
            let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("session");
            peer.send_raw(&first).expect("preamble");
            let end = server.serve_blocking();
            assert!(
                matches!(end.reason, EndReason::Frame(_)),
                "refused, got {:?}",
                end.reason
            );
            assert_eq!(peer.recv_raw(&mut [0u8; 4]).expect("closed"), 0);
        }
    }

    /// An r34 peer without the extension (AOSP libbinder) answers `UNKNOWN_TRANSACTION`: `None`.
    #[test]
    fn r34_get_fd_mode_unknown_to_the_peer_negotiates_none() {
        use super::super::transport::UnixTransport;
        let (a, peer) = UnixTransport::pair().expect("socketpair");
        let client = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        client
            .set_reply_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let answering = std::thread::spawn(move || {
            let mut io = RawTransportIo(&peer);
            let id = read_r34_session_preamble(&mut io).expect("preamble");
            assert_eq!(id, RPC_SESSION_ID_NEW);
            let frame = read_aosp_message(&mut io).expect("GET_FD_MODE");
            match R34Codec.decode_message(&frame).expect("decode") {
                WireMessage::Transact(t) => {
                    assert_eq!(t.code, SpecialTransaction::GetFdMode.code())
                }
                other => panic!("expected GET_FD_MODE, got {other:?}"),
            }
            let reply = WireReply {
                status: StatusCode::UnknownTransaction.into(),
                ..WireReply::default()
            };
            let reply = R34Codec.encode_reply(&reply).expect("encode");
            peer.send_raw(&reply).expect("reply");
            peer
        });
        assert_eq!(
            client.negotiate_fd_transport(FileDescriptorTransportMode::Unix),
            Ok(FileDescriptorTransportMode::None)
        );
        assert_eq!(
            client.fd_transport_mode(),
            FileDescriptorTransportMode::None
        );
        let _peer = answering.join().expect("peer");
        client.close_session();
    }

    /// A `"cci"` byte shows the server pooled the connection: a cut after it ends the session.
    #[test]
    fn an_incoming_attach_cut_inside_cci_ends_the_session() {
        use super::super::transport::UnixTransport;
        type Wrap = fn(UnixTransport) -> Box<dyn RpcTransport>;
        let wraps: [(&str, Wrap); 2] = [
            ("EOF", |t| Box::new(t)),
            ("no close_notify", |t| Box::new(NoCloseNotify(t))),
        ];
        for (close, wrap) in wraps {
            let (session, _founding_peer) = v2_initiator();
            let added = incoming_attach_against(
                &session,
                PROTOCOL_V2,
                Duration::from_secs(5),
                wrap,
                cut_inside_cci,
            );
            assert!(added.is_err(), "{close}: {added:?}");
            assert!(
                session.inner.shared.lifecycle.is_torn_down(),
                "{close} after one \"cci\" byte: the server pooled it, so the session must end"
            );
            assert_eq!(
                session.inner.slot_count(),
                0,
                "{close}: the dead pool is empty"
            );
        }
    }

    /// A TLS end without `close_notify` before any `"cci"` byte is a close like EOF.
    #[test]
    fn an_incoming_attach_uncleanly_closed_before_cci_leaves_the_session_up() {
        use crate::rpc::wire_android13::server_accept_deferred_init;
        let (session, _founding_peer) = v2_initiator();
        let added = incoming_attach_against(
            &session,
            PROTOCOL_V2,
            Duration::from_secs(5),
            |t| Box::new(NoCloseNotify(t)),
            |mut s| {
                server_accept_deferred_init(&mut s, PROTOCOL_V2).expect("server header");
            },
        );
        assert!(added.is_err(), "{added:?}");
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "a server that closed before any \"cci\" byte holds no slot, so the session goes on"
        );
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");
    }

    /// A refusal ahead of the header (an authorizer) closes it unread: the client reads a reset.
    #[test]
    fn an_incoming_attach_reset_before_cci_leaves_the_session_up() {
        // The premise: closing with unread bytes gives the peer `ECONNRESET`, not EOF (XNU: EOF).
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::io::{Read, Write};
            use std::os::unix::net::UnixStream;
            let (mut writer, reader) = UnixStream::pair().expect("pair");
            writer.write_all(b"header").expect("write");
            drop(reader);
            let premise = writer.read(&mut [0u8; 1]).map_err(|e| e.kind());
            assert_eq!(premise, Err(std::io::ErrorKind::ConnectionReset));
        }

        let (session, _founding_peer) = v2_initiator();
        let added = incoming_attach_against(
            &session,
            PROTOCOL_V2,
            Duration::from_secs(5),
            |t| Box::new(t),
            |s| {
                // Waits for the header without reading it, then closes as a refusal does.
                let mut probe = [0u8; 1];
                rustix::net::recv(&s, &mut probe[..], rustix::net::RecvFlags::PEEK).expect("peek");
            },
        );
        assert_eq!(added, Err(StatusCode::DeadObject));
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "a reset before any \"cci\" byte may be a refusal, so the session goes on"
        );
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");
    }

    /// Runs an outgoing attach whose server half `server` drives by hand over a socketpair.
    fn outgoing_attach_against(
        session: &RpcSession,
        deadline: Duration,
        wrap: impl FnOnce(super::super::transport::UnixTransport) -> Box<dyn RpcTransport>,
        server: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
    ) -> Result<u64> {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        let (attach_fd, server_fd) = unix_socketpair_fd();
        let server_side = armed_server_side(server_fd);
        let server = std::thread::spawn(move || server(server_side));
        let attach = wrap(UnixTransport::from_stream(UnixStream::from(attach_fd)).expect("unix"));
        let added = session.add_outgoing_connection_android13plus_transport(
            move || Ok(attach),
            PROTOCOL_V2,
            &[7u8; 32],
            FileDescriptorTransportMode::None,
            Some(deadline),
        );
        server.join().expect("server half");
        added
    }

    /// Reads the client's header, its `"cci"` and its `GET_SESSION_ID` probe.
    fn read_probe(s: &mut std::os::unix::net::UnixStream) -> Android13PlusCodec {
        use crate::rpc::wire_android13::server_accept;
        let (codec, ..) = server_accept(s, PROTOCOL_V2).expect("server handshake");
        read_aosp_message(s).expect("probe");
        codec
    }

    /// Answers the probe with `id`, as a server that added the attach to that session does.
    fn answer_probe(s: &mut std::os::unix::net::UnixStream, id: &[u8]) {
        let codec = read_probe(s);
        let mut p = Parcel::new();
        p.write(id).expect("id");
        let reply = WireReply {
            status: 0,
            data: p.rpc_data_bytes().to_vec(),
            object_positions: Vec::new(),
        };
        write_aosp_message(s, &codec.encode_reply(&reply).expect("encode")).expect("reply");
    }

    /// A Unix connection whose `send`th raw send or `arm`th read-deadline arm fails (0-based).
    struct FailsAt {
        t: super::super::transport::UnixTransport,
        send: usize,
        arm: usize,
        sends: AtomicUsize,
        arms: AtomicUsize,
        /// The error the failing send returns.
        err: fn() -> RpcError,
    }
    impl FailsAt {
        fn new(t: super::super::transport::UnixTransport, send: usize, arm: usize) -> Self {
            FailsAt {
                t,
                send,
                arm,
                sends: AtomicUsize::new(0),
                arms: AtomicUsize::new(0),
                err: Self::injected,
            }
        }
        /// The failing send returns `err` instead of an injected `Io`.
        fn failing_with(mut self, err: fn() -> RpcError) -> Self {
            self.err = err;
            self
        }
        fn injected() -> RpcError {
            RpcError::Io(std::io::Error::other("injected"))
        }
    }
    impl RpcTransport for FailsAt {
        fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
            self.t.send_frame(buf)
        }
        fn recv_frame(&self) -> RpcResult<Vec<u8>> {
            self.t.recv_frame()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.t.peer_identity()
        }
        fn describe(&self) -> &str {
            "fails-at"
        }
        fn supports_fd_passing(&self) -> bool {
            self.t.supports_fd_passing()
        }
        fn set_read_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            if timeout.is_some() && self.arms.fetch_add(1, Ordering::SeqCst) == self.arm {
                return Err(Self::injected());
            }
            self.t.set_read_timeout(timeout)
        }
        fn set_write_timeout(&self, timeout: Option<Duration>) -> RpcResult<()> {
            self.t.set_write_timeout(timeout)
        }
        fn shutdown(&self) -> RpcResult<()> {
            self.t.shutdown()
        }
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            if self.sends.fetch_add(1, Ordering::SeqCst) == self.send {
                return Err((self.err)());
            }
            self.t.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            self.t.recv_raw(buf)
        }
    }

    /// Asserts the attach failed and ended the session, or failed and left it up.
    fn assert_attach_outcome(session: &RpcSession, added: Result<u64>, ends: bool, why: &str) {
        assert!(added.is_err(), "{why}: {added:?}");
        assert_eq!(
            session.inner.shared.lifecycle.is_torn_down(),
            ends,
            "{why}: the session must {}",
            if ends { "end" } else { "go on" }
        );
        assert_eq!(
            session.inner.slot_count(),
            usize::from(!ends),
            "{why}: pool size"
        );
    }

    /// A `max_version` below the session's is refused before connecting, so nothing is held.
    #[test]
    fn an_outgoing_attach_below_the_session_version_never_connects() {
        let (session, _founding_peer) = v2_initiator();
        let added = session.add_outgoing_connection_android13plus_transport(
            || -> Result<Box<dyn RpcTransport>> { panic!("the attach must not connect") },
            PROTOCOL_V1,
            &[7u8; 32],
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(added, Err(StatusCode::BadType));
        assert_attach_outcome(&session, added, false, "below the session version");
    }

    /// A header write that fails leaves the server short of it: no server holds the connection.
    #[test]
    fn an_outgoing_attach_whose_header_write_fails_leaves_the_session_up() {
        let (session, _founding_peer) = v2_initiator();
        let added = outgoing_attach_against(
            &session,
            Duration::from_secs(5),
            |t| Box::new(FailsAt::new(t, 0, usize::MAX)),
            hold_until_client_closes,
        );
        assert_attach_outcome(&session, added, false, "header write failed");
    }

    /// Past the header a server may hold it: a failed `"cci"`, a failed probe arm, an expiry.
    #[test]
    fn an_outgoing_attach_failing_past_its_header_ends_the_session() {
        use super::super::transport::UnixTransport;
        type Wrap = fn(UnixTransport) -> Box<dyn RpcTransport>;
        // Arm 0 is the handshake's own deadline, arm 1 the probe's.
        let cases: [(&str, Wrap, Duration); 3] = [
            (
                "\"cci\" write",
                |t| Box::new(FailsAt::new(t, 1, usize::MAX)),
                Duration::from_secs(5),
            ),
            (
                "probe deadline arm",
                |t| Box::new(FailsAt::new(t, usize::MAX, 1)),
                Duration::from_secs(5),
            ),
            (
                "probe deadline expiry",
                |t| Box::new(t),
                Duration::from_millis(200),
            ),
        ];
        for (why, wrap, deadline) in cases {
            let (session, _founding_peer) = v2_initiator();
            let added = outgoing_attach_against(&session, deadline, wrap, |mut s| {
                // Reads what the client sends and never answers, holding the connection open.
                let _ = crate::rpc::wire_android13::server_accept(&mut s, PROTOCOL_V2);
                hold_until_client_closes(s);
            });
            assert_attach_outcome(&session, added, true, why);
        }
    }

    /// A close before any reply byte may be a refusal that ended the server's session too.
    #[test]
    fn an_outgoing_attach_closed_before_the_reply_ends_the_session() {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        type Wrap = fn(UnixTransport) -> Box<dyn RpcTransport>;
        type Server = fn(UnixStream);
        fn closes_after_the_probe(mut s: UnixStream) {
            read_probe(&mut s);
        }
        fn holds_after_the_header(mut s: UnixStream) {
            let _ = crate::rpc::wire_android13::server_accept(&mut s, PROTOCOL_V2);
            hold_until_client_closes(s);
        }
        // Send 1 is the `"cci"` write, send 2 the probe's.
        let cases: [(&str, Wrap, Server); 4] = [
            ("EOF", |t| Box::new(t), closes_after_the_probe),
            (
                "no close_notify",
                |t| Box::new(NoCloseNotify(t)),
                closes_after_the_probe,
            ),
            (
                "close at the \"cci\" write",
                |t| Box::new(FailsAt::new(t, 1, usize::MAX).failing_with(|| RpcError::EndOfStream)),
                holds_after_the_header,
            ),
            (
                "close at the probe write",
                |t| Box::new(FailsAt::new(t, 2, usize::MAX).failing_with(|| RpcError::EndOfStream)),
                holds_after_the_header,
            ),
        ];
        for (why, wrap, server) in cases {
            let (session, _founding_peer) = v2_initiator();
            let added = outgoing_attach_against(&session, Duration::from_secs(5), wrap, server);
            assert_attach_outcome(&session, added, true, why);
        }
    }

    /// A reply byte shows the server added the connection: a cut after it ends the session.
    #[test]
    fn an_outgoing_attach_cut_inside_the_reply_ends_the_session() {
        use super::super::transport::UnixTransport;
        type Wrap = fn(UnixTransport) -> Box<dyn RpcTransport>;
        let wraps: [(&str, Wrap); 2] = [
            ("EOF", |t| Box::new(t)),
            ("no close_notify", |t| Box::new(NoCloseNotify(t))),
        ];
        for (why, wrap) in wraps {
            let (session, _founding_peer) = v2_initiator();
            let added = outgoing_attach_against(&session, Duration::from_secs(5), wrap, |mut s| {
                read_probe(&mut s);
                std::io::Write::write_all(&mut s, &[1u8]).expect("reply byte");
            });
            assert_attach_outcome(&session, added, true, why);
        }
    }

    /// A reply naming another session ends it.
    #[test]
    fn an_outgoing_attach_refused_after_the_reply_ends_the_session() {
        let (session, _founding_peer) = v2_initiator();
        let added = outgoing_attach_against(
            &session,
            Duration::from_secs(5),
            |t| Box::new(t),
            |mut s| {
                answer_probe(&mut s, &[8u8; 32]);
                hold_until_client_closes(s);
            },
        );
        assert_attach_outcome(&session, added, true, "another session's id");
    }

    /// A transport unlike the founding one is refused before its header, so nothing is held.
    #[test]
    fn an_outgoing_attach_on_another_transport_kind_is_refused_before_its_header() {
        let (session, _founding_peer) = v2_initiator();
        let added = outgoing_attach_against(
            &session,
            Duration::from_secs(5),
            |t| Box::new(NoFds(t)),
            expect_no_header,
        );
        assert_eq!(added, Err(StatusCode::BadType));
        assert_attach_outcome(&session, added, false, "another transport kind");
    }

    /// The client-local id is never on the wire: refused before connecting, so nothing is held.
    #[test]
    fn an_outgoing_attach_echoing_the_client_local_id_never_connects() {
        let (session, _founding_peer) = v2_initiator();
        let added = session.add_outgoing_connection_android13plus_transport(
            || -> Result<Box<dyn RpcTransport>> { panic!("the attach must not connect") },
            PROTOCOL_V2,
            &session.session_id(),
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(added, Err(StatusCode::BadValue));
        assert_attach_outcome(&session, added, false, "client-local id");
    }

    /// An attach naming another server session's id never connects, outgoing or incoming.
    #[test]
    fn an_attach_naming_another_server_session_never_connects() {
        let (session, _founding_peer) = v2_initiator();
        let added = session.add_outgoing_connection_android13plus_transport(
            || -> Result<Box<dyn RpcTransport>> { panic!("the attach must not connect") },
            PROTOCOL_V2,
            &[9u8; 32],
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(added, Err(StatusCode::BadValue));
        assert_attach_outcome(&session, added, false, "foreign id, outgoing");
        let added = session.add_incoming_connection_android13plus_transport(
            || -> Result<Box<dyn RpcTransport>> { panic!("the attach must not connect") },
            PROTOCOL_V2,
            &[9u8; 32],
            FileDescriptorTransportMode::None,
            Some(Duration::from_secs(5)),
        );
        assert_eq!(added, Err(StatusCode::BadValue));
        assert_attach_outcome(&session, added, false, "foreign id, incoming");
    }

    /// The teardown gate is read under `conn_state` with the cap: a dead session admits no slot.
    #[test]
    fn callback_slot_refused_on_torn_down_session() {
        use crate::rpc::transport::MemTransport;
        let (t0, _p0) = MemTransport::pair();
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");
        session.close_session();

        let (t, _p) = MemTransport::pair();
        assert_eq!(
            session.inner.add_slot_inner_capped(Box::new(t), 2),
            Err(StatusCode::DeadObject),
            "no slot may be pushed onto a torn-down session"
        );
        assert_eq!(session.inner.slot_count(), 0, "the dead pool stays empty");
    }

    /// A failed send ends the whole fan-out session, not only the connection it rode.
    #[test]
    fn a_failed_send_ends_the_whole_session() {
        use super::super::transport::UnixTransport;
        let (a, pa) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let (b, _pb) = UnixTransport::pair().expect("socketpair");
        session
            .inner
            .add_outgoing_slot(Box::new(b))
            .expect("fan-out slot");
        // The founding slot is the first free `Outgoing` one; its peer is gone (`EPIPE`).
        drop(pa);

        let err = session
            .inner
            .client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &Parcel::new(),
                0,
            )
            .expect_err("the send must fail");
        assert_eq!(err, StatusCode::DeadObject);
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "the other outgoing connection does not keep the session up"
        );
        assert_eq!(session.inner.slot_count(), 0);
    }

    /// Every slot, founding and added, follows the session's two values; module doc "Liveness".
    #[test]
    fn liveness_follows_the_session_values_on_every_slot() {
        #[derive(Default)]
        struct Seen {
            send: Mutex<Vec<Option<Duration>>>,
            live: Mutex<Vec<Option<Duration>>>,
        }
        impl Seen {
            fn last(&self) -> (Option<Duration>, Option<Duration>) {
                let send = *self
                    .send
                    .lock()
                    .unwrap()
                    .last()
                    .expect("send deadline armed");
                let live = *self.live.lock().unwrap().last().expect("liveness armed");
                (send, live)
            }
        }
        struct Recorder(Arc<Seen>);
        impl RpcTransport for Recorder {
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "recorder"
            }
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                Ok(())
            }
            fn set_write_timeout(&self, t: Option<Duration>) -> RpcResult<()> {
                self.0.send.lock().unwrap().push(t);
                Ok(())
            }
            fn set_liveness(&self, t: Option<Duration>) -> RpcResult<()> {
                self.0.live.lock().unwrap().push(t);
                Ok(())
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
        }
        let secs = |n| Some(Duration::from_secs(n));
        let founding = Arc::new(Seen::default());
        let session = RpcSession::new(
            Box::new(Recorder(Arc::clone(&founding))),
            AddressSpace::Initiator,
        )
        .expect("session");
        assert_eq!(founding.last(), (None, None), "keepalive with no timeout");

        session.set_reply_timeout(secs(8)).unwrap();
        assert_eq!(founding.last(), (secs(8), secs(8)));
        // A server's idle deadline bounds sends too; the smaller one applies.
        session.set_serve_read_deadline(secs(3));
        assert_eq!(founding.last(), (secs(3), secs(8)));

        let added = Arc::new(Seen::default());
        session
            .inner
            .add_outgoing_slot(Box::new(Recorder(Arc::clone(&added))))
            .expect("fan-out slot");
        assert_eq!(added.last(), (secs(3), secs(8)), "a slot added later");

        session.set_reply_timeout(None).unwrap();
        assert_eq!(founding.last(), (secs(3), None));
        assert_eq!(added.last(), (secs(3), None));
    }

    /// A slot armed across a concurrent `set_timeout` ends with the stored value; "Liveness".
    #[test]
    fn a_slot_armed_across_a_set_timeout_ends_with_the_stored_value() {
        #[derive(Default)]
        struct Gate {
            send: Mutex<Vec<Option<Duration>>>,
            entered: Mutex<Option<mpsc::Sender<()>>>,
            release: Mutex<Option<mpsc::Receiver<()>>>,
        }
        struct Held(Arc<Gate>);
        impl RpcTransport for Held {
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "held"
            }
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                Ok(())
            }
            // The hook: the first arm holds here, its value already read, until released.
            fn set_write_timeout(&self, t: Option<Duration>) -> RpcResult<()> {
                let entered = self.0.entered.lock().unwrap().take();
                if let Some(entered) = entered {
                    let _ = entered.send(());
                    let release = self.0.release.lock().unwrap().take();
                    let _ = release.expect("release").recv();
                }
                self.0.send.lock().unwrap().push(t);
                Ok(())
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
        }
        let secs = |n| Some(Duration::from_secs(n));
        let session = RpcSession::new(
            Box::new(Held(Arc::new(Gate::default()))),
            AddressSpace::Initiator,
        )
        .expect("session");
        session.set_reply_timeout(secs(8)).unwrap();

        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let added = Arc::new(Gate {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
            ..Gate::default()
        });
        let push = {
            let inner = Arc::clone(&session.inner);
            let slot = Box::new(Held(Arc::clone(&added)));
            std::thread::spawn(move || inner.add_outgoing_slot(slot).map(|_| ()))
        };
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the push must arm its slot");
        let (stored_tx, stored) = mpsc::channel();
        {
            let session = session.clone();
            std::thread::spawn(move || {
                session.set_reply_timeout(secs(3)).unwrap();
                let _ = stored_tx.send(());
            });
        }
        assert!(
            stored.recv_timeout(Duration::from_millis(300)).is_err(),
            "the store must wait for the slot being armed"
        );
        release.send(()).expect("release");
        push.join().expect("push").expect("fan-out slot");
        stored
            .recv_timeout(Duration::from_secs(10))
            .expect("set_timeout must finish");
        assert_eq!(added.send.lock().unwrap().last(), Some(&secs(3)));
    }

    /// A `set_timeout` during teardown leaves `shutdown`'s own send bound; module doc "Liveness".
    #[test]
    fn a_set_timeout_during_teardown_leaves_the_shutdown_send_bound() {
        const CLOSE_BOUND: Option<Duration> = Some(Duration::from_millis(500));
        #[derive(Default)]
        struct Gate {
            send: Mutex<Vec<Option<Duration>>>,
            at_write: Mutex<Option<Option<Duration>>>,
            entered: Mutex<Option<mpsc::Sender<()>>>,
            release: Mutex<Option<mpsc::Receiver<()>>>,
        }
        struct Closing(Arc<Gate>);
        impl RpcTransport for Closing {
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "closing"
            }
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                Ok(())
            }
            fn set_write_timeout(&self, t: Option<Duration>) -> RpcResult<()> {
                self.0.send.lock().unwrap().push(t);
                Ok(())
            }
            // The hook: TLS `shutdown`'s bound, then its closing write, with the gap held open.
            fn shutdown(&self) -> RpcResult<()> {
                self.set_write_timeout(CLOSE_BOUND)?;
                let entered = self.0.entered.lock().unwrap().take();
                if let Some(entered) = entered {
                    let _ = entered.send(());
                    let release = self.0.release.lock().unwrap().take();
                    let _ = release.expect("release").recv();
                }
                let now = *self.0.send.lock().unwrap().last().expect("bound set");
                *self.0.at_write.lock().unwrap() = Some(now);
                Ok(())
            }
        }
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let gate = Arc::new(Gate {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
            ..Gate::default()
        });
        let session = RpcSession::new(
            Box::new(Closing(Arc::clone(&gate))),
            AddressSpace::Initiator,
        )
        .expect("session");

        let teardown = {
            let inner = Arc::clone(&session.inner);
            std::thread::spawn(move || inner.fail_session())
        };
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("teardown must shut the transport down");
        let (stored_tx, stored) = mpsc::channel();
        {
            let session = session.clone();
            std::thread::spawn(move || {
                session.set_reply_timeout(None).unwrap();
                let _ = stored_tx.send(());
            });
        }
        // Room for an unserialized `set_timeout` to land inside the gap.
        let early = stored.recv_timeout(Duration::from_millis(300)).is_ok();
        release.send(()).expect("release");
        teardown.join().expect("teardown");
        if !early {
            stored
                .recv_timeout(Duration::from_secs(10))
                .expect("set_timeout must finish");
        }
        assert_eq!(
            *gate.at_write.lock().unwrap(),
            Some(CLOSE_BOUND),
            "the closing write must run under shutdown's own bound"
        );
    }

    /// A peer that stops reading ends the session one send deadline later; module doc "Liveness".
    #[test]
    fn a_send_the_peer_stops_reading_ends_the_session() {
        use super::super::transport::UnixTransport;
        let (a, _unread) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        session
            .set_reply_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut data = Parcel::new();
        // Far past any socket buffer, so the send has to wait for the peer.
        data.write(&vec![0u8; 8 << 20]).expect("payload");

        let (tx, rx) = mpsc::channel();
        let inner = Arc::clone(&session.inner);
        std::thread::spawn(move || {
            let r = inner.client_transact(RpcAddress::zero(), 1, &data, FLAG_ONEWAY);
            let _ = tx.send(r.map(|_| ()));
        });
        // Bounded, so a missing send deadline fails the test instead of hanging it.
        let sent = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the send must give up");
        assert_eq!(
            sent,
            Err(StatusCode::TimedOut),
            "a send deadline, mid-frame"
        );
        assert!(session.inner.shared.lifecycle.is_torn_down());
    }

    /// Only a `DEC_STRONG` may arrive while a transaction's send waits (AOSP `CONTROL_ONLY`).
    #[test]
    fn a_request_or_reply_read_while_a_send_waits_ends_the_session() {
        use super::super::transport::UnixTransport;
        let txn = R34Codec
            .encode_transact_ref(WireTransactionRef {
                address: &RpcAddress::zero(),
                code: 1,
                flags: FLAG_ONEWAY,
                async_number: 0,
                data: &[],
                object_positions: &[],
            })
            .expect("TRANSACT");
        let reply = R34Codec
            .encode_reply_ref(WireReplyRef {
                status: 0,
                data: &[],
                object_positions: &[],
            })
            .expect("REPLY");
        for (frame, want) in [(txn, StatusCode::BadType), (reply, StatusCode::DeadObject)] {
            let (a, peer) = UnixTransport::pair().expect("socketpair");
            let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
            // Written before the send and never followed by a read of ours.
            peer.send_raw(&frame).expect("peer frame");
            let sent = oneway_past_the_buffer(&session).expect("the send must stop at the frame");
            assert_eq!(sent, Err(want));
            assert!(session.inner.shared.lifecycle.is_torn_down());
        }
    }

    /// r34 judges a drained message by its header too: a body that never comes holds nothing.
    #[test]
    fn an_r34_drained_header_is_refused_before_its_body() {
        for (command, body_size, want) in [
            (0, 1000, StatusCode::BadType),
            (1, 1000, StatusCode::DeadObject),
            // r34's `DEC_STRONG` body is the 32-byte address alone.
            (2, 16, StatusCode::BadValue),
            (2, A13_DEC_STRONG_LEN as u32, StatusCode::BadValue),
            (9, RPC_ADDR_LEN as u32, StatusCode::DeadObject),
        ] {
            let (session, mut peer) = r34_client_with_raw_peer();
            std::io::Write::write_all(&mut peer, &a13_header(command, body_size)).expect("header");
            let sent = oneway_past_the_buffer(&session)
                .unwrap_or_else(|| panic!("command {command}: the send waited for the body"));
            assert_eq!(sent, Err(want), "command {command}, body {body_size}");
            assert!(session.inner.shared.lifecycle.is_torn_down());
        }
    }

    /// An r34 client session whose peer is a raw socket the test scripts; the preamble is unread.
    fn r34_client_with_raw_peer() -> (RpcSession, std::os::unix::net::UnixStream) {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        let (client_fd, peer_fd) = unix_socketpair_fd();
        let session = RpcSession::new(
            Box::new(UnixTransport::from_stream(UnixStream::from(client_fd)).expect("transport")),
            AddressSpace::Initiator,
        )
        .expect("session");
        (session, UnixStream::from(peer_fd))
    }

    /// An 8 MiB oneway from another thread, far past any socket buffer; `None` if still sending.
    fn oneway_past_the_buffer(session: &RpcSession) -> Option<Result<()>> {
        let mut data = Parcel::new();
        data.write(&vec![0u8; 8 << 20]).expect("payload");
        let (tx, rx) = mpsc::channel();
        let inner = Arc::clone(&session.inner);
        std::thread::spawn(move || {
            let r = inner.client_transact(RpcAddress::zero(), 1, &data, FLAG_ONEWAY);
            let _ = tx.send(r.map(|_| ()));
        });
        let sent = rx.recv_timeout(Duration::from_secs(10)).ok();
        if sent.is_none() {
            // Unblocks the sender, so a failing run does not leave it parked.
            session.close_session();
        }
        sent
    }

    /// An android-13+ client session whose peer is a raw socket the test scripts.
    fn a13_client_with_raw_peer() -> (RpcSession, std::os::unix::net::UnixStream) {
        use super::super::transport::UnixTransport;
        use std::os::unix::net::UnixStream;
        let (client_fd, peer_fd) = unix_socketpair_fd();
        let session = RpcSession::with_profile(
            Box::new(UnixTransport::from_stream(UnixStream::from(client_fd)).expect("transport")),
            AddressSpace::Initiator,
            WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")),
        )
        .expect("session");
        (session, UnixStream::from(peer_fd))
    }

    /// A 16-byte `RpcWireHeader` announcing `body_size` bytes that never follow.
    fn a13_header(command: u32, body_size: u32) -> [u8; 16] {
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&command.to_le_bytes());
        h[4..8].copy_from_slice(&body_size.to_le_bytes());
        h
    }

    /// android-13+ judges a drained message by its header: a body that never comes holds nothing.
    #[test]
    fn a_drained_header_is_refused_before_its_body() {
        for (command, body_size, want) in [
            (0, 1000, StatusCode::BadType),
            (1, 1000, StatusCode::DeadObject),
            (2, 1000, StatusCode::BadValue),
            (9, 1000, StatusCode::DeadObject),
        ] {
            let (session, mut peer) = a13_client_with_raw_peer();
            std::io::Write::write_all(&mut peer, &a13_header(command, body_size)).expect("header");
            let sent = oneway_past_the_buffer(&session)
                .unwrap_or_else(|| panic!("command {command}: the send waited for the body"));
            assert_eq!(sent, Err(want), "command {command}");
            assert!(session.inner.shared.lifecycle.is_torn_down());
        }
    }

    /// A drained message cut short fails the send at the send deadline instead of never.
    #[test]
    fn a_peer_that_stalls_inside_a_drained_message_fails_the_send_at_its_deadline() {
        let (session, mut peer) = a13_client_with_raw_peer();
        session
            .set_reply_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let len = crate::rpc::wire_android13::A13_DEC_STRONG_LEN as u32;
        let half = &a13_header(2, len)[..8];
        std::io::Write::write_all(&mut peer, half).expect("half a header");
        let sent = oneway_past_the_buffer(&session).expect("the send must end at its deadline");
        assert_eq!(sent, Err(StatusCode::TimedOut));
        assert!(session.inner.shared.lifecycle.is_torn_down());
    }

    /// A failed send ends the session first, so no drained release follows the cut frame.
    #[test]
    fn a_failed_send_writes_no_drained_release_after_its_cut_frame() {
        let (session, mut peer) = a13_client_with_raw_peer();
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let mut counter = 0;
        let pay = codec
            .encode_dec_strong(&RpcAddress::unique(&mut counter, AddressSpace::Acceptor), 1)
            .expect("a frame")
            .0;
        let Ok(WireMessage::DecStrong(addr, _)) = codec.decode_message(&pay) else {
            panic!("a DEC_STRONG");
        };
        {
            // A proxy of the peer's `addr` dropped while a send of it is unpaid: its release waits.
            let mut st = session.inner.shared.state.lock().expect("state");
            st.on_proxy_leaving(addr);
            assert_eq!(st.release_proxy(&addr, std::ptr::null()), 0);
        }
        // Armed before the session can end: macOS refuses `SO_RCVTIMEO` once the peer closed.
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("peer timeout");
        std::io::Write::write_all(&mut peer, &pay).expect("payment");
        std::io::Write::write_all(&mut peer, &a13_header(0, 1000)).expect("TRANSACT header");

        let sent = oneway_past_the_buffer(&session).expect("the send must end at the TRANSACT");
        assert_eq!(sent, Err(StatusCode::BadType));
        assert!(session.inner.shared.lifecycle.is_torn_down());
        let mut written = Vec::new();
        std::io::Read::read_to_end(&mut peer, &mut written).expect("to the session's end");
        let (release, _) = codec.encode_dec_strong(&addr, 1).expect("a frame");
        assert!(
            !written.windows(release.len()).any(|w| w == release),
            "the release followed the cut frame"
        );
    }

    /// A peer that closes while a send waits is `DeadObject` with or without a send deadline.
    #[test]
    fn a_peer_that_closes_while_a_send_waits_is_a_dead_object() {
        use super::super::transport::UnixTransport;
        for timeout in [None, Some(Duration::from_secs(5))] {
            let (a, peer) = UnixTransport::pair().expect("socketpair");
            let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
            session.set_reply_timeout(timeout).unwrap();
            let closer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                drop(peer);
            });
            let sent = oneway_past_the_buffer(&session).expect("the send must end at the close");
            closer.join().expect("closer");
            assert_eq!(sent, Err(StatusCode::DeadObject), "timeout {timeout:?}");
            assert!(session.inner.shared.lifecycle.is_torn_down());
        }
    }

    /// A deadline guard sets back what it replaced: an enclosing guard's value, else the baseline.
    #[test]
    fn a_deadline_guard_restores_the_deadline_it_replaced() {
        #[derive(Default)]
        struct Rec {
            read: Mutex<Option<Duration>>,
            closed: AtomicBool,
        }
        impl RpcTransport for Rec {
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                Ok(())
            }
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                Err(RpcError::EndOfStream)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "rec"
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
            // As XNU: `SO_RCVTIMEO` on a socket shut both ways is `EINVAL`.
            fn set_read_timeout(&self, t: Option<Duration>) -> RpcResult<()> {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(RpcError::Io(std::io::Error::from_raw_os_error(22)));
                }
                *self.read.lock().unwrap() = t;
                Ok(())
            }
            fn peer_closed(&self) -> Option<bool> {
                Some(self.closed.load(Ordering::SeqCst))
            }
        }
        let t = Rec::default();
        let now = || *t.read.lock().unwrap();
        let ms = Duration::from_millis;
        let baseline = Some(ms(900));
        {
            let _reply = ReplyDeadlineGuard::arm(&t, Some(ms(100)), baseline).expect("outer");
            {
                // A reentrant send's drain inside the outer reply wait.
                let _drain = ReplyDeadlineGuard::arm(&t, Some(ms(200)), baseline).expect("inner");
                assert_eq!(now(), Some(ms(200)));
            }
            assert_eq!(
                now(),
                Some(ms(100)),
                "the outer reply wait keeps its deadline"
            );
            {
                let _nested = NestedDeadlineGuard::lift(&t, Some(ms(100))).expect("lift");
                assert_eq!(now(), None);
                drop(ReplyDeadlineGuard::arm(&t, Some(ms(200)), baseline).expect("inner"));
                assert_eq!(now(), None, "the nested dispatch keeps its lifted deadline");
            }
            assert_eq!(now(), Some(ms(100)));
        }
        assert_eq!(now(), baseline);
        t.closed.store(true, Ordering::SeqCst);
        drop(ReplyDeadlineGuard::arm(&t, Some(ms(100)), baseline).expect("closed peer: skipped"));
        drop(NestedDeadlineGuard::lift(&t, Some(ms(100))).expect("closed peer: skipped"));
        t.closed.store(false, Ordering::SeqCst);
        assert_eq!(now(), baseline);
        ARMED_READ.with(|a| assert!(a.borrow().is_empty(), "every record is dropped"));
    }

    /// A slow but steady reader is not cut: the deadline counts from its last progress.
    #[test]
    fn a_peer_that_reads_slowly_is_not_cut_by_the_send_deadline() {
        use super::super::transport::UnixTransport;
        let (a, peer) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let deadline = Duration::from_secs(1);
        session.set_reply_timeout(Some(deadline)).unwrap();
        let payload = 1usize << 20;
        let mut data = Parcel::new();
        data.write(&vec![0u8; payload]).expect("payload");

        // 8 KiB per 50 ms (XNU's AF_UNIX buffer): ~128 short waits outlast the deadline.
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; 8 << 10];
            let mut total = 0usize;
            while total < payload {
                match peer.recv_raw(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n,
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            total
        });
        let started = std::time::Instant::now();
        let sent = session
            .inner
            .client_transact(RpcAddress::zero(), 1, &data, FLAG_ONEWAY);
        let took = started.elapsed();
        assert!(matches!(sent, Ok(None)), "got {sent:?}");
        assert!(
            took > deadline,
            "the send took {took:?}, within one deadline"
        );
        assert!(reader.join().expect("reader") >= payload);
        assert!(!session.inner.shared.lifecycle.is_torn_down());
    }

    /// A loop started on an ended session, or on an id not in the pool, serves nothing.
    #[test]
    fn a_loop_with_nothing_to_serve_ends_with_session_ended() {
        use super::super::transport::UnixTransport;
        use crate::rpc::{EndedBy, StreamState};
        let (a, _pa) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let lifecycle = &session.inner.shared.lifecycle;

        // An id this pool never had: the caller's mistake, not a reason to end the session.
        let end = session.serve_blocking_on(u64::MAX);
        assert_eq!(end.reason, EndReason::SessionEnded);
        assert!(
            !lifecycle.is_torn_down(),
            "a wrong id leaves the session up"
        );

        session.inner.fail_session();
        let end = session.spawn_serve().expect("spawn").join().expect("serve");
        assert_eq!(end.reason, EndReason::SessionEnded);
        assert_eq!(end.by, EndedBy::NotLocal, "a fault ended it, not this end");
        assert_eq!(end.stream, StreamState::Lost);
    }

    /// A loop parked between frames stops with `SessionEnded` when the session ends elsewhere.
    #[test]
    fn a_session_ended_under_a_parked_serve_loop_stops_it() {
        use super::super::transport::UnixTransport;
        use crate::rpc::{EndedBy, StreamState};
        let (a, _pa) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let (b, _pb) = UnixTransport::pair().expect("socketpair");
        session
            .inner
            .add_outgoing_slot(Box::new(b))
            .expect("fan-out slot");
        let founding = RpcSession::FOUNDING_SLOT_ID;
        // A caller waiting for its reply on the founding slot, so the loop parks in `slot_cv`.
        let mut st = session.inner.conn_state.lock().expect("conn_state");
        let slot = st
            .slots
            .iter_mut()
            .find(|s| s.id == founding)
            .expect("founding");
        slot.exclusive_tid = Some(current_tid());
        drop(st);

        let (parked_tx, parked) = mpsc::channel();
        *session.inner.shared.park_hook.lock().expect("park hook") = Some(parked_tx);
        let serve = session.spawn_serve().expect("spawn");
        // Sent under the pool lock, so `fail_session` below can only run once the loop waits.
        parked
            .recv_timeout(Duration::from_secs(10))
            .expect("the serve loop must park in `slot_cv`");
        // What the other connection's failed reply wait does: module doc "Session end".
        session.inner.fail_session();

        // Bounded, so a loop nothing wakes fails the test instead of hanging the suite.
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(serve.join());
        });
        let end = done
            .recv_timeout(Duration::from_secs(10))
            .expect("the parked loop must wake")
            .expect("serve");
        assert_eq!(end.reason, EndReason::SessionEnded);
        assert_eq!(end.by, EndedBy::NotLocal);
        assert_eq!(end.stream, StreamState::Lost);
        assert!(session.inner.shared.lifecycle.is_torn_down());
    }

    /// A handler's callback with no reply deadline waits under the idle value; "Reply deadlines".
    #[test]
    fn a_callback_on_a_serve_slot_waits_under_the_idle_value_by_default() {
        use super::super::transport::UnixTransport;
        let (t, _silent) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::from_android13plus(
            Box::new(t),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        let idle = Duration::from_millis(200);
        session.set_serve_read_deadline(Some(idle));
        let (slot_id, slot) = {
            let mut st = session.inner.conn_state.lock().expect("conn_state");
            // What a handler's dispatch arms for its own callbacks (`AllowNestedGuard`).
            st.slots[0].allow_nested = true;
            (st.slots[0].id, Arc::clone(&st.slots[0].transport))
        };
        // The socket holds no deadline, as inside a nested dispatch that lifted it.
        slot.set_read_timeout(None).expect("clear");
        let inner = Arc::clone(&session.inner);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The pin a handler's thread holds on the serve slot it dispatches from.
            let sess_ptr = &*inner as *const RpcSessionInner as usize;
            DRIVING.with(|d| d.borrow_mut().push((sess_ptr, slot_id)));
            let t0 = Instant::now();
            let r = inner.client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &Parcel::new(),
                0,
            );
            let _ = tx.send((r.map(|_| ()), t0.elapsed()));
        });
        // Bounded, so a wait with no deadline fails the test instead of hanging it.
        let (r, waited) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the idle value must bound the reply wait");
        assert_eq!(r, Err(StatusCode::TimedOut));
        assert!(waited >= idle, "gave up after {waited:?}");
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "a reply timeout ends the session"
        );
    }

    /// A oneway handler's callback leaves by a callback slot, still under the idle value.
    #[test]
    fn a_oneway_handlers_callback_waits_under_the_idle_value_by_default() {
        use super::super::transport::UnixTransport;
        let idle = Duration::from_millis(200);
        let (session, slots, peers) = idle_serve_slots(idle, 0);
        let (cb, silent) = UnixTransport::pair().expect("socketpair");
        let cb_slot = session.inner.add_slot_inner_capped(Box::new(cb), 8);
        cb_slot.expect("callback slot");
        let inner = Arc::clone(&session.inner);
        let serve_slot = slots[0];
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // A oneway dispatch's pin: `allow_nested` stays false, so the serve slot is not reused.
            let sess_ptr = &*inner as *const RpcSessionInner as usize;
            DRIVING.with(|d| d.borrow_mut().push((sess_ptr, serve_slot)));
            let t0 = Instant::now();
            let r = inner.client_transact(
                RpcAddress::zero(),
                SpecialTransaction::GetSessionId.code(),
                &Parcel::new(),
                0,
            );
            let _ = tx.send((r.map(|_| ()), t0.elapsed()));
        });
        // Bounded, so a wait with no deadline fails the test instead of hanging it.
        let (r, waited) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the idle value must bound the reply wait");
        assert_eq!(r, Err(StatusCode::TimedOut));
        assert!(waited >= idle, "gave up after {waited:?}");
        assert!(
            session.inner.shared.lifecycle.is_torn_down(),
            "a reply timeout ends the session"
        );
        // The frame went out on the callback connection; the serve one carried nothing (EOF).
        let mut buf = [0u8; 16];
        assert!(silent.recv_raw(&mut buf).expect("callback peer") > 0);
        assert_eq!(peers[0].recv_raw(&mut buf).expect("serve peer"), 0);
    }

    /// A session with no idle value settles a twoway's reply deadline without the pool lock.
    #[test]
    fn a_session_without_an_idle_value_decides_reply_deadlines_without_the_pool_lock() {
        use super::super::transport::UnixTransport;
        let (a, _pa) = UnixTransport::pair().expect("socketpair");
        let session = RpcSession::new(Box::new(a), AddressSpace::Initiator).expect("session");
        let inner = Arc::clone(&session.inner);
        // Held across both queries: one that took the lock would not answer.
        let held = session.inner.conn_state.lock().expect("conn_state");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The pin `find_conn` leaves for a client's own call.
            let sess_ptr = &*inner as *const RpcSessionInner as usize;
            let founding = RpcSession::FOUNDING_SLOT_ID;
            DRIVING.with(|d| d.borrow_mut().push((sess_ptr, founding)));
            let got = (
                inner.handler_read_deadline(),
                inner.slot_baseline_read_deadline(founding),
            );
            DRIVING.with(|d| d.borrow_mut().pop());
            let _ = tx.send(got);
        });
        let got = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        assert_eq!(got, Ok((None, None)), "answered without the pool lock");
    }

    /// A oneway whose write stalls for over `2d` keeps its session up; "Idle".
    #[test]
    fn a_frame_being_sent_is_activity() {
        use super::super::transport::UnixTransport;
        /// Parks the first `send_raw` until the test releases it.
        struct Parked {
            t: UnixTransport,
            sends: AtomicUsize,
            parked: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl RpcTransport for Parked {
            fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
                self.t.send_frame(buf)
            }
            fn recv_frame(&self) -> RpcResult<Vec<u8>> {
                self.t.recv_frame()
            }
            fn peer_identity(&self) -> PeerIdentity {
                self.t.peer_identity()
            }
            fn describe(&self) -> &str {
                "parked"
            }
            fn shutdown(&self) -> RpcResult<()> {
                self.t.shutdown()
            }
            fn supports_fd_passing(&self) -> bool {
                self.t.supports_fd_passing()
            }
            fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
                self.t.recv_raw(buf)
            }
            fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
                if self.sends.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = self.parked.send(());
                    let _ = self.release.lock().unwrap().recv();
                }
                self.t.send_raw(buf)
            }
        }
        let idle = Duration::from_millis(300);
        let (session, slots, _peers) = idle_serve_slots(idle, 0);
        let (t, _peer) = UnixTransport::pair().expect("socketpair");
        let (parked_tx, parked) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let transport = Parked {
            t,
            sends: AtomicUsize::new(0),
            parked: parked_tx,
            release: Mutex::new(release_rx),
        };
        let cb_slot = session.inner.add_slot_inner_capped(Box::new(transport), 8);
        cb_slot.expect("callback slot");
        let data = Parcel::new();
        let inner = Arc::clone(&session.inner);
        // A oneway off the serve slot, as a callback sent outside a handler: it opens no call.
        let sender = std::thread::spawn(move || {
            inner
                .client_transact(RpcAddress::zero(), 1, &data, FLAG_ONEWAY)
                .map(|_| ())
        });
        parked
            .recv_timeout(Duration::from_secs(10))
            .expect("the write must start");
        let (waiting_tx, waiting) = mpsc::channel();
        *session.inner.shared.serve_wait_hook.lock().expect("hook") = Some(waiting_tx);
        let ends = serve_all(&session, &slots);
        waiting
            .recv_timeout(Duration::from_secs(10))
            .expect("the serve loop must start its wait");
        // The reader takes nothing for three periods: the serve slot's wait expires three times.
        std::thread::sleep(3 * idle);
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "evicted while a frame was being written"
        );
        assert_eq!(
            session.inner.shared.open.load(Ordering::SeqCst),
            1,
            "the write is open"
        );
        release.send(()).expect("release");
        assert_eq!(sender.join().expect("sender"), Ok(()));
        assert!(evicted(&ends, slots.len()), "quiet at last, it is idle");
    }

    /// A frame part-way in is activity from its first byte, in both fd modes; "Idle".
    #[test]
    fn a_frame_being_received_is_activity() {
        use super::super::transport::UnixTransport;
        /// Hands out one byte on the first read and parks the second, inside the header.
        struct Parked {
            t: UnixTransport,
            reads: AtomicUsize,
            parked: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Parked {
            fn len_for_this_read(&self, want: usize) -> usize {
                match self.reads.fetch_add(1, Ordering::SeqCst) {
                    0 => want.min(1),
                    1 => {
                        let _ = self.parked.send(());
                        let _ = self.release.lock().unwrap().recv();
                        want
                    }
                    _ => want,
                }
            }
        }
        impl RpcTransport for Parked {
            fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
                self.t.send_frame(buf)
            }
            fn recv_frame(&self) -> RpcResult<Vec<u8>> {
                self.t.recv_frame()
            }
            fn peer_identity(&self) -> PeerIdentity {
                self.t.peer_identity()
            }
            fn describe(&self) -> &str {
                "parked"
            }
            fn shutdown(&self) -> RpcResult<()> {
                self.t.shutdown()
            }
            fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
                self.t.send_raw(buf)
            }
            fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
                let len = self.len_for_this_read(buf.len());
                self.t.recv_raw(&mut buf[..len])
            }
            fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<OwnedFd>)> {
                let len = self.len_for_this_read(buf.len());
                self.t.recv_raw_with_fds(&mut buf[..len])
            }
        }
        for fd_mode in [FD_MODE_NONE, FD_MODE_UNIX] {
            let (t, peer) = UnixTransport::pair().expect("socketpair");
            let (parked_tx, parked) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let transport = Parked {
                t,
                reads: AtomicUsize::new(0),
                parked: parked_tx,
                release: Mutex::new(release_rx),
            };
            let session = RpcSession::from_android13plus(
                Box::new(transport),
                Android13PlusCodec::android14_15(),
                fd_mode,
                true,
            )
            .expect("session");
            let unix = session.inner.fd_mode() == FileDescriptorTransportMode::Unix;
            assert_eq!(unix, fd_mode == FD_MODE_UNIX, "the reader under test");
            let slot = {
                let st = session.inner.conn_state.lock().expect("conn_state");
                Arc::clone(&st.slots[0].transport)
            };
            // A 16-byte `RpcWireHeader` with `bodySize` (LE at offset 4) = 4, then the body.
            let mut frame = [0u8; 20];
            frame[4] = 4;
            peer.send_raw(&frame).expect("send the frame");
            let mut seen = session.inner.activity();
            let inner = Arc::clone(&session.inner);
            let reader = std::thread::spawn(move || inner.recv_msg(&*slot).map(|(f, _)| f.len()));
            parked
                .recv_timeout(Duration::from_secs(10))
                .expect("the second read must start");
            assert_eq!(session.inner.shared.open.load(Ordering::SeqCst), 0);
            assert!(
                session.inner.active_since(&mut seen),
                "a frame part-way in, fd mode {fd_mode}"
            );
            release.send(()).expect("release");
            assert_eq!(reader.join().expect("reader").ok(), Some(20));
        }
    }

    /// An android-13+ server session with `1 + extra` serve slots armed at `idle`, and their peers.
    fn idle_serve_slots(
        idle: Duration,
        extra: usize,
    ) -> (
        RpcSession,
        Vec<u64>,
        Vec<super::super::transport::UnixTransport>,
    ) {
        use super::super::transport::UnixTransport;
        let (t, peer) = UnixTransport::pair().expect("socketpair");
        // What `RpcServer::arm_serve_timeouts` does to an accepted connection.
        t.set_read_timeout(Some(idle)).expect("arm");
        let session = RpcSession::from_android13plus(
            Box::new(t),
            Android13PlusCodec::android14_15(),
            FD_MODE_NONE,
            false,
        )
        .expect("session");
        session.set_serve_read_deadline(Some(idle));
        let mut slots = vec![RpcSession::FOUNDING_SLOT_ID];
        let mut peers = vec![peer];
        for _ in 0..extra {
            let (t, peer) = UnixTransport::pair().expect("socketpair");
            t.set_read_timeout(Some(idle)).expect("arm");
            let slot = session.add_incoming_slot_capped(Box::new(t), 8);
            slots.push(slot.expect("attach"));
            peers.push(peer);
        }
        (session, slots, peers)
    }

    /// A serve loop on each slot; each loop's end as it finishes.
    fn serve_all(session: &RpcSession, slots: &[u64]) -> mpsc::Receiver<SessionEnd> {
        let (tx, rx) = mpsc::channel();
        for &slot in slots {
            let (session, tx) = (session.clone(), tx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(session.serve_blocking_on(slot));
            });
        }
        rx
    }

    /// Every loop's end, and whether one of them was this end's idle eviction.
    fn evicted(ends: &mpsc::Receiver<SessionEnd>, loops: usize) -> bool {
        use crate::rpc::EndedBy;
        let ends: Vec<SessionEnd> = (0..loops)
            .map(|_| {
                ends.recv_timeout(Duration::from_secs(10))
                    .expect("every loop ends")
            })
            .collect();
        ends.iter().any(|end| {
            end.reason == EndReason::Frame(StatusCode::TimedOut) && end.by == EndedBy::Local
        })
    }

    /// An idle session ends at least `d` and under `2d` after its last activity; "Idle".
    #[test]
    fn an_idle_session_ends_between_d_and_2d_after_its_last_activity() {
        use super::super::transport::UnixTransport;
        let idle = Duration::from_secs(1);
        // Half a period into the serve slot's first wait, off that slot: an end, a send, a join.
        for case in [
            "call end",
            "callback frame",
            "serve attach",
            "callback attach",
        ] {
            let (session, slots, _peers) = idle_serve_slots(idle, 0);
            let (cb, _cb_peer) = UnixTransport::pair().expect("socketpair");
            let cb_slot = session.inner.add_slot_inner_capped(Box::new(cb), 8);
            cb_slot.expect("callback slot");
            let call = (case == "call end").then(|| OpenCall::enter(&session.inner.shared));
            let (joining, _joined_peer) = UnixTransport::pair().expect("socketpair");
            let (waiting_tx, waiting) = mpsc::channel();
            *session.inner.shared.serve_wait_hook.lock().expect("hook") = Some(waiting_tx);
            let ends = serve_all(&session, &slots);
            // Timed from the loop's own start, so a late serve thread shifts nothing.
            waiting
                .recv_timeout(Duration::from_secs(10))
                .expect("the serve loop must start its wait");
            std::thread::sleep(idle / 2);
            let last = Instant::now();
            match case {
                "call end" => drop(call),
                "callback frame" => {
                    let sent = session.inner.client_transact(
                        RpcAddress::zero(),
                        1,
                        &Parcel::new(),
                        FLAG_ONEWAY,
                    );
                    assert!(matches!(sent, Ok(None)), "{case}: {sent:?}");
                }
                "serve attach" => {
                    let joined = session.add_incoming_slot_capped(Box::new(joining), 8);
                    joined.expect("serve attach");
                }
                _ => {
                    let codec = Android13PlusCodec::android14_15();
                    let joined = session.add_callback_slot_and_init(Box::new(joining), 8, &codec);
                    joined.expect("callback attach");
                }
            }
            assert!(evicted(&ends, slots.len()), "{case}: an idle eviction");
            let quiet = last.elapsed();
            assert!(
                quiet >= idle && quiet < 2 * idle,
                "{case}: ended {quiet:?} after the last activity"
            );
        }
    }

    /// One frame trickled over `4d` in gaps of `d/4` keeps a quiet slot's session up; "Idle".
    #[test]
    fn a_frame_trickled_across_the_idle_period_is_not_idle() {
        let idle = Duration::from_millis(400);
        let (session, slots, peers) = idle_serve_slots(idle, 1);
        let ends = serve_all(&session, &slots);
        let (frame, _) = Android13PlusCodec::android14_15()
            .encode_dec_strong(&RpcAddress::zero(), 1)
            .expect("a frame");
        // A `DEC_STRONG` opens no call: only its bytes are activity. 32 bytes, two at a time.
        for pair in frame.chunks(2) {
            peers[1].send_raw(pair).expect("trickle");
            std::thread::sleep(idle / 4);
        }
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "evicted while the frame was arriving"
        );
        assert!(evicted(&ends, slots.len()), "quiet at last, it is idle");
    }

    /// Whole frames on one slot, `d/4` apart for `3d`, keep a quiet slot's session up; "Idle".
    #[test]
    fn frames_on_another_slot_keep_a_quiet_one_up() {
        let idle = Duration::from_millis(400);
        let (session, slots, peers) = idle_serve_slots(idle, 1);
        let ends = serve_all(&session, &slots);
        let codec = Android13PlusCodec::android14_15();
        for _ in 0..12 {
            let (frame, _) = codec
                .encode_dec_strong(&RpcAddress::zero(), 1)
                .expect("a frame");
            peers[1].send_raw(&frame).expect("a frame");
            std::thread::sleep(idle / 4);
        }
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "evicted while frames were arriving"
        );
        assert!(evicted(&ends, slots.len()), "quiet at last, it is idle");
    }

    /// An idle expiry the session's end overtakes reports that end, not an eviction of its own.
    #[test]
    fn an_idle_expiry_on_a_slot_the_session_dropped_is_not_an_eviction() {
        use super::super::transport::MemTransport;
        use crate::rpc::{EndedBy, StreamState};
        /// Ends the session, as another connection's fault does, then reports an idle expiry.
        struct Faulted(MemTransport, Arc<Mutex<Weak<RpcSessionInner>>>);
        impl RpcTransport for Faulted {
            fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
                self.0.send_raw(buf)
            }
            fn recv_raw(&self, _: &mut [u8]) -> RpcResult<usize> {
                let session = self.1.lock().unwrap().upgrade();
                if let Some(inner) = session {
                    inner.fail_session();
                }
                Err(RpcError::Timeout)
            }
            fn peer_identity(&self) -> PeerIdentity {
                self.0.peer_identity()
            }
            fn describe(&self) -> &str {
                "faulted"
            }
            fn shutdown(&self) -> RpcResult<()> {
                self.0.shutdown()
            }
        }
        let (t, _peer) = MemTransport::pair();
        let cell = Arc::new(Mutex::new(Weak::new()));
        let transport = Faulted(t, Arc::clone(&cell));
        let session =
            RpcSession::new(Box::new(transport), AddressSpace::Acceptor).expect("session");
        session.set_serve_read_deadline(Some(Duration::from_secs(60)));
        *cell.lock().unwrap() = Arc::downgrade(&session.inner);

        let end = session.serve_blocking_on(RpcSession::FOUNDING_SLOT_ID);
        assert_eq!(end.reason, EndReason::SessionEnded);
        assert_eq!(
            end.by,
            EndedBy::NotLocal,
            "another connection's fault ended it"
        );
        assert_eq!(end.stream, StreamState::Lost);
    }

    /// The kernel's `ETIMEDOUT` under an armed first-frame deadline is a lost peer, not eviction.
    #[test]
    fn the_kernels_etimedout_under_an_armed_deadline_is_not_an_eviction() {
        use crate::rpc::{EndedBy, StreamState};
        // A socket whose peer's host stopped answering: TCP gave up, every read is `ETIMEDOUT`.
        struct Etimedout;
        impl std::io::Read for Etimedout {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                let etimedout = rustix::io::Errno::TIMEDOUT.raw_os_error();
                Err(std::io::Error::from_raw_os_error(etimedout))
            }
        }
        struct Gone;
        impl RpcTransport for Gone {
            fn send_raw(&self, _: &[u8]) -> RpcResult<()> {
                Err(RpcError::EndOfStream)
            }
            // A socket backend's raw read, so the session reader's timeout split is what is tested.
            fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
                std::io::Read::read(&mut Etimedout, buf).map_err(RpcError::from)
            }
            fn peer_identity(&self) -> PeerIdentity {
                PeerIdentity::Anonymous
            }
            fn describe(&self) -> &str {
                "etimedout"
            }
            fn shutdown(&self) -> RpcResult<()> {
                Ok(())
            }
        }

        let session = RpcSession::new(Box::new(Gone), AddressSpace::Acceptor).expect("session");
        let end = session.serve_blocking_clearing_deadline_after_first();
        assert_eq!(end.reason, EndReason::Frame(StatusCode::DeadObject));
        assert_eq!(end.by, EndedBy::NotLocal, "the peer's host went away");
        assert_eq!(end.stream, StreamState::Lost);
    }

    /// `caps` answers from the founding connection, so a slot of another transport kind is refused.
    #[test]
    fn slot_of_a_different_transport_kind_is_refused() {
        use crate::rpc::transport::{MemTransport, UnixTransport};
        let (t0, _p0) = MemTransport::pair();
        let codec = Android13PlusCodec::android14_15();
        let session = RpcSession::from_android13plus(Box::new(t0), codec, FD_MODE_NONE, false)
            .expect("build session");

        // All three report `BadType`: the session is alive, so `DeadObject` would mislead.
        let (u, _pu) = UnixTransport::pair().expect("socketpair");
        assert_eq!(
            session.inner.add_outgoing_slot(Box::new(u)),
            Err(StatusCode::BadType)
        );
        let (u, _pu) = UnixTransport::pair().expect("socketpair");
        assert_eq!(
            session.inner.add_slot_inner_capped(Box::new(u), 2),
            Err(StatusCode::BadType)
        );
        let (u, _pu) = UnixTransport::pair().expect("socketpair");
        assert_eq!(
            session.inner.add_incoming_slot_capped(Box::new(u), 2),
            Err(StatusCode::BadType)
        );
        assert_eq!(session.inner.slot_count(), 1, "only the founding slot");

        let (m, _pm) = MemTransport::pair();
        assert!(
            session.inner.add_outgoing_slot(Box::new(m)).is_ok(),
            "a slot of the founding kind still joins"
        );
    }

    /// Another session's proxy names a foreign peer's node: `InvalidOperation`, as AOSP refuses.
    #[test]
    fn proxy_of_another_session_is_refused() {
        use crate::rpc::proxy::RpcProxy;
        use crate::rpc::transport::MemTransport;
        let make = || {
            let (t, p) = MemTransport::pair();
            let s = RpcSession::from_android13plus(
                Box::new(t),
                Android13PlusCodec::android14_15(),
                FD_MODE_NONE,
                false,
            )
            .expect("build session");
            (s, p)
        };
        let (a, _pa) = make();
        let (b, _pb) = make();
        let mut counter = 0u64;
        let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
        let proxy_of_a = SIBinder::new(Arc::new(RpcProxy::new(addr, a.inner.clone())))
            .expect("SIBinder::new(RpcProxy)");

        let mut parcel = Parcel::new();
        assert_eq!(
            b.inner.write_binder(Some(&proxy_of_a), &mut parcel),
            Err(StatusCode::InvalidOperation),
            "another session's proxy must not be addressed on this wire"
        );
        let mut parcel = Parcel::new();
        a.inner
            .write_binder(Some(&proxy_of_a), &mut parcel)
            .expect("the owning session writes its own proxy back");
    }

    /// A local binder for the send-state tests below.
    struct LocalSvc;
    impl crate::Interface for LocalSvc {}
    impl crate::Remotable for LocalSvc {
        fn descriptor() -> &'static str {
            "rsbinder.test.LocalSvc"
        }
        fn on_transact(
            &self,
            _: crate::TransactionCode,
            _: &mut Parcel,
            _: &mut Parcel,
        ) -> Result<()> {
            Ok(())
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
            Ok(())
        }
    }

    /// A copy into a parcel whose session is gone is `DeadObject`, not the no-positions refusal.
    #[test]
    fn append_from_after_the_session_is_gone_is_dead_object() {
        use crate::rpc::transport::MemTransport;
        let (t, _peer) = MemTransport::pair();
        let session = RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("session");
        let ops = session.inner.parcel_ops();
        let inner = Arc::downgrade(&session.inner);
        drop(session);
        assert!(inner.upgrade().is_none(), "the session's inner is freed");
        let mut source = Parcel::new();
        source.attach_rpc_ops(ops.clone());
        source.write(&7i32).expect("scalar");
        let mut dest = Parcel::new();
        dest.attach_rpc_ops(ops);
        let size = source.data_size();
        assert_eq!(
            dest.append_from(&mut source, 0, size),
            Err(StatusCode::DeadObject)
        );
    }

    /// `WouldBlock` leaves the parcel owning its bump (no rollback); its drop gives the bump back.
    #[test]
    fn would_block_leaves_the_parcel_owning_its_bump() {
        use crate::rpc::transport::MemTransport;
        let (t, _peer) = MemTransport::pair();
        // An acceptor with no `Outgoing` slot: `find_conn` is `WouldBlock` without a wait.
        let session = RpcSession::new(Box::new(t), AddressSpace::Acceptor).expect("session");
        let local = crate::Interface::as_binder(&crate::Binder::new(LocalSvc));
        let mut d = Parcel::new();
        d.configure_rpc(
            session.inner.parcel_ops(),
            session.inner.fd_mode(),
            session.inner.records_fd_positions(),
        );
        d.write(&local).expect("write a local binder");
        assert_eq!(session.local_node_count(), 1, "the write took its bump");
        assert_eq!(
            session
                .inner
                .client_transact(RpcAddress::zero(), crate::FIRST_CALL_TRANSACTION, &d, 0)
                .err(),
            Some(StatusCode::WouldBlock)
        );
        assert_eq!(
            session.local_node_count(),
            1,
            "a request never sent keeps its bump for the retry with the same parcel"
        );
        drop(d);
        assert_eq!(
            session.local_node_count(),
            0,
            "the unsent parcel's drop gives the bump back"
        );
    }

    /// A session with no live peer, for parcel-level wire tests.
    fn parcel_session(profile: WireProfile) -> RpcSession {
        let (t, _peer) = crate::rpc::transport::MemTransport::pair();
        RpcSession::with_profile(Box::new(t), AddressSpace::Acceptor, profile).expect("session")
    }

    fn a13_v2() -> WireProfile {
        WireProfile::Android13Plus(Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2"))
    }

    /// A null binder carries `UNDECLARED` on every wire: the bare level, or r34's `Category` 1.
    #[test]
    fn a_null_binder_carries_its_stability_on_every_wire() {
        for (profile, words) in [
            (a13_v2(), [0i32, 0]),
            (WireProfile::R34(R34Codec), [0, 0x0000_0001]),
        ] {
            let session = parcel_session(profile);
            let mut p = Parcel::new();
            p.attach_rpc_ops(session.inner.parcel_ops());
            p.write(&None::<SIBinder>).expect("write null");
            assert_eq!(p.data_size(), words.len() * 4);
            p.set_data_position(0);
            for w in words {
                assert_eq!(p.read::<i32>().expect("word"), w);
            }
            p.set_data_position(0);
            assert!(p.read::<Option<SIBinder>>().expect("read null").is_none());
            assert_eq!(
                p.data_position(),
                p.data_size(),
                "the stability is consumed"
            );
        }
    }

    /// AOSP `Stability::setRepr` on a null binder's stability; android-12 wants `version >= 1`.
    #[test]
    fn a_null_binder_stability_is_checked_as_aosp_does() {
        for (profile, stability, want) in [
            (a13_v2(), 0x0c, Err(StatusCode::BadType)),
            // android-13+ reads the word as an `int16_t`: the high 16 bits are not a level.
            (a13_v2(), 0x0001_0000, Ok(())),
            (a13_v2(), 0x0001_000c, Err(StatusCode::BadType)),
            (
                WireProfile::R34(R34Codec),
                0x0c00_0001,
                Err(StatusCode::BadType),
            ),
            (WireProfile::R34(R34Codec), 0, Err(StatusCode::BadType)),
            // Only the level and `kBinderWireFormatOldest` are checked, not the exact version.
            (WireProfile::R34(R34Codec), 0x0000_0002, Ok(())),
        ] {
            let session = parcel_session(profile);
            let mut p = Parcel::new();
            p.attach_rpc_ops(session.inner.parcel_ops());
            p.write(&0i32).expect("present");
            p.write(&stability).expect("stability");
            p.set_data_position(0);
            assert_eq!(
                p.read::<Option<SIBinder>>().map(|b| assert!(b.is_none())),
                want,
                "stability {stability:#x}"
            );
        }
    }

    /// r34 writes a binder as android-12 does: `1`, the 32-byte address, then its `Category`.
    #[test]
    fn an_r34_binder_is_followed_by_its_android12_category() {
        let session = parcel_session(WireProfile::R34(R34Codec));
        let local = crate::Interface::as_binder(&crate::Binder::new(LocalSvc));
        let mut p = Parcel::new();
        p.attach_rpc_ops(session.inner.parcel_ops());
        p.write(&local).expect("write");
        assert_eq!(p.data_size(), 4 + RPC_ADDR_LEN + 4);
        p.set_data_position(0);
        assert_eq!(p.read::<i32>().expect("present"), 1);
        p.set_data_position(4 + RPC_ADDR_LEN);
        assert_eq!(p.read::<i32>().expect("stability"), 0x0c00_0001);
    }

    /// The RPC word is the profile's form alone: android-13+ never writes a `Category`.
    #[test]
    fn an_rpc_binder_stability_word_follows_the_profile_alone() {
        let levels = [
            (None, 0),
            (Some(Stability::Local), 0),
            (Some(Stability::Vendor), 0b00_0011),
            (Some(Stability::System), 0b00_1100),
            (Some(Stability::Vintf), 0b11_1111),
        ];
        for (wire, profile) in [
            (
                "v0",
                WireProfile::Android13Plus(Android13PlusCodec::android13()),
            ),
            (
                "v1",
                WireProfile::Android13Plus(Android13PlusCodec::android14_15()),
            ),
            ("v2", a13_v2()),
        ] {
            for (stability, level) in levels {
                let word = profile.binder_stability_repr(stability);
                assert_eq!(word, level, "{wire} {stability:?}");
                assert_eq!(
                    profile.check_binder_stability(word, stability.is_none()),
                    Ok(level),
                    "{wire} {stability:?} round trip"
                );
            }
        }
        let r34 = WireProfile::R34(R34Codec);
        for (stability, level) in levels {
            let word = r34.binder_stability_repr(stability);
            assert_eq!(word, (level << 24) | 1, "r34 {stability:?}");
            assert_eq!(
                r34.check_binder_stability(word, stability.is_none()),
                Ok(level),
                "r34 {stability:?} round trip"
            );
        }
    }

    /// A received proxy reports and goes back with the stability it arrived with (AOSP `getRepr`).
    #[test]
    fn a_received_proxy_is_sent_back_with_its_stability() {
        for (stability, level) in [
            (Stability::Vendor, 0b00_0011),
            (Stability::Vintf, 0b11_1111),
        ] {
            for (wire, profile, word) in [
                (
                    "android-13+",
                    WireProfile::Android13Plus(Android13PlusCodec::android14_15()),
                    level,
                ),
                ("r34", WireProfile::R34(R34Codec), (level << 24) | 1),
            ] {
                let session = parcel_session(profile);
                let mut counter = 0u64;
                let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
                let mut sent = Parcel::new();
                sent.write(&1i32).expect("present");
                session
                    .inner
                    .wire_write_binder_addr(&mut sent, &addr)
                    .expect("address");
                sent.write(&word).expect("stability");
                let (mut received, entered) = session.inner.receive_parcel(
                    sent.rpc_data_bytes().to_vec(),
                    Vec::new(),
                    Vec::new(),
                );
                entered.expect("entered");
                let proxy: SIBinder = received.read().expect("proxy");
                assert_eq!(proxy.stability(), stability, "{wire} getter");

                let mut back = Parcel::new();
                back.attach_rpc_ops(session.inner.parcel_ops());
                back.write(&proxy).expect("write back");
                back.set_data_position(back.data_size() - 4);
                let sent_back = back.read::<i32>().expect("stability");
                assert_eq!(sent_back, word, "{wire} {stability:?} word");
            }
        }
    }

    /// AOSP `rpcSetDataReference`: the fds a parcel names are the fds it came with, from v1 on.
    #[test]
    fn a_parcel_that_names_other_fds_than_it_came_with_is_refused() {
        use crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR;
        let fd = || -> OwnedFd { std::fs::File::open("/dev/null").expect("a file").into() };
        let fd_object = || {
            let mut data = TYPE_NATIVE_FILE_DESCRIPTOR.to_le_bytes().to_vec();
            data.extend_from_slice(&0i32.to_le_bytes());
            data
        };
        let session = parcel_session(WireProfile::Android13Plus(
            Android13PlusCodec::android14_15(),
        ));
        let entered = |data: Vec<u8>, positions: Vec<u32>, fds: Vec<OwnedFd>| {
            session.inner.receive_parcel(data, positions, fds).1
        };
        assert_eq!(entered(fd_object(), vec![0], vec![fd()]), Ok(()));
        assert_eq!(entered(vec![0; 8], vec![], vec![]), Ok(()));
        let refused = Err(StatusCode::BadValue);
        assert_eq!(entered(fd_object(), vec![0], vec![]), refused, "none came");
        assert_eq!(entered(vec![0; 8], vec![], vec![fd()]), refused, "unnamed");
        let two = vec![fd(), fd()];
        assert_eq!(entered(fd_object(), vec![0], two), refused, "one too many");
        assert!(
            !session.inner.shared.lifecycle.is_torn_down(),
            "only refused"
        );

        // As AOSP terminates: `Parcel.cpp` "Binder objects should only be in object positions".
        let binder = 1i32.to_le_bytes().to_vec();
        assert_eq!(entered(binder, vec![0], vec![]), refused, "a binder at v1");
        assert!(session.inner.shared.lifecycle.is_torn_down());
        let session = parcel_session(WireProfile::Android13Plus(
            Android13PlusCodec::android14_15(),
        ));
        let unknown = 7i32.to_le_bytes().to_vec();
        let received = session.inner.receive_parcel(unknown, vec![0], vec![]);
        assert_eq!(received.1, refused, "an object of no known type");
        assert!(session.inner.shared.lifecycle.is_torn_down());

        // No object table to count: an fd's read says what is missing.
        let session = parcel_session(WireProfile::R34(R34Codec));
        let received = session.inner.receive_parcel(vec![0; 8], vec![], vec![fd()]);
        assert_eq!(received.1, Ok(()));
    }

    /// `UNDECLARED` and a re-read own word fill nothing: unset until a declared word arrives.
    #[test]
    fn an_undeclared_word_leaves_an_rpc_proxy_at_the_local_level() {
        for (wire, profile, undeclared, system, vintf) in [
            (
                "android-13+",
                WireProfile::Android13Plus(Android13PlusCodec::android14_15()),
                0,
                0b00_1100,
                0b11_1111,
            ),
            (
                "r34",
                WireProfile::R34(R34Codec),
                0x0000_0001,
                0x0c00_0001,
                0x3f00_0001,
            ),
        ] {
            let session = parcel_session(profile);
            let mut counter = 0u64;
            let addr = RpcAddress::unique(&mut counter, AddressSpace::Initiator);
            let receive = |word: i32| {
                let mut sent = Parcel::new();
                sent.write(&1i32).expect("present");
                session
                    .inner
                    .wire_write_binder_addr(&mut sent, &addr)
                    .expect("address");
                sent.write(&word).expect("stability");
                let (mut received, entered) = session.inner.receive_parcel(
                    sent.rpc_data_bytes().to_vec(),
                    Vec::new(),
                    Vec::new(),
                );
                entered.expect("entered");
                received.read::<SIBinder>().expect("proxy")
            };
            let sent_back = |proxy: &SIBinder| {
                let mut back = Parcel::new();
                back.attach_rpc_ops(session.inner.parcel_ops());
                back.write(proxy).expect("write back");
                back.set_data_position(back.data_size() - 4);
                back.read::<i32>().expect("stability")
            };

            let proxy = receive(undeclared);
            assert_eq!(proxy.stability(), Stability::System, "{wire} getter");
            assert_eq!(sent_back(&proxy), system, "{wire} word sent back");

            // An unsent parcel read back holds this proxy's own fallback word: it fills nothing.
            let mut unsent = Parcel::new();
            unsent.attach_rpc_ops(session.inner.parcel_ops());
            unsent.write(&proxy).expect("write");
            unsent.set_data_position(0);
            let reread: SIBinder = unsent.read().expect("re-read");
            assert!(
                Arc::ptr_eq(proxy.as_arc(), reread.as_arc()),
                "{wire} re-read proxy"
            );

            // The cell stayed unset, so a later declared word still fills it.
            let again = receive(vintf);
            assert!(
                Arc::ptr_eq(proxy.as_arc(), again.as_arc()),
                "{wire} same proxy"
            );
            assert_eq!(proxy.stability(), Stability::Vintf, "{wire} filled later");
            assert_eq!(sent_back(&proxy), vintf, "{wire} filled word");
        }
    }

    /// A binder built with `Stability::Local` is sent with this process's level on both wires.
    #[test]
    fn a_local_binder_is_sent_with_the_local_level() {
        for (wire, profile, word) in [
            (
                "android-13+",
                WireProfile::Android13Plus(Android13PlusCodec::android14_15()),
                0b00_1100,
            ),
            ("r34", WireProfile::R34(R34Codec), 0x0c00_0001),
        ] {
            let session = parcel_session(profile);
            let local = crate::Interface::as_binder(&crate::Binder::new_with_stability(
                LocalSvc,
                Stability::Local,
            ));
            assert_eq!(local.stability(), Stability::System, "{wire} getter");
            let mut p = Parcel::new();
            p.attach_rpc_ops(session.inner.parcel_ops());
            p.write(&local).expect("write");
            p.set_data_position(p.data_size() - 4);
            assert_eq!(p.read::<i32>().expect("stability"), word, "{wire} word");
        }
    }

    /// One claim at a time: a failed attempt frees the parcel for a retry, a sent one stays sent.
    #[test]
    fn concurrent_senders_of_one_parcel_are_serialized() {
        let d = Parcel::new_data_only();
        assert_eq!(d.rpc_begin_send(), Ok(()));
        assert_eq!(
            d.rpc_begin_send(),
            Err(StatusCode::InvalidOperation),
            "a second sender is refused while the first is in flight"
        );
        d.rpc_end_send(false);
        assert_eq!(
            d.rpc_begin_send(),
            Ok(()),
            "a failed attempt returns to `NotSent`"
        );
        d.rpc_end_send(true);
        assert_eq!(
            d.rpc_begin_send(),
            Err(StatusCode::InvalidOperation),
            "a sent parcel is never sent again"
        );
    }

    /// A target that a nested call may run under: its handler calls back if it has a session.
    struct TargetOfACall(Mutex<Option<Arc<RpcSessionInner>>>);

    impl crate::Interface for TargetOfACall {}

    impl crate::Remotable for TargetOfACall {
        fn descriptor() -> &'static str {
            "rsbinder.test.ITargetOfACall"
        }
        fn on_transact(
            &self,
            _code: crate::TransactionCode,
            _reader: &mut Parcel,
            _reply: &mut Parcel,
        ) -> Result<()> {
            let session = self.0.lock().expect("session cell").clone();
            if let Some(session) = session {
                let nested = Parcel::new();
                session.client_transact(RpcAddress::zero(), 1, &nested, 0)?;
            }
            Ok(())
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
            Ok(())
        }
    }

    /// The next frame a raw peer reads, decoded.
    fn raw_peer_reads(peer: &mut std::os::unix::net::UnixStream) -> WireMessage {
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let frame = read_aosp_message(peer).expect("a frame");
        codec.decode_message(&frame).expect("decode")
    }

    /// A target's receipt goes right before the `REPLY` from every exit, whatever the status.
    #[test]
    fn every_reply_to_a_found_target_comes_right_behind_its_receipt() {
        use crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR;
        use crate::{Binder, Interface, Remotable};
        use std::io::Write;

        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        let target: SIBinder = Binder::new(TargetOfACall(Mutex::new(None))).as_binder();
        let addr = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&target)
            .expect("an address");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let mut token = Parcel::new();
        write_rpc_interface_token(&mut token, TargetOfACall::descriptor()).expect("token");
        let token = token.rpc_data_bytes().to_vec();
        let mut unnamed_fd = TYPE_NATIVE_FILE_DESCRIPTOR.to_le_bytes().to_vec();
        unnamed_fd.extend_from_slice(&0i32.to_le_bytes());
        let call = crate::FIRST_CALL_TRANSACTION;
        let exits = [
            ("ping", PING_TRANSACTION, Vec::new(), Vec::new(), true),
            (
                "interface",
                INTERFACE_TRANSACTION,
                Vec::new(),
                Vec::new(),
                true,
            ),
            ("objects refused", call, unnamed_fd, vec![0], false),
            ("a failed handler", call, Vec::new(), Vec::new(), false),
            ("a handler", call, token, Vec::new(), true),
        ];
        for (what, code, data, object_positions, ok) in exits {
            let frame = codec
                .encode_transact(&WireTransaction {
                    address: addr,
                    code,
                    flags: 0,
                    async_number: 0,
                    data,
                    object_positions,
                })
                .expect("encode");
            peer.write_all(&frame).expect("the call");
            assert!(
                matches!(
                    session
                        .inner
                        .serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID),
                    ServeStep::Continue
                ),
                "{what}"
            );
            match raw_peer_reads(&mut peer) {
                WireMessage::DecStrong(paid, 1) => assert_eq!(paid, addr, "{what}"),
                other => panic!("{what}: the receipt comes first, got {other:?}"),
            }
            match raw_peer_reads(&mut peer) {
                WireMessage::Reply(reply) => assert_eq!(reply.status == 0, ok, "{what}"),
                other => panic!("{what}: the reply comes next, got {other:?}"),
            }
        }
        session.close_session();
    }

    /// A nested call goes out ahead of the target's receipt, which waits for its own `REPLY`.
    #[test]
    fn a_target_receipt_waits_past_a_nested_call_for_its_reply() {
        use crate::{Binder, Interface, Remotable};
        use std::io::Write;

        let (session, mut peer) = a13_client_with_raw_peer();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("peer timeout");
        session
            .set_reply_timeout(Some(Duration::from_secs(10)))
            .expect("deadline");
        let target: SIBinder =
            Binder::new(TargetOfACall(Mutex::new(Some(session.inner.clone())))).as_binder();
        let addr = session
            .inner
            .shared
            .state
            .lock()
            .expect("rpc state")
            .on_binder_leaving(&target)
            .expect("an address");
        let codec = Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2");
        let mut token = Parcel::new();
        write_rpc_interface_token(&mut token, TargetOfACall::descriptor()).expect("token");
        let frame = codec
            .encode_transact(&WireTransaction {
                address: addr,
                code: crate::FIRST_CALL_TRANSACTION,
                flags: 0,
                async_number: 0,
                data: token.rpc_data_bytes().to_vec(),
                object_positions: Vec::new(),
            })
            .expect("encode");
        peer.write_all(&frame).expect("the call");
        let served = {
            let inner = session.inner.clone();
            std::thread::spawn(move || {
                let step = inner.serve_once_on_slot(RpcSession::FOUNDING_SLOT_ID);
                matches!(step, ServeStep::Continue)
            })
        };
        match raw_peer_reads(&mut peer) {
            WireMessage::Transact(nested) => assert!(nested.address.is_zero()),
            other => panic!("the nested call comes first, got {other:?}"),
        }
        let reply = codec
            .encode_reply(&WireReply::default())
            .expect("a nested reply");
        peer.write_all(&reply).expect("the nested reply");
        match raw_peer_reads(&mut peer) {
            WireMessage::DecStrong(paid, 1) => assert_eq!(paid, addr),
            other => panic!("the receipt comes next, got {other:?}"),
        }
        assert!(matches!(raw_peer_reads(&mut peer), WireMessage::Reply(_)));
        assert!(served.join().expect("the serve step"));
        session.close_session();
    }
}

#[cfg(test)]
#[path = "ref_accounting_tests.rs"]
mod ref_accounting_tests;

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
#[path = "peer_watch_tests.rs"]
mod peer_watch_tests;
