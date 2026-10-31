// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Hermetic end-to-end: drive the
//! `IAccessor → addConnection() → preconnected-fd RpcSession →
//! getRootObject()` bridge entirely in-process against an rsbinder
//! `MockAccessor` and an rsbinder `RpcServer`.
//!
//! Hermetic ⇒ symmetric: this binary green is the in-process gate. The
//! non-negotiable real-libbinder gate runs against the android-16
//! emulator in a separate harness and is *not* in this file's scope.
//!
//! Separate test binary: no shared process with the kernel
//! binder unit tests. Each test owns its own server + sessions ⇒
//! parallel-safe.
//!
//! # Mutation gates
//!
//! - `kernel_parcel_refuses_the_accessor_root_wrapper`: `AccessorRoot::as_any` forwards to
//!   the inner `RpcProxy` (so `as_remote` / `as_proxy` see the concrete type); that forward
//!   lets the one check in `SerializeOption for SIBinder` cover the wrapper without a second
//!   branch. Dropping the forward turns this test red.
//! - `accessor_arm_handles_nonblocking_fd_from_libbinder`: AOSP `singleSocketConnection`
//!   (`frameworks/native/libs/binder/RpcSession.cpp:614`, android-16.0.0_r4) opens its
//!   preconnected socket with `SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK`, so every fd
//!   `IAccessor::addConnection()` returns is non-blocking. rsbinder RPC I/O is blocking (the
//!   handshake reads the `RpcNewSessionResponse` via `read_exact_raw`, which assumes `read`
//!   blocks); without the `O_NONBLOCK` clear in `RpcSession::from_preconnected_fd` the
//!   handshake's first `read()` returns `EAGAIN` and tears the connection down. Removing that
//!   clear fails this test hermetically, before the android-16 emulator STAGE3 run.
//! - `process_local_provider_resolves_root_via_resolve_helper`: removing the
//!   `or_else(|| try_process_local_fallback(name))` from
//!   `hub::servicemanager_16::dispatch_typed_service` (the *consume*-side wire-up) does not
//!   break this test, because the test calls `try_resolve_via_process_local` directly;
//!   `hub::servicemanager_16::tests` catches it
//!   (`dispatch_falls_back_when_accessor_arm_binder_is_none`).
//! - `resolve_via_process_local_keys_strictly_by_instance_name`: a provider that returns
//!   `Some(_)` for every name *and* a registry that ignores its instance set, both at once,
//!   resolve the `unknown` lookup, and the `is_none` assertions fail. Either mutant alone
//!   passes: the test provider answers only its own name. This is the contract the
//!   `hub::servicemanager_16::dispatch_typed_service` fallback relies on; the dispatcher's own
//!   routing is exercised by the unit tests in `hub::servicemanager_16::tests`.

#![cfg(all(feature = "rpc", feature = "android_16"))]

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::hub::android_16::{
    accessor_error_name, try_resolve_accessor, BnAccessor, IAccessor,
    ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
    ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
    ERROR_UNSUPPORTED_SOCKET_FAMILY,
};
use rsbinder::rpc::{RpcProxy, RpcServer, RpcServerHandle};
use rsbinder::{
    ExceptionCode, Interface, Parcel, ParcelFileDescriptor, Remotable, RemoteProxy, Result,
    SIBinder, Status, StatusCode, Strong, TransactionCode, FIRST_CALL_TRANSACTION,
};

// ---- echo service (the RPC root behind the Accessor) ----------------

const ECHO_DESC: &str = "rsbinder.test.IEchoForAccessor";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;

trait IEcho: Interface {
    fn echo(&self, s: &str) -> Result<String>;
}

struct EchoSvc {
    calls: Arc<AtomicI64>,
}
impl Interface for EchoSvc {}
impl IEcho for EchoSvc {
    fn echo(&self, s: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(s.to_string())
    }
}

