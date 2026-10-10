# Cross-Transport Services

The AIDL interface, the generated `Bp*`/`Bn*` stubs, your service `impl`,
and the call sites are **already transport-agnostic** — the same `impl IFoo`
runs unchanged over kernel binder or RPC. What differs is only the
*bootstrap*: kernel binder uses `ProcessState` + the [`hub`](./service-manager.md)
service manager, while RPC uses `RpcServer` / `RpcSession`.

`rsbinder::serve` and `rsbinder::connect` make that bootstrap
transport-agnostic too: the destination is a **`Uri`** value, and
everything else is the same three calls. The low-level APIs remain the
direct path and are unchanged; this is a thin layer over them with no wire
change.

## Three lines per side

```rust
use rsbinder::*;

// Server — kernel binder: ProcessState init + thread pool + service
// manager registration + join. The same code with Endpoint::unix("/tmp/x.sock")
// binds a socket, publishes the service, and runs the accept loop.
rsbinder::serve(Uri::kernel())?
    .add("hello", BnHello::new_binder(MyService))?
    .run()?;

// Client — open, look up, cast. The proxy alone keeps an RPC session
// alive; there is nothing else to hold.
let hello: Strong<dyn IHello> = rsbinder::connect(Uri::kernel().with_service("hello"))?;
//  rsbinder::connect(Endpoint::unix("/tmp/x.sock").with_service("hello"))?
hello.echo("hi")?;
```

## Destinations: `Uri`

A `Uri` is an `Endpoint` (where), an optional service name (what to look
up), and the RPC wire to speak. `serve`, `connect`, `Client::open` and
`Reconnecting::builder` take anything that converts into one: a `Uri`, a
`&Uri`, an `Endpoint`, or a `KernelEndpoint`.

| Endpoint | Constructor | Notes |
|---|---|---|
| kernel binder | `Uri::kernel()`, `Endpoint::kernel()`, `KernelEndpoint::default().with_driver(..).with_threads(..).with_mmap_size(..)` | settings apply to the process-wide `ProcessState` |
| Unix socket | `Endpoint::unix(path)` | absolute path; any bytes the OS accepts |
| abstract Unix socket | `Endpoint::unix_abstract(name)` | Linux/Android |
| vsock | `Endpoint::vsock(cid, port)` | feature `rpc-vsock` |
| TLS over TCP | `Endpoint::tls(host, port)` | feature `rpc-tls` (TCP is TLS-only) |

- `endpoint.with_service("hello")` (or `uri.with_service(..)`) names the
  service. `serve()` ignores it, so a server and its clients can share one
  value; `connect()` requires it; `Client::open()` rejects it.
- `uri.with_wire(WireProfile::Android13Plus(WireVersion::MAX))` selects the
  AOSP versioned wire on an RPC endpoint; the default `WireProfile::R34`
  speaks the r34 wire. A kernel endpoint takes only `R34`.
