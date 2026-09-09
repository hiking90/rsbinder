// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-22 Phase B — the **gateway**: a process that re-publishes a
//! service it reached over one transport onto another.
//!
//! The whole point is that it costs one line. The generator emits
//! `impl IFoo for Strong<dyn IFoo>` (delegating through `Deref`) and
//! rsbinder has a blanket `Interface for Strong<I>`, so a proxy
//! satisfies `Bn*::new_binder`'s `T: IFoo + Send + Sync + 'static`
//! bound and becomes a *local* binder this process owns:
//!
//! ```ignore
//! serve(b).add("mesh", BnMeshNode::new_binder(proxy_of_c))
//! ```
//!
//! What it does **not** do is hide the stack boundary: a binder-typed
//! argument forwarded as-is is still refused (`InvalidOperation`), and
//! re-wrapping it is the gateway's job. Both halves are pinned here.
//!
//! Hermetic, in-process (three sessions over `unix://`), no kernel
//! binder — runs on macOS. Separate `#![cfg(feature = "rpc")]` binary.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rsbinder::bridge::Rewrap;
use rsbinder::{Interface, StatusCode, Strong};

include!(concat!(env!("OUT_DIR"), "/mesh.rs"));

use mesh::IMeshNode::{BnMeshNode, IMeshNode};
use mesh::IMeshObserver::{BnMeshObserver, IMeshObserver};
use mesh::MeshMessage::MeshMessage;
use mesh::MeshValue::MeshValue;
use mesh::NodeKind::NodeKind;

// ---- the upstream service (node C) ----------------------------------

#[derive(Default)]
struct MeshNodeImpl {
    name: String,
    received: AtomicI32,
    total: AtomicI64,
    observers: Mutex<Vec<Strong<dyn IMeshObserver>>>,
}

impl Interface for MeshNodeImpl {}

impl IMeshNode for MeshNodeImpl {
    fn r#exchange(&self, req: &MeshMessage) -> rsbinder::BinderResult<MeshMessage> {
        self.received.fetch_add(1, Ordering::Relaxed);
        Ok(MeshMessage {
            r#seq: req.r#seq + 1,
            r#nonce: req.r#nonce,
            r#origin: self.name.clone(),
            r#originKind: NodeKind::RPC,
            r#blob: req.r#blob.clone(),
        })
    }

    fn r#accumulate(&self, v: &MeshValue) -> rsbinder::BinderResult<i64> {
        let delta = match v {
            MeshValue::I(i) => *i as i64,
            MeshValue::L(l) => *l,
            MeshValue::S(s) => s.len() as i64,
        };
        Ok(self.total.fetch_add(delta, Ordering::Relaxed) + delta)
    }

    fn r#notify(&self, msg: &MeshMessage) -> rsbinder::BinderResult<()> {
        self.received.fetch_add(1, Ordering::Relaxed);
        let obs = self.observers.lock().expect("observers").clone();
        for o in obs {
            let _ = o.r#onEvent(msg);
        }
        Ok(())
    }

    fn r#registerObserver(&self, obs: &Strong<dyn IMeshObserver>) -> rsbinder::BinderResult<()> {
        self.observers.lock().expect("observers").push(obs.clone());
        Ok(())
    }

    fn r#receivedCount(&self) -> rsbinder::BinderResult<i32> {
        Ok(self.received.load(Ordering::Relaxed))
    }
}

fn upstream(name: &str) -> Strong<dyn IMeshNode> {
    BnMeshNode::new_binder(MeshNodeImpl {
        name: name.to_string(),
        ..Default::default()
    })
}

// ---- an observer A can register -------------------------------------

struct CountingObserver(Arc<AtomicI32>);
impl Interface for CountingObserver {}
impl IMeshObserver for CountingObserver {
    fn r#onEvent(&self, _msg: &MeshMessage) -> rsbinder::BinderResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---- scaffolding ----------------------------------------------------

struct SockPath(PathBuf);
impl SockPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rsb_gw_{}_{}_{}.sock",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SockPath(p)
    }
    fn uri(&self, frag: &str) -> String {
        format!("unix://{}{frag}", self.0.display())
    }
}
impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn msg(seq: i32, origin: &str) -> MeshMessage {
    MeshMessage {
        seq,
        r#nonce: 0xfeed_face,
        r#origin: origin.to_string(),
        r#originKind: NodeKind::RPC,
        r#blob: vec![1, 2, 3, 4],
    }
}

