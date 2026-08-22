// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One entry point for every transport: [`serve`] a service and
//! [`connect`] to one with a URI, whether the transport is kernel binder
//! (`binder://`) or binder-over-socket RPC (`unix://`, `unix-abstract://`,
//! `vsock://`, `tls://`). The AIDL-generated stubs, service `impl`s and
//! call sites are already transport-agnostic; this module makes the
//! bootstrap (publish / look up) transport-agnostic too.
//!
//! ```no_run
//! # use rsbinder::*;
//! # fn demo(service: SIBinder) -> Result<()> {
//! // server — transport chosen by the URI only
//! rsbinder::serve("binder://")?          // or "unix:///tmp/hello.sock"
//!     .add("hello", service)?
//!     .run()?;
//! # Ok(()) }
//! # fn client() -> Result<SIBinder> {
//! // client
//! rsbinder::connect_binder("binder://hello")
//! //  rsbinder::connect::<dyn IHello>("unix:///tmp/hello.sock#hello")
//! # }
//! ```
//!
//! The low-level types ([`crate::ProcessState`], [`crate::hub`],
//! `crate::rpc::{RpcServer, RpcSession}`) are unchanged and remain the
//! direct path; this is a thin layer over them with no wire change.
//!
//! # What the URI does *not* hide
//!
//! | | `binder://` | RPC schemes |
//! |---|---|---|
//! | `@EnforcePermission` | enforced via the system permission service | **denied** (no caller uid model) — authorize with `ServeOptions::authorizer` |
//! | [`crate::get_calling_uid`] | kernel-vouched | kernel-vouched on `unix://`, fail-closed sentinel elsewhere |
//! | death | process death | session disconnect |
//! | `list_services`, notifications, lazy services | via [`crate::hub`] | not available |
//! | an option that does not apply | [`crate::StatusCode::BadValue`] at `run`/`open`, logged |

pub mod uri;

mod client;
mod server;

pub use client::{Client, ClientOptions};
#[cfg(feature = "rpc")]
pub use server::Authorizer;
pub use server::{ServeOptions, Server, ServerGuard};
pub use uri::Endpoint;

use crate::error::Result;
use crate::{FromIBinder, SIBinder, Strong};

/// Start assembling a server on `uri` (see [`uri`] for the grammar; a
/// `#service` fragment is ignored). Add services with [`Server::add`],
/// then [`Server::run`] or [`Server::spawn`].
pub fn serve(uri: &str) -> Result<Server> {
    server::new_server(uri::parse(uri)?)
}

/// Resolve the service named by the URI's `#fragment` (or `binder://name`)
/// and cast it to `T`. Kernel: waits for registration; RPC: opens a
/// session and looks the name up. The returned proxy keeps its RPC
/// session alive — nothing else needs to be held.
pub fn connect<T: FromIBinder + ?Sized>(uri: &str) -> Result<Strong<T>> {
    FromIBinder::try_from(connect_binder(uri)?)
}

/// [`connect`] without the interface cast.
pub fn connect_binder(uri: &str) -> Result<SIBinder> {
    let mut parsed = uri::parse(uri)?;
    let Some(name) = parsed.service.take() else {
        log::error!("rsbinder::connect: no service in {uri:?} (use `#name`, or `binder://name`)");
        return Err(crate::StatusCode::BadValue);
    };
    client::new_client(parsed, ClientOptions::default())?.binder(&name)
}

/// Async [`connect`] for the Tokio runtime: runs the blocking connect on
/// `spawn_blocking`. `T` may be a generated `dyn IFooAsync<Tokio>`.
#[cfg(feature = "tokio")]
pub async fn connect_async<T: FromIBinder + ?Sized + 'static>(uri: &str) -> Result<Strong<T>> {
    if crate::is_handling_transaction() {
        return connect::<T>(uri);
    }
    let uri = uri.to_string();
    match tokio::task::spawn_blocking(move || connect::<T>(&uri)).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) if e.is_cancelled() => Err(crate::StatusCode::FailedTransaction),
        Err(_) => Err(crate::StatusCode::Unknown),
    }
}