struct BnEcho(Box<dyn IEcho + Send + Sync>);
impl Remotable for BnEcho {
    fn descriptor() -> &'static str {
        ECHO_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_ECHO => {
                let s: String = reader.read()?;
                let out = self.0.echo(&s);
                match out {
                    Ok(v) => {
                        reply.write(&Status::from(StatusCode::Ok))?;
                        reply.write(&v)
                    }
                    Err(e) => reply.write(&Status::from(e)),
                }
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn make_echo(calls: Arc<AtomicI64>) -> SIBinder {
    Interface::as_binder(&rsbinder::Binder::new(BnEcho(Box::new(EchoSvc { calls }))))
}

// ---- MockAccessor (the IAccessor implementation under test) ---------

/// Test `IAccessor`; `name` may differ from the lookup to drive the `validateAccessor` reject.
struct MockAccessor {
    /// Socket path; every `addConnection()` connects anew (AOSP allows repeats; rsbinder uses one).
    server_path: PathBuf,
    /// Reported instance name; the bridge requires it to match the lookup name.
    name: String,
    /// If set, `addConnection()` fails with `ServiceSpecificError(code)` (decode + reject path).
    add_connection_error: Option<i32>,
    /// Counts `addConnection()` calls: one per bridge attempt, none after a name reject.
    addconnection_calls: Arc<AtomicU32>,
    /// Return the fd `O_NONBLOCK`, as AOSP `singleSocketConnection` does (module doc).
    nonblocking: bool,
}

impl Interface for MockAccessor {}
impl IAccessor for MockAccessor {
    fn r#addConnection(&self) -> rsbinder::BinderResult<ParcelFileDescriptor> {
        self.addconnection_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = self.add_connection_error {
            return Err(Status::new_service_specific_error(
                code,
                Some(format!("MockAccessor: simulated ERROR={code}")),
            ));
        }
        let stream = UnixStream::connect(&self.server_path).map_err(|e| {
            Status::new_service_specific_error(
                ERROR_FAILED_TO_CONNECT_TO_SOCKET,
                Some(format!("MockAccessor: connect failed: {e}")),
            )
        })?;
        if self.nonblocking {
            stream.set_nonblocking(true).map_err(|e| {
                Status::new_service_specific_error(
                    ERROR_FAILED_TO_CONNECT_TO_SOCKET,
                    Some(format!("MockAccessor: set_nonblocking failed: {e}")),
                )
            })?;
        }
        Ok(ParcelFileDescriptor::new(stream))
    }

    fn r#getInstanceName(&self) -> rsbinder::BinderResult<String> {
        Ok(self.name.clone())
    }
}

fn make_mock_accessor(mock: MockAccessor) -> SIBinder {
    let strong: Strong<dyn IAccessor> = BnAccessor::new_binder(mock);
    Interface::as_binder(&*strong)
}

// ---- harness helpers ------------------------------------------------

fn tmp_sock(tag: &str) -> PathBuf {
    // Unique per test, PID and time; short prefix for macOS's 104-byte `sun_path`.
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_acc_{tag}_{}_{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}

fn wait_for_sock(path: &PathBuf) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("server socket {path:?} did not appear in time");
}

/// `RpcServer` (android-13+ wire, max=2 ⇒ android-16 v2) serving `EchoSvc`; drop stops it.
struct EchoServerGuard {
    path: PathBuf,
    calls: Arc<AtomicI64>,
    server: Arc<RpcServer>,
    bg: Option<RpcServerHandle>,
}

impl EchoServerGuard {
    fn start(tag: &str) -> Self {
        let path = tmp_sock(tag);
        let calls = Arc::new(AtomicI64::new(0));
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(2); // android-16 v2 ceiling
        server.set_max_threads(2).expect("set_max_threads");
        server.set_root(make_echo(calls.clone())).expect("set_root");
        let bg = server.run_background().expect("run_background");
        wait_for_sock(&path);
        EchoServerGuard {
            path,
            calls,
            server,
            bg: Some(bg),
        }
    }
}

