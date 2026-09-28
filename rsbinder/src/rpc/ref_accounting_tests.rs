// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-23: every receipt of a binder is paid back to its sender exactly once.
//!
//! Hermetic: both ends sit on a Unix socketpair and are built with a fixed wire profile, so
//! no handshake runs. An rsbinder peer ignores a `DEC_STRONG` for an address that is not one
//! of its nodes, which is every `DEC_STRONG` a transaction target or a returning binder
//! produces; so the assertions read `RpcState::dec_received`, which counts each inbound
//! `DEC_STRONG` per address before that filter. A raw peer counts the frames itself.
//!
//! # Tests
//!
//! - `t1_a_twoway_target_is_paid_before_its_reply`: on the android-13+ wire each twoway,
//!   `INTERFACE_TRANSACTION` and `PING_TRANSACTION` included, costs one `DEC_STRONG` of the
//!   target, applied by the caller's reply wait; the r34 wire does not count targets.
//! - `t2_every_oneway_the_gate_resolves_is_paid`: out-of-order oneways from a raw client,
//!   with a stale number and a parked duplicate; the amounts that precede the next `REPLY`
//!   add up to the transactions the gate resolved, in one command.
//! - `t3_a_returning_local_binder_is_paid_once`: the server's own binder sent back as an
//!   argument and read twice costs one `DEC_STRONG` (plus the target's on android-13+).
//! - `t4_a_holder_retry_pays_its_binder_once`: a `ParcelableHolder` whose first
//!   `get_parcelable` fails after its binder, then succeeds, pays the binder once, at v1
//!   (entered by the sub-parcel on first read) and v2 (entered on arrival, cloned in).
//! - `t5_a_parcel_that_was_not_received_owes_nothing`: a proxy and a local binder written
//!   into a request and read back send no `DEC_STRONG`; a binder written into a received
//!   parcel is refused.
//! - `t6_an_unread_v2_reply_pays_its_binders`: at v2 a reply dropped unread still releases
//!   the node it named; at v1 it does not (AOSP pays only what it reads there).
//! - `t7_a_failed_v2_entry_enters_the_rest_and_keeps_the_connection`: a v2 reply whose first
//!   binder cannot be entered fails the call, the second binder is entered and paid back,
//!   and the next call on the connection succeeds.
//! - `t8_oneway_only_traffic_never_stalls`: 10⁴ oneways carrying a callback, with no
//!   callback connection, finish in bounded time; the owed `DEC_STRONG`s arrive in one
//!   command per address before the next reply. With a callback connection they arrive on
//!   it, without a twoway.
//! - `t9_a_proxy_release_waits_for_the_owners_receipt`: against a raw android-13+ owner, a
//!   proxy dropped right after a oneway on it sends nothing until the owner pays the
//!   oneway's target back, then its `DEC_STRONG` 1. Sent at once, it could be read before the
//!   oneway on another connection, which libbinder aborts on.
//! - `t10_a_proxy_argument_counts_at_v2`: the proxy also sent as that oneway's argument
//!   needs both receipts at v2 and only the target's at v1.
//! - `t11_the_release_waits_on_the_wires_that_count_targets`: rsbinder on both ends; on r34
//!   the release goes at once, on android-13+ with the server's receipt before its next reply.
//! - `t12_a_proxy_in_a_v2_reply_waits_for_its_receipt`: a server's proxy returned in a v2
//!   reply is released only after the raw caller pays that receipt.
//!
//! # Mutation gates
//!
//! Each gate names a change to `session.rs` / `parcel.rs` that makes the test fail.
//!
//! - T1: hold 0 instead of 1 for the twoway target in `execute_dispatched`.
//! - T2: drop the `StaleAsyncNumber` count, the `purged` count, or the post-drain send in
//!   `dispatch_oneway_ordered`.
//! - T3: drop the local-node `owed` in `enter_binder` (R1b), or the `rpc_entered_at` hit in
//!   `read_binder` (R2).
//! - T4: drop the `rpc_entered_at` hit (fails at v1), the `entered` clone in
//!   `carry_rpc_objects` (fails at v2), or the sub-parcel's inherited `Received` (fails at v1).
//! - T5: drop the `rpc_received_at` branch in `read_binder` (R4: the spurious `DEC_STRONG`
//!   frees the server's root and the next call is `DeadObject`), or the send-state check for
//!   a proxy in `write_binder` (R6).
//! - T6: skip `enter_every_binder` in `receive_parcel` (R3).
//! - T7: stop `enter_every_binder` at the first failure.
//! - T8: let `dec_route` take an `Incoming` pin whatever its `allow_nested`; both halves time
//!   out (with a callback connection the pin still comes first).
//! - T9, T11: drop the target count in `client_transact` (`counts_target`).
//! - T10, T12: drop the proxy count in `write_binder` (fails at v2).
//! - `state::tests::a_held_release_keeps_the_send_counter_for_a_successor`: close the
//!   `async_number` book in `RpcState::release_proxy`'s held arm.
//!
//! Outside this file, `strong_session_tests::argument_proxy_dec_strong_follows_the_reply_on_the_serving_connection`
//! fails when `execute_dispatched` arms `AllowNestedGuard` after building `reply` (its proxies
//! then drop without `allow_nested` and take the idle callback slot), and
//! `tests/rpc_e2e.rs` `a_sent_parcel_is_refused_on_resend` fails without the `drop(reader)`
//! before the reply (the argument's `DEC_STRONG` then trails the reply unread).

