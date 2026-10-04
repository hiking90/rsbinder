// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcState` — per-session object table + RPC ref-count.
//!
//! The rsbinder equivalent of android `RpcState::mNodeForAddress`. This
//! is **strictly per-session** — there is no `static`, `OnceLock` or
//! `lazy_static` anywhere in the RPC stack, so two sessions never share
//! an address space and the RPC test suite is parallel-safe by
//! construction (unlike the kernel binder singleton).
//!
//! # Ref-count model
//!
//! AOSP `RpcState::BinderNode` keeps two counts per address: `timesSent`,
//! the sends of the address the peer still owes a `DEC_STRONG` for, and
//! `timesRecd`, the receipts this end owes the peer. rsbinder keeps the same
//! books this way:
//!
//! * **Sending a local object.** It gets one address by *identity* (`Arc`
//!   pointer dedup, so the same object always marshals to the same address),
//!   but the node's strong count is **`timesSent`**: +1 on every send
//!   ([`RpcState::on_binder_leaving`]), −`amount` on every inbound
//!   `DEC_STRONG`, and the node with its strong `SIBinder` is dropped at 0.
//! * **Sending a proxy.** Counted where the peer pays each send back (AOSP bumps
//!   the proxy node's `timesSent` and keeps it in `sentRef`): a transaction's
//!   target on the android-13+ wire, and a proxy flattened into a parcel at
//!   wire v2, where the peer enters every binder of a parcel on receipt. The
//!   count is [`RpcState::on_proxy_leaving`]; the peer's `DEC_STRONG` naming
//!   its own address pays it ([`RpcState::pay_proxy_sends`]). While a send is
//!   unpaid, a dropped proxy's own `DEC_STRONG` is held
//!   ([`RpcState::release_proxy`]) and goes out with the payment. Without the
//!   hold it can go out on another connection than a oneway still waiting to
//!   be read, and a peer that handles it first frees the node that oneway
//!   names (libbinder aborts: `Local binder must have been sent`). Below v2 a
//!   peer pays an argument only if it reads it, so an argument is not counted;
//!   the parcel pins the proxy until it drops, which orders its `DEC_STRONG`
//!   after a reply on the same connection. A held release goes out as late as
//!   the payment is read: a payment written where this end reads only inside a
//!   reply wait waits for the next twoway there, or for the session's end.
//! * **A send that never left.** A parcel dropped unsent rolls its
//!   `on_binder_leaving` bump back ([`RpcState::cancel_binder_leaving`], AOSP
//!   `cancelBinderLeaving`): the peer never received the binder, so it never
//!   sends the matching `DEC_STRONG`. A transport failure ends the session (as
//!   AOSP), so the rollback matters for a send refused before any byte went
//!   out, after which the session goes on. Bump and rollback are a commutative
//!   ±1, safe against a concurrent send of the same binder, and the node drops
//!   at 0 as on an inbound `DEC_STRONG`, outside the state lock.
//! * **Receiving a peer's address.** One `RpcProxy` per address
//!   ([`RpcState::remote_proxy`]). Each receipt owes the sender one
//!   `DEC_STRONG`: the receipt that mints the proxy is paid when the proxy
//!   drops, and a receipt deduped onto a live proxy (`excess`) is paid at once
//!   (AOSP `flushExcessBinderRefs`). The total equals AOSP's `timesRecd`.
//! * **Receiving one of our own addresses.** The peer took a `timesSent` for
//!   it when it sent it, so each receipt is paid with `DEC_STRONG` 1 at once
//!   (AOSP `flushExcessBinderRefs` on a local binder). On the android-13+ wire
//!   the target of an inbound transaction is a receipt too: AOSP
//!   `transactInternal` calls `onBinderLeaving` on the target proxy
//!   (android-13.0.0_r1 `RpcState.cpp:466`, android-17.0.0_r1 `:615`) and the
//!   server's `processTransactInternal` enters it. Each transaction the oneway
//!   gate or the twoway lookup resolves to a local node therefore owes one
//!   `DEC_STRONG`, whatever happens to it next (run, parked and drained,
//!   dropped as stale or as a duplicate, flushed on `Terminate`). The r34 wire
//!   does not count targets.
//! * **Where a receipt happens.** A received parcel enters each object position
//!   at most once and keeps what it entered (AOSP `mAcquiredEnteringBinders`,
//!   android-16.0.0_r4), so reading a position again owes nothing. At wire v2
//!   every binder position is entered when the parcel arrives, read or not. A
//!   parcel this end did not receive only looks an address up and owes nothing.
//!   The `parcel` module doc "RPC fields" has the table; the session module doc
//!   "Deferred `DEC_STRONG`" has the connection each `DEC_STRONG` goes out on.
//!
//! Net: exactly one `DEC_STRONG` per send, whether the binder is sent N× to
//! one peer (dedup + N−1 excess DECs + 1 drop DEC) **or** once to each of N
//! peer connections sharing a session (N sends, N drop DECs). Pinning the
//! count at 1 by identity would break the latter: the first connection's
//! proxy drop would free a node the sibling connection's proxy still names
//! (`DeadObject`). Every `DEC_STRONG` goes out **outside** the state lock.
//!
//! # Oneway backlog
//!
//! AOSP `RpcState.cpp` `kArbitraryOnewayCallTerminateLevel`
//! (`ASYNC_TODO_TERMINATE_LEVEL`): once that many out-of-order oneways are
//! parked on one node, the peer is treated as hostile or buggy. The node's
//! parked backlog is flushed (reclaiming its memory and held fds at once)
//! and the session ends (as AOSP `shutdownAndWait`), rather than letting the
//! per-node `async_todo` queue grow without bound (memory + fd exhaustion
//! DoS). A node therefore holds at most that many parked entries at any
//! instant.
//!
//! # Async-number wrap
//!
//! The per-node `async_number` is a `u64`, so a wrap means 2^64 oneways to
//! one node — effectively unreachable. AOSP `nodeProgressAsyncNumber`
//! returns `false` and tears the session down at overflow; rsbinder wraps
//! and logs instead of ending the session, since 2^64 oneways to one node
//! is unreachable, and lets the peer surface the duplicate as a protocol
//! error.
//!
//! The send-side counters ([`RpcState::next_send_async_number`], AOSP
//! `nodeProgressAsyncNumber` on the send path) live apart from
//! `remote_proxies`, so a counter survives a stale proxy `Drop` whose
//! address was already re-cached (`forget_remote_if` checks identity): the
//! peer's node is still alive, and a reset counter would replay numbers its
//! `asyncTodo` already processed.

use std::cmp::Reverse;
use std::collections::binary_heap::PeekMut;
use std::collections::{BinaryHeap, HashMap};
use std::os::fd::OwnedFd;
use std::sync::{self, Arc};

use crate::binder::{IBinder, SIBinder};

use super::address::{AddressSpace, RpcAddress};
use super::wire::WireTransaction;

/// `asyncTodo` entry, `Ord` by `async_number` for a `Reverse` min-heap (AOSP `AsyncTodo`).
struct AsyncTodo {
    async_number: u64,
    transaction: WireTransaction,
    in_fds: Vec<OwnedFd>,
}

impl PartialEq for AsyncTodo {
    fn eq(&self, other: &Self) -> bool {
        self.async_number == other.async_number
    }
}
impl Eq for AsyncTodo {}
impl PartialOrd for AsyncTodo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for AsyncTodo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.async_number.cmp(&other.async_number)
    }
}

