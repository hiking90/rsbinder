// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `RpcState` — per-session object table + RPC ref-count
//! (subplan 2-2 S-c).
//!
//! The rsbinder equivalent of android `RpcState::mNodeForAddress`. Per
//! **P6** this is **strictly per-session** — there is no `static`,
//! `OnceLock` or `lazy_static` anywhere in the RPC stack, so two
//! sessions never share an address space and the RPC test suite is
//! parallel-safe by construction (unlike the kernel binder singleton).
//!
//! Ref-count model (AOSP `RpcState` `BinderNode::timesSent` /
//! `flushExcessBinderRefs` — subplan 2-12 Phase A **F7**): a local
//! object gets one address by *identity* (`Arc` pointer dedup, so the
//! same object always marshals to the same address), but the entry's
//! strong count is **`timesSent`**: it starts at 1 on the first send
//! and is **incremented on every subsequent send** (each flatten to
//! the peer is one reference the peer will eventually `DEC_STRONG`).
//! Each inbound `DEC_STRONG` decrements it; the entry (and its strong
//! `SIBinder`) is dropped at 0, so there is no leak (AC-2.5).
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
//! N independent peer connections sharing a session (Phase A0b: N
//! sends, N drop DECs). Before F7 the count was pinned at 1 by
//! identity, which silently broke the latter (the first connection's
//! proxy drop freed the node ⇒ the sibling connection's proxy
//! `DeadObject`).

use std::collections::HashMap;
use std::sync::{self, Arc};

use crate::binder::{IBinder, SIBinder};

use super::address::{AddressSpace, RpcAddress};

/// A local object exposed to the peer under [`RpcAddress`].
struct LocalNode {
    /// Strong ref keeps the local object alive while the peer holds it.
    binder: SIBinder,
    /// RPC strong count the peer holds (0 ⇒ drop the node).
    strong: i64,
}