// ---- AC-22.7: the one-line gateway ----------------------------------

/// A → B → C, where B publishes nothing of its own: it hands the
/// generator's delegating `impl` straight to `new_binder`. Values must
/// round-trip through both hops unchanged.
#[test]
fn gateway_forwards_calls_with_no_handwritten_delegate() {
    let c_sock = SockPath::new("c");
    let b_sock = SockPath::new("b");

    // C — the real service.
    let _c = rsbinder::serve(&c_sock.uri(""))
        .expect("serve C")
        .add("mesh", upstream("node-c"))
        .expect("add C")
        .spawn()
        .expect("spawn C");

    // B — the gateway. One line: connect, then re-publish.
    let c_proxy: Strong<dyn IMeshNode> = rsbinder::connect(&c_sock.uri("#mesh")).expect("B→C");
    let _b = rsbinder::serve(&b_sock.uri(""))
        .expect("serve B")
        .add("mesh", BnMeshNode::new_binder(c_proxy))
        .expect("AC-22.7: a proxy wrapped in a Bn* is publishable")
        .spawn()
        .expect("spawn B");

    // A — talks only to B, but the answers come from C.
    let a: Strong<dyn IMeshNode> = rsbinder::connect(&b_sock.uri("#mesh")).expect("A→B");

    let reply = a.r#exchange(&msg(7, "node-a")).expect("exchange");
    assert_eq!(reply.r#seq, 8, "seq must come back incremented by C");
    assert_eq!(reply.r#origin, "node-c", "the answer is C's, not B's");
    assert_eq!(reply.r#nonce, 0xfeed_face, "nonce echoed through both hops");
    assert_eq!(reply.r#blob, vec![1, 2, 3, 4], "byte array survives 2 hops");

    assert_eq!(a.r#accumulate(&MeshValue::I(5)).unwrap(), 5);
    assert_eq!(a.r#accumulate(&MeshValue::L(10)).unwrap(), 15);
    assert_eq!(
        a.r#accumulate(&MeshValue::S("abc".into())).unwrap(),
        18,
        "the accumulator lives in C — B holds no state"
    );

    assert_eq!(
        a.r#receivedCount().unwrap(),
        1,
        "only `exchange` counts as received"
    );
}

// ---- AC-22.8: binder arguments are NOT forwarded for free -----------

/// The gateway's limit, pinned. A binder argument travelling A→B→C is a
/// proxy of session A being written into a parcel of session C, which
/// the boundary refuses — the caller sees the failure, and the plain
/// delegate cannot fix it.
#[test]
fn gateway_refuses_a_forwarded_callback() {
    let c_sock = SockPath::new("cbc");
    let b_sock = SockPath::new("cbb");

    let _c = rsbinder::serve(&c_sock.uri(""))
        .expect("serve C")
        .add("mesh", upstream("node-c"))
        .expect("add C")
        .spawn()
        .expect("spawn C");

    let c_proxy: Strong<dyn IMeshNode> = rsbinder::connect(&c_sock.uri("#mesh")).expect("B→C");
    let _b = rsbinder::serve(&b_sock.uri(""))
        .expect("serve B")
        .add("mesh", BnMeshNode::new_binder(c_proxy))
        .expect("add B")
        .spawn()
        .expect("spawn B");

    let a: Strong<dyn IMeshNode> = rsbinder::connect(&b_sock.uri("#mesh")).expect("A→B");
    let hits = Arc::new(AtomicI32::new(0));
    let obs = BnMeshObserver::new_binder(CountingObserver(hits.clone()));

    let err = a
        .r#registerObserver(&obs)
        .expect_err("AC-22.8: forwarding a callback binder across sessions must fail");
    assert_eq!(
        rsbinder::StatusCode::from(err),
        StatusCode::InvalidOperation,
        "AC-22.8: the failure is the stack-boundary refusal, reported to A"
    );
}

/// The remedy, and the reason Phase D exists: B re-wraps the callback in
/// a `Bn*` of its own before passing it on. Then C's `notify` reaches
/// A's observer — through two hops and two re-wraps.
#[test]
fn gateway_rewrapping_a_callback_reaches_the_original_observer() {
    let c_sock = SockPath::new("rwc");
    let b_sock = SockPath::new("rwb");

    // A server→client callback needs a connection the server may send on
    // outside the reply, which is what `incoming_connections` opens (plan
    // 2-20); it rides the versioned wire, hence `?profile=android13plus`
    // on both ends of both hops.
    const A13: &str = "?profile=android13plus";
    let callback_ready = |o: &mut rsbinder::ClientOptions, _: &rsbinder::Endpoint| {
        o.incoming_connections = Some(1);
    };

    let _c = rsbinder::serve(&format!("{}{A13}", c_sock.uri("")))
        .expect("serve C")
        .add("mesh", upstream("node-c"))
        .expect("add C")
        .spawn()
        .expect("spawn C");

    // B's delegate overrides exactly one method: the one with a binder
    // argument. Everything else rides the generated delegation. The
    // re-wrap goes through `Rewrap`, so the same observer always yields
    // the same local object (see `rewrap_*` below).
    struct RewrappingGateway {
        upstream: Strong<dyn IMeshNode>,
        observers: Rewrap<dyn IMeshObserver>,
    }
    impl Interface for RewrappingGateway {}
    impl IMeshNode for RewrappingGateway {
        fn r#exchange(&self, req: &MeshMessage) -> rsbinder::BinderResult<MeshMessage> {
            self.upstream.r#exchange(req)
        }
        fn r#accumulate(&self, v: &MeshValue) -> rsbinder::BinderResult<i64> {
            self.upstream.r#accumulate(v)
        }
        fn r#notify(&self, msg: &MeshMessage) -> rsbinder::BinderResult<()> {
            self.upstream.r#notify(msg)
        }
        fn r#registerObserver(
            &self,
            obs: &Strong<dyn IMeshObserver>,
        ) -> rsbinder::BinderResult<()> {
            // The re-wrap: a *local* binder of B's, delegating to A's
            // observer. `Rewrap` mints it once and hands the same object
            // back on every later call for the same observer.
            self.upstream.r#registerObserver(&self.observers.wrap(obs))
        }
        fn r#receivedCount(&self) -> rsbinder::BinderResult<i32> {
            self.upstream.r#receivedCount()
        }
    }

    let b_to_c = rsbinder::Client::open_with(&format!("{}{A13}", c_sock.uri("")), callback_ready)
        .expect("B→C");
    let c_proxy: Strong<dyn IMeshNode> = b_to_c.get("mesh").expect("B→C mesh");
    let _b = rsbinder::serve(&format!("{}{A13}", b_sock.uri("")))
        .expect("serve B")
        .add(
            "mesh",
            BnMeshNode::new_binder(RewrappingGateway {
                upstream: c_proxy,
                observers: Rewrap::new(BnMeshObserver::new_binder),
            }),
        )
        .expect("add B")
        .spawn()
        .expect("spawn B");

    let a_to_b = rsbinder::Client::open_with(&format!("{}{A13}", b_sock.uri("")), callback_ready)
        .expect("A→B");
    let a: Strong<dyn IMeshNode> = a_to_b.get("mesh").expect("A→B mesh");
    let hits = Arc::new(AtomicI32::new(0));
    let obs = BnMeshObserver::new_binder(CountingObserver(hits.clone()));
    a.r#registerObserver(&obs)
        .expect("re-wrapped observer lands");

    a.r#notify(&msg(1, "node-a")).expect("oneway notify");

    // `notify` is oneway on both hops, so wait for the callback rather
    // than assuming it has already run.
    for _ in 0..400 {
        if hits.load(Ordering::SeqCst) > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "AC-22.8: a re-wrapped callback reaches the original observer"
    );
}

// ---- AC-22.13: the gateway reports the upstream's version -----------

mod trunk_v1_gen {
    include!(concat!(env!("OUT_DIR"), "/trunk_v1.rs"));
}
mod trunk_v2_gen {
    include!(concat!(env!("OUT_DIR"), "/trunk_v2.rs"));
}

use trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::{
    BnTrunkStableTest as BnTrunkV1, ITrunkStableTest as ITrunkV1,
    MyParcelable::MyParcelable as V1Parcelable,
};
use trunk_v2_gen::android::aidl::test::trunk::ITrunkStableTest::{
    BnTrunkStableTest as BnTrunkV2, ITrunkStableTest as ITrunkV2,
};

struct TrunkV1Svc;
impl Interface for TrunkV1Svc {}
impl ITrunkV1 for TrunkV1Svc {
    fn r#repeatParcelable(&self, input: &V1Parcelable) -> rsbinder::BinderResult<V1Parcelable> {
        Ok(input.clone())
    }
    fn r#repeatEnum(
        &self,
        input: trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::MyEnum::MyEnum,
    ) -> rsbinder::BinderResult<
        trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::MyEnum::MyEnum,
    > {
        Ok(input)
    }
    fn r#repeatUnion(
        &self,
        input: &trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::MyUnion::MyUnion,
    ) -> rsbinder::BinderResult<
        trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::MyUnion::MyUnion,
    > {
        Ok(input.clone())
    }
    fn r#callMyCallback(
        &self,
        _cb: &Strong<
            dyn trunk_v1_gen::android::aidl::test::trunk::ITrunkStableTest::IMyCallback::IMyCallback,
        >,
    ) -> rsbinder::BinderResult<()> {
        Ok(())
    }
}

