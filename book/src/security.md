# Security & Authorization

A service handler often needs to answer "**who** is calling, and are they
allowed to?" rsbinder gives you the caller's identity and several ways to
authorize — but the right tool **differs by transport**, and that
difference is deliberately made explicit rather than hidden.

> **Core principle.** Kernel binder and RPC have genuinely different trust
> boundaries. Kernel binder gives you a kernel-vouched uid/pid and SELinux
> context; RPC's trust boundary is the transport itself (Unix peer
> credentials, a TLS certificate, or hypervisor VM isolation). rsbinder
> never papers over this: an authorization check that is safe on kernel
> binder either keeps working, fails closed, or is denied over RPC — never
> silently weakened.

## Caller identity inside a handler

The calling context is **ambient thread-local state**, set by the dispatch
machinery just before your handler runs and cleared just after (this
mirrors AOSP's `IPCThreadState`). So inside a `transact` handler you read
it with no parameter threading:

```rust
use rsbinder::{Caller, ExceptionCode, Status};

impl IExample for MyService {
    fn do_thing(&self, arg: i32) -> rsbinder::status::Result<()> {
        let uid = rsbinder::get_calling_uid();   // who is calling, right now
        // ... authorize, then act ...
        Ok(())
    }
}
```

The accessors (all `rsbinder::…`):

| Accessor | Returns | Works over |
|---|---|---|
| `get_calling_uid()` | caller effective uid | kernel binder, **Unix RPC** |
| `get_calling_pid()` | caller pid | kernel binder, **Unix RPC** |
| `get_calling_sid()` | SELinux context (`Option<CString>`) | kernel binder only (RPC → `None`) |
| `is_handling_transaction()` | `bool` | kernel binder, RPC |
| `calling_caller()` | `Option<Caller>` (transport-tagged) | kernel binder, RPC |

These are only meaningful **inside a handler, on the dispatching thread**.
Outside a transaction they return `0` / `None`. If you spawn a new thread
inside a handler, that thread has no calling context (it reads `0`) — just
like AOSP.

### Fail-closed values, never fabricated ones

`get_calling_uid()` returns a real uid only where the transport carries
one — kernel binder, and Unix-domain RPC (`SO_PEERCRED` on Linux/Android,
`getpeereid` on macOS/BSD). Over a transport with **no** uid (vsock, TLS
certificate, anonymous) it returns the **sentinel `u32::MAX`**, which is
never `0` (root) and never a real privileged uid:

```rust
// Over vsock / TLS / anonymous, get_calling_uid() == u32::MAX:
if rsbinder::get_calling_uid() == 0 {      // u32::MAX != 0  ⇒ never matches
    // ... "allow root" ...                // so a uid-less peer can't slip through
}
if rsbinder::get_calling_uid() != AID_SYSTEM {
    return Err(Status::from(ExceptionCode::Security));  // uid-less peer ⇒ denied
}
```

This is intentional: fabricating a uid (say `0`) for a transport that
doesn't have one would be a privilege-escalation hole. **uid ACLs simply
fail closed over uid-less transports** — use a transport-appropriate check
there instead (see below).

## The transport-tagged caller

For anything beyond a plain uid check, match on
[`Caller`](https://docs.rs/rsbinder/latest/rsbinder/enum.Caller.html), which
tags the identity by transport so you authorize **explicitly**:

```rust
use rsbinder::{Caller, ExceptionCode, Status};
use rsbinder::rpc::PeerIdentity;

fn authorize() -> rsbinder::status::Result<()> {
    match rsbinder::calling_caller() {
        // Kernel binder: Android permission, uid, or SELinux `sid`.
        Some(Caller::Kernel { uid, .. }) if uid == 1000 => Ok(()),
        // Unix RPC: kernel-vouched peer uid.
        Some(Caller::Rpc(PeerIdentity::Local { uid, .. })) if uid == 1000 => Ok(()),
        // TLS RPC: certificate subject allowlist.
        Some(Caller::Rpc(PeerIdentity::Certificate(cert)))
            if cert.subject() == "CN=trusted-client" => Ok(()),
        // vsock cid, anonymous, no transaction, or a future variant:
        // fail closed.
        _ => Err(Status::from(ExceptionCode::Security)),
    }
}
```

`Caller` and `PeerIdentity` are `#[non_exhaustive]`, so the compiler forces
a catch-all arm — which nudges you toward a fail-closed default.

## Four ways to authorize

| When | Use |
|---|---|
| uid check, common to kernel + Unix RPC | `get_calling_uid()` |
| different rule per transport (uid / cert / cid / sid) | `match calling_caller()` |
| Android permission on a kernel service | `@EnforcePermission(...)` (declarative) |
| one policy across many methods / introducing RPC authz | `PermissionAuthority` (injected) |

### `@EnforcePermission` (declarative, kernel-only)

Annotate the AIDL method and the generated code checks Android's
`PermissionManagerService` before your handler runs — no authorization
code in the body:

```aidl
interface IExample {
    @EnforcePermission("android.permission.INTERNET")
    void doNetworking();
}
```

**Over RPC this method is denied** (returns `EX_SECURITY`), unconditionally
and regardless of process shape. Android permissions are a kernel-binder
concept; the RPC dispatch path has no PMS-backed uid, so granting would
mean granting *root* to any anonymous peer. rsbinder fails closed instead.
For RPC authorization use the transport-native means above.

### `PermissionAuthority` (injected, cross-transport policy)

To back every `@EnforcePermission` check with one centralized,
transport-aware policy — for example to *introduce* authorization over RPC
that the default denies — install a
[`PermissionAuthority`](https://docs.rs/rsbinder/latest/rsbinder/permission_controller/trait.PermissionAuthority.html)
at startup:

```rust
use rsbinder::Caller;
use rsbinder::permission_controller::{PermissionAuthority, set_permission_authority};
use rsbinder::rpc::PeerIdentity;
use std::sync::Arc;

struct MyPolicy;
impl PermissionAuthority for MyPolicy {
    fn check(&self, permission: &str, caller: &Caller) -> bool {
        match caller {
            Caller::Rpc(PeerIdentity::Local { uid, .. }) => *uid == 1000,
            Caller::Rpc(PeerIdentity::Certificate(c)) => c.subject() == "CN=admin",
            _ => false,   // fail closed
        }
    }
}

set_permission_authority(Arc::new(MyPolicy));
```

When installed, the authority **owns the whole decision** for every
generated `check_permission` call, on every transport, receiving the
transport-tagged `Caller`. With **no** authority installed, the default is
unchanged: kernel → PMS, RPC → deny.

> The core crate ships only this *slot*, never a policy. Token/JWT formats,
> certificate→permission tables, and uid→permission maps are deployment
> concerns. **Caveat:** an installed authority also replaces the kernel PMS
> path — if you want "kernel = PMS, RPC = custom", handle the
> `Caller::Kernel` arm explicitly (most deployments that inject an authority
> are pure-RPC, where that arm never fires).

### Connection-level authorization (RPC)

For RPC, the most natural granularity is often the **whole connection**,
decided at handshake before any transaction — this is the right tool for
vsock and TLS, which carry no per-call uid. Use
[`RpcServer::set_authorizer`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_authorizer):

```rust
server.set_authorizer(|peer| match peer {
    rsbinder::rpc::PeerIdentity::Certificate(cert) => cert.subject() == "CN=trusted",
    rsbinder::rpc::PeerIdentity::Vsock { cid } => *cid == TRUSTED_VM_CID,
    _ => false,
});
```

A rejected peer's socket is closed before any RPC byte is exchanged.

## Worked example

`example-hello` ships a runnable handler-authorization demo —
`bin/authz_service.rs` / `bin/authz_client.rs`:

```text
cargo run -p example-hello --features rpc --bin authz_service
cargo run -p example-hello --features rpc --bin authz_client
# whoami    -> OK: unix-rpc caller uid=1000 pid=12345
# adminOnly -> DENIED (Security)
```

`whoami()` authorizes any identifiable local caller and reports it;
`adminOnly()` requires uid 0 and denies a normal user — demonstrating both
the allow and the fail-closed deny path.

## Summary

- Read the caller with `get_calling_uid/pid/sid` and `calling_caller()` —
  ambient thread-local state, valid only inside a handler.
- uid ACLs work on kernel binder and Unix RPC, and **fail closed**
  (`u32::MAX` sentinel) on uid-less transports.
- `@EnforcePermission` is kernel-only and **denies over RPC**.
- Use `match calling_caller()` for transport-aware rules, `set_authorizer`
  for connection-level RPC policy, and `PermissionAuthority` to centralize.
- The default everywhere is fail-closed; weakening a boundary is always an
  explicit, opt-in choice.