impl Drop for EchoServerGuard {
    fn drop(&mut self) {
        if let Some(bg) = self.bg.take() {
            let _ = bg.stop_and_join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Echo `s` via `TX_ECHO` on the bridged RPC root.
fn rpc_echo(root: &SIBinder, s: &str) -> Result<String> {
    let rp = (**root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .ok_or(StatusCode::BadType)?;
    let mut data = rp.prepare_request(ECHO_DESC)?;
    data.write(&s.to_owned())?;
    let mut reply = rp
        .submit_transact(TX_ECHO, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = reply.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    reply.read::<String>()
}

// ---- happy path -----------------------------------------------------

#[test]
fn accessor_arm_resolves_root_and_echoes() {
    let server = EchoServerGuard::start("happy");

    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });

    let root = try_resolve_accessor("test.echo", accessor).expect("bridge yields an RPC root");

    // Exactly one `addConnection()` call (single-connection model).
    assert_eq!(addconn_calls.load(Ordering::SeqCst), 1);

    // Real RPC echo through the bridged proxy.
    assert_eq!(rpc_echo(&root, "hello").unwrap(), "hello");
    assert_eq!(rpc_echo(&root, "").unwrap(), "");
    for i in 0..20 {
        assert_eq!(rpc_echo(&root, &format!("n{i}")).unwrap(), format!("n{i}"));
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 22);

    // The wrapper's `Drop` releases the inner proxy (DEC_STRONG) before the session.
    drop(root);
}

/// AC-22.1: the `AccessorRoot` wrapper is an RPC binder, so a kernel parcel refuses it too.
#[test]
fn kernel_parcel_refuses_the_accessor_root_wrapper() {
    let server = EchoServerGuard::start("xstack");
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });

    let root = try_resolve_accessor("test.echo", accessor).expect("bridge yields an RPC root");

    let mut p = Parcel::new();
    assert_eq!(
        p.write(&root).unwrap_err(),
        StatusCode::InvalidOperation,
        "AC-22.1: an Accessor-bridged RPC root cannot cross into kernel binder"
    );

    drop(root);
}

// ---- mutant 1: instance-name mismatch reject ------------------------

#[test]
fn accessor_instance_name_mismatch_rejects() {
    let server = EchoServerGuard::start("mismatch");
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        // Accessor lies about its name; bridge must reject before `addConnection()`.
        name: "evil.imposter".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });

    // AOSP `validateAccessor` answers a mismatch with `NAME_NOT_FOUND`.
    let err = try_resolve_accessor("test.echo", accessor).expect_err("mismatch must reject");
    assert_eq!(err.exception_code(), ExceptionCode::TransactionFailed);
    assert_eq!(err.transaction_error(), StatusCode::NameNotFound);
    // As AOSP `validateAccessor`: reject before a misbehaving Accessor gets a socket.
    assert_eq!(
        addconn_calls.load(Ordering::SeqCst),
        0,
        "instance-name reject must short-circuit before addConnection()"
    );
}

/// A binder that is not an `IAccessor` is `BadType`, as AOSP `validateAccessor` returns.
#[test]
fn accessor_cast_failure_is_bad_type() {
    let not_an_accessor = make_echo(Arc::new(AtomicI64::new(0)));
    let err = try_resolve_accessor("test.echo", not_an_accessor).expect_err("cast must fail");
    assert_eq!(err.transaction_error(), StatusCode::BadType);
}

// ---- mutant 2: addConnection ServiceSpecificError reject ------------