/// A gateway speaks for the service it fronts, including its *version*.
///
/// The trait's `getInterfaceVersion` has a default body returning the
/// **generating module's** `VERSION`, so a delegate that forgets to
/// override it still compiles and quietly answers with its own number.
/// Here C is built from the frozen V1 AIDL (`VERSION` 1, hash
/// `88311b…`) and B from V2 (`VERSION` 2, hash `notfrozen`) — same
/// descriptor, so the proxy casts across. A must see **C's** values;
/// dropping the override in the generated delegating impl turns this
/// red with 2 / "notfrozen".
#[test]
fn gateway_reports_the_upstream_version_and_hash() {
    let c_sock = SockPath::new("verc");
    let b_sock = SockPath::new("verb");

    let _c = rsbinder::serve(&c_sock.uri(""))
        .expect("serve C")
        .add("trunk", BnTrunkV1::new_binder(TrunkV1Svc))
        .expect("add C")
        .spawn()
        .expect("spawn C");

    // B sees C through the *V2* stub and re-publishes it with the V2 `Bn`.
    let c_proxy: Strong<dyn ITrunkV2> = rsbinder::connect(&c_sock.uri("#trunk")).expect("B→C");
    let _b = rsbinder::serve(&b_sock.uri(""))
        .expect("serve B")
        .add("trunk", BnTrunkV2::new_binder(c_proxy))
        .expect("add B")
        .spawn()
        .expect("spawn B");

    let a: Strong<dyn ITrunkV2> = rsbinder::connect(&b_sock.uri("#trunk")).expect("A→B");
    assert_eq!(
        a.r#getInterfaceVersion().expect("version"),
        1,
        "AC-22.13: the gateway reports C's version, not its own module's"
    );
    assert_eq!(
        a.r#getInterfaceHash().expect("hash"),
        "88311b9118fb6fe9eff4a2ca19121de0587f6d5f",
        "AC-22.13: and C's hash"
    );
}

