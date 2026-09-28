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