use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread::JoinHandle;

use super::*;
use crate::rpc::transport::UnixTransport;
use crate::{
    Binder, Interface, Parcelable, ParcelableHolder, ParcelableMetadata, Remotable,
    TransactionCode, FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IAccounting";
const CARRIER: &str = "rsbinder.test.Carrier";
/// `Option<SIBinder>` in, the same binder out.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
/// `SIBinder` in, read twice from the same position.
const TX_READ_TWICE: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// A fresh local binder out.
const TX_MAKE: TransactionCode = FIRST_CALL_TRANSACTION + 2;
/// A `ParcelableHolder` in: `Strict` must fail after the binder, then `Lenient` succeeds.
const TX_HOLDER: TransactionCode = FIRST_CALL_TRANSACTION + 3;
/// `SIBinder` in, kept by the service.
const TX_KEEP: TransactionCode = FIRST_CALL_TRANSACTION + 4;
/// Oneway: `Option<SIBinder>` in, dropped.
const TX_TAKE: TransactionCode = FIRST_CALL_TRANSACTION + 5;

#[derive(Default)]
struct Acct {
    kept: Mutex<Vec<SIBinder>>,
}
impl Interface for Acct {}
impl Remotable for Acct {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_ECHO => {
                let b: Option<SIBinder> = reader.read()?;
                reply.write(&b)
            }
            TX_READ_TWICE => {
                let pos = reader.data_position();
                let first: SIBinder = reader.read()?;
                reader.set_data_position(pos);
                let second: SIBinder = reader.read()?;
                if first == second {
                    Ok(())
                } else {
                    Err(StatusCode::BadValue)
                }
            }
            TX_MAKE => reply.write(&local()),
            TX_HOLDER => {
                let holder: ParcelableHolder = reader.read()?;
                if holder.get_parcelable::<Strict>().is_ok() {
                    return Err(StatusCode::BadValue);
                }
                let got = holder
                    .get_parcelable::<Lenient>()?
                    .ok_or(StatusCode::BadValue)?;
                got.binder
                    .as_ref()
                    .map(drop)
                    .ok_or(StatusCode::UnexpectedNull)
            }
            TX_KEEP => {
                let b: SIBinder = reader.read()?;
                self.kept.lock().unwrap().push(b);
                Ok(())
            }
            TX_TAKE => {
                let _b: Option<SIBinder> = reader.read()?;
                Ok(())
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Reads the whole payload: what the sender wrote.
#[derive(Debug, Default)]
struct Lenient {
    binder: Option<SIBinder>,
}
impl ParcelableMetadata for Lenient {
    fn descriptor() -> &'static str {
        CARRIER
    }
}
impl Parcelable for Lenient {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.binder)
    }
    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
        self.binder = parcel.read()?;
        Ok(())
    }
}