// ---- AC-22.11: Rewrap keeps one local object per remote -------------

/// A `Rewrap` server: it hands every observer it is given to `Rewrap` and
/// records what came back, so a test can ask whether two registrations of
/// the same remote produced the same local object.
struct Recorder {
    rewrap: Rewrap<dyn IMeshObserver>,
    seen: Mutex<Vec<Strong<dyn IMeshObserver>>>,
}
impl Interface for Recorder {}
impl IMeshNode for Recorder {
    fn r#exchange(&self, req: &MeshMessage) -> rsbinder::BinderResult<MeshMessage> {
        Ok(req.clone())
    }
    fn r#accumulate(&self, _v: &MeshValue) -> rsbinder::BinderResult<i64> {
        Ok(0)
    }
    fn r#notify(&self, _msg: &MeshMessage) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#registerObserver(&self, obs: &Strong<dyn IMeshObserver>) -> rsbinder::BinderResult<()> {
        self.seen.lock().expect("seen").push(self.rewrap.wrap(obs));
        Ok(())
    }
    /// Reports how many **distinct** local objects `Rewrap` handed out —
    /// the number the test cares about. Without the table this equals the
    /// number of `registerObserver` calls.
    fn r#receivedCount(&self) -> rsbinder::BinderResult<i32> {
        let seen = self.seen.lock().expect("seen");
        let mut distinct: Vec<rsbinder::SIBinder> = Vec::new();
        for s in seen.iter() {
            let b = s.as_binder();
            if !distinct.contains(&b) {
                distinct.push(b);
            }
        }
        Ok(distinct.len() as i32)
    }
}

