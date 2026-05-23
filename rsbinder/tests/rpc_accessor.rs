// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-13 D.7 — STAGE1 hermetic end-to-end: drive the
//! `IAccessor → addConnection() → preconnected-fd RpcSession →
//! getRootObject()` bridge entirely in-process against an rsbinder
//! `MockAccessor` and an rsbinder `RpcServer`. Exercises A0.1–A0.3 +
//! A.4 + A.5 + B.6.
//!
//! Plan §3 calls this "hermetic ⇒ symmetric": this binary green is the
//! AC-13.1/13.2/13.3 gate. The non-negotiable real-libbinder gate is
//! D.8 (separate harness, android-16 emulator) and is *not* in this
//! file's scope; see [plans/2-13-rpc-accessor.md](
//! ../../plans/2-13-rpc-accessor.md) §3 STAGE3.
//!
//! Separate test binary (master §6): no shared process with the kernel
//! binder unit tests. P6: each test owns its own server + sessions ⇒
//! parallel-safe.

#![cfg(all(feature = "rpc", feature = "android_16"))]

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::hub::android_16::{
    self as a16, accessor_error_name, resolve_accessor, BnAccessor, IAccessor,
    ERROR_CONNECTION_INFO_NOT_FOUND, ERROR_FAILED_TO_CONNECT_EACCES,
    ERROR_FAILED_TO_CONNECT_TO_SOCKET, ERROR_FAILED_TO_CREATE_SOCKET,
    ERROR_UNSUPPORTED_SOCKET_FAMILY,
};
use rsbinder::rpc::{RpcProxy, RpcServer};
use rsbinder::{
    Interface, Parcel, ParcelFileDescriptor, Remotable, Result, SIBinder, Status, StatusCode,
    Strong, TransactionCode, FIRST_CALL_TRANSACTION,
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

/// What the Accessor reports on `getInstanceName()`. Tests can lie
/// (return a different name than the looked-up service) to drive the
/// `validateAccessor` rejection path (AC-13.3).
struct MockAccessor {
    /// Server-side socket path; `addConnection()` opens a fresh
    /// `UnixStream::connect()` to it on every call (the AOSP shape:
    /// "may be called multiple times" — rsbinder's single-connection
    /// session model uses exactly one).
    server_path: PathBuf,
    /// Reported instance name (the bridge enforces match vs. the
    /// caller-supplied lookup name).
    name: String,
    /// Optional override returning a synthetic
    /// `ServiceSpecificError(code)` from `addConnection()` — exercises
    /// the B.6 decode + reject path.
    add_connection_error: Option<i32>,
    /// Bumped on every `addConnection()` so a test can assert the
    /// bridge made exactly one connection attempt (or none, for a
    /// pre-empted instance-name rejection).
    addconnection_calls: Arc<AtomicU32>,
    /// Set `O_NONBLOCK` on the fd before returning it. Mirrors AOSP
    /// `singleSocketConnection` (frameworks/native/libs/binder/
    /// RpcSession.cpp:614, android-16.0.0_r4), which always creates
    /// its preconnected socket with `SOCK_NONBLOCK`. The bridge under
    /// test must clear that flag in `from_preconnected_fd` so the
    /// blocking RPC machinery doesn't trip over EAGAIN mid-handshake.
    nonblocking: bool,
}

impl Interface for MockAccessor {}
impl IAccessor for MockAccessor {
    fn r#addConnection(&self) -> rsbinder::status::Result<ParcelFileDescriptor> {
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

    fn r#getInstanceName(&self) -> rsbinder::status::Result<String> {
        Ok(self.name.clone())
    }
}

fn make_mock_accessor(mock: MockAccessor) -> SIBinder {
    let strong: Strong<dyn IAccessor> = BnAccessor::new_binder(mock);
    Interface::as_binder(&*strong)
}

// ---- harness helpers ------------------------------------------------

