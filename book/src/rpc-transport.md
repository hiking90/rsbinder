# RPC Transport (binder-over-socket)

rsbinder ships **two parallel Binder stacks**:

1. **Kernel binder** — the traditional path through `/dev/binder` /
   `/dev/binderfs/binder`, the rsb_hub or Android `servicemanager`,
   and the kernel binder driver. This is what every chapter so far has
   covered.
2. **RPC transport** — binder-over-socket, AOSP's `RpcServer` /
   `RpcSession` equivalent in pure Rust. No kernel binder, no
   `ProcessState`, no service manager. A socket (Unix-domain, vsock,
   or TLS over TCP) carries the same `Parcel` payload between two
   processes, optionally on different machines.

Both stacks drive the **same generated AIDL stubs**. A client written
against `IHello` doesn't know — and doesn't need to know — whether the
proxy underneath talks to a kernel binder handle or an RPC socket.

This chapter introduces when to use RPC, how to stand up a minimal
client/server pair, and the runtime/platform/security trade-offs that
distinguish RPC from kernel binder.

> **Feature flag.** RPC is gated behind the `rpc` Cargo feature, off
> by default. Builds without `rpc` carry zero RPC code and zero extra
> dependencies. Enable per crate:
>
> ```toml
> [dependencies]
> rsbinder = { version = "0.12", features = ["rpc"] }
> ```

## When to use RPC vs. kernel binder

| Need                                                | Stack          |
|-----------------------------------------------------|----------------|
| Two processes on the **same Linux/Android** host    | Kernel binder  |
| **Pure user-space**, no kernel binder driver        | RPC (Unix)     |
| **Cross-host / cross-VM** binder calls              | RPC (vsock/TLS)|
| Runs on **macOS** as the host platform              | RPC only       |
| Wants Android's `getCallingUid()` / SELinux for free| Kernel binder  |
| Needs custom transport (TLS, mTLS, …)               | RPC            |

Kernel binder is faster (single `ioctl` per transaction, shared
memory) and gives you Android's security model for free. RPC trades
some of that for portability and reach — it works on Linux, Android,
**and macOS**, with no kernel driver and no service manager.

## A minimal RPC service and client