/// Why an inbound oneway was dropped (no dispatch, no enqueue) so
/// callers can log / meter the two cases separately.
#[derive(Debug, Clone, Copy)]
pub enum DropReason {
    /// `mNodeForAddress.find` miss — peer addressed a binder we have
    /// never published or have already released. Benign for oneway.
    UnknownAddress,
    /// `wire_async < node.asyncNumber` — duplicate / replay / peer bug.
    /// AOSP parks every number other than the expected one
    /// (android-17.0.0_r1 `RpcState.cpp:1099-1133`), so a stale one waits in
    /// `asyncTodo` for the node's lifetime; rsbinder drops it at once. Either
    /// way it was entered, so it still owes the peer one `DEC_STRONG` on the
    /// android-13+ wire (module doc "Ref-count model").
    StaleAsyncNumber,
}

/// What [`RpcState::advance_and_pop_async`] found once the node's counter advanced.
#[derive(Debug, Default)]
pub struct AsyncAdvance {
    /// The parked entry now in order, to dispatch outside the state lock.
    pub next: Option<(WireTransaction, Vec<OwnedFd>)>,
    /// Parked entries below the new expected number, dropped without running.
    pub purged: u32,
}

/// AOSP `kArbitraryOnewayCallTerminateLevel`: per-node parked cap; see module doc "Oneway backlog".
const ASYNC_TODO_TERMINATE_LEVEL: usize = 10000;
/// AOSP `kArbitraryOnewayCallWarn{Level,Per}` (both 1000): warn at each multiple of this depth.
const ASYNC_TODO_WARN_PER: usize = 1000;

/// Outcome of [`RpcState::dispatch_async_or_enqueue`] for an inbound
/// oneway. Caller dispatches the [`AsyncDecision::Dispatch`] variant
/// outside the state lock, then calls
/// [`RpcState::advance_and_pop_async`] to advance the per-node counter
/// and drain newly-eligible queued entries.
#[derive(Debug)]
pub enum AsyncDecision {
    Dispatch(WireTransaction, Vec<OwnedFd>),
    Enqueued,
    Drop(DropReason),
    /// Watermark hit and the backlog flushed (count = DECs owed); module doc "Oneway backlog".
    Terminate(usize),
}

/// AOSP `BinderNode::timesSent` for a proxy: the peer pays each send back with `DEC_STRONG`.
#[derive(Default)]
struct RemoteSends {
    /// Sends not yet paid back.
    unpaid: u64,
    /// Proxy releases (`DEC_STRONG` 1 each) held until `unpaid` reaches 0.
    held: u32,
}

/// A local object exposed to the peer under [`RpcAddress`].
struct LocalNode {
    /// Strong ref keeps the local object alive while the peer holds it.
    binder: SIBinder,
    /// `binder_ptr(&binder)`: the `local_by_ptr` key, so removal needs no scan of every node.
    ptr: usize,
    /// RPC strong count the peer holds (0 ⇒ drop the node).
    strong: i64,
    /// AOSP `BinderNode::asyncNumber` (server side); per node, not session-global.
    next_async_number: u64,
    /// AOSP `BinderNode::asyncTodo`: min-heap (via `Reverse`) of out-of-order inbound oneways.
    async_todo: BinaryHeap<Reverse<AsyncTodo>>,
}

/// Per-session object/address table. Owned by `RpcSessionInner` behind
/// a `Mutex`; never global (enforced by the `rpc_stack_has_no_globals`
/// gate). No `Parcel` drops under that lock: an unsent one's `Drop` re-takes it, and so
/// does a received one's, whose entered proxies send their `DEC_STRONG` and
/// `forget_remote_if` on drop.
///
/// # Send-side async numbers
///
/// The per-remote-address send counters (AOSP `BinderNode::asyncNumber`,
/// client side) are dropped when the address's last `DEC_STRONG` goes out: with the
/// proxy slot in `forget_remote_if`, or, for a release held by unpaid sends, when
/// `pay_proxy_sends` sends it and no successor proxy uses the counter. The peer's
/// `timesSent` also reaches 0 then, so its
/// `BinderNode` is GC'd and the counter restart matches. The narrow race —
/// a `DEC_STRONG` still in flight when a sibling connection re-resolves the
/// same address — drops every oneway numbered below the peer's counter on
/// the peer's `Drop(StaleAsyncNumber)` arm.
pub struct RpcState {
    /// Objects we exposed to the peer, keyed by assigned address.
    local_nodes: HashMap<RpcAddress, LocalNode>,
    /// Dedup: local object `Arc` identity → address, so one object always marshals to one address.
    local_by_ptr: HashMap<usize, RpcAddress>,
    /// Remote proxies by address, `Weak`: dedups one `RpcProxy` per address and sees its last drop.
    remote_proxies: HashMap<RpcAddress, sync::Weak<dyn IBinder>>,
    /// AOSP `BinderNode::asyncNumber` (client side); see "Send-side async numbers" above.
    remote_send_async_counters: HashMap<RpcAddress, u64>,
    /// Sends of a peer address the peer has not paid back yet; present only while one is unpaid.
    remote_sends: HashMap<RpcAddress, RemoteSends>,
    /// Monotonic address allocator (per-session).
    addr_counter: u64,
    /// This endpoint's address subspace (initiator vs acceptor): the two peers never collide.
    space: AddressSpace,
    /// Test: inbound `DEC_STRONG` (amount sum, command count) per address, ignored ones included.
    #[cfg(test)]
    dec_received: HashMap<RpcAddress, (u64, u64)>,
}

/// Identity of a local binder's allocation: the data pointer of its trait-object `Arc`.
fn binder_ptr(b: &SIBinder) -> usize {
    Arc::as_ptr(b.as_arc()) as *const () as usize
}

impl RpcState {
    /// New empty per-session state for the given address subspace.
    pub fn new(space: AddressSpace) -> Self {
        RpcState {
            local_nodes: HashMap::new(),
            local_by_ptr: HashMap::new(),
            remote_proxies: HashMap::new(),
            remote_send_async_counters: HashMap::new(),
            remote_sends: HashMap::new(),
            addr_counter: 0,
            space,
            #[cfg(test)]
            dec_received: HashMap::new(),
        }
    }

