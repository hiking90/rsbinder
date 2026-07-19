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
//! Ref-count model (AOSP `RpcState` `BinderNode::timesSent` /
//! `flushExcessBinderRefs`): a local
//! object gets one address by *identity* (`Arc` pointer dedup, so the
//! same object always marshals to the same address), but the entry's
//! strong count is **`timesSent`**: it starts at 1 on the first send
//! and is **incremented on every subsequent send** (each flatten to
//! the peer is one reference the peer will eventually `DEC_STRONG`).
//! Each inbound `DEC_STRONG` decrements it; the entry (and its strong
//! `SIBinder`) is dropped at 0, so there is no leak.
//!
//! The peer dedups one `RpcProxy` per address. To keep the books
//! balanced when it *receives the same binder more than once* while a
//! proxy is still live, it owes the sender one `DEC_STRONG` per excess
//! receipt — the rsbinder equivalent of AOSP `flushExcessBinderRefs`
//! ([`RpcState::remote_proxy`] reports the excess; the session sends
//! the `DEC_STRONG` **outside** the state lock — see
//! `RpcSessionInner::read_binder`). Net: exactly one `DEC_STRONG` per
//! send ⇒ balanced ⇒ no leak whether the binder is sent N× to one
//! peer (dedup + N−1 excess DECs + 1 drop DEC) **or** once to each of
//! N independent peer connections sharing a session (N sends, N drop
//! DECs). Pinning the count at 1 by identity would silently break the
//! latter (the first connection's proxy drop frees the node ⇒ the
//! sibling connection's proxy `DeadObject`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::os::fd::OwnedFd;
use std::sync::{self, Arc};

use crate::binder::{IBinder, SIBinder};

use super::address::{AddressSpace, RpcAddress};
use super::wire::WireTransaction;

/// Per-node `asyncTodo` queue entry. `Ord` by `async_number` so a
/// `BinaryHeap<Reverse<AsyncTodo>>` gives min-heap top-is-smallest
/// (AOSP `BinderNode::AsyncTodo` `operator<` uses the same trick).
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
    /// `wire_async < node.asyncNumber` — duplicate / replay / peer
    /// bug; AOSP-divergent (AOSP terminates the session here).
    StaleAsyncNumber,
}

/// AOSP `RpcState.cpp` `kArbitraryOnewayCallTerminateLevel`: once this
/// many out-of-order oneways are parked on a single node, the peer is
/// treated as hostile/buggy — the node's parked backlog is flushed
/// (reclaiming its memory + held fds at once) and the delivering
/// connection is torn down, rather than letting the per-node
/// `async_todo` queue grow without bound (memory + fd exhaustion DoS).
/// This bounds a node to at most this many parked entries at any instant.
const ASYNC_TODO_TERMINATE_LEVEL: usize = 10000;
/// AOSP `kArbitraryOnewayCallWarnLevel` / `kArbitraryOnewayCallWarnPer`
/// (both 1000): emit a warning at each multiple of this once the queue
/// is this deep, so a building backlog is observable before the
/// terminate watermark.
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
    /// The per-node `async_todo` queue reached the terminate watermark
    /// (count carried for logging); the backlog has already been flushed
    /// here. The caller must tear the delivering connection down with
    /// `FAILED_TRANSACTION`. Unlike AOSP `shutdownAndWait` this is
    /// connection-level, not whole-session — but the flush above means
    /// the backlog is reclaimed regardless of how many connections the
    /// session has.
    Terminate(usize),
}

/// A local object exposed to the peer under [`RpcAddress`].
struct LocalNode {
    /// Strong ref keeps the local object alive while the peer holds it.
    binder: SIBinder,
    /// RPC strong count the peer holds (0 ⇒ drop the node).
    strong: i64,
    /// AOSP `BinderNode::asyncNumber` (server side) — per-node, not
    /// session-global.
    next_async_number: u64,
    /// AOSP `BinderNode::asyncTodo` — min-heap (via `Reverse`) of
    /// out-of-order inbound oneway transactions.
    async_todo: BinaryHeap<Reverse<AsyncTodo>>,
}

