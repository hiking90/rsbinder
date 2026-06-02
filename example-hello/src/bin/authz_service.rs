// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 — **handler-side authorization** over RPC.
//!
//! The service `impl` authorizes each call by inspecting the
//! transport-tagged caller ([`rsbinder::calling_caller`]) and denies with
//! `EX_SECURITY`. This is the explicit, transport-aware approach: the
//! kernel and RPC trust boundaries differ, so the handler `match`es on the
//! [`Caller`] arm and applies the right rule. Run with `authz_client`:
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin authz_service
//! cargo run -p example-hello --features rpc --bin authz_client
//! ```
//!
//! Over a Unix-domain socket the peer uid is the connecting process's
//! (kernel-vouched via `SO_PEERCRED`), so `whoami()` is authorized and
//! reports it, while `adminOnly()` (requires uid 0) is denied for a normal
//! user — demonstrating both the allow and deny paths.
//!
//! ## Other ways to authorize (not shown here)
//!
//! - **Declarative, kernel-only:** annotate the AIDL method with
//!   `@EnforcePermission("android.permission.X")` — generated code checks
//!   `PermissionManagerService`; over RPC it auto-denies (Plan 2-16 Phase A).
//! - **Centralized policy:** install a
//!   [`rsbinder::permission_controller::PermissionAuthority`] via
//!   `set_permission_authority` to back every `@EnforcePermission` check
//!   with one injected, transport-aware policy.
//! - **Connection-level (RPC):** `RpcServer::set_authorizer(|peer| …)`
//!   admits/refuses a whole connection at handshake — the right
//!   granularity for vsock/TLS, which carry no uid.

use env_logger::Env;
use example_hello::authz::*;
use rsbinder::rpc::PeerIdentity;
use rsbinder::service::{rpc, Registry as _};
use rsbinder::{Caller, ExceptionCode, Interface, Status};

struct AuthzService;
impl Interface for AuthzService {}

impl IAuthz for AuthzService {
    fn whoami(&self) -> rsbinder::status::Result<String> {
        // Authorize: only an *identifiable local* caller is allowed. A
        // uid-less RPC transport (vsock / TLS cert / anonymous) or no
        // in-flight transaction falls through to a fail-closed deny.
        match rsbinder::calling_caller() {
            Some(Caller::Kernel { uid, pid, .. }) => {
                Ok(format!("kernel caller uid={uid} pid={pid}"))
            }
            Some(Caller::Rpc(PeerIdentity::Local { uid, pid })) => {
                Ok(format!("unix-rpc caller uid={uid} pid={pid}"))
            }
            other => {
                // `Caller`/`PeerIdentity` are `#[non_exhaustive]`: vsock,
                // certificate, anonymous, and "no caller" all land here.
                eprintln!("authz_service: denying unidentifiable caller: {other:?}");
                Err(Status::from(ExceptionCode::Security))
            }
        }
    }

    fn adminOnly(&self) -> rsbinder::status::Result<String> {
        // `get_calling_uid()` is the kernel sender uid or the Unix-RPC peer
        // uid; a uid-less transport returns the `u32::MAX` sentinel (never
        // 0), so this comparison fail-closes there too — no root bypass.
        if rsbinder::get_calling_uid() == 0 {
            Ok("admin action performed".to_string())
        } else {
            Err(Status::from(ExceptionCode::Security))
        }
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // The same `AuthzService` impl would also work over kernel binder
    // (`kernel::Host` + `add_service`); served here over RPC so the
    // transport-aware authorization is demonstrable without `/dev/binder`.
    let _ = std::fs::remove_file(RPC_SOCKET);
    let host = rpc::Host::unix(RPC_SOCKET)?;
    host.add_service(SERVICE_NAME, BnAuthz::new_binder(AuthzService).as_binder())?;

    println!("authz_service listening on {RPC_SOCKET}");
    host.serve()?;
    Ok(())
}
