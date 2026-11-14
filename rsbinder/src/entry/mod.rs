// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One entry point for every transport: [`serve`] a service and
//! [`connect`] to one at a [`Uri`], whether the transport is kernel binder
//! or binder-over-socket RPC (Unix, abstract Unix, vsock, TLS). The
//! AIDL-generated stubs, service `impl`s and call sites are already
//! transport-agnostic; this module makes the bootstrap (publish / look up)
//! transport-agnostic too.
//!
//! ```no_run
//! # use rsbinder::*;
//! # fn demo(service: SIBinder) -> Result<()> {
//! // server — transport chosen by the `Uri` only
//! rsbinder::serve(Uri::kernel())?          // or Endpoint::unix("/tmp/hello.sock")
//!     .add("hello", service)?
//!     .run()?;
//! # Ok(()) }
//! # fn client() -> Result<SIBinder> {
//! // client
//! rsbinder::connect_binder(Uri::kernel().with_service("hello"))
//! //  rsbinder::connect::<dyn IHello>("unix:///tmp/hello.sock#hello".parse::<Uri>()?)
//! # }
//! ```
//!
//! A [`Uri`] also has a string form (`binder://hello`,
//! `unix:///tmp/hello.sock#hello`) for configuration files and command
//! lines; see [`uri`] for the grammar.
//!
//! The low-level types ([`crate::ProcessState`], [`crate::hub`],
//! `crate::rpc::{RpcServer, RpcSession}`) remain the direct path; this is
//! a thin layer over them with no wire change.
//!
//! # What the `Uri` does *not* hide
//!
//! | | kernel binder | RPC endpoints |
//! |---|---|---|
//! | `@EnforcePermission` | enforced via the system permission service | **denied** (no caller uid model) — authorize with `ServeOptions::authorizer` |
//! | [`crate::get_calling_uid`] | kernel-vouched | kernel-vouched on Unix sockets, fail-closed sentinel elsewhere |
//! | death | process death | session disconnect |
//! | `list_services`, notifications, lazy services | via [`crate::hub`] | not available |
//! | an option that does not apply, or that the process cannot honor | [`crate::StatusCode::BadValue`] at `serve`/`run`/`open`, logged |

pub mod uri;

mod client;
mod server;

pub(crate) use client::{open_staged, OpenStage};
pub use client::{Client, ClientOptions};
#[cfg(feature = "rpc")]
pub use server::Authorizer;
pub use server::{ServeOptions, Server, ServerGuard};
pub use uri::{Endpoint, KernelEndpoint, Uri, UriError, UriErrorKind, WireProfile, WireVersion};

use crate::error::Result;
use crate::{FromIBinder, SIBinder, Strong};

/// Start assembling a server on `uri`. Its service name, if any, is
/// ignored, so a server and its clients can share one value (an empty one
/// is [`StatusCode::BadValue`](crate::StatusCode::BadValue), as at
/// [`connect`]). Add services
/// with [`Server::add`], then [`Server::run`] or [`Server::spawn`].
///
/// On kernel binder this also initializes the process-wide
/// [`ProcessState`](crate::ProcessState), so a [`KernelEndpoint`] driver
/// or mapping size that disagrees with one already initialized is
/// [`StatusCode::BadValue`](crate::StatusCode::BadValue) here rather than
/// at [`Server::run`]. A thread count is not fixed at init: one that
/// differs from the ceiling in force is applied by [`Server::run`] /
/// [`Server::spawn`] ([`ServeOptions::threads`]). See [`Server`].
pub fn serve(uri: impl Into<Uri>) -> Result<Server> {
    server::new_server(uri.into())
}

/// Resolve the service named by [`Uri::service`] and cast it to `T`.
/// Kernel: waits for registration; RPC: opens a session and looks the name
/// up. The returned proxy keeps its RPC session alive — nothing else needs
/// to be held.
pub fn connect<T: FromIBinder + ?Sized>(uri: impl Into<Uri>) -> Result<Strong<T>> {
    connect_binder(uri)?.into_interface_checked()
}

/// [`connect`] without the interface cast.
pub fn connect_binder(uri: impl Into<Uri>) -> Result<SIBinder> {
    let (uri, name) = split_service(uri.into(), "rsbinder::connect")?;
    client::new_client(uri, ClientOptions::default())?.binder(&name)
}