/// Same descriptor, one field more than the payload has: fails after reading the binder.
#[derive(Debug, Default)]
struct Strict {
    binder: Option<SIBinder>,
}
impl ParcelableMetadata for Strict {
    fn descriptor() -> &'static str {
        CARRIER
    }
}
impl Parcelable for Strict {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.binder)?;
        parcel.write(&0i32)
    }
    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
        self.binder = parcel.read()?;
        let _missing: i32 = parcel.read()?;
        Ok(())
    }
}

fn local() -> SIBinder {
    Interface::as_binder(&Binder::new(Acct::default()))
}

#[derive(Clone, Copy, Debug)]
enum Wire {
    R34,
    V(u32),
}

fn session(stream: UnixStream, space: AddressSpace, wire: Wire) -> RpcSession {
    let profile = match wire {
        Wire::R34 => WireProfile::R34(R34Codec),
        Wire::V(v) => {
            WireProfile::Android13Plus(Android13PlusCodec::with_version(v).expect("version"))
        }
    };
    let transport = UnixTransport::from_stream(stream).expect("transport");
    RpcSession::with_profile(Box::new(transport), space, profile).expect("session")
}

/// `(amount sum, command count)` of `DEC_STRONG`s `s` received for `addr`.
fn dec_received(s: &RpcSession, addr: &RpcAddress) -> (u64, u64) {
    s.inner.shared.state.lock().unwrap().dec_received(addr)
}

fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}

/// A server session serving `Acct` as its root, and a client holding the root proxy.
struct Pair {
    server: RpcSession,
    client: RpcSession,
    root: Option<SIBinder>,
    serve: Option<JoinHandle<SessionEnd>>,
}

impl Pair {
    fn new(wire: Wire) -> Self {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let server = session(a, AddressSpace::Acceptor, wire);
        server.set_root(local()).expect("set_root");
        let s = server.clone();
        let serve = std::thread::spawn(move || s.serve_blocking());
        let client = session(b, AddressSpace::Initiator, wire);
        let root = client.get_root().expect("get_root");
        Pair {
            server,
            client,
            root: Some(root),
            serve: Some(serve),
        }
    }

    fn root(&self) -> &SIBinder {
        self.root.as_ref().expect("root")
    }

    fn root_addr(&self) -> RpcAddress {
        rpc_of(self.root()).address()
    }

    fn call(&self, code: TransactionCode, arg: Option<&SIBinder>) -> Result<Parcel> {
        let rp = rpc_of(self.root());
        let mut d = rp.build_request(DESC)?;
        d.write(&arg.cloned())?;
        rp.transact(code, &d, 0)?.ok_or(StatusCode::UnexpectedNull)
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.root = None;
        self.client.close_session();
        self.server.close_session();
        if let Some(h) = self.serve.take() {
            let _ = h.join();
        }
    }
}

/// T1: an android-13+ twoway costs its target one `DEC_STRONG`, read before the reply.
#[test]
fn t1_a_twoway_target_is_paid_before_its_reply() {
    for (wire, per_call) in [(Wire::R34, 0u64), (Wire::V(1), 1), (Wire::V(2), 1)] {
        let p = Pair::new(wire);
        let addr = p.root_addr();
        p.call(TX_ECHO, None).expect("echo");
        assert_eq!(
            dec_received(&p.client, &addr),
            (per_call, per_call),
            "{wire:?}"
        );

        let rp = rpc_of(p.root());
        let d = rp.build_request(DESC).expect("request");
        p.client
            .inner
            .client_transact(addr, INTERFACE_TRANSACTION, &d, 0)
            .expect("interface");
        assert_eq!(dec_received(&p.client, &addr).0, 2 * per_call, "{wire:?}");

        p.root().ping_binder().expect("ping");
        assert_eq!(
            dec_received(&p.client, &addr),
            (3 * per_call, 3 * per_call),
            "{wire:?}: each target DEC arrives before its reply"
        );
    }
}

/// A raw android-13+ client of an rsbinder server; it reads every frame itself.
struct RawClient {
    stream: UnixStream,
    codec: Android13PlusCodec,
}

impl RawClient {
    fn send(&mut self, txn: WireTransaction) {
        let frame = self.codec.encode_transact(&txn).expect("encode");
        write_aosp_message(&mut self.stream, &frame).expect("write");
    }

    fn recv(&mut self) -> WireMessage {
        let frame = read_aosp_message(&mut self.stream).expect("read");
        self.codec.decode_message(&frame).expect("decode")
    }

