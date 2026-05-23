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
> rsbinder = { version = "0.8", features = ["rpc"] }
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
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // No ProcessState, no hub. RPC never touches the kernel binder.
    let _ = std::fs::remove_file(RPC_SOCKET);
    let server = RpcServer::setup_unix_server(RPC_SOCKET)?;

    // Publish a root binder. Clients fetch this via get_root().
    server.set_root(BnHello::new_binder(IHelloService {}).as_binder());

    // Accept loop runs on this thread until shutdown().
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
  equivalent of `hub::get_interface(name)` for kernel binder, just
  without a service manager.
- **`server.run()`** runs the accept loop until
  [`RpcServer::shutdown`] is called. Use
  [`RpcServer::run_background`] to spawn it on a dedicated thread
  instead.

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

Mixing `set_root` with `add_service` on the same server is **not**
supported — `add_service` rebuilds the root each call to keep the set
consistent. Pick one publishing model per server.

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
server.set_root(my_root);

// Client: offer up to v2. Negotiation picks min(client, server).
let session = RpcSession::setup_unix_client_android13plus(
    "/tmp/foo.sock", 2,
)?;
```

Both stacks have been validated end-to-end against **real AOSP
`libbinder`** on Android 13/14/15/16 emulators (full parcel-body
transact, byte-correct), so an rsbinder RPC server can serve a real
Android `libbinder` client and vice versa.

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
rsbinder = { version = "0.8", features = ["rpc", "rpc-vsock"] }
```

Each backend implements the
[`RpcTransport`](https://docs.rs/rsbinder/latest/rsbinder/rpc/transport/trait.RpcTransport.html)
trait — you can implement your own if you need a custom carrier.

### TLS example

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

## Security

> **RPC is not a drop-in for kernel binder's security model.** Kernel
> binder gives you `getCallingUid()` and SELinux for free. RPC does
> not.

Each transport defines its own trust boundary, surfaced as a
[`PeerIdentity`](https://docs.rs/rsbinder/latest/rsbinder/rpc/enum.PeerIdentity.html):

- **`Local { uid, pid }`** — Unix socket peer with kernel-vouched
  credentials.
- **`Vsock { cid }`** — VM context id (a routing address, **not** an
  ACL basis on its own — the trust comes from hypervisor isolation).
- **`Certificate(CertId)`** — TLS peer authenticated by its leaf cert.
- **`Anonymous`** — no identity at all. ACL is impossible against an
  anonymous peer. Only the debug TCP backend returns this.

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

## Capabilities

The RPC stack is feature-complete for the everyday cases you'd use
kernel binder for, with a few extras specific to socket transport:

- **AIDL interfaces** — every type the AIDL compiler supports works
  over RPC: primitives, strings, arrays, parcelables, nested
  interfaces, enums, unions, oneway methods.
- **Callbacks (nested binders)** — a callback object created on the
  client crosses the socket like any other Binder and the server
  invokes it back through the same session.
- **`ParcelFileDescriptor`** — opt in with
  `RpcSession::negotiate_fd_transport` and
  `RpcServer::set_supported_fd_modes`. File descriptors ride
  out-of-band over `SCM_RIGHTS` on Unix-domain sockets (android-14+
  wire required).
- **Death notifications** — link a `DeathRecipient` on the proxy as
  usual. A session whose socket disconnects fires every linked
  recipient (the RPC analogue of "the remote process died").
- **Async** — the same `into_async::<Tokio>()` adapter that wraps a
  blocking kernel-binder proxy works over RPC. See
  [Async Service](./async-service.md) — for RPC the only difference is
  how you obtain the proxy.

### Sessions with multiple connections

`RpcServer::set_max_threads(N)` matches AOSP's
`setMaxIncomingThreads`. The default `N == 1` (one connection per
session) is fully supported and validated against real Android 13–16
libbinder peers — this is the mode every example in the book
uses.

**`N ≥ 2` (multi-connection sessions) is currently experimental.**
The hermetic rsbinder↔rsbinder tests pass, but the real-libbinder
interop gate has not yet been cleared, so production code that needs
to interoperate with a real Android `libbinder` peer should stay on
the default. See the rustdoc on
[`RpcServer::set_max_threads`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_max_threads)
for the current status.

`set_max_threads` caps *incoming slots per session*. To cap
*server-wide concurrent connections* — for example to bound the
worker-thread fan-out regardless of how many sessions a single client
opens — use
[`RpcServer::set_max_connections(N)`](https://docs.rs/rsbinder/latest/rsbinder/rpc/struct.RpcServer.html#method.set_max_connections)
(default: unlimited). Both knobs are independent and additive.

## Bridging RPC and the service manager: the Accessor pattern

Android 16 introduced `IAccessor` — a kernel-binder interface whose
sole job is to hand a client a connected RPC socket fd. The client
asks the system service manager for the accessor (a kernel-binder
service), calls `addConnection()`, and the returned
`ParcelFileDescriptor` is the RPC socket the client then drives
through the android-13+ handshake.

rsbinder implements **both sides** of this pattern:

- **Consume side** — `hub::get_service(name)` transparently follows
  the `IAccessor` arm: if the service manager hands back an
  `IAccessor` instead of a regular binder, rsbinder calls
  `addConnection()`, adopts the fd, runs the v2 handshake, and gives
  you back the RPC root. Your client code looks identical to a
  regular `hub::get_service` call.

- **Register side** — `hub::android_16::create_accessor(instance,
  addr_provider)` builds a `LocalAccessor` `BnAccessor` you can
  publish via `hub::add_service`. The provider closure resolves an
  instance name to an
  [`AccessorSockAddr`](https://docs.rs/rsbinder/latest/rsbinder/hub/android_16/enum.AccessorSockAddr.html)
  (`Unix(path)`, `Vsock { cid, port }`, or `Inet(addr)`), and the
  accessor opens the connection on demand:

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
  server.set_root(my_root);
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
through the system service manager. Lookups via `hub::get_service` in
the same process fall back to the process-local registry when the
service manager returns nothing.

Both sides of the Accessor pattern have been validated against real
Android 16 `libbinder` on the emulator.

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
reactor-based async serve loop. Decision record:
[`plans/2-10-async-rpc-io.md`](https://github.com/hiking90/rsbinder/blob/master/plans/2-10-async-rpc-io.md)
in the repo.

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