#[test]
fn accessor_add_connection_service_specific_error_rejects() {
    let server = EchoServerGuard::start("sserror");
    for &code in &[
        ERROR_CONNECTION_INFO_NOT_FOUND,
        ERROR_FAILED_TO_CREATE_SOCKET,
        ERROR_FAILED_TO_CONNECT_TO_SOCKET,
        ERROR_FAILED_TO_CONNECT_EACCES,
        ERROR_UNSUPPORTED_SOCKET_FAMILY,
    ] {
        let calls = Arc::new(AtomicU32::new(0));
        let accessor = make_mock_accessor(MockAccessor {
            server_path: server.path.clone(),
            name: "test.echo".to_string(),
            add_connection_error: Some(code),
            addconnection_calls: calls.clone(),
            nonblocking: false,
        });
        // The accessor's own `Status` comes back unchanged, as AOSP `toBinderService` returns it.
        let err = try_resolve_accessor("test.echo", accessor).expect_err("ERROR must reject");
        assert_eq!(err.exception_code(), ExceptionCode::ServiceSpecific);
        assert_eq!(err.service_specific_error(), code, "ERROR={code}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

// ---- session lifetime — root stays usable, then dies ----------------

#[test]
fn accessor_root_keeps_session_alive_then_terminates_on_drop() {
    let server = EchoServerGuard::start("lifetime");
    // The guard and the accept thread; each serving worker holds one more.
    let idle_refs = Arc::strong_count(&server.server);
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });
    let root = try_resolve_accessor("test.echo", accessor).expect("bridge");
    // The wrapper holds the `RpcSession` the caller never sees.
    assert_eq!(rpc_echo(&root, "alive").unwrap(), "alive");

    // A clone of the SIBinder must stay usable too.
    let clone = root.clone();
    assert_eq!(rpc_echo(&clone, "still-alive").unwrap(), "still-alive");
    drop(root);
    // The clone keeps the session alive, as with AOSP `setSessionSpecificRoot`.
    assert_eq!(rpc_echo(&clone, "after-drop").unwrap(), "after-drop");
    assert!(
        server.server.live_session_node_count() >= 1,
        "the root node is live"
    );
    assert!(
        Arc::strong_count(&server.server) > idle_refs,
        "a worker serves the session"
    );
    drop(clone);
    // The last reference closes the session's connections, so every worker exits.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Arc::strong_count(&server.server) != idle_refs
        || server.server.live_session_node_count() != 0
    {
        assert!(
            Instant::now() < deadline,
            "the session outlived its last root reference"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

// ---- STAGE3 regression gate: non-blocking preconnected fd -----------

/// A non-blocking preconnected fd, as AOSP returns, still handshakes; see module doc gates.
#[test]
fn accessor_arm_handles_nonblocking_fd_from_libbinder() {
    let server = EchoServerGuard::start("nonblock");
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        // Faithfully mimic AOSP: returned fd has `O_NONBLOCK` set.
        nonblocking: true,
    });
    let root = try_resolve_accessor("test.echo", accessor)
        .expect("bridge must clear O_NONBLOCK before the v2 handshake");
    assert_eq!(addconn_calls.load(Ordering::SeqCst), 1);

    // Completes only because the bridge cleared the inherited `O_NONBLOCK`.
    assert_eq!(rpc_echo(&root, "hello-nonblock").unwrap(), "hello-nonblock");
    for i in 0..10 {
        assert_eq!(rpc_echo(&root, &format!("n{i}")).unwrap(), format!("n{i}"));
    }
}

// ---- process-local AccessorProvider fallback e2e --------------------

use rsbinder::hub::android_16::{
    add_accessor_provider, create_accessor, try_resolve_via_process_local, AccessorAddrProvider,
    AccessorProviderFn, AccessorSockAddr,
};
use std::collections::HashSet;

/// A provider answering only `want`, with an accessor that reports `reports` and dials `path`.
fn provider_for(want: String, reports: String, path: PathBuf) -> AccessorProviderFn {
    Box::new(move |name: &str| {
        if name == want {
            let addr_provider: AccessorAddrProvider = Box::new({
                let p = path.clone();
                move |_| Ok(AccessorSockAddr::Unix(p.clone()))
            });
            Some(create_accessor(reports.clone(), addr_provider))
        } else {
            None
        }
    })
}

/// `add_accessor_provider` → `try_resolve_via_process_local` root echoes; see module doc gates.
#[test]
fn process_local_provider_resolves_root_via_resolve_helper() {
    let server = EchoServerGuard::start("a5-helper");

    // The registry is process-wide, so the name is unique per process and line.
    let instance = format!("rsb.test.a5.helper.{}.{}", std::process::id(), line!());

    // A `LocalAccessor` whose `addr_provider` always returns the server's UDS path.
    let provider = provider_for(instance.clone(), instance.clone(), server.path.clone());
    let provider_handle =
        add_accessor_provider(HashSet::from([instance.clone()]), provider).expect("registry add");

    // Public entrypoint: combined `getInjectedAccessor` + `toBinderService` path.
    let root = try_resolve_via_process_local(&instance)
        .expect("process-local fallback yields root")
        .expect("a provider claims the instance");

    // An echo proves `addr_provider` ran and `addConnection` reached the server's listener.
    assert_eq!(
        rpc_echo(&root, "a5-hello").unwrap(),
        "a5-hello",
        "round-trip via process-local provider must echo"
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    drop(provider_handle);
}

/// Unregistered names find nothing even beside a sibling registration; see module doc.
#[test]
fn resolve_via_process_local_keys_strictly_by_instance_name() {
    let server = EchoServerGuard::start("a5-fallback");
    let unknown = format!(
        "rsb.test.a5.unregistered.{}.{}",
        std::process::id(),
        line!()
    );
    // An instance no provider ever claimed is `Ok(None)`, not a phantom binder.
    let found = try_resolve_via_process_local(&unknown).expect("a lookup, not an error");
    assert!(found.is_none(), "unregistered name");

    // A provider for a *different* instance must answer only its registered name.
    let target = format!("rsb.test.a5.targeted.{}.{}", std::process::id(), line!());
    let provider = provider_for(target.clone(), target.clone(), server.path.clone());
    // Named, not `_`: dropping the RAII handle unregisters the provider.
    let provider_handle =
        add_accessor_provider(HashSet::from([target.clone()]), provider).expect("registry add");

    let found = try_resolve_via_process_local(&unknown).expect("a lookup, not an error");
    assert!(
        found.is_none(),
        "registered sibling must not leak into unrelated names"
    );
    assert!(
        matches!(try_resolve_via_process_local(&target), Ok(Some(_))),
        "registered name must resolve"
    );
    drop(provider_handle);
}

/// A provider's accessor that reports another instance is `InvalidOperation`, never a root.
#[test]
fn process_local_instance_mismatch_is_invalid_operation() {
    let server = EchoServerGuard::start("a5-mismatch");
    let target = format!("rsb.test.a5.mismatch.{}.{}", std::process::id(), line!());
    // AOSP `getInjectedAccessor` answers a failed `validateAccessor` with `INVALID_OPERATION`.
    let provider = provider_for(
        target.clone(),
        "evil.imposter".to_string(),
        server.path.clone(),
    );
    let provider_handle =
        add_accessor_provider(HashSet::from([target.clone()]), provider).expect("registry add");
    let err = try_resolve_via_process_local(&target).expect_err("mismatch must reject");
    assert_eq!(err.transaction_error(), StatusCode::InvalidOperation);
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
    drop(provider_handle);
}

/// The deprecated `Option` forms are the `try_` forms with the error dropped.
#[test]
#[expect(
    deprecated,
    reason = "pins the deprecated Option forms while they are kept"
)]
fn deprecated_option_forms_wrap_the_try_forms() {
    use rsbinder::hub::android_16::{resolve_accessor, resolve_via_process_local};

    let server = EchoServerGuard::start("a5-deprecated");
    let ok = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: Arc::new(AtomicU32::new(0)),
        nonblocking: false,
    });
    let swm = resolve_accessor("test.echo", ok).expect("bridge resolves");
    assert!(!swm.r#isLazyService, "an RPC root is never a LazyService");
    let root = swm.r#service.expect("an RPC root binder");
    assert_eq!(rpc_echo(&root, "wrapped").unwrap(), "wrapped");

    let imposter = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "evil.imposter".to_string(),
        add_connection_error: None,
        addconnection_calls: Arc::new(AtomicU32::new(0)),
        nonblocking: false,
    });
    assert!(resolve_accessor("test.echo", imposter).is_none());

    let unknown = format!("rsb.test.a5.deprecated.{}.{}", std::process::id(), line!());
    assert!(resolve_via_process_local(&unknown).is_none());
}

