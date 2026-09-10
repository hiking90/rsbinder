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

let hello: Strong<dyn IHello> = rsbinder::Client::open_with("tls://host:9000", |o, _endpoint| {
    o.tls = Some(client_tls_config);
})?
.get("hello")?;
```

`ServeOptions`: `threads`, `max_connections`, `handshake_timeout`,
`idle_timeout`, `reply_timeout`, `authorizer`, `tls`, `fd_modes`,
`call_restriction`.
`ClientOptions`: `tls` / `tls_server_name`, `session_id`,
`outgoing_connections`, `incoming_connections`, `fd_mode`, `timeout`,
`handshake_timeout`, `driver`.

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

## Bridging two transports in one process

Everything above moves *one* service onto whichever transport you pick. A
different problem is reaching a service that lives on the *other* transport
— a socket client that needs a kernel service, or the reverse. rsbinder has
three answers, and which one applies depends on the direction:

| Direction | Mechanism | Where |
|---|---|---|
| kernel client → socket service | **Accessor** (`IAccessor`, Android 16 / `rsb_hub`) — the service manager hands the client a connection to the socket | [RPC Transport § Bridging](./rpc-transport.md) |
| one service, both transports | **dual-hosting** — the same `Bn*` passed to two `serve(...).add(...)` calls | this chapter |
| socket client → kernel service | **gateway** — a process that re-publishes an upstream proxy as a local service | below |

None of the three moves raw bytes between the stacks. A binder that belongs
to one stack **cannot be written into a parcel of the other**: rsbinder
refuses it at write time with `InvalidOperation`, exactly as libbinder does
(`Parcel::flattenBinder`, `RpcState::onBinderLeaving`). There is no
transparent bridge, and building one is a non-goal — a generic byte
forwarder would silently change the caller identity, fd rights, stability
and oneway ordering that the two stacks define differently.

### The gateway

A gateway process B connects to the upstream service C and publishes it
again on its own endpoint. Because `Bn*::new_binder` accepts anything
implementing the interface, and a typed handle implements the interface it
points at, that is one line:

```rust
// B: reach C over kernel binder, re-publish it on a Unix socket.
let upstream: Strong<dyn IHello> = rsbinder::connect("binder://my.hello")?;

rsbinder::serve("unix:///tmp/rsb_gw.sock")?
    .add("hello", BnHello::new_binder(upstream))?   // <- the gateway
    .run()?;
```

Passing the *proxy* instead — `.add("hello", upstream.as_binder())` — is the
mistake this refuses (`InvalidOperation` at `add`). Wrapping is not
ceremony: the `Bn*` is a **local** binder that B owns, and that is what
makes it publishable.

**The rule: every binder object crossing B is unwrapped by B and wrapped
again. Plain data flows through untouched.** So a method with a binder
argument — a callback registration, say — needs B to override just that
method and re-wrap the argument before forwarding it:

```rust
fn register(&self, cb: &Strong<dyn ICallback>) -> BinderResult<()> {
    self.upstream.register(&BnCallback::new_binder(cb.clone()))
}
```

Forwarding `cb` as-is fails with `InvalidOperation`, reported back to the
original caller.

That inline `new_binder` is correct for one call and wrong across calls:
each one mints a *new* object, so `register(cb)` and `unregister(cb)` reach C
as two unrelated binders and the removal matches nothing.
[`bridge::Rewrap`](https://docs.rs/rsbinder/latest/rsbinder/bridge/struct.Rewrap.html)
is the table that fixes it — same remote in, same local out:

```rust
struct Gateway {
    upstream: Strong<dyn IFoo>,
    callbacks: Rewrap<dyn ICallback>,   // Rewrap::new(BnCallback::new_binder)
}

fn register(&self, cb: &Strong<dyn ICallback>) -> BinderResult<()> {
    self.upstream.register(&self.callbacks.wrap(cb))
}
fn unregister(&self, cb: &Strong<dyn ICallback>) -> BinderResult<()> {
    self.upstream.unregister(&self.callbacks.wrap(cb))   // the same object
}
```

It holds only weak references, so it never keeps a wrapper alive: the
wrapper lives as long as C holds it, and the entry goes when the remote dies
or the wrapper is dropped.

What a gateway costs, all of it visible in B:

- **Identity.** C sees *B* as its caller, not A. C's `@EnforcePermission`
  checks pass with B's credentials, so authenticating A is B's job —
  `ServeOptions::authorizer` on B's endpoint.
- **File descriptors.** They only cross a Unix-domain link, and only with
  FD passing negotiated on that hop.
- **Uid.** Over TCP or vsock, B cannot learn A's uid at all.
- **Plaintext TCP** is bring-up only; a real deployment uses TLS or a
  Unix socket.
- **Death.** There are now two lifetimes. A's proxy dies when B goes away;
  B's upstream dies when C does. B does not forward one as the other.
- **Threads.** Each forwarded call occupies one of B's RPC workers for the
  whole upstream round trip — size `ServeOptions::threads` accordingly.

`example-hello` ships a runnable one:

```text
cargo run -p example-hello --bin hello_service                 # C, on kernel binder
cargo run -p example-hello --features rpc --bin gateway_service \
    binder://my.hello unix:///tmp/rsb_gw.sock                  # B
cargo run -p example-hello --features rpc --bin unified_client \
    unix:///tmp/rsb_gw.sock                                    # A
```

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
