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
//! Ref-count model (single-session loopback, 2-2 scope): a local object
//! gets one address by *identity* (`Arc` pointer dedup); the entry's
//! strong count starts at 1 and is decremented by a `DEC_STRONG`
//! command — when it reaches 0 the entry (and its strong `SIBinder`) is
//! dropped, so no leak (AC-2.5). The proxy side dedups one `RpcProxy`
//! per address and sends exactly one `DEC_STRONG` when that proxy's
//! last `Arc` drops.

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
    /// session-stable address (android `onBinderLeaving`). Idempotent
    /// by object identity: sending the same object twice reuses the
    /// address and does not double the strong count (object-identity
    /// semantics — sufficient and correct for single-session loopback,
    /// AC-2.5).
    pub fn on_binder_leaving(&mut self, binder: &SIBinder) -> RpcAddress {
        let ptr = binder_ptr(binder);
        if let Some(addr) = self.local_by_ptr.get(&ptr) {
            return *addr;
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
    pub fn remote_proxy<F>(&mut self, addr: RpcAddress, make: F) -> SIBinder
    where
        F: FnOnce() -> SIBinder,
    {
        if let Some(weak) = self.remote_proxies.get(&addr) {
            if let Some(arc) = weak.upgrade() {
                return SIBinder::from_arc(arc);
            }
        }
        let sib = make();
        self.remote_proxies
            .insert(addr, Arc::downgrade(sib.as_arc()));
        sib
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
        let got1 = st.remote_proxy(addr, || sib1.clone());
        drop(got1);
        drop(sib1);

        // Concurrent re-resolve: another `read_binder` for the SAME
        // address sees the dead `Weak` and mints + re-caches P2.
        let sib2 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let got2 = st.remote_proxy(addr, || sib2.clone());
        let p2 = Arc::as_ptr(got2.as_arc()) as *const ();

        // P1's delayed `Drop` now runs `forget_remote_if(addr, P1)`.
        // The old unconditional remove would evict the live P2 slot.
        st.forget_remote_if(&addr, p1);
        let again = st.remote_proxy(addr, || panic!("must dedup to P2, not re-make"));
        assert!(
            Arc::ptr_eq(again.as_arc(), got2.as_arc()),
            "stale P1 Drop must not split the per-address dedup (AC-2.5/P5)"
        );

        // The genuinely-current proxy's Drop *does* evict.
        drop(again);
        drop(got2);
        drop(sib2);
        st.forget_remote_if(&addr, p2);
        let sib3 = SIBinder::new(Arc::new(Dummy)).unwrap();
        let mut remade = false;
        let _p3 = st.remote_proxy(addr, || {
            remade = true;
            sib3.clone()
        });
        assert!(
            remade,
            "after identity-checked forget, the address re-mints"
        );
    }
}
