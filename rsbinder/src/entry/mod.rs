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
/// Called while handling a transaction
/// ([`is_handling_transaction`](crate::is_handling_transaction)), on either
/// endpoint, the whole connect runs **inline** on the handler's thread, as
/// every async API of this crate does there (see
/// [`BinderAsyncPool`](crate::BinderAsyncPool)'s *Which thread a transact
/// runs on*), and needs no Tokio runtime. The first poll then blocks that
/// thread until the name is registered (kernel, the inline
/// `wait_for_interface_async` described in its *From inside a transaction*)
/// or the lookup ends (RPC). Dropping the future cannot end that wait, and
/// a `timeout` around it never fires. Look services up before serving, not
/// while serving.
#[cfg(feature = "tokio")]
pub fn connect_async<T: FromIBinder + ?Sized + 'static>(
    uri: impl Into<Uri>,
) -> impl std::future::Future<Output = Result<Strong<T>>> + 'static {
    let uri = uri.into();
    async move {
        if uri.endpoint.is_kernel() {
            let (uri, name) = split_service(uri, "rsbinder::connect_async")?;
            // Initializes `ProcessState` as `connect` does; opens the driver, never waits.
            client::new_client(uri, ClientOptions::default())?;
            return crate::wait_for_interface_async::<T>(&name).await;
        }
        crate::rt::blocking_op(move || connect::<T>(uri)).await
    }
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
}
