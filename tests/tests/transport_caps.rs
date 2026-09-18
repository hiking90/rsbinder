// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-0 — `TransportCaps`, the read-only summary of what the
//! transport under a binder can do.
//!
//! Three things are pinned here. First the derivation table: each transport
//! shape gets exactly the bits it earns, and in particular
//! `FD_PASSING` follows the **negotiated** fd mode rather than the socket
//! family, so a Unix session that never negotiated does not claim it.
//!
//! Second, and the point of the type: caps summarize checks that other
//! code still performs. A test that ignores the caps and writes a file
//! descriptor anyway must get the same refusal it always did — the
//! summary is informative, never load-bearing. That is
//! `caps_do_not_replace_the_write_time_check`.
//!
//! Third, the split `require` draws: a requirement is met only when every
//! bit is present, so one absent bit out of two is still a refusal. That
//! is `require_refuses_a_partially_satisfied_requirement`.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`: most cases need a
//! live session, and the `Endpoint` rows — the kernel one included — are
//! asserted without touching a device.

#![cfg(feature = "rpc")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{
    AddressSpace, FileDescriptorTransportMode, PeerIdentity, RpcSession, RpcTransport,
};
use rsbinder::{Endpoint, Interface, SIBinder, StatusCode, TransportCaps};

include!(concat!(env!("OUT_DIR"), "/rpc_caller.rs"));

use rpccaller::IRpcCaller::{BnRpcCaller, IRpcCaller};

const FD_TRUST_HOST: TransportCaps = TransportCaps::FD_PASSING
    .union(TransportCaps::TRUSTED_UID)
    .union(TransportCaps::SAME_HOST);
const TRUST_HOST: TransportCaps = TransportCaps::TRUSTED_UID.union(TransportCaps::SAME_HOST);

// ---- a service that reports what the call arrived over ---------------

/// Records `calling_caps()` observed inside the handler, so the caps a
/// *server* sees can be compared with the caps the client reports.
struct CapsSvc(Arc<Mutex<Option<TransportCaps>>>);
impl Interface for CapsSvc {}
impl IRpcCaller for CapsSvc {
    fn r#callingUid(&self) -> rsbinder::BinderResult<i64> {
        Ok(rsbinder::get_calling_uid() as i64)
    }
    fn r#callingPid(&self) -> rsbinder::BinderResult<i64> {
        Ok(rsbinder::get_calling_pid() as i64)
    }
    fn r#handlingTransaction(&self) -> rsbinder::BinderResult<bool> {
        Ok(rsbinder::is_handling_transaction())
    }
    /// Reused as the caps channel: the string is the observed set, so no
    /// new `.aidl` method is needed.
    fn r#callerKind(&self) -> rsbinder::BinderResult<String> {
        let caps = rsbinder::calling_caps();
        *self.0.lock().unwrap() = caps;
        Ok(match caps {
            Some(c) => c.to_string(),
            None => "none".to_string(),
        })
    }
}