    /// Register a local object leaving this process and return its
    /// session-stable address (android `onBinderLeaving`). The address
    /// is idempotent by object identity (same object ⇒ same address),
    /// but the strong count is AOSP `timesSent`: **+1 on every send**.
    /// The first send creates the node at `strong = 1`; a
    /// re-send of the same object reuses the address and **increments**
    /// `strong` (the peer will `DEC_STRONG` once per receipt — directly
    /// at proxy drop, or as an `flushExcessBinderRefs` excess DEC if it
    /// dedups; see the module doc). Returning without bumping would let
    /// the first connection's DEC free a node still referenced over a
    /// sibling connection (`DeadObject`).
    ///
    /// Minting a new address fails with `FailedTransaction` once the counter
    /// reaches `u32::MAX`: the android-13+ wire encodes only the low 32 bits
    /// of the address counter (`encode_addr`), so past that two live nodes
    /// would alias to one `RpcWireAddress` and mis-dispatch. This is a hard
    /// stop rather than an alias, unlike the async-number wrap (an
    /// ordering-only concern that only warns); ~2^32 live local objects per
    /// session is unreachable in practice.
    pub fn on_binder_leaving(&mut self, binder: &SIBinder) -> crate::Result<RpcAddress> {
        let ptr = binder_ptr(binder);
        if let Some(&addr) = self.local_by_ptr.get(&ptr) {
            if let Some(node) = self.local_nodes.get_mut(&addr) {
                node.strong += 1;
            }
            return Ok(addr);
        }
        // Refuse rather than alias a 32-bit wire address (see fn doc).
        if self.addr_counter >= u32::MAX as u64 {
            log::error!(
                "RPC: local address counter exhausted (>= u32::MAX) on one session; \
                 refusing to mint an aliasing address"
            );
            return Err(crate::StatusCode::FailedTransaction);
        }
        let addr = RpcAddress::unique(&mut self.addr_counter, self.space);
        self.local_nodes.insert(
            addr,
            LocalNode {
                binder: binder.clone(),
                ptr,
                strong: 1,
                next_async_number: 0,
                async_todo: BinaryHeap::new(),
            },
        );
        self.local_by_ptr.insert(ptr, addr);
        Ok(addr)
    }

    /// The local object registered at `addr`, if any (an address that
    /// is one of *our* nodes means the object is returning home, not a
    /// remote — android `onBinderEntering` local branch).
    pub fn lookup_local(&self, addr: &RpcAddress) -> Option<SIBinder> {
        self.local_nodes.get(addr).map(|n| n.binder.clone())
    }

    /// Undo an unsent parcel's `on_binder_leaving` bump; module doc "A send that never left".
    #[must_use = "drop the returned SIBinder outside the RpcState lock"]
    pub fn cancel_binder_leaving(&mut self, addr: &RpcAddress) -> Option<SIBinder> {
        if let Some(node) = self.local_nodes.get_mut(addr) {
            node.strong -= 1;
            if node.strong <= 0 {
                return self.remove_local(addr);
            }
        }
        None
    }

    /// Roll back one send of `addr` recorded in an unsent parcel, one of our nodes
    /// ([`cancel_binder_leaving`](Self::cancel_binder_leaving)) or a peer address
    /// ([`pay_proxy_sends`](Self::pay_proxy_sends)). Returns the node to drop and the held
    /// release to send, both outside the lock.
    #[must_use = "drop the node and send the release outside the RpcState lock"]
    pub fn cancel_leaving(&mut self, addr: &RpcAddress) -> (Option<SIBinder>, u32) {
        (
            self.cancel_binder_leaving(addr),
            self.pay_proxy_sends(addr, 1),
        )
    }

    fn remove_local(&mut self, addr: &RpcAddress) -> Option<SIBinder> {
        let node = self.local_nodes.remove(addr)?;
        self.local_by_ptr.remove(&node.ptr);
        Some(node.binder)
    }

    /// Apply an inbound `DEC_STRONG` for `addr` by `amount` (AOSP
    /// `doDecStrong`: `timesSent -= amount`). A compliant peer may batch more
    /// than one decrement into a single command, so the amount must be honored —
    /// applying a fixed 1 would leak the node on a batched drop. Drops the node
    /// (and its strong `SIBinder`) once the count reaches 0 — no leak. A hostile
    /// over-decrement simply removes the node early (contained: the peer loses
    /// access), and `strong: i64` cannot underflow for a `u32` amount.
    ///
    /// Returns the node's strong `SIBinder` if this removed it. The caller
    /// **must drop it outside the state lock**, like [`clear_local`](Self::clear_local)'s
    /// result: it may be the last ref to a user service whose `Drop` releases
    /// an `RpcProxy` of this same session, and `RpcProxy::drop` re-takes this
    /// lock (`forget_remote_if`) — dropping it in here deadlocks the session.
    #[must_use = "drop the returned SIBinder outside the RpcState lock"]
    pub fn dec_strong_local(&mut self, addr: &RpcAddress, amount: u32) -> Option<SIBinder> {
        #[cfg(test)]
        {
            let seen = self.dec_received.entry(*addr).or_default();
            seen.0 += u64::from(amount);
            seen.1 += 1;
        }
        if let Some(node) = self.local_nodes.get_mut(addr) {
            node.strong -= amount as i64;
            if node.strong <= 0 {
                return self.remove_local(addr);
            }
        }
        None
    }

    /// Session death: release every local object the peer held (AOSP
    /// `RpcState::clear`). Returns the strong refs so the caller drops
    /// them **outside** the state lock (a dropped service may run
    /// arbitrary user `Drop` code). After this the proxy→session strong
    /// ref ([`super::proxy::RpcProxy`]) cannot form a cycle through a
    /// service that stored a proxy of this same session.
    pub fn clear_local(&mut self) -> Vec<SIBinder> {
        self.local_by_ptr.clear();
        self.local_nodes.drain().map(|(_, n)| n.binder).collect()
    }

    /// Get or create the deduped remote-proxy `SIBinder` for `addr`.
    /// `make` is only called when there is no live proxy yet.
    ///
    /// Returns `(proxy, excess)`. `excess == true` means a still-live
    /// proxy for `addr` was reused — i.e. this is a **duplicate
    /// receipt** of a binder we already proxy. Because the sender bumps
    /// its `timesSent` on every send (`on_binder_leaving`) but our
    /// one deduped proxy only `DEC_STRONG`s once at its drop, the
    /// caller owes the sender one excess `DEC_STRONG` for this receipt
    /// (AOSP `flushExcessBinderRefs`). The caller must send it
    /// **outside** the `RpcState` lock (no I/O under the lock — see
    /// `RpcSessionInner::read_binder`). A fresh / re-minted proxy
    /// (dead `Weak`) is **not** excess: it is the single proxy that
    /// will itself `DEC_STRONG` at drop.
    ///
    /// An unknown address from **our own** subspace is refused
    /// (`BadValue`): we mint those, so one we do not have in `local_nodes`
    /// was forged by the peer. Minting a remote proxy for it would let a
    /// single `RpcAddress` name both a local node and a remote proxy, and
    /// the next legitimately minted node would collide with it (AOSP
    /// `RpcState::onBinderEntering`: "Server received unrecognized address
    /// which we should own the creation of").
    pub fn remote_proxy<F>(&mut self, addr: RpcAddress, make: F) -> crate::Result<(SIBinder, bool)>
    where
        F: FnOnce() -> SIBinder,
    {
        if let Some(weak) = self.remote_proxies.get(&addr) {
            if let Some(arc) = weak.upgrade() {
                return Ok((SIBinder::from_arc(arc), true));
            }
        }
        if addr.minted_by(self.space) {
            log::error!("RPC: peer sent an unknown address from our own subspace: {addr:?}");
            return Err(crate::StatusCode::BadValue);
        }
        let sib = make();
        self.remote_proxies
            .insert(addr, Arc::downgrade(sib.as_arc()));
        Ok((sib, false))
    }

    /// The live proxy for `addr`, if any, without minting one; drop it outside the state lock.
    pub fn lookup_remote(&self, addr: &RpcAddress) -> Option<SIBinder> {
        self.remote_proxies
            .get(addr)?
            .upgrade()
            .map(SIBinder::from_arc)
    }