    /// Sends `txn` and returns the reply with the `DEC_STRONG`s that preceded it.
    fn call(&mut self, txn: WireTransaction) -> (WireReply, Vec<(RpcAddress, u32)>) {
        self.send(txn);
        let mut decs = Vec::new();
        loop {
            match self.recv() {
                WireMessage::Reply(r) => return (r, decs),
                WireMessage::DecStrong(a, n) => decs.push((a, n)),
                WireMessage::Transact(_) => panic!("unexpected transaction"),
            }
        }
    }
}

/// T2: each oneway the gate resolves owes one `DEC_STRONG`, stale and purged ones included.
#[test]
fn t2_every_oneway_the_gate_resolves_is_paid() {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let server = session(a, AddressSpace::Acceptor, Wire::V(1));
    server.set_root(local()).expect("set_root");
    let s = server.clone();
    let serve = std::thread::spawn(move || s.serve_blocking());
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut raw = RawClient {
        stream: b,
        codec: Android13PlusCodec::with_version(PROTOCOL_V1).unwrap(),
    };

    let (root_reply, _) = raw.call(WireTransaction {
        code: SpecialTransaction::GetRoot.code(),
        ..Default::default()
    });
    // `i32` TYPE_BINDER, the 8-byte address, the stability `i32`.
    let root = Android13PlusCodec::decode_addr(&root_reply.data, 4).expect("root address");

    let mut body = Parcel::new();
    write_rpc_interface_token(&mut body, DESC).unwrap();
    body.write(&0i32).unwrap();
    let oneway = |async_number: u64| WireTransaction {
        address: root,
        code: TX_TAKE,
        flags: FLAG_ONEWAY,
        async_number,
        data: body.rpc_data_bytes().to_vec(),
        object_positions: Vec::new(),
    };
    // Owed: 0 runs and drains 1 (2); 1 is stale (1); 2 runs, drains a 3, purges the other (3).
    for n in [1u64, 0, 1, 3, 3, 2] {
        raw.send(oneway(n));
    }
    let (reply, decs) = raw.call(WireTransaction {
        address: root,
        code: PING_TRANSACTION,
        ..Default::default()
    });
    assert_eq!(reply.status, 0);
    assert_eq!(
        decs,
        vec![(root, 7)],
        "six oneways and the PING itself, held for this reply and sent as one command"
    );

    drop(raw);
    let _ = serve.join();
}

/// T3: our own binder coming back is paid once, however often its position is read.
#[test]
fn t3_a_returning_local_binder_is_paid_once() {
    for (wire, target) in [(Wire::R34, 0u64), (Wire::V(1), 1), (Wire::V(2), 1)] {
        let p = Pair::new(wire);
        let addr = p.root_addr();
        p.call(TX_READ_TWICE, Some(p.root())).expect("read twice");
        assert_eq!(
            dec_received(&p.client, &addr).0,
            target + 1,
            "{wire:?}: one receipt of our own address, plus the target's"
        );
        assert_eq!(
            p.server.local_node_count(),
            1,
            "{wire:?}: the root node stays"
        );
    }
}

/// T4: a holder read twice through `get_parcelable` pays its binder once.
#[test]
fn t4_a_holder_retry_pays_its_binder_once() {
    for wire in [Wire::V(1), Wire::V(2)] {
        let p = Pair::new(wire);
        let cb = local();
        // The server keeps a proxy, so each later receipt of `cb` is an excess one.
        p.call(TX_KEEP, Some(&cb)).expect("keep");
        let addr = p
            .client
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .local_address_of(&cb)
            .expect("cb was sent");
        assert_eq!(dec_received(&p.client, &addr).0, 0);

        let mut holder = ParcelableHolder::default();
        holder
            .set_parcelable(Arc::new(Lenient {
                binder: Some(cb.clone()),
            }))
            .unwrap();
        let rp = rpc_of(p.root());
        let mut d = rp.build_request(DESC).unwrap();
        d.write(&holder).unwrap();
        rp.transact(TX_HOLDER, &d, 0)
            .expect("holder")
            .expect("reply");
        assert_eq!(
            dec_received(&p.client, &addr),
            (1, 1),
            "{wire:?}: the holder's receipt of cb is paid exactly once"
        );
        assert_eq!(
            p.client.local_node_count(),
            1,
            "{wire:?}: the node the server still holds survives"
        );
    }
}