/// Per-session object/address table. Owned by `RpcSessionInner` behind
/// a `Mutex`; never global (P6 — enforced by the `rpc_no_globals`
/// grep gate).
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
    /// Monotonic address allocator (per-session — P6).
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
            addr_counter: 0,
            space,
        }
    }

    /// Register a local object leaving this process and return its
    /// session-stable address (android `onBinderLeaving`). The address
    /// is idempotent by object identity (same object ⇒ same address),
    /// but the strong count is AOSP `timesSent`: **+1 on every send**
    /// (Phase A F7). The first send creates the node at `strong = 1`; a
    /// re-send of the same object reuses the address and **increments**
    /// `strong` (the peer will `DEC_STRONG` once per receipt — directly
    /// at proxy drop, or as an `flushExcessBinderRefs` excess DEC if it
    /// dedups; see the module doc). Pre-F7 this branch returned without
    /// bumping, which under Phase A0b multi-connection let the first
    /// connection's DEC free a node still referenced over a sibling
    /// connection (`DeadObject`).
    pub fn on_binder_leaving(&mut self, binder: &SIBinder) -> RpcAddress {
        let ptr = binder_ptr(binder);
        if let Some(&addr) = self.local_by_ptr.get(&ptr) {
            if let Some(node) = self.local_nodes.get_mut(&addr) {
                node.strong += 1;
            }
            return addr;
        }
        let addr = RpcAddress::unique(&mut self.addr_counter, self.space);
        self.local_nodes.insert(
            addr,
            LocalNode {
                binder: binder.clone(),
                strong: 1,
            },
        );
        self.local_by_ptr.insert(ptr, addr);
        addr
    }

    /// The local object registered at `addr`, if any (an address that
    /// is one of *our* nodes means the object is returning home, not a
    /// remote — android `onBinderEntering` local branch).
    pub fn lookup_local(&self, addr: &RpcAddress) -> Option<SIBinder> {
        self.local_nodes.get(addr).map(|n| n.binder.clone())
    }

    /// Apply an inbound `DEC_STRONG` for `addr`. Drops the node (and
    /// its strong `SIBinder`) once the count reaches 0 — no leak.
    /// Returns `true` if the node was removed.
    pub fn dec_strong_local(&mut self, addr: &RpcAddress) -> bool {
        if let Some(node) = self.local_nodes.get_mut(addr) {
            node.strong -= 1;
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
    /// `DEC_STRONG`" invariant (AC-2.5 / P5). The identity check makes
    /// a stale `Drop` a no-op against a re-cached successor.
    pub fn forget_remote_if(&mut self, addr: &RpcAddress, who: *const ()) {
        if let Some(weak) = self.remote_proxies.get(addr) {
            if weak.as_ptr() as *const () == who {
                self.remote_proxies.remove(addr);
            }
        }
    }

    /// Test/diagnostic: number of live local nodes (AC-2.5 leak check).
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
        let a1 = st.on_binder_leaving(&b);
        let a2 = st.on_binder_leaving(&b);
        assert_eq!(a1, a2, "same object → same address");
        assert_eq!(st.local_node_count(), 1);
        assert!(st.lookup_local(&a1).is_some());
    }

    /// AC-2.5: a single DEC_STRONG drops the node to 0 → removed, no
    /// leak.
    #[test]
    fn dec_strong_releases_node() {
        let mut st = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = st.on_binder_leaving(&b);
        assert_eq!(st.local_node_count(), 1);
        assert!(st.dec_strong_local(&a), "node removed at strong 0");
        assert_eq!(st.local_node_count(), 0, "no leak");
        assert!(st.lookup_local(&a).is_none());
        // DEC_STRONG on an unknown address is safe (idempotent).
        assert!(!st.dec_strong_local(&a));
    }

    /// AC-2.3 / T2.3: two `RpcState` instances have **independent**
    /// tables and counters (P6). Addresses are only ever resolved
    /// within their own session/connection, so the per-session counter
    /// scheme (both sessions start at 1) is correct — a fresh session
    /// simply does not know any address it never registered, and
    /// mutating one session never touches another.
    #[test]
    fn two_states_are_isolated() {
        let mut s1 = RpcState::new(AddressSpace::Acceptor);
        let s2 = RpcState::new(AddressSpace::Acceptor); // fresh, empty, independent table
        let b1 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a1 = s1.on_binder_leaving(&b1);

        // s2 registered nothing → it does not resolve s1's address,
        // even though a per-session counter could mint the same bytes.
        assert!(
            s2.lookup_local(&a1).is_none(),
            "independent tables: a fresh session knows no foreign address"
        );
        assert_eq!(s1.local_node_count(), 1);
        assert_eq!(s2.local_node_count(), 0);

        // Mutating s1 never affects s2 (no shared storage — P6).
        s1.dec_strong_local(&a1);
        assert_eq!(s1.local_node_count(), 0);
        assert_eq!(s2.local_node_count(), 0);
    }

    /// AC-2.5 / P5 regression: a stale `RpcProxy::drop` (its `Arc` hit
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

    /// **Phase A F7** balance (state level): the AOSP
    /// `timesSent`/`flushExcessBinderRefs` accounting nets to exactly
    /// one `DEC_STRONG` per send, so the node is freed (no leak —
    /// AC-2.5) in both shapes the model must support.
    #[test]
    fn f7_timessent_balance_no_leak() {
        // (a) Same object sent N× to *one* peer that dedups to one
        //     proxy: server strong = N (timesSent); peer owes N−1
        //     excess DECs + 1 at proxy drop = N.
        let mut srv = RpcState::new(AddressSpace::Acceptor);
        let b = SIBinder::new(Arc::new(Dummy)).unwrap();
        let a = srv.on_binder_leaving(&b);
        let a2 = srv.on_binder_leaving(&b);
        let a3 = srv.on_binder_leaving(&b);
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
        assert!(!srv.dec_strong_local(&a));
        assert!(!srv.dec_strong_local(&a));
        assert!(srv.dec_strong_local(&a), "3rd DEC frees the node");
        assert_eq!(srv.local_node_count(), 0, "no leak (AC-2.5)");

        // (b) Same object sent once to each of 2 *independent* peer
        //     connections sharing one server session (Phase A0b): 2
        //     sends ⇒ strong 2; each peer's lone proxy DECs once ⇒ 2.
        //     Pre-F7 this freed the node on the *first* DEC (the F7
        //     bug); now the sibling survives until the 2nd.
        let mut s = RpcState::new(AddressSpace::Acceptor);
        let o = SIBinder::new(Arc::new(Dummy)).unwrap();
        let x = s.on_binder_leaving(&o); // conn #1 send
        let _ = s.on_binder_leaving(&o); // conn #2 send (timesSent ⇒ 2)
        assert!(
            !s.dec_strong_local(&x),
            "conn #1 proxy drop must NOT free a node conn #2 still holds (F7)"
        );
        assert!(s.lookup_local(&x).is_some(), "sibling still reachable");
        assert!(s.dec_strong_local(&x), "conn #2 proxy drop frees it");
        assert_eq!(s.local_node_count(), 0, "no leak");
    }
}