    /// Forget the remote-proxy table entry for `addr`, but **only if
    /// the slot still points at the proxy `who`** (the dropping
    /// `RpcProxy`'s data address). Called from `RpcProxy::drop` after
    /// its `DEC_STRONG` is sent.
    ///
    /// A proxy whose `Arc` strong-count hit 0 in the window *before*
    /// its `Drop` body runs may already have been replaced in the
    /// cache by a freshly-resolved live proxy for the same address (a
    /// concurrent `read_binder` on a `Clone`d session observed the
    /// stale `Weak` and re-`make`d). An unconditional `remove` would
    /// then evict that **live** entry, splitting the per-address dedup
    /// and breaking the "exactly one live proxy ⇒ exactly one
    /// `DEC_STRONG`" invariant. The identity check makes
    /// a stale `Drop` a no-op against a re-cached successor.
    ///
    /// On a match it also drops the per-address send-side `async_number`
    /// counter. The proxy that owned the address is gone and its matching
    /// `DEC_STRONG` is sent shortly; after it lands, the peer's `BinderNode`
    /// either survives (`timesSent > 0` on the peer side — the next resolve
    /// restarts from 0) or is GC'd (the counter is irrelevant). Either way
    /// the book is closed for *this* proxy generation, and dropping it keeps
    /// the map bounded by the live address set.
    pub fn forget_remote_if(&mut self, addr: &RpcAddress, who: *const ()) {
        if self.forget_remote_slot_if(addr, who) {
            // Close this generation's `async_number` book (see fn doc).
            self.remote_send_async_counters.remove(addr);
        }
    }

    /// The identity-checked table half of [`forget_remote_if`](Self::forget_remote_if).
    fn forget_remote_slot_if(&mut self, addr: &RpcAddress, who: *const ()) -> bool {
        let matches = self
            .remote_proxies
            .get(addr)
            .is_some_and(|weak| weak.as_ptr() as *const () == who);
        if matches {
            self.remote_proxies.remove(addr);
        }
        matches
    }

    /// One send of the peer's `addr`, which the peer pays back with a `DEC_STRONG` (AOSP
    /// `onBinderLeaving` on a proxy): a proxy flattened into a parcel, on every wire, and a
    /// transaction's target on the android-13+ wire. Module doc "Ref-count model".
    pub fn on_proxy_leaving(&mut self, addr: RpcAddress) {
        self.remote_sends.entry(addr).or_default().unpaid += 1;
    }

    /// A proxy of `addr` dropped (`RpcProxy::drop`). Returns the `DEC_STRONG` amount to send
    /// now, outside the lock: 1, or 0 while sends of `addr` are unpaid. Then the release is
    /// held for [`pay_proxy_sends`](Self::pay_proxy_sends) (AOSP keeps the proxy in `sentRef`
    /// until then), and the `async_number` book stays open: the peer's node is still alive.
    #[must_use = "send the returned DEC_STRONG amount outside the RpcState lock"]
    pub fn release_proxy(&mut self, addr: &RpcAddress, who: *const ()) -> u32 {
        match self.remote_sends.get_mut(addr) {
            Some(sends) => {
                sends.held += 1;
                self.forget_remote_slot_if(addr, who);
                0
            }
            None => {
                self.forget_remote_if(addr, who);
                1
            }
        }
    }

    /// The peer paid back `amount` sends of its `addr` (an inbound `DEC_STRONG` naming a peer
    /// address), or an unsent one is rolled back. Returns the held release amount to send
    /// outside the lock once nothing is unpaid. An overpayment settles at 0: a peer that pays
    /// early only lets the release reach it early.
    #[must_use = "send the returned DEC_STRONG amount outside the RpcState lock"]
    pub fn pay_proxy_sends(&mut self, addr: &RpcAddress, amount: u32) -> u32 {
        let Some(sends) = self.remote_sends.get_mut(addr) else {
            return 0;
        };
        sends.unpaid = sends.unpaid.saturating_sub(u64::from(amount));
        if sends.unpaid > 0 {
            return 0;
        }
        let held = sends.held;
        self.remote_sends.remove(addr);
        // The last release of this generation goes out; a live successor keeps the book.
        let successor = self
            .remote_proxies
            .get(addr)
            .is_some_and(|weak| weak.strong_count() > 0);
        if held > 0 && !successor {
            self.remote_send_async_counters.remove(addr);
        }
        held
    }

    /// Session death: every unpaid send and held release goes with the peer's counts.
    pub fn clear_remote_sends(&mut self) {
        self.remote_sends.clear();
    }

    /// Test/diagnostic: number of live local nodes (leak check).
    pub fn local_node_count(&self) -> usize {
        self.local_nodes.len()
    }

    /// Live proxies for the obituary sweep (AOSP `sendObituaries`); fire `binder_died` unlocked.
    pub(crate) fn remote_proxy_snapshot(&self) -> Vec<sync::Arc<dyn IBinder>> {
        self.remote_proxies
            .values()
            .filter_map(sync::Weak::upgrade)
            .collect()
    }

    /// Post-increment `addr`'s send-side oneway number (from 0); module doc "Async-number wrap".
    pub fn next_send_async_number(&mut self, addr: RpcAddress) -> u64 {
        let counter = self.remote_send_async_counters.entry(addr).or_insert(0);
        let n = *counter;
        *counter = n.wrapping_add(1);
        if *counter == 0 {
            warn_async_wrap(&addr);
        }
        n
    }

    /// Roll back a `next_send_async_number(addr)` reservation when the oneway
    /// transaction that consumed it fails to send. The peer's receive-side
    /// counter expects a contiguous sequence, so a consumed-but-never-sent
    /// number leaves a permanent gap that parks every later oneway to `addr`
    /// in the peer's `async_todo`. A transport failure ends the session
    /// anyway (`session` module doc "Failed sends", as AOSP), so the rollback
    /// matters for a send refused before any byte — a codec error — after
    /// which the session goes on. Unlike the strong-count rollback this
    /// is an *ordering* sequence, so only roll back when we were the last
    /// consumer (`counter == consumed + 1`); if another thread already reserved
    /// the next number, rolling back would hand it out twice, so we leave the
    /// gap (the existing `ASYNC_TODO_TERMINATE_LEVEL` watermark is the backstop).
    pub fn cancel_send_async_number(&mut self, addr: RpcAddress, consumed: u64) {
        if let Some(counter) = self.remote_send_async_counters.get_mut(&addr) {
            if *counter == consumed.wrapping_add(1) {
                *counter = consumed;
            }
        }
    }