/// T5: a parcel this end built is looked up, not entered; a received one takes no binder.
#[test]
fn t5_a_parcel_that_was_not_received_owes_nothing() {
    for wire in [Wire::R34, Wire::V(1), Wire::V(2)] {
        let p = Pair::new(wire);
        let addr = p.root_addr();
        let rp = rpc_of(p.root());
        let mut d = rp.build_request(DESC).unwrap();
        let pos = d.data_position();
        d.write(p.root()).unwrap();
        let mine = local();
        d.write(&mine).unwrap();
        d.set_data_position(pos);
        assert_eq!(d.read::<SIBinder>().unwrap(), *p.root(), "{wire:?}");
        assert_eq!(d.read::<SIBinder>().unwrap(), mine, "{wire:?}");
        drop(d);
        // Ordered after any DEC the reads sent, on the same connection.
        p.root().ping_binder().expect("ping");
        assert_eq!(
            dec_received(&p.server, &addr),
            (0, 0),
            "{wire:?}: reading back what this end wrote owes the server nothing"
        );
        assert_eq!(p.server.local_node_count(), 1, "{wire:?}: root node intact");

        let mut reply = p.call(TX_ECHO, None).expect("echo");
        assert_eq!(
            reply.write(p.root()),
            Err(StatusCode::InvalidOperation),
            "{wire:?}: a proxy written into a received parcel"
        );
        assert_eq!(
            reply.write(&mine),
            Err(StatusCode::BadType),
            "{wire:?}: a local binder written into a received parcel"
        );
    }
}

/// T6: a v2 reply dropped unread pays for the binder it carried.
#[test]
fn t6_an_unread_v2_reply_pays_its_binders() {
    for (wire, released) in [(Wire::V(1), false), (Wire::V(2), true)] {
        let p = Pair::new(wire);
        let reply = p.call(TX_MAKE, None).expect("make");
        assert_eq!(
            p.server.local_node_count(),
            2,
            "{wire:?}: root and the new one"
        );
        drop(reply);
        // Ordered after the DEC the drop sent, on the same connection.
        p.root().ping_binder().expect("ping");
        let left = if released { 1 } else { 2 };
        assert_eq!(p.server.local_node_count(), left, "{wire:?}");
    }
}

/// T7: a v2 reply whose first binder fails still enters the second, and the session goes on.
#[test]
fn t7_a_failed_v2_entry_enters_the_rest_and_keeps_the_connection() {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let client = session(a, AddressSpace::Initiator, Wire::V(2));
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut raw = RawClient {
        stream: b,
        codec: Android13PlusCodec::with_version(PROTOCOL_V2).unwrap(),
    };
    let mut server_ctr = 0u64;
    let target = RpcAddress::unique(&mut server_ctr, AddressSpace::Acceptor);
    let second = RpcAddress::unique(&mut server_ctr, AddressSpace::Acceptor);
    // An address in the client's own subspace that it never minted: refused on entry.
    let mut forged_ctr = 1000u64;
    let forged = RpcAddress::unique(&mut forged_ctr, AddressSpace::Initiator);

    let caller = client.clone();
    let calls = std::thread::spawn(move || {
        let first = caller
            .inner
            .client_transact(target, FIRST_CALL_TRANSACTION, &Parcel::new(), 0)
            .map(drop);
        let next = caller
            .inner
            .client_transact(target, FIRST_CALL_TRANSACTION, &Parcel::new(), 0)
            .map(drop);
        (first, next)
    });

    let binder = |addr: &RpcAddress| {
        let mut obj = 1i32.to_le_bytes().to_vec();
        obj.extend_from_slice(&Android13PlusCodec::encode_addr(addr));
        obj.extend_from_slice(&0i32.to_le_bytes());
        obj
    };
    assert!(matches!(raw.recv(), WireMessage::Transact(_)));
    let mut data = binder(&forged);
    data.extend(binder(&second));
    let frame = raw
        .codec
        .encode_reply(&WireReply {
            status: 0,
            data,
            object_positions: vec![0, 16],
        })
        .unwrap();
    write_aosp_message(&mut raw.stream, &frame).unwrap();
    match raw.recv() {
        WireMessage::DecStrong(a, 1) if a == second => {}
        other => panic!("the second binder must be entered and paid back, got {other:?}"),
    }
    assert!(matches!(raw.recv(), WireMessage::Transact(_)));
    let frame = raw.codec.encode_reply(&WireReply::default()).unwrap();
    write_aosp_message(&mut raw.stream, &frame).unwrap();

    let (first, next) = calls.join().expect("caller");
    assert_eq!(
        first,
        Err(StatusCode::BadValue),
        "the call fails with the entry"
    );
    assert_eq!(next, Ok(()), "the connection carries the next call");
    client.close_session();
}