/// Async [`connect`] for the Tokio runtime. `T` may be a generated
/// `dyn IFooAsync<Tokio>`.
///
/// `uri` is converted when this is called, so the future does not borrow
/// it and can be handed to `tokio::spawn`.
///
/// Kernel: the wait for registration is
/// [`wait_for_interface_async`](crate::wait_for_interface_async), so
/// dropping this future — under `tokio::time::timeout` or `select!` —
/// ends the wait instead of leaving a blocking-pool thread parked until
/// the service appears. RPC: the connect runs on `spawn_blocking`; it does
/// not wait for a name to appear, only for the connection and the lookup.
///
/// # A silent RPC peer
///
/// With the default [`ClientOptions`], connecting has no deadline, so a
/// peer that accepts the socket and then never answers the handshake or the
/// lookup holds the pool thread for good. A `timeout` around this future
/// returns to the caller but cannot release that thread, which blocks in a
/// socket read, and `Runtime::drop`, which waits for the blocking pool,
/// then never returns. Use [`connect_async_with`] and set
/// [`ClientOptions::timeout`] whenever the peer is not trusted to answer.
///
/// # From inside a transaction
///
/// Called while handling a transaction
/// ([`is_handling_transaction`](crate::is_handling_transaction)), on either
/// endpoint, the whole connect runs **inline** on the handler's thread
/// before this returns, as every async API of this crate does there (see
/// [`BinderAsyncPool`](crate::BinderAsyncPool)'s *Which thread a transact
/// runs on*), and needs no Tokio runtime. The call blocks that thread until
/// the name is registered (kernel, the inline `wait_for_interface_async`
/// described in its *From inside a transaction*) or the lookup ends (RPC).
/// Dropping the future cannot end that wait, and a `timeout` around it
/// never fires. Called anywhere else, the first poll decides the same way
/// for the thread that polls. Look services up before serving, not while
/// serving.
#[cfg(feature = "tokio")]
pub fn connect_async<T: FromIBinder + ?Sized + 'static>(
    uri: impl Into<Uri>,
) -> impl std::future::Future<Output = Result<Strong<T>>> + Send + 'static {
    connect_async_with(uri, |_, _| {})
}

/// [`connect_async`] with [`ClientOptions`], as [`Client::open_with`] is to
/// [`Client::open`]; the closure also sees the [`Endpoint`]. The usual
/// option is [`ClientOptions::timeout`], which bounds each step of an RPC
/// connect, so a `timeout` around this future no longer leaves a pool
/// thread behind (see [`connect_async`]'s *A silent RPC peer*).
#[cfg(feature = "tokio")]
pub fn connect_async_with<T: FromIBinder + ?Sized + 'static>(
    uri: impl Into<Uri>,
    f: impl FnOnce(&mut ClientOptions, &Endpoint),
) -> impl std::future::Future<Output = Result<Strong<T>>> + Send + 'static {
    let uri = uri.into();
    let mut o = ClientOptions::default();
    f(&mut o, &uri.endpoint);
    if uri.endpoint.is_kernel() {
        // Initializes `ProcessState` as `connect` does; opens the driver, never waits.
        let wait = split_service(uri, "rsbinder::connect_async").and_then(|(uri, name)| {
            client::new_client(uri, o)?;
            Ok(crate::wait_for_interface_async::<T>(&name))
        });
        let wait: crate::BoxFuture<'static, Result<Strong<T>>> = match wait {
            Ok(wait) => Box::pin(wait),
            Err(e) => Box::pin(std::future::ready(Err(e))),
        };
        return wait;
    }
    crate::rt::blocking_op(move || {
        let (uri, name) = split_service(uri, "rsbinder::connect_async")?;
        client::new_client(uri, o)?
            .binder(&name)?
            .into_interface_checked()
    })
}

/// Validate first: an empty name taken out here would skip the check in `new_client`.
fn split_service(mut uri: Uri, who: &str) -> Result<(Uri, String)> {
    uri.validate()?;
    let Some(name) = uri.service.take() else {
        log::error!(
            "{who}: no service in {uri} (use `Uri::with_service`, `#name` or `binder://name`)"
        );
        return Err(crate::StatusCode::BadValue);
    };
    Ok((uri, name))
}

#[cfg(all(test, feature = "tokio", feature = "rpc"))]
mod tests {
    use super::*;
    use crate::{Interface, StatusCode};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    struct Absent;

    impl Interface for Absent {}