    /// Decide whether to dispatch an inbound oneway now
    /// or park it. Pass the wire `async_number` and the transaction
    /// body / fds (moved in; given back in [`AsyncDecision::Dispatch`]
    /// or owned by the heap on [`AsyncDecision::Enqueued`]). Twoway
    /// transactions never reach this method.
    ///
    /// AOSP `RpcState::processTransactInternal` lines 1093–1133.
    pub fn dispatch_async_or_enqueue(
        &mut self,
        addr: RpcAddress,
        wire_async: u64,
        txn: WireTransaction,
        in_fds: Vec<OwnedFd>,
    ) -> AsyncDecision {
        let Some(node) = self.local_nodes.get_mut(&addr) else {
            return AsyncDecision::Drop(DropReason::UnknownAddress);
        };
        match wire_async.cmp(&node.next_async_number) {
            std::cmp::Ordering::Equal => AsyncDecision::Dispatch(txn, in_fds),
            std::cmp::Ordering::Greater => {
                node.async_todo.push(Reverse(AsyncTodo {
                    async_number: wire_async,
                    transaction: txn,
                    in_fds,
                }));
                // AOSP RpcState.cpp:1109–1129: bound the out-of-order backlog and the fds it owns.
                let num_pending = node.async_todo.len();
                if num_pending >= ASYNC_TODO_TERMINATE_LEVEL {
                    // Free the backlog's memory/fds now, before session end reaches `clear_local`.
                    node.async_todo.clear();
                    return AsyncDecision::Terminate(num_pending);
                }
                if num_pending % ASYNC_TODO_WARN_PER == 0 {
                    log::warn!(
                        "RPC: {num_pending} pending out-of-order oneway transactions on {addr:?}"
                    );
                }
                AsyncDecision::Enqueued
            }
            std::cmp::Ordering::Less => AsyncDecision::Drop(DropReason::StaleAsyncNumber),
        }
    }

    /// After a successful dispatch (by the caller) of
    /// the previously-returned [`AsyncDecision::Dispatch`], advance
    /// the per-node counter and pop the next eligible queued entry
    /// (if its `async_number` matches the now-advanced counter). The
    /// caller calls this in a loop until `next` is `None`, then
    /// stops draining. Each pop dispatches outside the state lock.
    ///
    /// AOSP `RpcState::processTransactInternal` lines 1247–1278 (the
    /// `goto processTransactInternalTailCall` loop).
    ///
    /// Parked entries left below the new expected number (duplicates of a
    /// number already run) are dropped and counted in
    /// [`AsyncAdvance::purged`]: each was entered, so the caller still owes
    /// the peer a `DEC_STRONG` for it. AOSP leaves such an entry parked,
    /// unreachable, until the node goes.
    pub fn advance_and_pop_async(&mut self, addr: RpcAddress) -> AsyncAdvance {
        let mut out = AsyncAdvance::default();
        let Some(node) = self.local_nodes.get_mut(&addr) else {
            return out;
        };
        node.next_async_number = node.next_async_number.wrapping_add(1);
        if node.next_async_number == 0 {
            warn_async_wrap(&addr);
        }
        while let Some(Reverse(top)) = node.async_todo.peek() {
            if top.async_number >= node.next_async_number {
                break;
            }
            node.async_todo.pop();
            out.purged = out.purged.saturating_add(1);
        }
        if let Some(top) = node.async_todo.peek_mut() {
            if top.0.async_number == node.next_async_number {
                let Reverse(todo) = PeekMut::pop(top);
                out.next = Some((todo.transaction, todo.in_fds));
            }
        }
        out
    }

    /// Test: `(amount sum, command count)` of `DEC_STRONG`s received for `addr`, local or not.
    #[cfg(test)]
    pub(crate) fn dec_received(&self, addr: &RpcAddress) -> (u64, u64) {
        self.dec_received.get(addr).copied().unwrap_or_default()
    }

    /// Test: the address a local binder was sent under, if it has a node.
    #[cfg(test)]
    pub(crate) fn local_address_of(&self, binder: &SIBinder) -> Option<RpcAddress> {
        self.local_by_ptr.get(&binder_ptr(binder)).copied()
    }

    /// Test: depth of the `async_todo` queue at local `addr` (0 if no node).
    #[cfg(test)]
    pub(crate) fn async_todo_len(&self, addr: &RpcAddress) -> usize {
        self.local_nodes
            .get(addr)
            .map(|n| n.async_todo.len())
            .unwrap_or(0)
    }

    /// Test: `next_async_number` of the local node at `addr` (0 if no node).
    #[cfg(test)]
    pub(crate) fn next_async_number(&self, addr: &RpcAddress) -> u64 {
        self.local_nodes
            .get(addr)
            .map(|n| n.next_async_number)
            .unwrap_or(0)
    }
}