/// T8 part one: runs the oneways on a thread, bounded by a deadline.
fn oneways_finish(p: &Pair, cb: &SIBinder, n: usize) -> bool {
    let root = p.root().clone();
    let cb = cb.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rp = rpc_of(&root);
        for _ in 0..n {
            let mut d = rp.build_request(DESC).expect("request");
            d.write(&Some(cb.clone())).expect("arg");
            rp.transact(TX_TAKE, &d, FLAG_ONEWAY).expect("oneway");
        }
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(60)).is_ok()
}

/// T8: oneway-only traffic without a callback connection holds its `DEC_STRONG`s, never stalls.
#[test]
fn t8_oneway_only_traffic_never_stalls() {
    const N: usize = 10_000;
    let p = Pair::new(Wire::V(1));
    let root = p.root_addr();
    let cb = local();
    if !oneways_finish(&p, &cb, N) {
        // Unblock the stuck sender before failing, so the process does not hang on it.
        p.client.close_session();
        p.server.close_session();
        panic!("{N} oneways stalled: a DEC_STRONG went where the client never reads");
    }
    let cb_addr = p
        .client
        .inner
        .shared
        .state
        .lock()
        .unwrap()
        .local_address_of(&cb);
    assert_eq!(dec_received(&p.client, &root), (0, 0), "held until a reply");
    p.root().ping_binder().expect("ping");
    assert_eq!(
        dec_received(&p.client, &root),
        (N as u64 + 1, 1),
        "the held target receipts and the PING's own, in one command before its reply"
    );
    let cb_addr = cb_addr.expect("cb was sent");
    assert_eq!(dec_received(&p.client, &cb_addr), (N as u64, 1));
    assert_eq!(
        p.client.local_node_count(),
        0,
        "every send of cb was paid back"
    );
}