/// A server session on `t`, serving on its own thread, plus the caps its
/// handler observed.
struct Served {
    session: RpcSession,
    observed: Arc<Mutex<Option<TransportCaps>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Served {
    fn start(t: Box<dyn RpcTransport>) -> Served {
        let observed = Arc::new(Mutex::new(None));
        let session = RpcSession::new(t, AddressSpace::Acceptor).expect("server session");
        let svc: SIBinder = BnRpcCaller::new_binder(CapsSvc(Arc::clone(&observed))).as_binder();
        session.set_root(svc).expect("set_root");
        let serving = session.clone();
        let thread = std::thread::spawn(move || {
            let _ = serving.serve_blocking();
        });
        Served {
            session,
            observed,
            thread: Some(thread),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.session.close_session();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn caller_of(session: &RpcSession) -> rsbinder::Strong<dyn IRpcCaller> {
    let root = session.get_root().expect("get_root");
    <dyn IRpcCaller as rsbinder::FromIBinder>::try_from(root).expect("cast")
}

// ---- the derivation table -------------------------------------------

/// Kernel binder has every bit, and `Endpoint::static_caps` says so
/// without opening a device — the row of the table that needs no session.
#[test]
fn kernel_endpoint_has_every_capability() {
    let kernel = Endpoint::Kernel {
        driver: None,
        threads: None,
        mmap_size: None,
    };
    assert_eq!(kernel.static_caps(), TransportCaps::KERNEL);
    for bit in [
        TransportCaps::FD_PASSING,
        TransportCaps::TRUSTED_UID,
        TransportCaps::CALLBACKS,
        TransportCaps::SAME_HOST,
        TransportCaps::KERNEL_KNOBS,
    ] {
        assert!(kernel.static_caps().contains(bit), "kernel lacks {bit}");
    }
}

/// What each RPC endpoint could offer at best. `static_caps` answers for
/// the transport, before negotiation: it never claims `CALLBACKS` (that
/// depends on the client opening incoming connections) and never claims
/// `KERNEL_KNOBS` (there is no driver underneath).
#[test]
fn rpc_endpoints_offer_what_their_socket_can_carry() {
    assert_eq!(
        Endpoint::Unix(PathBuf::from("/tmp/x")).static_caps(),
        FD_TRUST_HOST
    );
    assert_eq!(
        Endpoint::UnixAbstract(b"x".to_vec()).static_caps(),
        FD_TRUST_HOST
    );
    // vsock crosses a VM boundary: no shared kernel, no uid, no fds.
    assert_eq!(Endpoint::Vsock(2, 5000).static_caps(), TransportCaps::NONE);
    // TLS identifies the peer by certificate, which is not a uid.
    assert_eq!(
        Endpoint::Tls("h".into(), 443).static_caps(),
        TransportCaps::NONE
    );
    for e in [
        Endpoint::Unix(PathBuf::from("/tmp/x")),
        Endpoint::UnixAbstract(b"x".to_vec()),
        Endpoint::Vsock(2, 5000),
        Endpoint::Tls("h".into(), 443),
    ] {
        let c = e.static_caps();
        assert!(
            !c.contains(TransportCaps::CALLBACKS),
            "{e:?} claims CALLBACKS"
        );
        assert!(
            !c.contains(TransportCaps::KERNEL_KNOBS),
            "{e:?} claims KERNEL_KNOBS"
        );
    }
}

/// **The distinction `static_caps` cannot make.** A session over a Unix
/// socket carries fds only once it has negotiated the `Unix` mode; the
/// default is `None`. A caps value derived from the socket family rather
/// than the negotiated mode would claim `FD_PASSING` on the left-hand
/// session below, and the fd write would then fail anyway — which is the
/// whole failure mode this bit exists to prevent.
#[test]
fn fd_passing_follows_the_negotiated_mode_not_the_socket() {
    let (sa, sb) = UnixTransport::pair().expect("socketpair");
    let server = Served::start(Box::new(sa));
    let client = RpcSession::new(Box::new(sb), AddressSpace::Initiator).expect("client");

    // Before negotiating: a Unix socket underneath, but no fd mode.
    assert_eq!(
        client.fd_transport_mode(),
        FileDescriptorTransportMode::None
    );
    assert_eq!(client.caps(), TRUST_HOST);
    assert!(!client.caps().contains(TransportCaps::FD_PASSING));
    // The endpoint would have said yes — the two answers are different
    // questions, and this is why both exist.
    assert!(Endpoint::Unix(PathBuf::from("/tmp/x"))
        .static_caps()
        .contains(TransportCaps::FD_PASSING));

    // Both ends opt in, and the bit appears.
    server
        .session
        .set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
    let agreed = client
        .negotiate_fd_transport(FileDescriptorTransportMode::Unix)
        .expect("negotiate");
    assert_eq!(agreed, FileDescriptorTransportMode::Unix);
    assert_eq!(client.caps(), FD_TRUST_HOST);

    drop(client);
}

/// The in-memory transport reports this process as the peer, so it earns
/// the local-peer bits; it carries no fds, so it must not claim
/// `FD_PASSING` however it is asked.
#[test]
fn mem_transport_is_a_local_peer_that_carries_no_fds() {
    let (ta, tb) = MemTransport::pair();
    assert!(matches!(ta.peer_identity(), PeerIdentity::Local { .. }));
    let server = Served::start(Box::new(ta));
    let client = RpcSession::new(Box::new(tb), AddressSpace::Initiator).expect("client");

    assert_eq!(client.caps(), TRUST_HOST);
    assert_eq!(
        client.fd_transport_mode(),
        FileDescriptorTransportMode::None
    );

    // A negotiation the server never advertised leaves the mode `None`,
    // and the caps follow it rather than the request.
    let agreed = client
        .negotiate_fd_transport(FileDescriptorTransportMode::Unix)
        .expect("negotiate");
    assert_eq!(agreed, FileDescriptorTransportMode::None);
    assert_eq!(client.caps(), TRUST_HOST);

    // Now the other half of the conjunction: let the negotiation agree
    // `Unix`, so only the transport's own answer is left to keep the bit
    // away. This is the guard a vsock/TLS session needs — a mode it can
    // reach over a transport that fails every fd send.
    server
        .session
        .set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
    let agreed = client
        .negotiate_fd_transport(FileDescriptorTransportMode::Unix)
        .expect("negotiate");
    assert_eq!(agreed, FileDescriptorTransportMode::Unix);
    assert_eq!(
        client.fd_transport_mode(),
        FileDescriptorTransportMode::Unix
    );
    assert_eq!(
        client.caps(),
        TRUST_HOST,
        "a transport that carries no fds must not gain FD_PASSING from the mode alone"
    );
    drop(server);
    drop(client);
}

/// A session with no connection it may send on outside a dispatch has no
/// `CALLBACKS`, and `require` says so with the error AOSP uses for "this
/// transport cannot do this".
#[test]
fn a_session_without_callback_connections_refuses_up_front() {
    let (ta, tb) = MemTransport::pair();
    let server = Served::start(Box::new(ta));
    let client = RpcSession::new(Box::new(tb), AddressSpace::Initiator).expect("client");

    // The *server* side of a single-connection session serves its only
    // slot, so it cannot open a transaction of its own: no CALLBACKS.
    assert!(!server.session.caps().contains(TransportCaps::CALLBACKS));
    assert_eq!(
        server
            .session
            .caps()
            .require(TransportCaps::CALLBACKS, "streaming sink"),
        Err(StatusCode::InvalidOperation)
    );
    // Requiring what it does have succeeds.
    assert_eq!(
        server
            .session
            .caps()
            .require(TransportCaps::TRUSTED_UID, "a uid ACL"),
        Ok(())
    );
    drop(client);
}

/// What the handler sees. A server dispatching a call reports the caps of
/// the session it arrived on.
#[test]
fn a_handler_observes_the_caps_of_the_call_it_is_serving() {
    let (ta, tb) = UnixTransport::pair().expect("socketpair");
    let server = Served::start(Box::new(ta));
    let client = RpcSession::new(Box::new(tb), AddressSpace::Initiator).expect("client");
    let caller = caller_of(&client);

    assert_eq!(caller.r#callerKind().unwrap(), TRUST_HOST.to_string());
    assert_eq!(*server.observed.lock().unwrap(), Some(TRUST_HOST));

    drop(caller);
    drop(client);
}

/// **The invariant the type exists under.** Caps summarize checks that
/// still happen: ignore `FD_PASSING` and write a file descriptor into a
/// parcel whose mode forbids one, and the write refuses it exactly as it
/// did before caps existed. If this ever passes, the summary has become
/// the rule, which it must never be.
#[test]
fn caps_do_not_replace_the_write_time_check() {
    let (ta, tb) = MemTransport::pair();
    let _server = Served::start(Box::new(ta));
    let client = RpcSession::new(Box::new(tb), AddressSpace::Initiator).expect("client");
    assert!(!client.caps().contains(TransportCaps::FD_PASSING));

    // A real request parcel from this session, which stamps the
    // negotiated fd mode (`None`) into it.
    let root = client.get_root().expect("get_root");
    let proxy = (*root)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("RpcProxy");
    let mut parcel = proxy.build_request("rsbinder.test.ICaps").expect("request");
    let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
    let pfd = rsbinder::ParcelFileDescriptor::new(devnull);
    assert_eq!(
        parcel.write(&pfd),
        Err(StatusCode::FdsNotAllowed),
        "the fd write must refuse on its own, with no caps consulted"
    );
    drop(root);
    drop(client);
}

/// **The positive `CALLBACKS` case.** A client that opened an incoming
/// connection can be called back, and the bit appears on *both* ends —
/// they are the two ends of the same connection. The founding connection
/// alone never grants it, which `a_session_without_callback_connections_refuses_up_front`
/// and `outgoing_connections_do_not_grant_callbacks` pin.
#[test]
fn incoming_connections_grant_callbacks_on_both_ends() {
    use rsbinder::rpc::{RpcServer, RpcUnixClientConfig};

    let mut path = std::env::temp_dir();
    path.push(format!("rsb_caps_cb_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let observed = Arc::new(Mutex::new(None));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    server
        .set_root(BnRpcCaller::new_binder(CapsSvc(Arc::clone(&observed))).as_binder())
        .expect("set_root");
    let bg = server.run_background();

    // The listener is bound by `setup_unix_server`, so the socket exists
    // before `run_background` — connect straight away.
    let client = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&path, 2).incoming_connections(1),
    )
    .expect("connect with a callback connection");

    assert!(
        client.caps().contains(TransportCaps::CALLBACKS),
        "a client that opened an incoming connection can be called back: {}",
        client.caps()
    );
    assert_eq!(
        client.caps(),
        TRUST_HOST.union(TransportCaps::CALLBACKS),
        "…and nothing else: fds were never negotiated"
    );
    assert_eq!(
        client
            .caps()
            .require(TransportCaps::CALLBACKS, "streaming sink"),
        Ok(())
    );

    // The server end of the same session reports it too, and so does a
    // handler dispatching on it.
    let caller = caller_of(&client);
    caller.r#callerKind().expect("call the server");
    let seen = observed.lock().unwrap().expect("the handler ran");
    assert!(
        seen.contains(TransportCaps::CALLBACKS),
        "the server's handler must see the callback connection too: {seen}"
    );

    drop(caller);
    client.close_session();
    server.stop_accepting();
    let _ = bg.join();
    server.terminate();
    let _ = std::fs::remove_file(&path);
}

/// A requirement holds only when every bit is present. The log line
/// `require` writes names the missing bits and the transport, but the
/// message is not an API contract; what is pinned here is that a
/// partially-satisfied requirement fails.
#[test]
fn require_refuses_a_partially_satisfied_requirement() {
    let unix_no_fds = TRUST_HOST;
    let missing = (TransportCaps::FD_PASSING | TransportCaps::CALLBACKS).difference(unix_no_fds);
    assert_eq!(
        missing,
        TransportCaps::FD_PASSING | TransportCaps::CALLBACKS
    );
    assert_eq!(missing.to_string(), "FD_PASSING|CALLBACKS");
    assert_eq!(
        unix_no_fds.difference(TransportCaps::TRUSTED_UID),
        TransportCaps::SAME_HOST
    );
    // The split `require` itself draws: what the set has passes, what it
    // lacks — even partially — does not.
    assert_eq!(
        unix_no_fds.require(
            TransportCaps::TRUSTED_UID | TransportCaps::SAME_HOST,
            "an ACL"
        ),
        Ok(())
    );
    assert_eq!(
        unix_no_fds.require(
            TransportCaps::FD_PASSING | TransportCaps::CALLBACKS,
            "a pipe handed to a callback"
        ),
        Err(StatusCode::InvalidOperation)
    );
    assert_eq!(
        unix_no_fds.require(
            TransportCaps::TRUSTED_UID | TransportCaps::CALLBACKS,
            "an ACL checked from a callback"
        ),
        Err(StatusCode::InvalidOperation),
        "one missing bit out of two is still a refusal"
    );
}

/// **The negative fan-out case.** Extra *outgoing* connections are not
/// callback connections: the client can make more calls at once, but the
/// server still has nothing to write a request into. Neither end reports
/// `CALLBACKS`.
#[test]
fn outgoing_connections_do_not_grant_callbacks() {
    use rsbinder::rpc::{RpcServer, RpcUnixClientConfig};

    let mut path = std::env::temp_dir();
    path.push(format!("rsb_caps_fanout_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let observed = Arc::new(Mutex::new(None));
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_android13plus(2);
    // fan-out is min(local, remote); the server default of 1 silently folds
    // outgoing_connections(2) into a single-connection session.
    server.set_max_threads(2);
    server
        .set_root(BnRpcCaller::new_binder(CapsSvc(Arc::clone(&observed))).as_binder())
        .expect("set_root");
    let bg = server.run_background();

    let client = RpcSession::setup_unix_client_android13plus_with_config(
        RpcUnixClientConfig::path(&path, 2).outgoing_connections(2),
    )
    .expect("connect with a fan-out");
    assert_eq!(
        client.negotiated_max_threads(),
        2,
        "the second outgoing connection must exist for this case to mean anything"
    );

    assert_eq!(
        client.caps(),
        TRUST_HOST,
        "outgoing connections add throughput, not a callback path: {}",
        client.caps()
    );
    assert_eq!(
        client
            .caps()
            .require(TransportCaps::CALLBACKS, "streaming sink"),
        Err(StatusCode::InvalidOperation)
    );

    let caller = caller_of(&client);
    caller.r#callerKind().expect("call the server");
    let seen = observed.lock().unwrap().expect("the handler ran");
    assert_eq!(
        seen, TRUST_HOST,
        "the server's handler must not see a callback path either"
    );

    drop(caller);
    client.close_session();
    server.stop_accepting();
    let _ = bg.join();
    server.terminate();
    let _ = std::fs::remove_file(&path);
}