/// Logs an `async_number` wrap on either path; see module doc "Async-number wrap".
fn warn_async_wrap(addr: &RpcAddress) {
    log::warn!(
        "RPC: per-address async_number wrapped at u64::MAX for {addr:?} — \
         AOSP-divergent (AOSP terminates the session)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::*;
    use std::mem::ManuallyDrop;
    use std::sync::Arc;

    struct Dummy;
    impl IBinder for Dummy {
        fn link_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> crate::Result<()> {
            Err(crate::StatusCode::InvalidOperation)
        }
        fn unlink_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> crate::Result<()> {
            Err(crate::StatusCode::InvalidOperation)
        }
        fn ping_binder(&self) -> crate::Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_transactable(&self) -> Option<&dyn Transactable> {
            None
        }
        fn descriptor(&self) -> &str {
            "rsbinder.test.Dummy"
        }
        fn is_remote(&self) -> bool {
            false
        }
        fn inc_strong(&self, _: &SIBinder) -> crate::Result<()> {
            Ok(())
        }
        fn attempt_inc_strong(&self) -> bool {
            true
        }
        fn dec_strong(&self, _: Option<ManuallyDrop<SIBinder>>) -> crate::Result<()> {
            Ok(())
        }
        fn inc_weak(&self, _: &WIBinder) -> crate::Result<()> {
            Ok(())
        }
        fn dec_weak(&self) -> crate::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn leaving_is_idempotent_by_identity() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a1 = st.on_binder_leaving(&b).unwrap();
        let a2 = st.on_binder_leaving(&b).unwrap();
        assert_eq!(a1, a2, "same object → same address");
        assert_eq!(st.local_node_count(), 1);
        assert!(st.lookup_local(&a1).is_some());
    }

    /// A single DEC_STRONG drops the node to 0 → removed, no leak.
    #[test]
    fn dec_strong_releases_node() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();
        assert_eq!(st.local_node_count(), 1);
        assert!(
            st.dec_strong_local(&a, 1).is_some(),
            "node removed at strong 0"
        );
        assert_eq!(st.local_node_count(), 0, "no leak");
        assert!(st.lookup_local(&a).is_none());
        // DEC_STRONG on an unknown address is safe (idempotent).
        assert!(st.dec_strong_local(&a, 1).is_none());
    }

    /// One batched DEC_STRONG (`amount > 1`) frees a node sent that often; a fixed 1 would leak.
    #[test]
    fn dec_strong_honors_batched_amount() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap(); // strong 1
        st.on_binder_leaving(&b).unwrap(); // strong 2
        st.on_binder_leaving(&b).unwrap(); // strong 3
        assert_eq!(st.local_node_count(), 1);
        assert!(
            st.dec_strong_local(&a, 3).is_some(),
            "one batched DEC of amount 3 frees a node sent 3×"
        );
        assert_eq!(st.local_node_count(), 0, "no leak on batched drop");
    }

    /// Each send failure rolls back one bump; the node is freed only when the last bump is undone.
    #[test]
    fn cancel_binder_leaving_rolls_back_one_bump() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap(); // strong 1
        let a2 = st.on_binder_leaving(&b).unwrap(); // strong 2 (resend)
        assert_eq!(a, a2);
        assert_eq!(st.local_node_count(), 1);

        let _ = st.cancel_binder_leaving(&a);
        assert_eq!(
            st.local_node_count(),
            1,
            "still referenced by the other send"
        );
        assert!(st.lookup_local(&a).is_some());

        let _ = st.cancel_binder_leaving(&a);
        assert_eq!(st.local_node_count(), 0, "last bump cancelled → node freed");
        assert!(st.lookup_local(&a).is_none());

        // A stale `leaving_addrs` entry (the peer's DEC came first) must not touch another node.
        assert!(st.cancel_binder_leaving(&a).is_none());
        assert_eq!(st.local_node_count(), 0, "a stale cancel changes nothing");
        let other = SIBinder::new(Arc::new(Dummy)).unwrap();
        let o = st.on_binder_leaving(&other).unwrap();
        assert!(st.cancel_binder_leaving(&a).is_none());
        assert_eq!(
            st.local_node_count(),
            1,
            "a stale cancel leaves live nodes alone"
        );
        assert!(st.lookup_local(&o).is_some());
    }

    /// A reserved `async_number` rolls back only if no later one was reserved (else issued twice).
    #[test]
    fn cancel_send_async_number_only_when_last_consumer() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();

        assert_eq!(st.next_send_async_number(a), 0);
        let consumed = st.next_send_async_number(a); // 1
        assert_eq!(consumed, 1);
        // We were the last consumer → rollback; the number is handed out again.
        st.cancel_send_async_number(a, consumed);
        assert_eq!(st.next_send_async_number(a), 1, "rolled back");

        // Now simulate a concurrent send advancing past us before we cancel.
        let consumed2 = st.next_send_async_number(a); // 2
        let _other = st.next_send_async_number(a); // 3 (another in-flight send)
        st.cancel_send_async_number(a, consumed2);
        assert_eq!(
            st.next_send_async_number(a),
            4,
            "no rollback when not the last consumer"
        );
    }

    /// At `u32::MAX` a new mint fails (the 32-bit wire address would alias); resends still work.
    #[test]
    fn address_counter_exhaustion_is_rejected_not_aliased() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        // Just below the bound: minting still succeeds.
        st.addr_counter = (u32::MAX as u64) - 1;
        let b1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        assert!(st.on_binder_leaving(&b1).is_ok());
        // At the bound: a *new* object cannot be minted (would alias).
        let b2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        assert!(st.on_binder_leaving(&b2).is_err());
        // A resend of the already-registered object reuses its address.
        assert!(st.on_binder_leaving(&b1).is_ok());
    }

    /// Two `RpcState`s share no table or counter: neither resolves nor mutates the other's nodes.
    #[test]
    fn two_states_are_isolated() {
        let mut s1 = RpcState::new(AddressSpace::Acceptor);
        let s2 = RpcState::new(AddressSpace::Acceptor); // fresh, empty, independent table
        let b1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a1 = s1.on_binder_leaving(&b1).unwrap();

        // s2 registered nothing, so it cannot resolve s1's address even with equal bytes.
        assert!(
            s2.lookup_local(&a1).is_none(),
            "independent tables: a fresh session knows no foreign address"
        );
        assert_eq!(s1.local_node_count(), 1);
        assert_eq!(s2.local_node_count(), 0);

        // Mutating s1 never affects s2 (no shared storage).
        let _ = s1.dec_strong_local(&a1, 1);
        assert_eq!(s1.local_node_count(), 0);
        assert_eq!(s2.local_node_count(), 0);
    }

    /// An r34-random address whose byte 8 is our tag is a remote; our own shape is refused.
    #[test]
    fn own_subspace_refusal_is_by_minted_shape() {
        for space in [AddressSpace::Initiator, AddressSpace::Acceptor] {
            let mut st = RpcState::new(space);
            let mut r34 = [0x5au8; 32];
            r34[8] = space.tag();
            let sib = SIBinder::new(Arc::new(Dummy)).unwrap();
            let (_p, excess) = st
                .remote_proxy(RpcAddress::from_wire_bytes(r34), || sib.clone())
                .expect("an android-12 r34 random address is a remote, whatever byte 8 holds");
            assert!(!excess);

            let mut ctr = 0u64;
            let ours = RpcAddress::unique(&mut ctr, space);
            assert_eq!(
                st.remote_proxy(ours, || panic!("must not mint")).err(),
                Some(crate::StatusCode::BadValue),
                "an unknown address of our own minted shape is forged"
            );
        }
    }

    /// A stale `RpcProxy::drop` after a re-cache must not evict the successor proxy (no threads).
    #[test]
    fn stale_drop_does_not_split_remote_dedup() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([7u8; 32]); // RPC_ADDR_LEN

        // P1's last strong ref goes (cached `Weak` dead) before P1's `Drop` has run.
        let sib1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let p1 = Arc::as_ptr(sib1.as_arc()) as *const ();
        let (got1, ex1) = st
            .remote_proxy(addr, || sib1.clone())
            .expect("remote_proxy");
        assert!(!ex1, "first receipt mints a proxy — not an excess");
        drop(got1);
        drop(sib1);

        // A concurrent `read_binder` for the same address sees the dead `Weak`, caches P2.
        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got2, ex2) = st
            .remote_proxy(addr, || sib2.clone())
            .expect("remote_proxy");
        assert!(!ex2, "dead-Weak ⇒ re-mint, not an excess receipt");
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();

        // P1's delayed `Drop` runs `forget_remote_if(addr, P1)`; the live P2 slot must stay.
        st.forget_remote_if(&addr, p1);
        let (again, ex_again) = st
            .remote_proxy(addr, || panic!("must dedup to P2, not re-make"))
            .expect("remote_proxy");
        assert!(
            Arc::ptr_eq(again.as_arc(), got2.as_arc()),
            "stale P1 Drop must not split the per-address dedup"
        );
        assert!(
            ex_again,
            "reusing the live P2 is a duplicate receipt (excess)"
        );

        // The genuinely-current proxy's Drop *does* evict.
        drop(again);
        drop(got2);
        drop(sib2);
        st.forget_remote_if(&addr, p2);
        let sib3 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let mut remade = false;
        let (_p3, ex3) = st
            .remote_proxy(addr, || {
                remade = true;
                sib3.clone()
            })
            .expect("remote_proxy");
        assert!(
            remade,
            "after identity-checked forget, the address re-mints"
        );
        assert!(!ex3, "re-mint after forget is a fresh proxy — not excess");
    }

    /// A matching `forget_remote_if` resets the send counter to 0; a stale one leaves it intact.
    #[test]
    fn forget_remote_if_gcs_send_counter() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([3u8; 32]);

        let sib1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got1, _) = st
            .remote_proxy(addr, || sib1.clone())
            .expect("remote_proxy");
        let p1 = Arc::as_ptr(got1.as_arc()) as *const ();
        assert_eq!(st.next_send_async_number(addr), 0);
        assert_eq!(st.next_send_async_number(addr), 1);

        // Stale Drop (P2 re-cached): identity mismatch makes `forget_remote_if` a no-op.
        drop(got1);
        drop(sib1);
        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got2, _) = st
            .remote_proxy(addr, || sib2.clone())
            .expect("remote_proxy");
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();
        st.forget_remote_if(&addr, p1);
        assert_eq!(
            st.next_send_async_number(addr),
            2,
            "stale forget must not evict the live counter"
        );

        // Genuine `forget`: the counter drops and the next read restarts at 0.
        drop(got2);
        drop(sib2);
        st.forget_remote_if(&addr, p2);
        assert_eq!(
            st.next_send_async_number(addr),
            0,
            "post-forget counter restarts from 0 (peer's BinderNode \
             reaches timesSent=0 in lockstep with the matching DEC)"
        );
    }

    /// A release held by an unpaid send keeps the `async_number` book open (the peer's node is
    /// alive), a successor proxy numbers on from it, and the book closes with the last release.
    #[test]
    fn a_held_release_keeps_the_send_counter_for_a_successor() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([3u8; 32]);

        let sib1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got1, _) = st.remote_proxy(addr, || sib1.clone()).unwrap();
        let p1 = Arc::as_ptr(got1.as_arc()) as *const ();
        assert_eq!(st.next_send_async_number(addr), 0);
        st.on_proxy_leaving(addr);
        drop((got1, sib1));
        assert_eq!(st.release_proxy(&addr, p1), 0, "held: a send is unpaid");
        assert_eq!(st.next_send_async_number(addr), 1, "the book stays open");

        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got2, excess) = st.remote_proxy(addr, || sib2.clone()).unwrap();
        assert!(
            !excess,
            "P1 is gone: a new proxy, which owes its own release"
        );
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();
        assert_eq!(
            st.next_send_async_number(addr),
            2,
            "the successor numbers on"
        );
        assert_eq!(st.pay_proxy_sends(&addr, 1), 1, "P1's release goes");
        assert_eq!(st.next_send_async_number(addr), 3, "P2 still uses the book");

        drop((got2, sib2));
        assert_eq!(st.release_proxy(&addr, p2), 1, "nothing unpaid: at once");
        assert_eq!(st.next_send_async_number(addr), 0, "the book closed");
    }

    /// With no successor the held release closes the book; payments saturate at 0, a rolled
    /// back send counts as paid, and session death drops what is held.
    #[test]
    fn held_releases_settle_on_payment_rollback_and_session_death() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([3u8; 32]);
        let sib = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got, _) = st.remote_proxy(addr, || sib.clone()).unwrap();
        let p = Arc::as_ptr(got.as_arc()) as *const ();
        assert_eq!(st.next_send_async_number(addr), 0);
        st.on_proxy_leaving(addr);
        st.on_proxy_leaving(addr);
        st.on_proxy_leaving(addr);
        drop((got, sib));
        assert_eq!(st.release_proxy(&addr, p), 0);
        assert_eq!(
            st.cancel_leaving(&addr).1,
            0,
            "rolled back: two still unpaid"
        );
        assert_eq!(st.pay_proxy_sends(&addr, 1), 0, "one still unpaid");
        assert_eq!(
            st.pay_proxy_sends(&addr, 5),
            1,
            "an overpayment settles at 0"
        );
        assert_eq!(st.pay_proxy_sends(&addr, 1), 0, "nothing left to pay");
        assert_eq!(st.next_send_async_number(addr), 0, "the book closed");

        st.on_proxy_leaving(addr);
        assert_eq!(st.release_proxy(&addr, std::ptr::null()), 0);
        st.clear_remote_sends();
        assert_eq!(st.pay_proxy_sends(&addr, 1), 0, "dropped with the session");
    }

    /// `timesSent` nets one `DEC_STRONG` per send: N sends to one peer, or one send per connection.
    #[test]
    fn times_sent_balance_frees_node() {
        // (a) N sends to one deduping peer: strong = N = (N−1 excess DECs) + 1 at proxy drop.
        let mut srv = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = srv.on_binder_leaving(&b).unwrap();
        let a2 = srv.on_binder_leaving(&b).unwrap();
        let a3 = srv.on_binder_leaving(&b).unwrap();
        assert_eq!((a, a), (a2, a3), "identity ⇒ same address on re-send");
        assert_eq!(srv.local_node_count(), 1, "one node, strong = timesSent");
        // Peer: 3 receipts, proxy live ⇒ receipts 2 and 3 are excess DECs; proxy drop = 1.
        let mut peer = RpcState::new(AddressSpace::Initiator);
        let pb = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (_p, e1) = peer.remote_proxy(a, || pb.clone()).expect("remote_proxy");
        let (_p2, e2) = peer.remote_proxy(a, || pb.clone()).expect("remote_proxy");
        let (_p3, e3) = peer.remote_proxy(a, || pb.clone()).expect("remote_proxy");
        assert_eq!(
            (e1, e2, e3),
            (false, true, true),
            "1st mints; 2nd/3rd are excess receipts (owe a flush DEC)"
        );
        // 2 excess DECs + 1 proxy-drop DEC = 3 = timesSent ⇒ freed.
        assert!(srv.dec_strong_local(&a, 1).is_none());
        assert!(srv.dec_strong_local(&a, 1).is_none());
        assert!(
            srv.dec_strong_local(&a, 1).is_some(),
            "3rd DEC frees the node"
        );
        assert_eq!(srv.local_node_count(), 0, "no leak (AC-2.5)");

        // (b) One send per sibling connection ⇒ strong 2; the node must survive the 1st DEC.
        let mut s = RpcState::new(AddressSpace::Acceptor);
        let o = SIBinder::new(Arc::new(Dummy)).unwrap();
        let x = s.on_binder_leaving(&o).unwrap(); // conn #1 send
        let _ = s.on_binder_leaving(&o).unwrap(); // conn #2 send (timesSent ⇒ 2)
        assert!(
            s.dec_strong_local(&x, 1).is_none(),
            "conn #1 proxy drop must NOT free a node conn #2 still holds"
        );
        assert!(s.lookup_local(&x).is_some(), "sibling still reachable");
        assert!(
            s.dec_strong_local(&x, 1).is_some(),
            "conn #2 proxy drop frees it"
        );
        assert_eq!(s.local_node_count(), 0, "no leak");
    }

    fn mk_txn(addr: RpcAddress, async_n: u64) -> WireTransaction {
        WireTransaction {
            address: addr,
            code: 1,
            flags: crate::binder::FLAG_ONEWAY,
            async_number: async_n,
            data: vec![],
            object_positions: vec![],
        }
    }

    /// Send side: `next_send_async_number` post-increments per address, each one independently.
    #[test]
    fn send_async_number_is_per_address_monotonic() {
        let mut st = RpcState::new(AddressSpace::Initiator);
        let a = RpcAddress::from_wire_bytes([1u8; 32]);
        let b = RpcAddress::from_wire_bytes([2u8; 32]);
        assert_eq!(st.next_send_async_number(a), 0);
        assert_eq!(st.next_send_async_number(a), 1);
        assert_eq!(
            st.next_send_async_number(b),
            0,
            "per-address — b starts at 0"
        );
        assert_eq!(st.next_send_async_number(a), 2);
        assert_eq!(st.next_send_async_number(b), 1);
    }

    /// In-order `async_number` dispatches at once; the advance runs in `advance_and_pop_async`.
    #[test]
    fn in_order_dispatches_and_advances_counter() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();
        assert_eq!(st.next_async_number(&a), 0);
        for i in 0..5u64 {
            let txn = mk_txn(a, i);
            match st.dispatch_async_or_enqueue(a, i, txn, vec![]) {
                AsyncDecision::Dispatch(t, _) => assert_eq!(t.async_number, i),
                other => panic!("in-order async_number {i} must dispatch, got {other:?}"),
            }
            assert_eq!(st.async_todo_len(&a), 0, "in-order ⇒ never enqueued");
            assert!(
                st.advance_and_pop_async(a).next.is_none(),
                "queue empty ⇒ drain returns None"
            );
            assert_eq!(st.next_async_number(&a), i + 1);
        }
    }

    /// Early `async_number`s park, then drain in order (AOSP `processTransactInternal` enqueue).
    #[test]
    fn out_of_order_enqueues_then_drains_in_priority_order() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();

        // libbinder round-robin over 2 slots can arrive 2,4,1,3,0; dispatch must be 0..=4.
        for arrival_async in [2u64, 4, 1, 3, 0] {
            let txn = mk_txn(a, arrival_async);
            let decision = st.dispatch_async_or_enqueue(a, arrival_async, txn, vec![]);
            if arrival_async == 0 {
                // Last to arrive: 0 matches expected, so it dispatches.
                match decision {
                    AsyncDecision::Dispatch(t, _) => assert_eq!(t.async_number, 0),
                    _ => panic!("arrival 0 must dispatch"),
                }
                break;
            } else {
                assert!(
                    matches!(decision, AsyncDecision::Enqueued),
                    "out-of-order arrival {arrival_async} must enqueue (expected was 0)"
                );
            }
        }
        // After 0, `advance_and_pop_async` must drain 1, 2, 3, 4 in strict order.
        assert_eq!(st.async_todo_len(&a), 4, "1, 2, 3, 4 parked");
        let mut dispatched = vec![0u64];
        while let Some((t, _)) = st.advance_and_pop_async(a).next {
            dispatched.push(t.async_number);
        }
        assert_eq!(
            dispatched,
            vec![0, 1, 2, 3, 4],
            "per-node monotonic dispatch despite wire reorder"
        );
        assert_eq!(st.async_todo_len(&a), 0, "drained");
        // The dispatch of 4 still advances the counter, so 5 is expected next.
        assert_eq!(st.next_async_number(&a), 5);
    }

    /// Parking up to the terminate watermark yields `Terminate` and flushes the node's backlog.
    #[test]
    fn async_todo_terminate_caps_and_flushes_backlog() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();

        // Expected is 0 and never arrives; 1..watermark all park.
        for n in 1..ASYNC_TODO_TERMINATE_LEVEL as u64 {
            let txn = mk_txn(a, n);
            assert!(
                matches!(
                    st.dispatch_async_or_enqueue(a, n, txn, vec![]),
                    AsyncDecision::Enqueued
                ),
                "async_number {n} below the watermark must park"
            );
        }
        assert_eq!(st.async_todo_len(&a), ASYNC_TODO_TERMINATE_LEVEL - 1);

        // The push that reaches the watermark terminates and flushes.
        let n = ASYNC_TODO_TERMINATE_LEVEL as u64;
        let txn = mk_txn(a, n);
        match st.dispatch_async_or_enqueue(a, n, txn, vec![]) {
            AsyncDecision::Terminate(pending) => {
                assert_eq!(pending, ASYNC_TODO_TERMINATE_LEVEL)
            }
            other => panic!("watermark must terminate, got {other:?}"),
        }
        assert_eq!(st.async_todo_len(&a), 0, "backlog flushed on terminate");
    }

    /// An unknown address drops the oneway instead of parking it (AOSP `mNodeForAddress.find`).
    #[test]
    fn unknown_address_drops_not_enqueues() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let unknown = RpcAddress::from_wire_bytes([9u8; 32]);
        let txn = mk_txn(unknown, 0);
        assert!(matches!(
            st.dispatch_async_or_enqueue(unknown, 0, txn, vec![]),
            AsyncDecision::Drop(DropReason::UnknownAddress)
        ));
        // `advance_and_pop_async` on an unknown address is a no-op.
        let advance = st.advance_and_pop_async(unknown);
        assert!(advance.next.is_none());
        assert_eq!(advance.purged, 0);
    }

    /// A stale `async_number` drops unqueued; parked entries below expected drain on advance.
    #[test]
    fn stale_arrival_drops_and_heap_drains_below_expected() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();

        // Advance expected to 3 by dispatching 0,1,2 in order.
        for i in 0..3u64 {
            let txn = mk_txn(a, i);
            assert!(matches!(
                st.dispatch_async_or_enqueue(a, i, txn, vec![]),
                AsyncDecision::Dispatch(_, _)
            ));
            let _ = st.advance_and_pop_async(a);
        }
        assert_eq!(st.next_async_number(&a), 3);

        // A stale arrival (1 < 3) reports the reason and does not enqueue.
        let txn = mk_txn(a, 1);
        assert!(matches!(
            st.dispatch_async_or_enqueue(a, 1, txn, vec![]),
            AsyncDecision::Drop(DropReason::StaleAsyncNumber)
        ));
        assert_eq!(st.async_todo_len(&a), 0);

        // Heap stale-drain: a duplicate 5 still parked once expected passes 5 is reaped.
        for _ in 0..2 {
            let _ = st.dispatch_async_or_enqueue(a, 5, mk_txn(a, 5), vec![]);
        }
        assert_eq!(st.async_todo_len(&a), 2);
        // Dispatch a matching 3 + 4 to advance up to 5.
        let _ = st.dispatch_async_or_enqueue(a, 3, mk_txn(a, 3), vec![]);
        let _ = st.advance_and_pop_async(a); // expected → 4
        let _ = st.dispatch_async_or_enqueue(a, 4, mk_txn(a, 4), vec![]);
        let popped = st.advance_and_pop_async(a); // expected → 5; pops one parked 5.
        assert_eq!(popped.purged, 0);
        assert_eq!(popped.next.map(|(t, _)| t.async_number), Some(5));
        assert_eq!(st.async_todo_len(&a), 1, "the duplicate 5 stays parked");
        // Advancing to 6 drains the duplicate below expected instead of leaving it on top.
        let advance = st.advance_and_pop_async(a);
        assert!(advance.next.is_none());
        assert_eq!(
            advance.purged, 1,
            "the dropped duplicate is reported: it owes a DEC"
        );
        assert_eq!(st.async_todo_len(&a), 0);
        assert_eq!(st.next_async_number(&a), 6);
    }

    /// Each `LocalNode` has its own counter and queue: a stalled node A does not block node B.
    #[test]
    fn async_order_is_per_node() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let b2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a1 = st.on_binder_leaving(&b1).unwrap();
        let a2 = st.on_binder_leaving(&b2).unwrap();

        // Node a1: arrival 1 enqueued (expected 0).
        let txn = mk_txn(a1, 1);
        assert!(matches!(
            st.dispatch_async_or_enqueue(a1, 1, txn, vec![]),
            AsyncDecision::Enqueued
        ));
        assert_eq!(st.async_todo_len(&a1), 1);
        // Node a2: independent counter at 0 ⇒ arrival 0 dispatches though a1 is blocked.
        let txn = mk_txn(a2, 0);
        match st.dispatch_async_or_enqueue(a2, 0, txn, vec![]) {
            AsyncDecision::Dispatch(t, _) => assert_eq!(t.async_number, 0),
            _ => panic!("a2 must dispatch independently of a1's stalled queue"),
        }
        assert_eq!(st.async_todo_len(&a2), 0);
        // Counters are truly independent.
        assert_eq!(st.next_async_number(&a1), 0, "a1 not yet advanced");
        assert!(st.advance_and_pop_async(a2).next.is_none());
        assert_eq!(st.next_async_number(&a2), 1);
    }
}