/// T8 with a callback connection: the `DEC_STRONG`s go there as they arise.
#[test]
fn t8_with_a_callback_connection_the_decs_go_there() {
    const N: usize = 1_000;
    let p = Pair::new(Wire::V(1));
    let (srv_end, cli_end) = UnixStream::pair().expect("socketpair");
    let srv_t = UnixTransport::from_stream(srv_end).expect("transport");
    p.server
        .add_callback_slot(Box::new(srv_t), 2)
        .expect("callback slot");
    let cli_t = UnixTransport::from_stream(cli_end).expect("transport");
    let slot = p
        .client
        .inner
        .add_slot_inner(Box::new(cli_t), SlotRole::Incoming)
        .expect("incoming slot");
    let c = p.client.clone();
    let callback_serve = std::thread::spawn(move || c.serve_blocking_on(slot));

    let root = p.root_addr();
    let cb = local();
    assert!(oneways_finish(&p, &cb, N), "oneways stalled");
    let deadline = Instant::now() + Duration::from_secs(20);
    while dec_received(&p.client, &root).0 < N as u64 {
        assert!(
            Instant::now() < deadline,
            "the DECs never came over the callback"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (amount, commands) = dec_received(&p.client, &root);
    assert_eq!(
        amount, N as u64,
        "no twoway ran, so nothing was held for a reply"
    );
    assert!(commands > 1, "sent as they arose, not held as one command");
    let held: usize = p
        .server
        .inner
        .conn_state
        .lock()
        .unwrap()
        .slots
        .iter()
        .map(|s| s.pending_dec.len())
        .sum();
    assert_eq!(held, 0, "nothing held on the serving slot");

    drop(p);
    let _ = callback_serve.join();
}

impl RawClient {
    /// The next message, or `None` when nothing arrives within `wait`.
    fn recv_within(&mut self, wait: Duration) -> Option<WireMessage> {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        let frame = read_aosp_message(&mut self.stream).ok();
        self.stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        frame.map(|f| self.codec.decode_message(&f).expect("decode"))
    }

    fn reply(&mut self, reply: WireReply) {
        let frame = self.codec.encode_reply(&reply).expect("encode");
        write_aosp_message(&mut self.stream, &frame).expect("write");
    }

    fn dec_strong(&mut self, addr: &RpcAddress, amount: u32) {
        for frame in self.codec.encode_dec_strong(addr, amount) {
            write_aosp_message(&mut self.stream, &frame).expect("write");
        }
    }
}

/// A raw android-13+ owner of `target`, and an rsbinder client holding a proxy of it.
fn raw_owner(version: u32) -> (RpcSession, RawClient, RpcAddress, SIBinder) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let client = session(a, AddressSpace::Initiator, Wire::V(version));
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut raw = RawClient {
        stream: b,
        codec: Android13PlusCodec::with_version(version).unwrap(),
    };
    let mut ctr = 0u64;
    let target = RpcAddress::unique(&mut ctr, AddressSpace::Acceptor);
    let c = client.clone();
    let root = std::thread::spawn(move || c.get_root());
    assert!(matches!(raw.recv(), WireMessage::Transact(_)));
    let mut data = 1i32.to_le_bytes().to_vec();
    data.extend_from_slice(&Android13PlusCodec::encode_addr(&target));
    data.extend_from_slice(&0i32.to_le_bytes());
    let object_positions = if version >= PROTOCOL_V2 {
        vec![0]
    } else {
        vec![]
    };
    raw.reply(WireReply {
        status: 0,
        data,
        object_positions,
    });
    let root = root.join().expect("get_root").expect("root");
    (client, raw, target, root)
}

/// A `GET_MAX_THREADS` the raw owner answers with `acks` receipts of `target` paid first.
fn pay_before_a_reply(client: &RpcSession, raw: &mut RawClient, target: &RpcAddress, acks: u32) {
    let c = client.clone();
    let call = std::thread::spawn(move || {
        c.inner
            .client_transact(RpcAddress::zero(), 1, &Parcel::new(), 0)
            .map(drop)
    });
    assert!(matches!(raw.recv(), WireMessage::Transact(_)));
    if acks > 0 {
        raw.dec_strong(target, acks);
    }
    raw.reply(WireReply {
        status: 0,
        data: 0i32.to_le_bytes().to_vec(),
        object_positions: vec![],
    });
    call.join().expect("caller").expect("GET_MAX_THREADS");
}

/// T9: a dropped proxy's `DEC_STRONG` waits until the owner paid the oneway sent on it.
#[test]
fn t9_a_proxy_release_waits_for_the_owners_receipt() {
    for version in [1, PROTOCOL_V2] {
        let (client, mut raw, target, root) = raw_owner(version);
        let d = rpc_of(&root).build_request(DESC).expect("request");
        rpc_of(&root)
            .transact(FIRST_CALL_TRANSACTION, &d, FLAG_ONEWAY)
            .expect("oneway");
        drop(d);
        assert!(matches!(raw.recv(), WireMessage::Transact(t) if t.address == target));
        drop(root);
        // Sent now, it could be read before the oneway on another connection.
        assert!(
            raw.recv_within(Duration::from_millis(300)).is_none(),
            "v{version}: the proxy was released before the owner paid the oneway"
        );
        pay_before_a_reply(&client, &mut raw, &target, 1);
        match raw.recv() {
            WireMessage::DecStrong(a, 1) if a == target => {}
            other => panic!("v{version}: expected the release after the receipt, got {other:?}"),
        }
        client.close_session();
    }
}

/// T10: at v2 a proxy sent as an argument is a send too, and the release waits for both
/// receipts. Below v2 the owner pays an argument only if it reads it, so it is not counted.
#[test]
fn t10_a_proxy_argument_counts_at_v2() {
    for (version, sends) in [(1, 1), (PROTOCOL_V2, 2)] {
        let (client, mut raw, target, root) = raw_owner(version);
        let mut d = rpc_of(&root).build_request(DESC).expect("request");
        d.write(&Some(root.clone())).expect("argument");
        rpc_of(&root)
            .transact(FIRST_CALL_TRANSACTION, &d, FLAG_ONEWAY)
            .expect("oneway");
        assert!(matches!(raw.recv(), WireMessage::Transact(t) if t.address == target));
        drop(d);
        drop(root);
        for paid in 0..sends {
            assert!(
                raw.recv_within(Duration::from_millis(300)).is_none(),
                "v{version}: released after {paid} of {sends} receipts"
            );
            pay_before_a_reply(&client, &mut raw, &target, 1);
        }
        match raw.recv() {
            WireMessage::DecStrong(a, 1) if a == target => {}
            other => panic!("v{version}: expected the release after {sends}, got {other:?}"),
        }
        client.close_session();
    }
}

/// T12: at v2 a proxy a handler returns in its reply is a send; the proxy's release waits for
/// the caller to pay it back, however the reply and the release are routed.
#[test]
fn t12_a_proxy_in_a_v2_reply_waits_for_its_receipt() {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let server = session(a, AddressSpace::Acceptor, Wire::V(2));
    server.set_root(local()).expect("set_root");
    let s = server.clone();
    let serve = std::thread::spawn(move || s.serve_blocking());
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut raw = RawClient {
        stream: b,
        codec: Android13PlusCodec::with_version(PROTOCOL_V2).unwrap(),
    };
    let (root_reply, _) = raw.call(WireTransaction {
        code: SpecialTransaction::GetRoot.code(),
        ..Default::default()
    });
    let root = Android13PlusCodec::decode_addr(&root_reply.data, 4).expect("root address");
    let mut ctr = 0u64;
    let mine = RpcAddress::unique(&mut ctr, AddressSpace::Initiator);

    let mut token = Parcel::new();
    write_rpc_interface_token(&mut token, DESC).unwrap();
    let mut data = token.rpc_data_bytes().to_vec();
    let at = data.len() as u32;
    data.extend_from_slice(&1i32.to_le_bytes());
    data.extend_from_slice(&Android13PlusCodec::encode_addr(&mine));
    data.extend_from_slice(&0i32.to_le_bytes());
    let (reply, _) = raw.call(WireTransaction {
        address: root,
        code: TX_ECHO,
        data,
        object_positions: vec![at],
        ..Default::default()
    });
    assert_eq!(reply.status, 0);
    let ping = WireTransaction {
        address: root,
        code: PING_TRANSACTION,
        ..Default::default()
    };
    let (_, decs) = raw.call(ping.clone());
    assert!(
        decs.iter().all(|(a, _)| *a != mine),
        "released before the reply's send was paid back: {decs:?}"
    );
    // The receipt of our own binder in the reply, as an owner pays it.
    raw.dec_strong(&mine, 1);
    let (_, decs) = raw.call(ping);
    assert!(
        decs.contains(&(mine, 1)),
        "the release follows the payment: {decs:?}"
    );

    drop(raw);
    let _ = serve.join();
}

/// T11: rsbinder on both ends. The r34 wire does not count targets, so the release goes at
/// once; on android-13+ it waits for the server's receipt, which the server holds for its
/// next `REPLY`.
#[test]
fn t11_the_release_waits_on_the_wires_that_count_targets() {
    for (wire, counts_targets) in [(Wire::R34, false), (Wire::V(1), true)] {
        let mut p = Pair::new(wire);
        let addr = p.root_addr();
        let root = p.root.take().expect("root");
        let d = rpc_of(&root).build_request(DESC).expect("request");
        rpc_of(&root)
            .transact(TX_TAKE, &d, FLAG_ONEWAY)
            .expect("oneway");
        drop(d);
        drop(root);
        let released = |within: Duration| {
            let deadline = Instant::now() + within;
            while dec_received(&p.server, &addr).0 == 0 {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            true
        };
        if counts_targets {
            assert!(
                !released(Duration::from_millis(300)),
                "{wire:?}: released early"
            );
            p.client
                .inner
                .client_transact(RpcAddress::zero(), 1, &Parcel::new(), 0)
                .expect("GET_MAX_THREADS");
        }
        assert!(released(Duration::from_secs(5)), "{wire:?}: never released");
        assert_eq!(dec_received(&p.server, &addr), (1, 1), "{wire:?}");
    }
}