fn tmp_sock(tag: &str) -> PathBuf {
    // Per-test, per-PID, per-time: parallel test binaries never collide.
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsbinder-accessor-{tag}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
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

/// Spin up an `RpcServer` (android-13+ wire, max=2 ⇒ android-16 v2)
/// serving an `EchoSvc` root, return (socket path, call counter, drop
/// guard that shuts the server cleanly).
struct EchoServerGuard {
    path: PathBuf,
    calls: Arc<AtomicI64>,
    server: Arc<RpcServer>,
    bg: Option<thread::JoinHandle<()>>,
}

impl EchoServerGuard {
    fn start(tag: &str) -> Self {
        let path = tmp_sock(tag);
        let calls = Arc::new(AtomicI64::new(0));
        let server = RpcServer::setup_unix_server(&path).expect("bind");
        server.set_android13plus(2); // android-16 v2 ceiling
        server.set_max_threads(2);
        server.set_root(make_echo(calls.clone()));
        let bg = server.run_background();
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
        self.server.shutdown();
        if let Some(bg) = self.bg.take() {
            let _ = bg.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Tiny client-side typed wrapper around the RPC root SIBinder
/// returned by the bridge. Echos a string via TX_ECHO.
fn rpc_echo(root: &SIBinder, s: &str) -> Result<String> {
    let rp = (**root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .ok_or(StatusCode::BadType)?;
    let mut data = rp.build_request(ECHO_DESC)?;
    data.write(&s.to_owned())?;
    let mut reply = rp
        .transact(TX_ECHO, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = reply.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    reply.read::<String>()
}

// ---- AC-13.1 / 13.2: happy path -------------------------------------

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

    // AC-13.2 contract: a `Service::Accessor` arm resolves to
    // `ServiceWithMetadata { service: Some(rpc_root), isLazyService: false }`.
    let swm = resolve_accessor("test.echo", accessor).expect("bridge resolves");
    assert!(!swm.r#isLazyService);
    let root = swm.r#service.expect("bridge yields an RPC root binder");

    // Exactly one `addConnection()` call (single-connection model).
    assert_eq!(addconn_calls.load(Ordering::SeqCst), 1);

    // Real RPC echo through the bridged proxy.
    assert_eq!(rpc_echo(&root, "hello").unwrap(), "hello");
    assert_eq!(rpc_echo(&root, "").unwrap(), "");
    for i in 0..20 {
        assert_eq!(rpc_echo(&root, &format!("n{i}")).unwrap(), format!("n{i}"));
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 22);

    // Drop the user-visible root: the wrapper's `Drop` must release the
    // inner proxy first (best-effort DEC_STRONG), then the session
    // (peer-side serve loop exits on PeerClosed). No leak / no panic.
    drop(root);
}

// ---- AC-13.3 (mutant 1): instance-name mismatch reject --------------

#[test]
fn accessor_instance_name_mismatch_rejects() {
    let server = EchoServerGuard::start("mismatch");
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        // Accessor lies about its name; bridge must reject before
        // calling `addConnection()`.
        name: "evil.imposter".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });

    assert!(
        resolve_accessor("test.echo", accessor).is_none(),
        "instance-name mismatch must reject"
    );
    // AOSP `validateAccessor` rejects *before* the fd is allocated;
    // mirror that to keep a misbehaving Accessor from getting a free
    // socket out of us.
    assert_eq!(
        addconn_calls.load(Ordering::SeqCst),
        0,
        "instance-name reject must short-circuit before addConnection()"
    );
}

// ---- AC-13.3 (mutant 2): addConnection ServiceSpecificError decode --

#[test]
fn accessor_add_connection_service_specific_error_rejects_and_logs() {
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
        assert!(
            resolve_accessor("test.echo", accessor).is_none(),
            "ERROR={code} must reject"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Symbolic name lookup must round-trip — the deterministic
        // gate for B.6 (the log line is what an operator sees).
        let name = accessor_error_name(code);
        assert!(
            name.starts_with("ERROR_"),
            "unknown symbol for {code}: {name}"
        );
    }
}

// ---- AC-13.3 + A.5: session lifetime — root stays usable, then dies --

#[test]
fn accessor_root_keeps_session_alive_then_terminates_on_drop() {
    let server = EchoServerGuard::start("lifetime");
    let addconn_calls = Arc::new(AtomicU32::new(0));
    let accessor = make_mock_accessor(MockAccessor {
        server_path: server.path.clone(),
        name: "test.echo".to_string(),
        add_connection_error: None,
        addconnection_calls: addconn_calls.clone(),
        nonblocking: false,
    });
    let swm = resolve_accessor("test.echo", accessor).expect("bridge");
    let root = swm.r#service.expect("root");
    // The caller never sees an `RpcSession` directly — the wrapper
    // holds it. A transact through the proxy must succeed without
    // `DeadObject`.
    assert_eq!(rpc_echo(&root, "alive").unwrap(), "alive");

    // Cloning the SIBinder is allowed (cheap Arc clone); the clone
    // must also stay alive.
    let clone = root.clone();
    assert_eq!(rpc_echo(&clone, "still-alive").unwrap(), "still-alive");
    drop(root);
    // Dropping the original keeps the clone (and thus the session)
    // alive; AOSP `setSessionSpecificRoot` has the same shape.
    assert_eq!(rpc_echo(&clone, "after-drop").unwrap(), "after-drop");
    drop(clone);
    // After the last reference drops, the session shuts down — but
    // that's an internal detail. The test merely asserts no panic.
}

// ---- D.8 STAGE3 regression gate: non-blocking preconnected fd -------

/// AOSP `singleSocketConnection` (frameworks/native/libs/binder/
/// RpcSession.cpp:614, android-16.0.0_r4) opens its preconnected
/// socket with `SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK`, so every
/// fd `IAccessor::addConnection()` returns is **non-blocking**.
/// rsbinder RPC I/O is blocking by construction (the handshake reads
/// the `RpcNewSessionResponse` via `read_exact_raw` which assumes
/// `read` blocks); without explicit `O_NONBLOCK` clear in
/// `from_preconnected_fd`, the handshake's first `read()` returns
/// `EAGAIN` and tears the connection down — which D.8 STAGE3 caught
/// against the real android-16 emulator. This test is the hermetic
/// regression gate so a future refactor that removes the
/// `O_NONBLOCK` clear in [`RpcSession::from_preconnected_fd`] fails
/// here, before reaching the live emulator.
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
    let swm = resolve_accessor("test.echo", accessor)
        .expect("bridge must clear O_NONBLOCK before the v2 handshake");
    let root = swm.r#service.expect("RPC root");
    assert_eq!(addconn_calls.load(Ordering::SeqCst), 1);

    // Full transact must succeed — the handshake completed only because
    // the bridge cleared the inherited `O_NONBLOCK` flag.
    assert_eq!(rpc_echo(&root, "hello-nonblock").unwrap(), "hello-nonblock");
    for i in 0..10 {
        assert_eq!(rpc_echo(&root, &format!("n{i}")).unwrap(), format!("n{i}"));
    }
}

// ---- Subplan 2-14 A.5 e2e: process-local AccessorProvider fallback ----

use rsbinder::hub::android_16::{
    add_accessor_provider, create_accessor, resolve_via_process_local, AccessorAddrProvider,
    AccessorProviderFn, AccessorSockAddr,
};
use std::collections::HashSet;

/// 2-14 A.5: end-to-end from `add_accessor_provider` registration
/// to `resolve_via_process_local` returning an `RpcSession` root that
/// echoes. Drives the full A.4 + A.5 chain on top of A0.1–A0.3:
///
/// 1. Spin up an `RpcServer` (background) serving `EchoSvc` as root.
/// 2. Register a `LocalAccessor` (via `create_accessor`) under a
///    unique instance name through `add_accessor_provider`.
/// 3. Call `resolve_via_process_local(name)` — A.5's public entrypoint
///    (`getInjectedAccessor` + `Service::accessor → toBinder` AOSP
///    combined). Asserts:
///    * lookup hits the registered provider (the registry is global,
///      but the instance name is process+line-scoped so no
///      cross-test collision),
///    * 2-13 `resolve_accessor` runs cleanly against the live RPC
///      server: instance-name validation passes, `addConnection`
///      yields a connected fd, `from_preconnected_fd` handshakes v2,
///      `get_root()` returns the echo binder,
///    * a full `TX_ECHO` round-trip succeeds (so the registered
///      provider's `AccessorAddrProvider` closure was actually called
///      with the right path).
///
/// Mutant gate: removing the `or_else(|| try_process_local_fallback)`
/// from `hub::servicemanager_16::get_service` (the *consume*-side
/// wire-up) wouldn't break this test, because the test calls
/// `resolve_via_process_local` directly — that wire-up is exercised
/// by [`process_local_fallback_takes_priority_when_accessor_arm_is_none`]
/// below.
#[test]
fn process_local_provider_resolves_root_via_resolve_helper() {
    let server = EchoServerGuard::start("a5-helper");

    // Unique instance name per test (process-id + line for cross-test
    // safety; the registry is process-wide static).
    let instance = format!("rsb.test.a5.helper.{}.{}", std::process::id(), line!());

    // `LocalAccessor` whose `addr_provider` always hands back the
    // RpcServer's listening UDS path. Registered under `instance` via
    // the A.4 process-local registry.
    let server_path = server.path.clone();
    let provider: AccessorProviderFn = {
        let want = instance.clone();
        Box::new(move |name: &str| {
            if name == want {
                let addr_provider: AccessorAddrProvider = Box::new({
                    let p = server_path.clone();
                    move |_| Ok(AccessorSockAddr::Unix(p.clone()))
                });
                Some(create_accessor(name, addr_provider))
            } else {
                None
            }
        })
    };
    let provider_handle =
        add_accessor_provider(HashSet::from([instance.clone()]), provider).expect("registry add");

    // A.5 public entrypoint — combined `getInjectedAccessor` +
    // `toBinder` path.
    let swm = resolve_via_process_local(&instance).expect("process-local fallback yields root SWM");
    assert!(!swm.r#isLazyService, "RPC root is never a LazyService");
    let root = swm.r#service.expect("RPC root binder");

    // Full transact round-trip: the registered provider's
    // `addr_provider` closure was hit, `LocalAccessor::addConnection`
    // connected to the RpcServer's listener, and the resulting RPC
    // root services TX_ECHO.
    assert_eq!(
        rpc_echo(&root, "a5-hello").unwrap(),
        "a5-hello",
        "round-trip via process-local provider must echo"
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    drop(provider_handle);
}

/// 2-14 A.5 primitive gate: `resolve_via_process_local` keys lookups
/// **strictly** by instance name — unregistered names yield `None`
/// (not a phantom binder), and a sibling registration must not leak
/// into unrelated names. This is the contract the
/// `hub::servicemanager_16::dispatch_typed_service` fallback (Phase
/// 2-14 A.5) relies on; the dispatcher's own routing is exercised by
/// the unit tests in [`hub::servicemanager_16::tests`].
///
/// **Mutant gate**: a provider that returns `Some(_)` for every name
/// (or a registry that ignores its instance set) would resurrect the
/// `unknown` lookup ⇒ the `is_none()` assertions below fail.
#[test]
fn resolve_via_process_local_keys_strictly_by_instance_name() {
    let server = EchoServerGuard::start("a5-fallback");
    let unknown = format!(
        "rsb.test.a5.unregistered.{}.{}",
        std::process::id(),
        line!()
    );
    // Sanity: an instance no provider ever claimed yields `None` (not
    // a phantom binder), so the fallback path in `get_service` is
    // genuinely None when the registry is empty for `name`.
    assert!(
        resolve_via_process_local(&unknown).is_none(),
        "unregistered name must not yield a phantom binder"
    );

    // Now register a provider for a *different* instance and verify
    // the fallback dispatches *only* to the registered name — proving
    // the registry's `instance` keying is honored end-to-end (an
    // overly-eager provider that returns `Some(_)` for every name
    // would silently shadow legitimate misses).
    let target = format!("rsb.test.a5.targeted.{}.{}", std::process::id(), line!());
    let server_path = server.path.clone();
    let provider: AccessorProviderFn = {
        let want = target.clone();
        Box::new(move |name: &str| {
            if name == want {
                let addr_provider: AccessorAddrProvider = Box::new({
                    let p = server_path.clone();
                    move |_| Ok(AccessorSockAddr::Unix(p.clone()))
                });
                Some(create_accessor(name, addr_provider))
            } else {
                None
            }
        })
    };
    // RAII handle — name-prefixed (not `_h`) so a future refactor that
    // accidentally rebinds to `_` (immediate drop = unregister) is
    // visible at the patch site instead of silently breaking the
    // assertions below.
    let provider_handle =
        add_accessor_provider(HashSet::from([target.clone()]), provider).expect("registry add");

    // Unregistered name still None after registration of a sibling.
    assert!(
        resolve_via_process_local(&unknown).is_none(),
        "registered sibling must not leak into unrelated names"
    );
    // Target name resolves.
    assert!(
        resolve_via_process_local(&target).is_some(),
        "registered name must resolve"
    );
    drop(provider_handle);
}

// ---- AC-13.3 sanity: accessor_error_name surface lock ---------------

#[test]
fn accessor_error_name_unknown_codes_safe() {
    // The bridge `service_specific_error()` returns `0` for any
    // non-ServiceSpecific Status; a future regression that *adds* an
    // `ERROR_*=0` collision would silently mask other failures, so
    // lock the contract: `0` maps to `ERROR_CONNECTION_INFO_NOT_FOUND`
    // (AIDL constant value), all other unknowns to `"unknown"`.
    assert_eq!(accessor_error_name(0), "ERROR_CONNECTION_INFO_NOT_FOUND");
    assert_eq!(accessor_error_name(42), "unknown");
    assert_eq!(accessor_error_name(-1), "unknown");
    // Ensure the re-export path through `hub::android_16` matches the
    // module-local one (B.6 surface lock).
    assert_eq!(
        a16::accessor_error_name(0),
        "ERROR_CONNECTION_INFO_NOT_FOUND"
    );
}