    impl FromIBinder for Absent {
        fn interface_descriptor() -> &'static str {
            "rsbinder.test.Absent"
        }
        fn try_from(_: SIBinder) -> Result<Strong<Self>> {
            Err(StatusCode::BadType)
        }
    }

    /// In a handler, a kernel `connect_async` runs `connect` on the polling thread, no runtime.
    #[test]
    #[serial_test::serial(binder)]
    fn a_kernel_connect_async_in_a_handler_runs_inline_without_a_runtime() {
        crate::tests::in_own_process(
            "entry::tests::a_kernel_connect_async_in_a_handler_runs_inline_without_a_runtime",
            connect_async_in_a_handler,
        );
    }

    /// The test above, in its own process: `hub::default` keeps a handle-0 proxy for good.
    fn connect_async_in_a_handler() {
        // Without a driver both orders fail at `new_client` before any runtime use: no verdict.
        if client::new_client(Uri::kernel(), ClientOptions::default()).is_err() {
            eprintln!("skipped: no kernel binder driver");
            return;
        }
        let (done, watch) = mpsc::channel();
        std::thread::spawn(move || {
            let _calling = crate::thread_state::RpcCallingGuard::install(
                Arc::new(crate::rpc::transport::PeerIdentity::Local { uid: 0, pid: 7 }),
                crate::TransportCaps::SAME_HOST,
            );
            assert!(crate::is_handling_transaction());
            // The service manager's own name: a live one answers at once, an absent one fails.
            let uri = Uri::kernel().with_service("manager");
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let connect = connect_async::<Absent>(uri);
                let mut connect = std::pin::pin!(connect);
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                std::future::Future::poll(connect.as_mut(), &mut cx).is_ready()
            }));
            let _ = done.send(matches!(polled, Ok(true)));
        });
        match watch.recv_timeout(Duration::from_secs(10)) {
            Ok(inline) => assert!(
                inline,
                "the first poll needed a Tokio runtime or left the lookup to another thread"
            ),
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("the inline connect did not end"),
            Err(e) => panic!("the polling thread died: {e}"),
        }
    }

    /// A socket path nobody listens on: the connect fails at once, on whichever thread runs it.
    fn unreachable_unix() -> Uri {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rsbinder-entry-absent-{}-{n}.sock",
            std::process::id()
        ));
        Uri::from(Endpoint::unix(path)).with_service("absent")
    }

    fn poll_once<F: std::future::Future>(fut: F) -> std::task::Poll<F::Output> {
        let mut fut = std::pin::pin!(fut);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        fut.as_mut().poll(&mut cx)
    }

    /// The choice is made at the call, as a proxy's is: a future made in a handler has run there.
    #[test]
    fn an_rpc_connect_async_made_in_a_handler_runs_at_the_call() {
        let (made, take) = mpsc::channel();
        std::thread::spawn(move || {
            let _calling = crate::thread_state::RpcCallingGuard::install(
                Arc::new(crate::rpc::transport::PeerIdentity::Local { uid: 0, pid: 7 }),
                crate::TransportCaps::SAME_HOST,
            );
            let _ = made.send(connect_async::<Absent>(unreachable_unix()));
        });
        let connect = take
            .recv_timeout(Duration::from_secs(10))
            .expect("the handler thread died");
        // No runtime and no transaction here: a deferred hand-off would panic in `spawn_blocking`.
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(connect)));
        match polled {
            Ok(std::task::Poll::Ready(r)) => assert!(r.is_err(), "nothing listens there"),
            Ok(std::task::Poll::Pending) => panic!("the connect was left to the pool"),
            Err(_) => panic!("the connect was deferred to a poll off the handler"),
        }
    }

    /// A peer that accepts and never answers: `ClientOptions::timeout` ends the pool thread's wait.
    #[test]
    fn connect_async_with_a_timeout_frees_its_pool_thread_from_a_silent_peer() {
        let name = format!("rsbinder-entry-silent-{}", std::process::id());
        // Abstract where there is one: Android's SELinux refuses a socket file in a shell's dirs.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let (listener, endpoint, path) = {
            #[cfg(target_os = "android")]
            use std::os::android::net::SocketAddrExt;
            #[cfg(target_os = "linux")]
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(&name).expect("name");
            let listener = std::os::unix::net::UnixListener::bind_addr(&addr).expect("bind");
            (
                listener,
                Endpoint::unix_abstract(name),
                None::<std::path::PathBuf>,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let (listener, endpoint, path) = {
            let path = std::env::temp_dir().join(format!("{name}.sock"));
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
            (listener, Endpoint::unix(path.clone()), Some(path))
        };
        // Accepted and held, never read or written: the handshake waits on it.
        let held = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let uri = Uri::from(endpoint).with_service("silent");

        let (done, watch) = mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let r = rt.block_on(connect_async_with::<Absent>(uri, |o, _| {
                o.timeout = Some(Duration::from_millis(200));
            }));
            // `Runtime::drop` joins the blocking pool: it returns only if the wait ended.
            drop(rt);
            let _ = done.send(r.err());
        });
        let ended = watch.recv_timeout(Duration::from_secs(10));
        if let Some(path) = path {
            let _ = std::fs::remove_file(path);
        }
        drop(held.join());
        assert!(
            matches!(ended, Ok(Some(_))),
            "the connect or the runtime's shutdown waited on the silent peer: {ended:?}"
        );
    }

    /// Made elsewhere, the first poll decides for its own thread: inline in a handler.
    #[test]
    fn an_rpc_connect_async_first_polled_in_a_handler_runs_there() {
        let connect = connect_async::<Absent>(unreachable_unix());
        let (done, watch) = mpsc::channel();
        std::thread::spawn(move || {
            let _calling = crate::thread_state::RpcCallingGuard::install(
                Arc::new(crate::rpc::transport::PeerIdentity::Local { uid: 0, pid: 7 }),
                crate::TransportCaps::SAME_HOST,
            );
            let polled =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(connect)));
            let _ = done.send(matches!(polled, Ok(std::task::Poll::Ready(Err(_)))));
        });
        assert!(
            watch
                .recv_timeout(Duration::from_secs(10))
                .expect("the handler thread died"),
            "the first poll in a handler needed a runtime or left the connect to another thread"
        );
    }
}