- `KernelEndpoint::with_mmap_size` sizes the mapping this process receives
  transactions into — the real meaning of the "1 MB binder limit". See
  [Receive mapping size](#receive-mapping-size).
- `KernelEndpoint`, `WireProfile`, `WireVersion`, `UriError` and
  `UriErrorKind` live in `rsbinder::entry`.

### String form

Configuration files and command lines carry the same value as a string.
Parse it explicitly — the entry functions take no `&str`:

```rust
let uri: Uri = config.target.parse()?;      // or Uri::parse(&config.target)?
let hello: Strong<dyn IHello> = rsbinder::connect(uri)?;
```

```text
binder://[<service>][?driver=<path>&threads=<n>&mmap=<bytes>]
unix://<abs-path>[#<service>]               (three slashes: unix:///tmp/x.sock)
unix-abstract://<name>[#<service>]          Linux/Android
vsock://<cid>:<port>[#<service>]            feature rpc-vsock
tls://<host>:<port>[#<service>]             feature rpc-tls; IPv6 host in [ ]
```

- The service name is the `#fragment`; `binder://name` is a shorthand for
  `binder://#name`.
- `?profile=android13plus[-vN]` on any RPC scheme is
  `WireProfile::Android13Plus` (`WireVersion::MAX` when `-vN` is omitted).
- Paths, names, hosts and the service are percent-decoded, so a path with
  `#`, `?`, `%` or bytes that are not UTF-8 is written escaped.
- An unknown scheme or query key, a key given twice, an empty `#`, or
  `binder://a#b` is a `UriError`; `uri_error.kind()` says which.
  `UriError` converts into `StatusCode::BadValue` for `?` (logging the
  reason) and implements `std::error::Error` for `Box<dyn Error>`.
- `uri.to_string()` writes the string form back. Parsing it gives the same
  `Uri`, for every value `serve` / `connect` accept — so build URIs with the
  constructors, not with `format!`.

## `Server`

`serve(uri)` returns a `Server`:

| | |
|---|---|
| `add(name, svc)` | publish `svc` (anything `Into<SIBinder>`, e.g. `BnFoo::new_binder(..)`). Queued; `run` / `spawn` register it after checking the options (kernel: with the service manager, once the thread pool is up; RPC: in the server's directory). |
| `with(\|o\| ..)` | set [`ServeOptions`](#options) |
| `run()` | serve and block. Kernel: start the thread pool and join it (never returns normally). RPC: the accept loop, until shut down. |
| `spawn()` | serve in the background; returns a `ServerGuard`. RPC: dropping the guard (or `ServerGuard::stop_and_join()`) ends the server — every session is closed and the threads are joined. Kernel: the guard is inert (the process thread pool has no shutdown). |

Kernel `serve(Uri::kernel())` initializes the process-wide `ProcessState`
idempotently; a second kernel server in the same process reuses it, and is
refused with `StatusCode::BadValue` if it asked for a different driver,
thread count, or mapping size — the process cannot give it one. The refusal
lands where the option is read: `serve()` itself for the `KernelEndpoint`
settings (`?driver=` / `?threads=` / `?mmap=`), `run`/`spawn` for `ServeOptions::threads` and
`ServeOptions::mmap_size`, and `open` for `ClientOptions::driver` and
`ClientOptions::mmap_size`. RPC
servers bind their listener at `run`/`spawn`, so options (TLS config, limits)
set via `with` apply first.

## `Client`

`connect(uri)` is `Client::open(endpoint)?.get(name)`. Keep a `Client` only to
issue more lookups on the same endpoint:

```rust
let c = rsbinder::Client::open(Endpoint::unix("/run/app.sock"))?;
let hello: Strong<dyn IHello> = c.get("hello")?;
let stats: Strong<dyn IStats> = c.get("stats")?;
drop(c);                                  // proxies stay valid
```

| | |
|---|---|
| `get::<T>(name)` | kernel: waits for registration (AOSP `waitForService`); RPC: one directory lookup, `NameNotFound` if absent |
| `try_get::<T>(name)` | non-waiting; `Ok(None)` when absent |
| `binder(name)` | untyped `SIBinder` |
| `session()` | the underlying `RpcSession` (`None` on kernel binder) |
| `endpoint()` | the `Endpoint` (mirrors `Server::endpoint()`) |

`Client::open(Uri::kernel())` also starts the binder thread pool, so
callbacks, death notifications, and registration waits are delivered
promptly.

## Options

Anything that is not part of the `Uri` goes through `ServeOptions` /
`ClientOptions`:

```rust
rsbinder::serve(Endpoint::tls("0.0.0.0", 9000))?
    .with(|o| {
        o.tls = Some(server_tls_config);          // rustls::ServerConfig (feature rpc-tls)
        o.max_connections = Some(64);
        o.authorizer = Some(Box::new(|id| id.uid() == Some(1000)));
    })
    .add("hello", BnHello::new_binder(MyService))?
    .run()?;

let hello: Strong<dyn IHello> = rsbinder::Client::open_with(Endpoint::tls("host", 9000), |o, _endpoint| {
    o.tls = Some(client_tls_config);
})?
.get("hello")?;
```

`ServeOptions`: `threads`, `mmap_size`, `max_connections`,
`handshake_timeout`, `idle_timeout`, `reply_timeout`, `authorizer`, `tls`,
`fd_modes`, `call_restriction`.
`ClientOptions`: `tls` / `tls_server_name`, `session_id` (deprecated: a
non-empty id is refused),
`outgoing_connections`, `incoming_connections`, `fd_mode`, `timeout`,
`handshake_timeout` (deprecated: `timeout` bounds connecting), `driver`,
`mmap_size`. What each timeout bounds, and what its expiry ends, is in
[Timeouts](./rpc-transport.md#timeouts).

### Receive mapping size

A binder transaction is copied into a buffer the driver allocates out of
the **receiving** process's mapping, so that mapping — not the protocol —
is what bounds a call. rsbinder maps the same ~1 MB AOSP `libbinder` does;
raise it on a service that must accept larger calls:

```rust
use rsbinder::entry::KernelEndpoint;

rsbinder::serve(KernelEndpoint::default().with_mmap_size(4 << 20))?   // 4 MB, the driver's ceiling
    .add("bulk", BnBulk::new_binder(MyService))?
    .run()?;
```

- Bytes only (no `4M` shorthand), between one page and
  `MAX_BINDER_MMAP_SIZE` (4 MB, where the driver clamps silently — a
  larger request is refused here rather than quietly shrunk), rounded up
  to a page. A one-page mapping is legal and carries a call of a few KB.
- Set on the **receiver**. A caller needs nothing; a payload too large for
  the destination comes back as `StatusCode::FailedTransaction`. A client
  is a receiver of its own replies, which is what `ClientOptions::mmap_size`
  is for.
- Oneway transactions may use only half of it — the driver reserves the
  rest so an async flood cannot starve synchronous calls.
- Only address space is reserved; pages are faulted in as they are used.
- Process-wide and fixed at the first `serve` / `Client::open`, like
  the driver and thread count. `ProcessState::init_with_mmap_size` is the
  direct form, and `ProcessState::mmap_size()` reports what is in force.
- `ServeOptions::mmap_size` cannot make the mapping: `serve` initializes
  `ProcessState` before the option is read, so the field can only agree with
  the size already in force and a different one is `BadValue` at
  `run`/`spawn` — the same rule as `ServeOptions::threads`. What sets the
  size on a kernel server is `KernelEndpoint::with_mmap_size`
  (`binder://?mmap=`), or `ProcessState::init_with_mmap_size` called before
  `serve`.

`Client::open_with`'s closure also receives the `Endpoint`, so an option
that applies to only some transports can be set from it — useful when the
`Uri` was parsed from configuration:

```rust
let client = rsbinder::Client::open_with(&uri, |o, endpoint| {
    if endpoint.supports_fd_passing() {
        o.fd_mode = Some(FileDescriptorTransportMode::Unix);
    }
})?;
```

An option that does not apply to the transport (a TLS config on kernel
binder, `call_restriction` on a Unix socket) is **`BadValue` at
`run`/`spawn`/`open` time**, with a log line naming the option — never
silently ignored. A kernel setting that *does* apply but the process cannot
honor — a thread count or driver against a `ProcessState` already
initialized with another value — is refused the same way, at `serve()` for
the `KernelEndpoint` form. So is a `Uri` whose string form could not carry
it: a relative Unix path, an empty abstract name, TLS host or service name,
or a kernel endpoint with `WireProfile::Android13Plus`. Users who want that
checked at compile time use the low-level types directly.

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
let upstream: Strong<dyn IHello> = rsbinder::connect(Uri::kernel().with_service("my.hello"))?;

rsbinder::serve(Endpoint::unix("/tmp/rsb_gw.sock"))?
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

## What the `Uri` does *not* hide

Moving a service between transports changes its trust boundary — read
[Security & Authorization](./security.md).

| | `binder://` | RPC schemes |
|---|---|---|
| `@EnforcePermission` | enforced via the system permission service | **denied** (no caller-uid model) — authorize with `ServeOptions::authorizer` |
| `get_calling_uid()` | kernel-vouched | kernel-vouched on `unix://`, fail-closed sentinel elsewhere |
| death | process death | session disconnect |
| `list_services`, notifications, lazy services | via [`hub`](./service-manager.md) | not available |
| abandoning a session | nothing to do | call `RpcSession::close_session()` if a service you exported holds a proxy back into it (`client.session()`) |

## Transport capabilities

The differences above are enforced where they happen: writing a file
descriptor into a parcel that cannot carry one fails at the write, and
`get_calling_uid()` returns a sentinel that no ACL matches. Those answers
arrive at the moment of use, which is late if you were about to promise a
caller something the transport cannot do.

`TransportCaps` is the same information available up front. Ask before
you commit:

```rust,ignore
use rsbinder::TransportCaps;

let client = rsbinder::Client::open(rsbinder::Endpoint::unix("/run/app.sock"))?;
// Fails here, logging which bits are missing and when each one holds,
// instead of on the first callback the server tries to make.
client.caps().require(TransportCaps::CALLBACKS, "event subscription")?;
```

Read it from `Client::caps()`, `RpcSession::caps()`, `Endpoint::static_caps()`,
or — inside a handler, for the call being served — `calling_caps()`.

| | `binder://` | `unix://` | `vsock://` | `tls://` |
|---|:---:|:---:|:---:|:---:|
| `FD_PASSING` | ✅ | after negotiating `Unix` fd mode | ❌ | ❌ |
| `TRUSTED_UID` | ✅ | ✅ | ❌ | ❌ |
| `CALLBACKS` | ✅ | with `incoming_connections > 0` | with `incoming_connections > 0` | with `incoming_connections > 0` |
| `SAME_HOST` | ✅ | ✅ | ❌ | ❌ |
| `KERNEL_KNOBS` | ✅ | ❌ | ❌ | ❌ |

Three things to keep straight.

**Caps never replace the checks they summarize.** Skipping the check is
not less safe; the fd write still refuses, the uid is still a sentinel.
What you lose is being told early, and being told when the missing bit holds.

**`Endpoint::static_caps()` and the session's caps answer different
questions.** The endpoint answers for the transport family; the session says
what it has. The endpoint bounds the session in neither direction: a `unix://`
endpoint reports `FD_PASSING`, but a session over it carries no descriptors
until both ends negotiate the `Unix` fd mode — the default is `None`; and no
*RPC* endpoint reports `CALLBACKS` (the `binder://` column does, because kernel
binder has every bit), although a session opened with
`incoming_connections > 0` does. The table's two qualified cells are session
values for that reason, not endpoint ones.

**A session's caps are a snapshot.** `CALLBACKS` goes away when the last
callback connection dies. Read them where you act, not once at startup.

## Async

Under the `tokio` feature, `rsbinder::connect_async::<dyn IHelloAsync<Tokio>>(uri).await`
runs the connect on `spawn_blocking`. Async servers pass
`BnHello::new_async_binder(impl, TokioRuntime(handle))` to `add` and use
`spawn()` (never `run()`) from inside a runtime — see
[Async Service](./async-service.md).

## Worked example

`example-hello` ships `bin/unified_service.rs` / `bin/unified_client.rs`:
identical service, registration, and call code with the transport chosen by
the argument (`kernel`, `rpc`, or any URI string, parsed into a `Uri`).

```text
cargo run -p example-hello --features rpc --bin unified_service rpc
cargo run -p example-hello --features rpc --bin unified_client rpc
cargo run -p example-hello --features rpc --bin unified_client unix:///tmp/rsb_unified.sock
```
