# Cross-Transport Services

The AIDL interface, the generated `Bp*`/`Bn*` stubs, your service `impl`,
and the call sites are **already transport-agnostic** — the same `impl IFoo`
runs unchanged over kernel binder or RPC. What differs is only the
*bootstrap*: kernel binder uses `ProcessState` + the [`hub`](./service-manager.md)
service manager, while RPC uses `RpcServer` / `RpcSession`.

The `rsbinder::service` module is an **optional, additive** layer that makes
that bootstrap transport-agnostic too, so you can write registration and
lookup code **once** and pick the transport by construction. It is a thin
typed wrapper over the existing APIs — those remain the direct path and are
unchanged.

## The traits

```rust
use rsbinder::service::{Registry, Broker};
```

- [`Registry`](https://docs.rs/rsbinder/latest/rsbinder/service/trait.Registry.html)
  (server side) — `add_service(name, binder)`.
- [`Broker`](https://docs.rs/rsbinder/latest/rsbinder/service/trait.Broker.html)
  (client side) — `lookup(name)` plus a generic `get_interface::<T>(name)`
  convenience that does the `interface_cast`.

Only the genuine kernel∩RPC intersection is on the traits. Kernel-only
powers (`list_services`, service notifications, lazy services) stay on the
concrete kernel types / the `hub` module — they are not hidden behind the
trait, nor faked on RPC.

## Typed hosts and brokers

Each transport is a distinct type, so the differences stay visible at the
call site:

| | Server (`Registry`) | Client (`Broker`) |
|---|---|---|
| kernel binder | `service::kernel::Host` | `service::kernel::Broker` |
| RPC (`#[cfg(feature = "rpc")]`) | `service::rpc::Host` | `service::rpc::Broker` |

```rust
use rsbinder::service::{kernel, rpc, Registry, Broker};
use rsbinder::{SIBinder, Strong};

// Register once — generic over the transport.
fn register_all<R: Registry>(reg: &R, svc: SIBinder) -> rsbinder::Result<()> {
    reg.add_service("hello", svc)
}

// Look up + cast once — generic over the transport.
fn talk<B: Broker>(broker: &B) -> rsbinder::Result<Strong<dyn IHello>> {
    broker.get_interface("hello")
}
```

Picking the transport is one line:

```rust
// Server
let host = kernel::Host::new()?;            // kernel binder (process-global)
// let host = rpc::Host::unix("/tmp/x.sock")?;  // RPC over a Unix socket
register_all(&host, BnHello::new_binder(MyService).as_binder())?;
host.serve()?;                              // see "serve()" below

// Client
let broker = kernel::Broker::new()?;        // or rpc::Broker::unix("/tmp/x.sock")?
let hello = talk(&broker)?;
hello.echo("hi")?;
```

### Why typed pairs, not one `Endpoint` enum

`serve()` means different things per transport, the kernel host is a
process-global singleton while an RPC host is a real instance, and the
construction options don't overlap. Keeping them as distinct types makes a
wrong-transport option a **compile error** rather than a silent no-op, and
keeps the singleton nature of the kernel side honest.

- **`kernel::Host::new()`** is a process-global, idempotent handle over
  `ProcessState`. **`kernel::Host::serve()`** joins the **process-wide**
  binder thread pool (blocks). A second host in the same process reuses the
  existing `ProcessState`; a conflicting re-init (different driver /
  max_threads) logs a warning rather than silently dropping the request.
- **`rpc::Host::unix(path)`** binds one socket. **`rpc::Host::serve()`**
  drives that one socket (and `serve_background()` returns a `JoinHandle`).

Each host has a `builder()` for its own options — kernel: driver path,
`max_threads`, call restriction; RPC: `max_threads`, `max_connections`,
authorizer. RPC-only powers (TLS, fd modes, vsock, the connection
counters) stay reachable via `host.server()`.

## Security note

Moving a service between transports changes its trust boundary. Before
relying on the facade, read [Security & Authorization](./security.md):
`@EnforcePermission` denies over RPC, and `get_calling_uid()` is the
kernel-vouched peer uid on Unix RPC (fail-closed on uid-less transports).
The facade makes the transport swap easy; it does **not** make the security
models the same.

## Worked example

`example-hello` ships `bin/unified_service.rs` / `bin/unified_client.rs`:
identical service, registration, and call code with the transport chosen by
one `kernel::Host::new()` vs `rpc::Host::unix(path)` line.

```text
cargo run -p example-hello --features rpc --bin unified_service rpc
cargo run -p example-hello --features rpc --bin unified_client rpc
```