/// Per-session object/address table. Owned by `RpcSessionInner` behind
/// a `Mutex`; never global (enforced by the `rpc_no_globals` grep
/// gate).
pub struct RpcState {
    /// Objects we exposed to the peer, keyed by assigned address.
    local_nodes: HashMap<RpcAddress, LocalNode>,
    /// Dedup: local object `Arc` identity → its assigned address, so
    /// the same object always marshals to the same address.
    local_by_ptr: HashMap<usize, RpcAddress>,
    /// Remote proxies we hold, keyed by address. `Weak` so the table
    /// does not keep them alive; lets us dedup one `RpcProxy` per
    /// address and observe its last drop.
    remote_proxies: HashMap<RpcAddress, sync::Weak<dyn IBinder>>,
    /// AOSP `BinderNode::asyncNumber` (client side). Entries are
    /// dropped together with the proxy slot in `forget_remote_if`
    /// (peer's `timesSent` also reaches 0 then, so its `BinderNode`
    /// is GC'd and the counter restart matches). The narrow race —
    /// DEC still in flight when a sibling connection re-resolves the
    /// same address — degrades to a single best-effort oneway drop
    /// on the peer's `Drop(StaleAsyncNumber)` arm.
    remote_send_async_counters: HashMap<RpcAddress, u64>,
    /// Monotonic address allocator (per-session).
    addr_counter: u64,
    /// This endpoint's address subspace (initiator vs acceptor) so the
    /// two peers on a connection never mint colliding addresses.
    space: AddressSpace,
}