/// AC-22.11. The same remote wrapped twice is the same local binder; a
/// different remote is a different one. This is what makes an upstream's
/// `register(cb)` / `unregister(cb)` pair up — without it the second call
/// names an object the upstream has never seen.
#[test]
fn rewrap_returns_one_local_object_per_remote() {
    let sock = SockPath::new("rewrap");
    let rewrap = Rewrap::new(BnMeshObserver::new_binder);
    // The table is inside the service, so probe it through the calls.
    let svc = Recorder {
        rewrap,
        seen: Mutex::new(Vec::new()),
    };
    let _s = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add("mesh", BnMeshNode::new_binder(svc))
        .expect("add")
        .spawn()
        .expect("spawn");

    let client = rsbinder::Client::open(&sock.uri("")).expect("open");
    let node: Strong<dyn IMeshNode> = client.get("mesh").expect("mesh");

    let one = BnMeshObserver::new_binder(CountingObserver(Arc::new(AtomicI32::new(0))));
    let two = BnMeshObserver::new_binder(CountingObserver(Arc::new(AtomicI32::new(0))));

    node.r#registerObserver(&one).expect("register one");
    node.r#registerObserver(&one).expect("register one again");
    node.r#registerObserver(&two).expect("register two");

    // Everything above crossed a real session, so the service saw three
    // separate proxy arrivals — but two of them name the same remote
    // object, and `Rewrap` must collapse those onto one wrapper.
    assert_eq!(
        node.r#receivedCount().unwrap(),
        2,
        "AC-22.11: three registrations of two remotes must yield two wrappers"
    );
}

/// The table holds only weak references, so it neither keeps a wrapper
/// alive nor grows without bound. Driven directly (no session needed):
/// the "remote" here is a local binder, which `wrap` treats the same way.
#[test]
fn rewrap_holds_nothing_alive_and_collects_dead_entries() {
    let rewrap: Rewrap<dyn IMeshObserver> = Rewrap::new(BnMeshObserver::new_binder);
    assert!(rewrap.is_empty());

    let remote = BnMeshObserver::new_binder(CountingObserver(Arc::new(AtomicI32::new(0))));

    let first = rewrap.wrap(&remote);
    let second = rewrap.wrap(&remote);
    assert_eq!(
        first.as_binder(),
        second.as_binder(),
        "AC-22.11: the same remote must map to the same local object"
    );
    assert_eq!(rewrap.len(), 1, "one remote, one entry");

    // Nothing else holds the wrapper: dropping both handles must let it go,
    // because the table's reference is weak.
    drop(first);
    drop(second);
    assert_eq!(
        rewrap.purge_dead(),
        1,
        "AC-22.11: an entry whose wrapper is gone is collected"
    );
    assert!(rewrap.is_empty());

    // And a later call mints a fresh one rather than resurrecting.
    let third = rewrap.wrap(&remote);
    assert_eq!(rewrap.len(), 1);
    drop(third);

    // A different remote is a different entry.
    let other = BnMeshObserver::new_binder(CountingObserver(Arc::new(AtomicI32::new(0))));
    let a = rewrap.wrap(&remote);
    let b = rewrap.wrap(&other);
    assert_ne!(
        a.as_binder(),
        b.as_binder(),
        "AC-22.11: distinct remotes must not collapse onto one wrapper"
    );
}