The complete example lives under
[`example-hello`](https://github.com/hiking90/rsbinder/tree/master/example-hello).
Once you've enabled the `rpc` feature on the workspace, run:

```bash
# Terminal 1
$ cargo run -p example-hello --features rpc --bin rpc_hello_service

# Terminal 2
$ cargo run -p example-hello --features rpc --bin rpc_hello_client
```

### Server

```rust
use rsbinder::rpc::RpcServer;
use rsbinder::*;
use example_hello::*;

const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

struct IHelloService;
impl Interface for IHelloService {}

impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // No ProcessState, no hub. RPC never touches the kernel binder.
    let _ = std::fs::remove_file(RPC_SOCKET);
    let server = RpcServer::setup_unix_server(RPC_SOCKET)?;

    // Publish a root binder. Clients fetch this via get_root().
    server.set_root(BnHello::new_binder(IHelloService {}).as_binder())?;

    // Accept loop runs on this thread until stop_accepting().
    server.run()?;
    Ok(())
}
```

Key points:

- **No `ProcessState::init_default()`** — the RPC stack is independent
  of the kernel binder singleton. Mixing the two stacks in one
  process is fine (the Accessor pattern below relies on it), but a
  pure-RPC process needs neither `ProcessState` nor `rsb_hub`.
- **`set_root`** publishes the single root binder. The client fetches
  it back through `RpcSession::get_root` — this is the moral
  equivalent of `hub::wait_for_interface(name)` for kernel binder, just
  without a service manager.
- **`server.run()`** runs the accept loop until
  [`RpcServer::stop_accepting`] is called from another thread. Use
  [`RpcServer::run_background`] to spawn it on a dedicated thread
  instead. `stop_accepting` only raises the flag — sessions already
  connected drain as their peers leave. [`RpcServer::terminate`] ends
  those too (every session closed, every worker joined); it is what
  dropping a `ServerGuard` does. See [How a connection
  ends](#how-a-connection-ends).

### Client

```rust
use rsbinder::rpc::RpcSession;
use rsbinder::{FromIBinder, Strong};
use example_hello::*;

const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let session = RpcSession::setup_unix_client(RPC_SOCKET)?;
    let root = session.get_root()?;

    // Same generated stub as the kernel path — try_from picks the
    // RPC proxy under the hood.
    let hello: Strong<dyn IHello> =
        <dyn IHello as FromIBinder>::try_from(root)?;

    let reply = hello.echo("Hello over RPC!")?;
    println!("server replied {reply:?}");
    Ok(())
}
```

The generated `Bp*` proxies dispatch through `as_remote()`, which
returns either the kernel handle or the RPC proxy depending on what
the `SIBinder` came from. **One AIDL stub, two stacks** — there is no
separate "RPC client API" you need to learn.

### Multiple named services on one server

`RpcServer::set_root` publishes a single root binder. If you need to
expose **multiple** named services on the same socket — the moral
equivalent of the kernel-binder `hub::add_service(name, …)` flow — use
[`RpcServer::add_service`] instead. The first call automatically turns
the root into a built-in service directory, so clients reach each
service by name through [`RpcSession::get_service`]:

```rust
// Server
let server = RpcServer::setup_unix_server(SOCKET)?;
server.add_service("hello", BnHello::new_binder(IHelloService).as_binder())?;
server.add_service("echo",  BnEcho::new_binder(IEchoService).as_binder())?;
server.run()?;

// Client
let session = RpcSession::setup_unix_client(SOCKET)?;
let hello: Strong<dyn IHello> =
    <dyn IHello as FromIBinder>::try_from(session.get_service("hello")?)?;
let echo: Strong<dyn IEcho> =
    <dyn IEcho  as FromIBinder>::try_from(session.get_service("echo")?)?;
```

`add_service` is O(1) per call: the directory is built once and shares
the server's service map, so later registrations — even after the server
is already serving — are seen through the same root with no rebuild.
Mixing `set_root` with `add_service` on the same server is **not**
supported (the last one installed wins); pick one publishing model per
server.

> For registering and looking up services with the *same* code over
> kernel binder or RPC, see [Cross-Transport Services](./cross-transport-services.md).

## Wire-protocol profiles

rsbinder speaks **two** RPC wire profiles, chosen at connect time:

| Profile          | Default? | Constructor                                                       |
|------------------|----------|-------------------------------------------------------------------|
| Android 12 (r34) | yes      | `RpcSession::setup_unix_client(path)`                             |
| Android 13–16    | opt-in   | `RpcSession::setup_unix_client_android13plus(path, max_version)`  |

The android-13+ profile is the protocol real AOSP `libbinder` speaks
today. It runs a handshake on connect and negotiates a wire version:

| `max_version` | AOSP version           |
|---------------|------------------------|
| `0`           | Android 13             |
| `1`           | Android 14 / 15        |
| `2`           | Android 16             |

The server picks the highest version both sides advertise:

```rust
// Server: offer up to android-16 wire v2.
let server = RpcServer::setup_unix_server("/tmp/foo.sock")?;
server.set_android13plus(2);
server.set_root(my_root)?;

// Client: offer up to v2. Negotiation picks min(client, server).
let session = RpcSession::setup_unix_client_android13plus(
    "/tmp/foo.sock", 2,
)?;
```

The android-13+ profile has been validated end-to-end against **real
AOSP `libbinder`** on Android 13/14/15/16 emulators (full parcel-body
transact, byte-correct), so an rsbinder RPC server can serve a real
Android `libbinder` client and vice versa.

### The Android 12 (r34) profile

The default profile speaks the wire of Android 12 and 12L
(`android-12.0.0_r1` through `r34`, SDK 31 and 32), whose RPC code did
not change across those releases. It has no handshake:

- A client opens each connection by writing an `int32` session id: `-1`
  for a new session. After that every message is a 16-byte
  `RpcWireHeader` and the body it announces, with no length prefix.
- Every binder in a parcel, a null one included, is followed by its
  stability as Android 12 writes it (`0x0c000001` for the default
  `Stability::System`, `0x00000001` for null), whatever the host's SDK.
- An `RpcServer` mints an id for each new session. An Android 12 client
  reads `GET_MAX_THREADS` and `GET_SESSION_ID`, then opens
  `max_threads - 1` more connections that write that id; the server
  serves each as its own connection of the session, so calls on
  different connections run at once. A local connection joins only from
  the founding connection's uid, a check Android 12 does not make.
  `RpcSession::get_session_id` returns the id's 4 bytes.
- An rsbinder r34 client opens one connection and does not join more.

Android 12 libbinder differs from what rsbinder offers on this wire:

| | Android 12 libbinder | rsbinder r34 |
|---|---|---|
| Largest message body | 100,000 bytes (`kMaxTransactionAllocation`): it refuses to send more (`NO_MEMORY`). A server connection that receives more closes; a client waiting for a larger reply gets `NO_MEMORY` with the body left unread, and its next call on that connection ends its session | `MAX_FRAME_LEN` (64 MiB) |
| TLS | none | r34 inside TLS, between rsbinder peers only |
| Incoming (callback) connections | none | none |
| File descriptors | refused (`BAD_TYPE`) | `negotiate_fd_transport`, between rsbinder peers only; against Android 12 it returns `None` |
| A session ends | when its last connection does | when any one of its connections does |

Keep a parcel exchanged with Android 12 under 100,000 bytes. The last
row matters only when one of a session's connections fails on its own:
an Android 12 client opens and closes them together.

> **0.12.0 changed this wire.** Releases before it framed every r34
> message with a `u32` length and wrote no session id or stability, so
> they cannot talk to Android 12 libbinder, and a 0.12.0 peer and an
> older one refuse each other's first bytes. Upgrade both ends together.
> A custom `RpcTransport` must implement `send_raw` / `recv_raw`; see the
> CHANGELOG.

This profile has been validated against real Android 12 libbinder on an
SDK 31 emulator, both ways: an Android 12 client against an rsbinder
`RpcServer` with three connections per session (parallel calls, oneway
order, nested callbacks, null binders, reference counts, the size limit),
and an rsbinder client against an Android 12 `RpcServer`
(`example-hello/cpp/run_rpc_r34_interop.sh`).

## Transports

`RpcSession`/`RpcServer` are transport-agnostic. The bundled
backends are:

| Backend     | Feature flag       | Trust boundary                                  |
|-------------|--------------------|-------------------------------------------------|
| Unix socket | `rpc` (always on)  | Filesystem perms + `SO_PEERCRED`/`getpeereid`   |
| vsock       | `rpc-vsock`        | Hypervisor VM isolation (host ↔ VM)             |
| TLS / TCP   | `rpc-tls`          | TLS certificate chain (caller-owned `rustls`)   |
| Plain TCP   | `rpc-tcp-debug`    | **None — debug/interop only, never production** |

Add the matching feature in `Cargo.toml`:

```toml
[dependencies]
rsbinder = { version = "0.12", features = ["rpc", "rpc-vsock"] }
```

Each backend implements the
[`RpcTransport`](https://docs.rs/rsbinder/latest/rsbinder/rpc/transport/trait.RpcTransport.html)
trait — you can implement your own if you need a custom carrier. A
session moves its bytes through `send_raw` / `recv_raw` on both wire
profiles, so a transport of your own implements those; the trait's
`send_frame` / `recv_frame` are a separate length-prefixed API no session
calls.

### Abstract Unix-domain sockets (Linux/Android)

Since 0.10.0 the Unix backend can also bind the Linux **abstract socket
namespace** — a kernel-namespace name with no filesystem entry, so there
is no socket file to create, unlink, or leak:

```rust
// Server: no filesystem path, nothing to clean up on shutdown.
let server = RpcServer::setup_unix_server_abstract(b"my.rpc.name")?;

// Client, r34 wire:
let session = RpcSession::setup_unix_client_abstract(b"my.rpc.name")?;

// Client, android-13+ wire (negotiating up to v2):
let session =
    RpcSession::setup_unix_client_android13plus_abstract(b"my.rpc.name", 2)?;
```

These functions exist only on Linux and Android (the abstract namespace
is a Linux kernel feature).

For the android-13+ client there is also a builder,
[`RpcClientConfig`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcClientConfig.html),
consumed by `RpcSession::setup_client_android13plus_with_config`. It has
one constructor per transport — `unix`, `unix_abstract`, `vsock`, `tls`,
`tcp_debug`, and `new` for a connect function of your own — and the same
knobs on all of them: an outgoing-connection fan-out, incoming (callback)
connections, and the fd transport mode:

```rust
use rsbinder::rpc::{FileDescriptorTransportMode, RpcClientConfig, RpcSession};

let session = RpcSession::setup_client_android13plus_with_config(
    RpcClientConfig::unix_abstract(b"my.rpc.name", 2)
        .outgoing_connections(2)
        .fd_mode(FileDescriptorTransportMode::Unix),
)?;
```

The Unix-only predecessor `RpcUnixClientConfig`, and the
`setup_unix_client_android13plus_{with_config,with_id,fan_out}` helpers,
are deprecated since 0.12.0 and will be removed in the release after it.

The builder's `session_id` knob is for the manual attach calls,
`add_outgoing_connection_with_config` and
`add_incoming_connection_with_config`, which add one connection to a
session you already hold. Every entry that builds a new `RpcSession` —
`setup_client_android13plus_with_config`, the deprecated
`setup_unix_client_android13plus_with_id` and
`connect_android13plus_fd_with_id`, and the entry API's deprecated
`ClientOptions::session_id` — refuses a non-empty id with `BadValue`. A
second client `RpcSession` on one server session would keep its own oneway
numbering, binder addresses and lifetime: the server would drop one side's
oneway calls as out of order, could dispatch a call meant for one side's
binder to the other side's binder at the same address, and would end the
shared session when either side closed. AOSP has no public entry for this
either: `RpcSession::setupClient` is private, and the connections that
echo the id belong to the session that read it. For the same reason a
manual attach accepts only the id of the session it is called on: any
other id, such as one read from another `RpcSession`, is `BadValue`
before connecting.

> **The id you echo must come from `RpcSession::get_session_id()`** on the
> session you attach to — one round trip that asks the server for it, kept
> by the session. `RpcSession::session_id()` is a *different* method: on a
> client session it returns a client-local value that the server has never
> seen. Both return 32 opaque bytes, so nothing but the method name
> distinguishes them; the attach calls refuse every id but the session's
> own with `BadValue` before connecting. An attach the server refuses (for
> example, more connections than its `set_max_threads` allows) fails at the
> attach call, and on `add_outgoing_connection_with_config` that failure
> also ends the session it was attaching to (see
> [How a connection ends](#how-a-connection-ends)); stay within
> `negotiate()` connections.

> **Security.** An abstract socket has **no filesystem permissions**:
> any process in the same network namespace can connect (subject only to
> LSM policy such as SELinux), so the directory-mode access control a
> path-bound socket can rely on does not apply. When exposure matters,
> install an [`RpcServer::set_authorizer`] hook (see
> [Security](#security)) and check the peer identity (uid/gid/pid) it
> receives.

### TLS client

```rust
use rsbinder::rpc::{rustls, RpcSession};
use std::sync::Arc;

let mut roots = rustls::RootCertStore::empty();
// ... populate `roots` from your trust anchors ...
let config = Arc::new(
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth(),
);

// Third argument must be `Arc<rustls::ClientConfig>` — `setup_tcp_client_tls`
// takes the config by shared ownership so it can be cloned per connection.
let session = RpcSession::setup_tcp_client_tls(
    "binder.example.com:9999",
    "binder.example.com",
    config,
)?;
let root = session.get_root()?;
```

`rsbinder::rpc::rustls` is the **exact** `rustls` version the `rpc-tls`
backend links against, re-exported so you don't have to track it in
your own `Cargo.toml`.

### TLS server

The server side takes a `rustls::ServerConfig` carrying its certificate
chain and private key. `RpcServer::setup_tcp_server_tls` runs the TLS
handshake on each connection's own worker thread (so a slow-handshake
peer stalls only its worker, never the accept loop), then serves the
same session as any other backend:

```rust
use rsbinder::rpc::{rustls, RpcServer};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;

// Server certificate chain + private key, loaded from PEM.
let certs: Vec<_> =
    CertificateDer::pem_slice_iter(SERVER_CERT_PEM.as_bytes()).collect::<Result<_, _>>()?;
let key = PrivateKeyDer::from_pem_slice(SERVER_KEY_PEM.as_bytes())?;

let config = Arc::new(
    rustls::ServerConfig::builder()
        .with_no_client_auth()              // one-way TLS — see "Who needs a key?"
        .with_single_cert(certs, key)?,
);

// TCP is TLS-only by design: there is no plaintext-TCP server constructor.
let server = RpcServer::setup_tcp_server_tls("0.0.0.0:9999", config)?;
server.set_root(my_root_binder)?;
server.run()?;                              // or server.run_background()
```

`RpcServer::setup_unix_server_tls` and (with `rpc-vsock`)
`RpcServer::setup_vsock_server_tls` do the same over a Unix socket /
vsock — TLS is orthogonal to the socket kind (it mirrors AOSP's
`RpcTransportCtx::newTransport(fd)`).

### Who needs a key?

| Mode | Server | Client |
|------|--------|--------|
| One-way TLS (default) | cert chain **+ private key** (always) | trusted roots only — **no key** |
| mTLS (mutual)         | cert chain + private key **and** a client-cert verifier | cert chain **+ private key** |

- **One-way TLS** is the common case. The server proves its identity
  with its certificate; the client only verifies that certificate
  against its `RootCertStore` (`with_no_client_auth()` on both configs
  above). The client needs **no** key of its own — exactly like a
  browser connecting to an HTTPS site.

- **mTLS** additionally authenticates the *client*. Build the server
  config with a client-cert verifier
  (`rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?`
  → `.with_client_cert_verifier(verifier)`) instead of
  `.with_no_client_auth()`, and give the client a cert + key
  (`.with_client_auth_cert(client_certs, client_key)?` instead of the
  client's `.with_no_client_auth()`). The handshake then fails unless
  the client presents a certificate the verifier trusts, and the server
  sees that client's leaf cert as its `PeerIdentity::Certificate`.

Either way the certificate check happens **during the TLS handshake**:
an untrusted, missing, or expired certificate fails the connection
before a single RPC byte is exchanged. rsbinder never invents crypto —
all key / cert / root management and verification are your `rustls`
config and rustls itself.

## Security

> **RPC is not a drop-in for kernel binder's security model.** Kernel
> binder gives you a kernel-vouched uid/pid and SELinux for free; RPC's
> trust boundary is the transport itself. This section covers the RPC
> trust boundaries; for the full handler-side authorization story across
> both transports — `calling_caller()`, `@EnforcePermission` over RPC,
> uid ACLs, `PermissionAuthority` — see
> [Security & Authorization](./security.md).

Each transport defines its own trust boundary, surfaced as a
[`PeerIdentity`](https://docs.rs/rsbinder/latest/rsbinder/rpc/enum.PeerIdentity.html):

- **`Local { uid, pid }`** — Unix socket peer with kernel-vouched
  credentials.
- **`Vsock { cid }`** — VM context id (a routing address, **not** an
  ACL basis on its own — the trust comes from hypervisor isolation).
- **`Certificate(CertId)`** — TLS peer authenticated by its leaf cert.
- **`Anonymous`** — no identity at all. ACL is impossible against an
  anonymous peer. The debug TCP backend always returns this, and **any**
  backend falls back to it (logged loudly) if peer-credential resolution
  fails — so an authorizer must treat `Anonymous` as deny.

Use [`RpcServer::set_authorizer`] to enforce a per-connection policy.
The closure runs once on accept, **before any RPC byte is exchanged**:

```rust
const ALLOWED_UID: u32 = 1000;

server.set_authorizer(|peer| {
    peer.uid() == Some(ALLOWED_UID)
});
```

A rejected peer's socket is closed; its next operation sees
`DeadObject`. The hook is opt-in — without it, every connection is
accepted (matching the prior behaviour).

`set_authorizer` is **connection-level** (decided once, at handshake) —
the right granularity for vsock and TLS, which carry no per-call uid. For
**per-method** authorization, a handler can also read the caller directly:
over a Unix-domain RPC connection `rsbinder::get_calling_uid()` returns the
kernel-vouched peer uid (and `calling_caller()` the full transport-tagged
identity), exactly as on kernel binder — so a uid ACL written once runs on
both. Over a uid-less transport `get_calling_uid()` is the fail-closed
`u32::MAX` sentinel. See [Security & Authorization](./security.md).

## Capabilities

The RPC stack is feature-complete for the everyday cases you'd use
kernel binder for, with a few extras specific to socket transport:

- **AIDL interfaces** — every type the AIDL compiler supports works
  over RPC: primitives, strings, arrays, parcelables, nested
  interfaces, enums, unions, oneway methods.
- **Callbacks (nested binders)** — a callback object created on the
  client crosses the socket like any other Binder and the server
  invokes it back through the same session. A twoway call from inside
  a handler always works; a `oneway` call, or a call from *any other*
  server thread, needs the client to open an incoming connection — see
  [Callbacks outside a handler](#callbacks-outside-a-handler).
- **`ParcelFileDescriptor`** — opt in on both ends: the server with
  `RpcServer::set_supported_fd_modes`, the client with
  `RpcClientConfig::fd_mode` (the `fd_mode` connect option of the entry
  API). On the android-14+ wire the mode rides the connection header and
  is fixed for the session's lifetime, as in AOSP. A server that has not
  opted in refuses a client that requests `Unix`, as AOSP does: it closes
  the connection after the handshake, so the client's first call fails
  with `DeadObject`. On that wire
  `RpcSession::negotiate_fd_transport` sends nothing, and asking it for a
  `Unix` mode the header did not agree is `StatusCode::InvalidOperation`.
  An android-13 (v0) header carries no fd mode, so such a session passes
  no fds. `negotiate_fd_transport` negotiates only on the r34 wire, which
  has no fd mode in AOSP, as an rsbinder-only exchange after connect;
  against AOSP r34 libbinder it returns `None`. File descriptors ride
  out-of-band over `SCM_RIGHTS` on Unix-domain sockets.
- **Death notifications** — link a `DeathRecipient` on the proxy as
  usual. A session that ends — its peer closes, any one of its
  connections fails, or a deadline expires (see [Timeouts](#timeouts)) —
  fires every linked recipient (the RPC analogue of "the remote process
  died").
- **Async** — the same `into_async::<Tokio>()` adapter that wraps a
  blocking kernel-binder proxy works over RPC. See
  [Async Service](./async-service.md) — for RPC the only difference is
  how you obtain the proxy.

### Sessions with multiple connections

`RpcServer::set_max_threads(N)` matches AOSP's
`setMaxIncomingThreads`. Both `N == 1` (the default — one connection
per session, the mode every example in the book uses) and `N >= 2`
(multi-connection sessions) are validated against real Android 13–16
libbinder peers. On the r34 wire `N` is how many connections an
Android 12 client opens ([The Android 12 (r34) profile](#the-android-12-r34-profile)). See the rustdoc on
[`RpcServer::set_max_threads`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_max_threads)
for the per-mode details.

`set_max_threads` caps *incoming slots per session*. To cap
*server-wide concurrent connections* — for example to bound the
worker-thread fan-out regardless of how many sessions a single client
opens — use
[`RpcServer::set_max_connections(N)`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_max_connections)
(default: unlimited). Both knobs are independent and additive.

### Callbacks outside a handler

A server can always make a twoway call to a client's callback *while
it is answering that client* — the nested call rides the connection
the request came in on. Calling it from anywhere else (a timer, a
worker thread, a oneway notification fired later) needs a connection
the server can *send* on, and by default a session has none: the
server fails such a call at once with `WouldBlock` (AOSP `WOULD_BLOCK`)
rather than waiting for a slot that will never free up.

A `oneway` call needs that connection even from inside the handler.
It never rides the connection the request came in on, as in AOSP
libbinder ("asynchronous calls cannot be nested"): the server does not
wait for it and goes on to write its reply, while the client may still
be running the oneway inside its own reply wait — and a twoway call the
client's handler makes back on that connection would then read the
server's reply as its own.

The client provides that connection, exactly as libbinder's
`ARpcSession_setMaxIncomingThreads(n)` does:

```rust
use rsbinder::rpc::{RpcClientConfig, RpcSession};

let session = RpcSession::setup_client_android13plus_with_config(
    RpcClientConfig::unix(std::path::Path::new(RPC_SOCKET), 2).incoming_connections(1),
)?;
```

or, through the unified entry, `Client::open_with(uri, |o, _| {
o.incoming_connections = Some(1) })` on an `?profile=android13plus`
URI. This works on every RPC transport — `unix://`, `vsock://` and
`tls://` alike. Each incoming connection is a further connection to the
same endpoint (over `tls://`, its own TLS session), attached to the same
session and served by a thread the session owns; `n` of them let `n`
server threads call back in parallel. For a transport the entry does not
name — TLS over a Unix socket, say — build the connections yourself with
`RpcClientConfig::new(version, connect)`, whose `connect` is called once
per connection. The server budgets them at
`2 × set_max_threads` per session (two on a default server) and
refuses the rest, which the client sees as a setup error.

Two consequences worth knowing:

- **Death can be observed at all.** The serving thread sees the server's
  connection drop and fires the linked death recipients immediately. A
  client session without incoming connections has nobody reading its
  connection, so `link_to_death` is refused there with `InvalidOperation`
  (AOSP refuses it the same way). The other way to get notified is
  `RpcSession::spawn_serve`, which dedicates the founding connection to a
  serve loop: the session then makes no calls of its own, only nested ones
  from inside a handler.
- **Stop the session explicitly.** The serving threads keep the session
  alive; dropping the `RpcSession` handle and every proxy does not end
  them. Call `RpcSession::close_session()` when you are done — it shuts
  the connections down and joins the threads.

This needs the android-13+ profile (the attach echoes the session id).
[Streaming](./streaming.md) is the first thing in rsbinder itself that
depends on it: a producer pushes batches from a thread of its own, so
`Sink::open` checks for `TransportCaps::CALLBACKS` and refuses a session
that has no incoming connection.

### How a connection ends

A session ends as a whole, never one connection at a time. As in AOSP
libbinder (`RpcState::handleRpcError`), a send or receive that fails on
any of its connections shuts the session down: every connection
closes, calls in flight on the others fail with `DeadObject` — whether
the peer ran them is unknown — every linked death recipient fires, and
every local object the peer held is released. A single connection
cannot be resumed: the wire has no frame numbering or acknowledgement
that would tell which `oneway` call or reference-count frame the lost
connection took with it, and a session that carried on without them
would leave an object's later `oneway` calls queued behind a number
that never arrives. Reconnecting, fetching the root again and
registering callbacks again are the application's;
[`Reconnecting`](./reconnecting.md) does all three. A connection whose
handshake fails never joins the session. A server's callback
connection whose connection-init write fails is dropped alone, since
no frame rode it and the client's attach fails with it. A client's
attach, in either direction, ends the session once the server may hold
the connection: any failure after its header went out, except, for a
callback attach, a close (a reset included) before the first byte of
the server's `"cci"`; see `add_incoming_connection_with_config`. An
outgoing attach has no such exception, because libbinder
`android-16.0.0_r3` and later end their session when they refuse one
at the `setMaxThreads` cap, and the close that refusal produces looks
the same as any other; see `add_outgoing_connection_with_config`. A client's callback connection
whose serving thread cannot be started ends the session too, since the
server already holds it as a callback connection.

A serve loop — `RpcSession::serve_blocking` and its variants — returns
`rpc::SessionEnd`, not `Result<()>`. It answers three questions a
status code cannot hold at once:

- **`stream`** — is the stream still at a frame boundary
  (`StreamState::InSync`), or is its position lost (`Lost`: a cut
  frame, a TLS stream that ended without `close_notify`, a deadline
  that expired part-way through a frame)?
- **`by`** — did this end decide (`EndedBy::Local`: `close_session`,
  `terminate`, an idle deadline this end armed), or not (`NotLocal`)?
- **`reason`** — what happened (`EndReason`), for logs.

`SessionEnd::into_result()` is the one projection back to a `Result`:
`Ok(())` for an intact stream, `Err(DeadObject)` for a lost one. Only
`stream` decides it; `by` is attribution.

Each call on the ending side means one thing:

| call | does |
|---|---|
| `RpcSession::close_session()` | declare the session dead: every connection's transport is shut down, cached proxies get `binder_died`, the session's incoming threads are joined |
| `RpcServer::stop_accepting()` | raise the accept flag; connected sessions drain as their peers leave; nothing is joined |
| `RpcServer::terminate()` | stop accepting, close every session the server minted, join the workers |
| `ServerGuard::stop_and_join()` | `terminate`, after joining the accept thread — the same as dropping the guard |
| `RpcTransport::shutdown()` | the transport half-close itself (`shutdown(2)` in both directions): wakes a reader blocked on this end, fails this end's later sends |

What **this end's own reader** sees after its `shutdown` is
platform-dependent — Linux delivers frames already queued ahead of the
end of stream, macOS discards them — so code must not assume a queued
frame arrives, or that it does not. The peer reads the end of stream on
either platform; what differs is the peer's *next send* (Linux
`AF_UNIX`: `EPIPE` at once; macOS: accepted and discarded; TCP: a reset
on a later write).

### Timeouts

AOSP libbinder's RPC path has no timeouts: every socket wait is a
`poll` without a bound, and only a session shutdown ends it. Every
deadline below is an rsbinder addition, and each measures one thing.

| Setting | What it bounds | On expiry |
|---|---|---|
| `RpcSession::set_timeout(d)`; `RpcClientConfig::timeout`, `ClientOptions::timeout`; on a server `RpcServer::set_reply_timeout`, `ServeOptions::reply_timeout`, applied to every session it makes | How long the peer may leave this end without an answer: a reply wait; a send that makes no progress (`SO_SNDTIMEO`, every bundled socket transport; a transport of your own only if it implements `RpcTransport::set_write_timeout`); the peer's host not answering (the kernel's keepalive check, TCP and TLS over TCP); on a client, each connect and handshake step | The session ends; the call that waited returns `TimedOut`. A connect or handshake step: the setup or attach call returns `TimedOut`, and an attach whose step expires after its header went out ends the session ([How a connection ends](#how-a-connection-ends)) |
| The same `d`, for a free connection in the session's pool | A wait for a connection to send on; nothing has been sent yet | That call returns `WouldBlock`; the session goes on |
| `RpcServer::set_handshake_timeout`, `ServeOptions::handshake_timeout` (default 10 s) | An accepted connection that sends nothing before its first contact | The connection is dropped |
| `RpcServer::set_idle_timeout`, `ServeOptions::idle_timeout` | A session in which no byte crossed any connection and no call was open, judged per session ([`set_idle_timeout`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_idle_timeout) states the timing and what counts) | The session ends |

**Set the session timeout above the slowest legitimate handler.** It
is how long the peer may go without answering before it counts as
broken, not a budget for one call: a call that times out takes the
session with it, because the reply that is late may still come and
nothing could tell it apart from the next one. A caller that wants to
give up on one call and keep the session runs the call on another
thread, or as a future, and stops waiting for it. There is no default:
a library cannot know how long a handler may legitimately run, and
neither kernel binder nor libbinder bounds it.

**Keepalive is on for every TCP connection, TLS over TCP included.**
Without a session timeout it runs at the system's intervals, so a session whose peer host
vanished ends, and fires its death recipients, after hours instead of
never; `set_timeout(d)` sizes the check to `d`. The values, and what
Linux, Android and macOS each do with them, are stated once, in
[`RpcTransport::set_liveness`](https://docs.rs/rsbinder/latest/rsbinder/rpc/transport/trait.RpcTransport.html#method.set_liveness).
The probes are between the two kernels and change no byte on the wire,
so a libbinder peer needs nothing. Unix sockets and vsock, TLS over them
included, have no such check.

**A relay hides a break.** Keepalive reaches only the first TCP
endpoint on the path. Behind a relay that terminates TCP — `adb
forward`, `ssh -L`, a TLS terminator — the relay answers the probes,
and a break past it shows only when a call waits for a reply and the
session timeout expires. A stream that waits has no call outstanding,
so it pings its peer instead ([Streaming](./streaming.md#a-peer-that-goes-silent)).

**On a server the reply timeout is also the liveness bound.** A server
that never calls its clients back has no reply to wait for, so the
reply timeout reaches its clients only as keepalive and the send bound.
A server that wants a vanished client's session cleaned up sooner than
the system's keepalive sets `set_reply_timeout` for that.

**The idle timeout is for untrusted peers.** A peer that finishes the
handshake and then stays silent holds a worker thread and, under
`set_max_connections`, an admission slot; keepalive does not catch it
(its kernel answers) and neither does the reply timeout (it asks for
nothing). The idle timeout is for a server that admits unauthenticated
TCP or TLS peers and caps them; one that picks its peers with
`set_authorizer` or TLS client authentication needs it less. What
counts as idle, including a client that only waits for callbacks, is
stated once, in `set_idle_timeout`.

`RpcClientConfig::handshake_timeout` and
`ClientOptions::handshake_timeout` are deprecated: the client timeout
bounds connecting. A value still set there takes precedence for the
handshake.

## Bridging RPC and the service manager: the Accessor pattern

Android 16 introduced `IAccessor` — a kernel-binder interface whose
sole job is to hand a client a connected RPC socket fd. The client
asks the system service manager for the accessor (a kernel-binder
service), calls `addConnection()`, and the returned
`ParcelFileDescriptor` is the RPC socket the client then drives
through the android-13+ handshake.

rsbinder implements **both sides** of this pattern:

- **Consume side** — the shared `hub` lookup path (`hub::check_service`,
  `hub::wait_for_service`, and the typed/`try_*` variants) transparently
  follows the `IAccessor` arm: if the service manager hands back an
  `IAccessor` instead of a regular binder, rsbinder calls
  `addConnection()`, adopts the fd, runs the v2 handshake, and gives
  you back the RPC root. Your client code looks identical to a
  regular `hub::check_service` call.

- **Register side** — `hub::android_16::create_accessor(instance,
  addr_provider)` builds a `LocalAccessor` `BnAccessor` you can
  publish via `hub::add_service`. The provider closure resolves an
  instance name to an
  [`AccessorSockAddr`](https://docs.rs/rsbinder/latest/rsbinder/hub/android_16/enum.AccessorSockAddr.html)
  (`Unix(path)`, `UnixAbstract(name)` — added in 0.10.0 —
  `Vsock { cid, port }`, or `Inet(addr)`), and the accessor opens the
  connection on demand. The enum is intentionally exhaustive (not
  `#[non_exhaustive]`), so downstream exhaustive `match`es need an arm
  for the new `UnixAbstract` variant — see the 0.10.0 CHANGELOG:

  ```rust
  use rsbinder::hub::{self, android_16::{create_accessor, AccessorSockAddr}};
  use rsbinder::rpc::RpcServer;
  use rsbinder::ProcessState;
  use std::path::PathBuf;

  // 0. The kernel-binder side needs ProcessState to publish the
  //    accessor through the system service manager. The RPC server
  //    itself does NOT (it runs entirely in user space).
  ProcessState::init_default()?;
  ProcessState::start_thread_pool();

  // 1. Run a regular RPC server on a UDS.
  let sock = PathBuf::from("/data/local/tmp/my.sock");
  let server = RpcServer::setup_unix_server(&sock)?;
  server.set_android13plus(2);
  server.set_root(my_root)?;
  let _bg = server.run_background();

  // 2. Vend an IAccessor that hands clients an fd connected to
  //    `sock`, and publish it through the kernel service manager.
  let path = sock.clone();
  let accessor = create_accessor("my.service", Box::new(move |_name| {
      Ok(AccessorSockAddr::Unix(path.clone()))
  }));
  hub::add_service("my.service", accessor)?;

  // 3. Block on the kernel binder thread pool so the accessor stays
  //    reachable for the lifetime of the process.
  ProcessState::join_thread_pool()?;
  ```

Use
[`hub::android_16::add_accessor_provider`](https://docs.rs/rsbinder/latest/rsbinder/hub/android_16/fn.add_accessor_provider.html)
when you want a **process-local** accessor — one that doesn't go
through the system service manager. Lookups via `hub::check_service` /
`hub::wait_for_service` in the same process fall back to the
process-local registry when the service manager returns nothing.

Both sides of the Accessor pattern have been validated against real
Android 16 `libbinder` on the emulator.

The Accessor solves one direction — a **kernel** client reaching an RPC
service. For the reverse, and for the rule that binder objects never cross
between the two stacks, see
[Cross-Transport Services § Bridging two transports in one process](./cross-transport-services.md).

## Async over RPC

The blocking I/O the RPC stack uses is **deliberate** — it matches
the AOSP `RpcServer`/`RpcSession` model, where each connection has its
own pair of blocking threads. The standard
`spawn_blocking`/`block_on` adapters that already wrap kernel-binder
async work for RPC too:

```rust
use rsbinder::Tokio;

let session = RpcSession::setup_unix_client(RPC_SOCKET)?;
let root = session.get_root()?;
let hello: Strong<dyn IHello> =
    <dyn IHello as FromIBinder>::try_from(root)?;

// Convert the blocking proxy into the async one.
let async_hello = hello.into_async::<Tokio>();

let reply = async_hello.echo("hi").await?;
```

For services, `Bn*::new_async_binder(impl, TokioRuntime(handle))`
works exactly like it does on the kernel path. See
[Async Service](./async-service.md).

There is intentionally **no** non-blocking `RpcTransport` /
reactor-based async serve loop: async support is these adapters over
the blocking stack, not a second I/O engine underneath it.

## Platform support

| Platform | Kernel binder | RPC |
|----------|---------------|-----|
| Linux    | Yes (with binderfs) | Yes (Unix, vsock, TLS) |
| Android  | Yes (built in)      | Yes |
| macOS    | **No**              | Yes (Unix, TLS — for development & cross-stack interop testing) |
| Windows  | **No**              | **No** (untested) |

macOS support for RPC means you can develop, run, and test
RPC-only rsbinder applications on a macOS host without needing a
Linux VM. The kernel binder code paths remain Linux/Android only.

## Further reading

- [Async Service](./async-service.md) — applying the
  `into_async::<Tokio>()` and `new_async_binder` patterns over RPC.
- [Callbacks and Interfaces](./callbacks-and-interfaces.md) — nested
  binders and death recipients work the same over RPC as kernel
  binder.
- [ParcelFileDescriptor](./parcel-file-descriptor.md) — FD passing
  with the android-14+ wire and `SCM_RIGHTS`.
- `rsbinder::rpc` module on [docs.rs/rsbinder](https://docs.rs/rsbinder)
  for the full API surface (every public function, struct, and trait
  introduced above is documented in detail there).