/// Stable identity for a local binder's allocation (data pointer of
/// the trait-object `Arc`).
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
            addr_counter: 0,
            space,
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
    pub fn on_binder_leaving(&mut self, binder: &SIBinder) -> crate::Result<RpcAddress> {
        let ptr = binder_ptr(binder);
        if let Some(&addr) = self.local_by_ptr.get(&ptr) {
            if let Some(node) = self.local_nodes.get_mut(&addr) {
                node.strong += 1;
            }
            return Ok(addr);
        }
        // The android-13+ wire encodes only the low 32 bits of the address
        // counter (`encode_addr`), so past `u32::MAX` two live nodes would
        // alias to one `RpcWireAddress` and mis-dispatch. Hard-stop rather
        // than alias (vs. the async-number wrap, an ordering-only concern that
        // only warns); ~2^32 live local objects per session is unreachable.
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

    /// Roll back one `on_binder_leaving` strong bump for `addr` when the
    /// transaction that would have carried the binder fails to send (AOSP
    /// `cancelBinderLeaving`). The peer never received the binder, so it will
    /// never send the matching `DEC_STRONG`; without this the node (and the
    /// strong `SIBinder` it pins) would leak for the rest of a multi-connection
    /// session, which — unlike AOSP — rsbinder does not tear down on a send
    /// failure. The bump and this rollback are a commutative ±1 on a count, so
    /// this is safe even if another thread concurrently sends the same binder.
    /// Drops the node once the count reaches 0, exactly like an inbound DEC.
    pub fn cancel_binder_leaving(&mut self, addr: &RpcAddress) {
        if let Some(node) = self.local_nodes.get_mut(addr) {
            node.strong -= 1;
            if node.strong <= 0 {
                self.local_nodes.remove(addr);
                self.local_by_ptr.retain(|_, a| a != addr);
            }
        }
    }

    /// Apply an inbound `DEC_STRONG` for `addr` by `amount` (AOSP
    /// `doDecStrong`: `timesSent -= amount`). A compliant peer may batch more
    /// than one decrement into a single command, so the amount must be honored —
    /// applying a fixed 1 would leak the node on a batched drop. Drops the node
    /// (and its strong `SIBinder`) once the count reaches 0 — no leak. A hostile
    /// over-decrement simply removes the node early (contained: the peer loses
    /// access), and `strong: i64` cannot underflow for a `u32` amount.
    /// Returns `true` if the node was removed.
    pub fn dec_strong_local(&mut self, addr: &RpcAddress, amount: u32) -> bool {
        if let Some(node) = self.local_nodes.get_mut(addr) {
            node.strong -= amount as i64;
            if node.strong <= 0 {
                self.local_nodes.remove(addr);
                self.local_by_ptr.retain(|_, a| a != addr);
                return true;
            }
        }
        false
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
    pub fn remote_proxy<F>(&mut self, addr: RpcAddress, make: F) -> (SIBinder, bool)
    where
        F: FnOnce() -> SIBinder,
    {
        if let Some(weak) = self.remote_proxies.get(&addr) {
            if let Some(arc) = weak.upgrade() {
                return (SIBinder::from_arc(arc), true);
            }
        }
        let sib = make();
        self.remote_proxies
            .insert(addr, Arc::downgrade(sib.as_arc()));
        (sib, false)
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
    pub fn forget_remote_if(&mut self, addr: &RpcAddress, who: *const ()) {
        if let Some(weak) = self.remote_proxies.get(addr) {
            if weak.as_ptr() as *const () == who {
                self.remote_proxies.remove(addr);
                // The proxy that owned this address is gone; the
                // matching `DEC_STRONG` will be sent shortly. After it
                // lands, the peer's `BinderNode` either survives (if
                // `timesSent > 0` on the peer side — we re-resolve and
                // restart from 0) or is GC'd (counter is irrelevant).
                // Either way the per-address `async_number` book is
                // closed for *this* proxy generation; drop it so the
                // map stays bounded by the live address set.
                self.remote_send_async_counters.remove(addr);
            }
        }
    }

    /// Test/diagnostic: number of live local nodes (leak check).
    pub fn local_node_count(&self) -> usize {
        self.local_nodes.len()
    }

    /// Strong snapshot of every cached remote proxy still alive, for
    /// the session's connection-loss obituary sweep (AOSP
    /// `RpcState::sendObituaries` gathers strong pointers under the
    /// node lock, then the *caller* fires `binder_died` **after**
    /// releasing the lock — so a recipient may re-enter
    /// `unlink_to_death` without deadlocking). Dead `Weak`s are
    /// skipped (their proxies are already gone).
    pub(crate) fn remote_proxy_snapshot(&self) -> Vec<sync::Arc<dyn IBinder>> {
        self.remote_proxies
            .values()
            .filter_map(sync::Weak::upgrade)
            .collect()
    }

    /// Post-increment the per-remote-address
    /// send-side `async_number` (AOSP `nodeProgressAsyncNumber` on the
    /// send path). Returns the value to stamp on the outgoing wire.
    /// Auto-creates the counter at `0` if unseen. Decoupled from
    /// `remote_proxies` so the counter survives a proxy `Drop` + re-
    /// resolve on a sibling connection: the
    /// peer's `BinderNode` is still alive (`timesSent > 0` on any
    /// active connection), and resetting our counter would replay
    /// numbers the peer's `asyncTodo` already processed — stalling
    /// the per-node monotonic-stream contract forever.
    ///
    /// Overflow: u64 wrap means a session issued 2^64 oneways to one
    /// node, effectively unreachable; AOSP `nodeProgressAsyncNumber`
    /// returns `false` and tears down the session at overflow. We
    /// match by wrapping + logging (rsbinder has no `shutdownAndWait`
    /// equivalent on this path; the peer will surface as a protocol
    /// error on the duplicate).
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
    /// in the peer's `async_todo` (rsbinder, unlike AOSP, does not tear the
    /// session down on a send failure). Unlike the strong-count rollback this
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
        if wire_async == node.next_async_number {
            AsyncDecision::Dispatch(txn, in_fds)
        } else if wire_async > node.next_async_number {
            node.async_todo.push(Reverse(AsyncTodo {
                async_number: wire_async,
                transaction: txn,
                in_fds,
            }));
            // AOSP RpcState.cpp lines 1109–1129: bound the out-of-order
            // backlog so a peer that addresses a known node with
            // ever-increasing future async numbers (while the expected
            // one never arrives) cannot grow this heap — and the fds it
            // owns — without limit.
            let num_pending = node.async_todo.len();
            if num_pending >= ASYNC_TODO_TERMINATE_LEVEL {
                // Flush the abusive node's backlog now so its memory +
                // any held fds are reclaimed immediately, independent of
                // the caller's connection teardown. Without this, on a
                // multi-connection session the heap would persist (and a
                // reconnecting peer could re-accrete one entry per
                // terminate); flushing keeps the bound tight.
                node.async_todo.clear();
                return AsyncDecision::Terminate(num_pending);
            }
            if num_pending % ASYNC_TODO_WARN_PER == 0 {
                log::warn!(
                    "RPC: {num_pending} pending out-of-order oneway transactions on {addr:?}"
                );
            }
            AsyncDecision::Enqueued
        } else {
            AsyncDecision::Drop(DropReason::StaleAsyncNumber)
        }
    }

    /// After a successful dispatch (by the caller) of
    /// the previously-returned [`AsyncDecision::Dispatch`], advance
    /// the per-node counter and pop the next eligible queued entry
    /// (if its `async_number` matches the now-advanced counter). The
    /// caller calls this in a loop until it returns `None`, then
    /// stops draining. Each pop dispatches outside the state lock.
    ///
    /// AOSP `RpcState::processTransactInternal` lines 1247–1278 (the
    /// `goto processTransactInternalTailCall` loop).
    pub fn advance_and_pop_async(
        &mut self,
        addr: RpcAddress,
    ) -> Option<(WireTransaction, Vec<OwnedFd>)> {
        let node = self.local_nodes.get_mut(&addr)?;
        node.next_async_number = node.next_async_number.wrapping_add(1);
        if node.next_async_number == 0 {
            warn_async_wrap(&addr);
        }
        // Drop heap entries from a hostile/buggy peer that retried below
        // the expected number (AOSP-divergent — AOSP terminates the
        // session; we treat them as best-effort oneway loss).
        while let Some(Reverse(top)) = node.async_todo.peek() {
            if top.async_number >= node.next_async_number {
                break;
            }
            node.async_todo.pop();
        }
        if let Some(Reverse(top)) = node.async_todo.peek() {
            if top.async_number == node.next_async_number {
                let Reverse(todo) = node.async_todo.pop().expect("peek-pop");
                return Some((todo.transaction, todo.in_fds));
            }
        }
        None
    }

    /// Test/diagnostic: depth of the `async_todo` queue for a given
    /// local address (0 if no node). Used by unit tests to
    /// assert the parking behavior + drain.
    #[cfg(test)]
    pub(crate) fn async_todo_len(&self, addr: &RpcAddress) -> usize {
        self.local_nodes
            .get(addr)
            .map(|n| n.async_todo.len())
            .unwrap_or(0)
    }

    /// Test/diagnostic: current `next_async_number` for a local node
    /// (0 if no node) — used by unit tests to verify the
    /// counter advances exactly per dispatched oneway.
    #[cfg(test)]
    pub(crate) fn next_async_number(&self, addr: &RpcAddress) -> u64 {
        self.local_nodes
            .get(addr)
            .map(|n| n.next_async_number)
            .unwrap_or(0)
    }
}

/// Shared by send-side post-increment and receive-side advance: the
/// per-node `async_number` is a `u64`, so wrap is "issued 2^64 oneways
/// to one node" — effectively unreachable. AOSP's
/// `nodeProgressAsyncNumber` returns `false` and tears down the
/// session at overflow; rsbinder has no equivalent kill switch on this
/// path, so we log + let the peer surface it as a duplicate.
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
        assert!(st.dec_strong_local(&a, 1), "node removed at strong 0");
        assert_eq!(st.local_node_count(), 0, "no leak");
        assert!(st.lookup_local(&a).is_none());
        // DEC_STRONG on an unknown address is safe (idempotent).
        assert!(!st.dec_strong_local(&a, 1));
    }

    /// A batched DEC_STRONG (`amount > 1`, as a compliant libbinder peer sends
    /// on a deduped drop) must free a node sent multiple times in one command —
    /// applying a fixed 1 would leak `amount - 1` refs.
    #[test]
    fn dec_strong_honors_batched_amount() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap(); // strong 1
        st.on_binder_leaving(&b).unwrap(); // strong 2
        st.on_binder_leaving(&b).unwrap(); // strong 3
        assert_eq!(st.local_node_count(), 1);
        assert!(
            st.dec_strong_local(&a, 3),
            "one batched DEC of amount 3 frees a node sent 3×"
        );
        assert_eq!(st.local_node_count(), 0, "no leak on batched drop");
    }

    /// A send failure rolls back exactly one `on_binder_leaving` bump; the
    /// node survives while other sends still reference it and is removed only
    /// when the last bump is cancelled (no leak, no double-free).
    #[test]
    fn cancel_binder_leaving_rolls_back_one_bump() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap(); // strong 1
        let a2 = st.on_binder_leaving(&b).unwrap(); // strong 2 (resend)
        assert_eq!(a, a2);
        assert_eq!(st.local_node_count(), 1);

        st.cancel_binder_leaving(&a);
        assert_eq!(
            st.local_node_count(),
            1,
            "still referenced by the other send"
        );
        assert!(st.lookup_local(&a).is_some());

        st.cancel_binder_leaving(&a);
        assert_eq!(st.local_node_count(), 0, "last bump cancelled → node freed");
        assert!(st.lookup_local(&a).is_none());

        // Cancel on an unknown / already-freed address is safe.
        st.cancel_binder_leaving(&a);
    }

    /// A oneway send failure rolls back its reserved `async_number` only when
    /// it was the last reservation; if another send already advanced past it,
    /// rolling back would hand the same number out twice, so it must not.
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

    /// The android-13+ wire encodes only the low 32 bits of the address
    /// counter, so once it reaches `u32::MAX` two distinct live nodes would
    /// alias to one `RpcWireAddress`. `on_binder_leaving` must refuse to
    /// mint a *new* address past that bound (a hard error) rather than
    /// silently aliasing — while still serving a resend of an already
    /// registered object (no new mint).
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

    /// Two `RpcState` instances have **independent**
    /// tables and counters. Addresses are only ever resolved
    /// within their own session/connection, so the per-session counter
    /// scheme (both sessions start at 1) is correct — a fresh session
    /// simply does not know any address it never registered, and
    /// mutating one session never touches another.
    #[test]
    fn two_states_are_isolated() {
        let mut s1 = RpcState::new(AddressSpace::Acceptor);
        let s2 = RpcState::new(AddressSpace::Acceptor); // fresh, empty, independent table
        let b1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a1 = s1.on_binder_leaving(&b1).unwrap();

        // s2 registered nothing → it does not resolve s1's address,
        // even though a per-session counter could mint the same bytes.
        assert!(
            s2.lookup_local(&a1).is_none(),
            "independent tables: a fresh session knows no foreign address"
        );
        assert_eq!(s1.local_node_count(), 1);
        assert_eq!(s2.local_node_count(), 0);

        // Mutating s1 never affects s2 (no shared storage).
        s1.dec_strong_local(&a1, 1);
        assert_eq!(s1.local_node_count(), 0);
        assert_eq!(s2.local_node_count(), 0);
    }

    /// Regression: a stale `RpcProxy::drop` (its `Arc` hit
    /// 0 before `Drop` ran, and a concurrent `read_binder` already
    /// re-cached a fresh live proxy for the same address) must NOT
    /// evict the successor. Deterministically reproduces the exact
    /// drop / re-cache interleave at the `RpcState` level without
    /// thread timing, then asserts identity-checked `forget_remote_if`
    /// keeps "exactly one live proxy per address".
    #[test]
    fn stale_drop_does_not_split_remote_dedup() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([7u8; 32]); // RPC_ADDR_LEN

        // P1 resolved for `addr`, then its last strong ref goes away
        // (cached `Weak` now dead) — but P1's `Drop` has not yet run.
        let sib1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let p1 = Arc::as_ptr(sib1.as_arc()) as *const ();
        let (got1, ex1) = st.remote_proxy(addr, || sib1.clone());
        assert!(!ex1, "first receipt mints a proxy — not an excess");
        drop(got1);
        drop(sib1);

        // Concurrent re-resolve: another `read_binder` for the SAME
        // address sees the dead `Weak` and mints + re-caches P2.
        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got2, ex2) = st.remote_proxy(addr, || sib2.clone());
        assert!(!ex2, "dead-Weak ⇒ re-mint, not an excess receipt");
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();

        // P1's delayed `Drop` now runs `forget_remote_if(addr, P1)`.
        // The old unconditional remove would evict the live P2 slot.
        st.forget_remote_if(&addr, p1);
        let (again, ex_again) = st.remote_proxy(addr, || panic!("must dedup to P2, not re-make"));
        assert!(
            Arc::ptr_eq(again.as_arc(), got2.as_arc()),
            "stale P1 Drop must not split the per-address dedup (AC-2.5/P5)"
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
        let (_p3, ex3) = st.remote_proxy(addr, || {
            remade = true;
            sib3.clone()
        });
        assert!(
            remade,
            "after identity-checked forget, the address re-mints"
        );
        assert!(!ex3, "re-mint after forget is a fresh proxy — not excess");
    }

    /// `forget_remote_if` also drops the per-address send counter so
    /// `remote_send_async_counters` stays bounded by the live address
    /// set. A fresh re-mint starts the counter back at 0 (matches
    /// peer's `BinderNode` GC + recreate). The stale-Drop guard from
    /// `stale_drop_does_not_split_remote_dedup` extends here: an
    /// identity-mismatched `forget` must NOT evict the live counter.
    #[test]
    fn phase_c_forget_remote_if_gcs_send_counter() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let addr = RpcAddress::from_wire_bytes([3u8; 32]);

        let sib1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got1, _) = st.remote_proxy(addr, || sib1.clone());
        let p1 = Arc::as_ptr(got1.as_arc()) as *const ();
        assert_eq!(st.next_send_async_number(addr), 0);
        assert_eq!(st.next_send_async_number(addr), 1);

        // Stale-Drop pattern (P2 already re-cached): `forget_remote_if`
        // is a no-op on identity mismatch, so the counter survives.
        drop(got1);
        drop(sib1);
        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (got2, _) = st.remote_proxy(addr, || sib2.clone());
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();
        st.forget_remote_if(&addr, p1);
        assert_eq!(
            st.next_send_async_number(addr),
            2,
            "stale forget must not evict the live counter"
        );

        // Genuine `forget`: counter drops + next read auto-creates
        // (back to 0).
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

    /// `timesSent` balance (state level): the AOSP
    /// `timesSent`/`flushExcessBinderRefs` accounting nets to exactly
    /// one `DEC_STRONG` per send, so the node is freed (no leak) in
    /// both shapes the model must support.
    #[test]
    fn f7_timessent_balance_no_leak() {
        // (a) Same object sent N× to *one* peer that dedups to one
        //     proxy: server strong = N (timesSent); peer owes N−1
        //     excess DECs + 1 at proxy drop = N.
        let mut srv = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = srv.on_binder_leaving(&b).unwrap();
        let a2 = srv.on_binder_leaving(&b).unwrap();
        let a3 = srv.on_binder_leaving(&b).unwrap();
        assert_eq!((a, a), (a2, a3), "identity ⇒ same address on re-send");
        assert_eq!(srv.local_node_count(), 1, "one node, strong = timesSent");
        // Peer side: 3 receipts of the same addr, proxy stays live ⇒
        // receipts 2 and 3 are excess (2 flush DECs); proxy drop = 1.
        let mut peer = RpcState::new(AddressSpace::Initiator);
        let pb = SIBinder::new(Arc::new(Dummy)).unwrap();
        let (_p, e1) = peer.remote_proxy(a, || pb.clone());
        let (_p2, e2) = peer.remote_proxy(a, || pb.clone());
        let (_p3, e3) = peer.remote_proxy(a, || pb.clone());
        assert_eq!(
            (e1, e2, e3),
            (false, true, true),
            "1st mints; 2nd/3rd are excess receipts (owe a flush DEC)"
        );
        // 2 excess DECs + 1 proxy-drop DEC = 3 = timesSent ⇒ freed.
        assert!(!srv.dec_strong_local(&a, 1));
        assert!(!srv.dec_strong_local(&a, 1));
        assert!(srv.dec_strong_local(&a, 1), "3rd DEC frees the node");
        assert_eq!(srv.local_node_count(), 0, "no leak (AC-2.5)");

        // (b) Same object sent once to each of 2 *independent* peer
        //     connections sharing one server session: 2 sends ⇒ strong
        //     2; each peer's lone proxy DECs once ⇒ 2. Pinning at 1
        //     would free the node on the *first* DEC; the sibling must
        //     survive until the 2nd.
        let mut s = RpcState::new(AddressSpace::Acceptor);
        let o = SIBinder::new(Arc::new(Dummy)).unwrap();
        let x = s.on_binder_leaving(&o).unwrap(); // conn #1 send
        let _ = s.on_binder_leaving(&o).unwrap(); // conn #2 send (timesSent ⇒ 2)
        assert!(
            !s.dec_strong_local(&x, 1),
            "conn #1 proxy drop must NOT free a node conn #2 still holds (F7)"
        );
        assert!(s.lookup_local(&x).is_some(), "sibling still reachable");
        assert!(s.dec_strong_local(&x, 1), "conn #2 proxy drop frees it");
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

    /// Send side: per-remote-address counter
    /// post-increments on every `next_send_async_number(addr)`, with
    /// addresses tracked independently. Reset is impossible by design
    /// (the peer's `BinderNode::asyncNumber` lives across our proxy
    /// churn — see the field doc).
    #[test]
    fn phase_c_send_async_number_is_per_address_monotonic() {
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

    /// Receive side, in-order: wire `async_number`
    /// matches the per-node `next_async_number` ⇒ dispatch
    /// immediately. The advance + queue drain runs in a separate call
    /// (the dispatch happens *outside* the state lock).
    #[test]
    fn phase_c_in_order_dispatches_and_advances_counter() {
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
                st.advance_and_pop_async(a).is_none(),
                "queue empty ⇒ drain returns None"
            );
            assert_eq!(st.next_async_number(&a), i + 1);
        }
    }

    /// Receive side, out-of-order: wire `async_number`
    /// ahead of expected ⇒ parked. When the matching expected arrives,
    /// dispatch advances the counter and the drain loop pops the
    /// parked entries in priority order until a gap appears. This is
    /// the AOSP `RpcState::processTransactInternal` enqueue + drain
    /// behaviour, and the exact thing that makes libbinder's
    /// round-robin `mOutgoing` oneway distribution preserve per-node
    /// order on the rsbinder server.
    #[test]
    fn phase_c_out_of_order_enqueues_then_drains_in_priority_order() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b).unwrap();

        // Wire arrival: 2, 4, 1, 3, 0 (libbinder round-robin against
        // 2 outgoing slots delivers this kind of interleave). Expected
        // dispatch order: 0, 1, 2, 3, 4 (per-node monotonic).
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
        // After dispatching 0, the queue must drain 1, 2, 3, 4 in
        // strict order via advance_and_pop_async.
        assert_eq!(st.async_todo_len(&a), 4, "1, 2, 3, 4 parked");
        let mut dispatched = vec![0u64];
        while let Some((t, _)) = st.advance_and_pop_async(a) {
            dispatched.push(t.async_number);
        }
        assert_eq!(
            dispatched,
            vec![0, 1, 2, 3, 4],
            "per-node monotonic dispatch despite wire reorder"
        );
        assert_eq!(st.async_todo_len(&a), 0, "drained");
        // After draining 4 (the last one), counter still advances
        // once for the dispatch of 4 — so expected is now 5.
        assert_eq!(st.next_async_number(&a), 5);
    }

    /// A peer that parks out-of-order oneways while withholding the
    /// expected `async_number` cannot grow a node's `async_todo` without
    /// bound: at the AOSP terminate watermark the decision flips to
    /// `Terminate` and the backlog is flushed (memory + any held fds
    /// reclaimed), bounding the node to `ASYNC_TODO_TERMINATE_LEVEL`
    /// entries at any instant.
    #[test]
    fn phase_c_async_todo_terminate_caps_and_flushes_backlog() {
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

    /// Unknown address: AOSP `RpcState` only enqueues if
    /// `mNodeForAddress.find(addr)` succeeds; rsbinder must mirror
    /// that or unknown-address oneway would leak into the queue
    /// forever. Returns [`AsyncDecision::Drop`] so the caller logs +
    /// drops (oneway is best-effort).
    #[test]
    fn phase_c_unknown_address_drops_not_enqueues() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let unknown = RpcAddress::from_wire_bytes([9u8; 32]);
        let txn = mk_txn(unknown, 0);
        assert!(matches!(
            st.dispatch_async_or_enqueue(unknown, 0, txn, vec![]),
            AsyncDecision::Drop(DropReason::UnknownAddress)
        ));
        // `advance_and_pop_async` on an unknown address is a no-op
        // (would otherwise underflow / spuriously advance a future
        // node minted at the same address — but addresses are
        // monotonic so the latter cannot happen).
        assert!(st.advance_and_pop_async(unknown).is_none());
    }

    /// Stale receive: a `wire_async < next_async_number`
    /// arrival (peer replay / buggy retry) returns
    /// [`DropReason::StaleAsyncNumber`] without touching the queue;
    /// any heap entry already below the expected number is drained on
    /// the next `advance_and_pop_async` (so a hostile peer cannot OOM
    /// us by spamming stale futures).
    #[test]
    fn phase_c_stale_arrival_drops_and_heap_drains_below_expected() {
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

        // Now a stale arrival (1 < 3) reports the reason and does not
        // enqueue.
        let txn = mk_txn(a, 1);
        assert!(matches!(
            st.dispatch_async_or_enqueue(a, 1, txn, vec![]),
            AsyncDecision::Drop(DropReason::StaleAsyncNumber)
        ));
        assert_eq!(st.async_todo_len(&a), 0);

        // Heap stale-drain: inject a future arrival, advance past it,
        // then verify the next pop sees the heap empty (the stale
        // entry was reaped, not blocking).
        let txn5 = mk_txn(a, 5);
        let _ = st.dispatch_async_or_enqueue(a, 5, txn5, vec![]);
        assert_eq!(st.async_todo_len(&a), 1);
        // Dispatch a matching 3 + 4 to advance past 5's predecessor.
        let _ = st.dispatch_async_or_enqueue(a, 3, mk_txn(a, 3), vec![]);
        let _ = st.advance_and_pop_async(a); // expected → 4
        let _ = st.dispatch_async_or_enqueue(a, 4, mk_txn(a, 4), vec![]);
        let _ = st.advance_and_pop_async(a); // expected → 5; pops the parked 5.
        assert_eq!(st.async_todo_len(&a), 0);
        assert_eq!(st.next_async_number(&a), 5);
    }

    /// Multi-node independence: each `LocalNode` has its
    /// own `next_async_number` + `async_todo`, so a stalled queue on
    /// node A must not block dispatch on node B.
    #[test]
    fn phase_c_per_node_independence() {
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
        // Node a2: independent counter at 0 ⇒ arrival 0 dispatches
        // even though a1 is blocked.
        let txn = mk_txn(a2, 0);
        match st.dispatch_async_or_enqueue(a2, 0, txn, vec![]) {
            AsyncDecision::Dispatch(t, _) => assert_eq!(t.async_number, 0),
            _ => panic!("a2 must dispatch independently of a1's stalled queue"),
        }
        assert_eq!(st.async_todo_len(&a2), 0);
        // Counters are truly independent.
        assert_eq!(st.next_async_number(&a1), 0, "a1 not yet advanced");
        assert!(st.advance_and_pop_async(a2).is_none());
        assert_eq!(st.next_async_number(&a2), 1);
    }
}
