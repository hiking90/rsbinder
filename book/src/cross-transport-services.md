# Cross-Transport Services

The AIDL interface, the generated `Bp*`/`Bn*` stubs, your service `impl`,
and the call sites are **already transport-agnostic** — the same `impl IFoo`
runs unchanged over kernel binder or RPC. What differs is only the
*bootstrap*: kernel binder uses `ProcessState` + the [`hub`](./service-manager.md)
service manager, while RPC uses `RpcServer` / `RpcSession`.

`rsbinder::serve` and `rsbinder::connect` make that bootstrap
transport-agnostic too: the transport is a **URI**, and everything else is
the same three calls. The low-level APIs remain the direct path and are
unchanged; this is a thin layer over them with no wire change.

## Three lines per side

```rust
use rsbinder::*;

// Server — kernel binder: ProcessState init + thread pool + service
// manager registration + join. The same code with "unix:///tmp/x.sock"
// binds a socket, publishes the service, and runs the accept loop.
rsbinder::serve("binder://")?
    .add("hello", BnHello::new_binder(MyService))?
    .run()?;

// Client — open, look up, cast. The proxy alone keeps an RPC session
// alive; there is nothing else to hold.
let hello: Strong<dyn IHello> = rsbinder::connect("binder://hello")?;
//  rsbinder::connect("unix:///tmp/x.sock#hello")?
hello.echo("hi")?;
```

## URIs

```text
binder://[<service>][?driver=<path>&threads=<n>]
unix://<abs-path>[#<service>]               (three slashes: unix:///tmp/x.sock)
unix-abstract://<name>[#<service>]          Linux/Android
vsock://<cid>:<port>[#<service>]            feature rpc-vsock
tls://<host>:<port>[#<service>]             feature rpc-tls (TCP is TLS-only)
```

- The service name is the `#fragment`; `binder://name` is a shorthand for
  `binder://#name`.
- `?profile=android13plus[-vN]` on any RPC scheme selects the AOSP
  versioned wire (default `v2`); without it the session speaks the r34 wire.
- `serve()` ignores the fragment, `connect()` requires it, and
  `Client::open()` rejects it.
- An unknown scheme or query key, an empty `#`, or `binder://a#b` is
  `StatusCode::BadValue` with a log line saying why.

## `Server`

`serve(uri)` returns a `Server`:

| | |
|---|---|
| `add(name, svc)` | publish `svc` (anything `Into<SIBinder>`, e.g. `BnFoo::new_binder(..)`). Kernel: registered with the service manager right away. RPC: queued until the server starts. |
| `with(\|o\| ..)` | set [`ServeOptions`](#options) |
| `run()` | serve and block. Kernel: start the thread pool and join it (never returns normally). RPC: the accept loop, until shut down. |
| `spawn()` | serve in the background; returns a `ServerGuard`. RPC: dropping the guard (or `ServerGuard::stop_and_join()`) ends the server — every session is closed and the threads are joined. Kernel: the guard is inert (the process thread pool has no shutdown). |

Kernel `serve("binder://")` initializes the process-wide `ProcessState`
idempotently; a second kernel server in the same process reuses it and logs
a warning if it asked for a different driver / thread count. RPC servers
bind their listener at `run`/`spawn`, so options (TLS config, limits) set
via `with` apply first.

## `Client`

`connect(uri)` is `Client::open(base)?.get(name)`. Keep a `Client` only to
issue more lookups on the same endpoint:

```rust
let c = rsbinder::Client::open("unix:///run/app.sock")?;
let hello: Strong<dyn IHello> = c.get("hello")?;
let stats: Strong<dyn IStats> = c.get("stats")?;
drop(c);                                  // proxies stay valid
```

| | |
|---|---|
| `get::<T>(name)` | kernel: waits for registration (AOSP `waitForService`); RPC: one directory lookup, `NameNotFound` if absent |
| `try_get::<T>(name)` | non-waiting; `Ok(None)` when absent |
| `binder(name)` | untyped `SIBinder` |
| `session()` | the underlying `RpcSession` (`None` on `binder://`) |
| `endpoint()` | the parsed `Endpoint` (mirrors `Server::endpoint()`) |

`Client::open("binder://")` also starts the binder thread pool, so
callbacks, death notifications, and registration waits are delivered
promptly.

## Options

Anything that does not fit a URI goes through `ServeOptions` /
`ClientOptions`:

```rust
rsbinder::serve("tls://0.0.0.0:9000")?
    .with(|o| {
        o.tls = Some(server_tls_config);          // rustls::ServerConfig (feature rpc-tls)
        o.max_connections = Some(64);
        o.authorizer = Some(Box::new(|id| id.uid() == Some(1000)));
    })
    .add("hello", BnHello::new_binder(MyService))?
    .run()?;

let hello: Strong<dyn IHello> = rsbinder::Client::open_with("tls://host:9000", |o| {
    o.tls = Some(client_tls_config);
})?
.get("hello")?;
```

`ServeOptions`: `threads`, `max_connections`, `handshake_timeout`,
`idle_timeout`, `authorizer`, `tls`, `fd_modes`, `call_restriction`.
`ClientOptions`: `tls` / `tls_server_name`, `session_id`,
`outgoing_connections`, `fd_mode`, `timeout`, `driver`.

`Client::open_with`'s closure also receives the parsed `Endpoint`, so an
option that applies to only some transports is set from the endpoint rather
than from the URI text:

```rust
let client = rsbinder::Client::open_with(&uri, |o, endpoint| {
    if endpoint.supports_fd_passing() {
        o.fd_mode = Some(FileDescriptorTransportMode::Unix);
    }
})?;
```

An option that does not apply to the transport (a TLS config on
`binder://`, `call_restriction` on `unix://`) is **`BadValue` at
`run`/`spawn`/`open` time**, with a log line naming the option — never
silently ignored. Users who want that checked at compile time use the
low-level types directly.

## What the URI does *not* hide

Moving a service between transports changes its trust boundary — read
[Security & Authorization](./security.md).

| | `binder://` | RPC schemes |
|---|---|---|
| `@EnforcePermission` | enforced via the system permission service | **denied** (no caller-uid model) — authorize with `ServeOptions::authorizer` |
| `get_calling_uid()` | kernel-vouched | kernel-vouched on `unix://`, fail-closed sentinel elsewhere |
| death | process death | session disconnect |
| `list_services`, notifications, lazy services | via [`hub`](./service-manager.md) | not available |
| abandoning a session | nothing to do | call `RpcSession::close_session()` if a service you exported holds a proxy back into it (`client.session()`) |

## Async

Under the `tokio` feature, `rsbinder::connect_async::<dyn IHelloAsync<Tokio>>(uri).await`
runs the connect on `spawn_blocking`. Async servers pass
`BnHello::new_async_binder(impl, TokioRuntime(handle))` to `add` and use
`spawn()` (never `run()`) from inside a runtime — see
[Async Service](./async-service.md).

## Worked example

`example-hello` ships `bin/unified_service.rs` / `bin/unified_client.rs`:
identical service, registration, and call code with the transport chosen by
the argument (`kernel`, `rpc`, or any URI).

```text
cargo run -p example-hello --features rpc --bin unified_service rpc
cargo run -p example-hello --features rpc --bin unified_client rpc
cargo run -p example-hello --features rpc --bin unified_client unix:///tmp/rsb_unified.sock
```