// ---- sanity: accessor_error_name surface lock -----------------------

#[test]
fn accessor_error_name_unknown_codes_safe() {
    // Non-ServiceSpecific statuses read as `0`, so another `ERROR_*=0` would mask failures.
    assert_eq!(accessor_error_name(0), "ERROR_CONNECTION_INFO_NOT_FOUND");
    // Each name is what `try_resolve_accessor`'s operator-facing log line shows.
    for (code, name) in [
        (
            ERROR_CONNECTION_INFO_NOT_FOUND,
            "ERROR_CONNECTION_INFO_NOT_FOUND",
        ),
        (
            ERROR_FAILED_TO_CREATE_SOCKET,
            "ERROR_FAILED_TO_CREATE_SOCKET",
        ),
        (
            ERROR_FAILED_TO_CONNECT_TO_SOCKET,
            "ERROR_FAILED_TO_CONNECT_TO_SOCKET",
        ),
        (
            ERROR_FAILED_TO_CONNECT_EACCES,
            "ERROR_FAILED_TO_CONNECT_EACCES",
        ),
        (
            ERROR_UNSUPPORTED_SOCKET_FAMILY,
            "ERROR_UNSUPPORTED_SOCKET_FAMILY",
        ),
    ] {
        assert_eq!(accessor_error_name(code), name, "code {code}");
    }
    assert_eq!(accessor_error_name(42), "unknown");
    assert_eq!(accessor_error_name(-1), "unknown");
}
