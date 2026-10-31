# Migrating from 0.12 to 0.13

Every change in 0.13.0 that can break a program built against 0.12.0: a
compile error or behavior a running program can observe. Entries are grouped
by area. None of them changes the bytes between peers.

The additions and fixes themselves are listed in
[`CHANGELOG.md`](https://github.com/hiking90/rsbinder/blob/master/CHANGELOG.md);
a reference such as "(CHANGELOG *Fixed*)" points to the 0.13.0 section there.

## Typed casts and interface descriptors

- `Debug` of an `SIBinder`, `dyn IBinder` or `ProxyHandle` prints a
  descriptor it cannot learn without a transaction as `(not cached)`: a
  kernel or RPC proxy whose descriptor nothing has asked for yet (CHANGELOG
  *Changed*). 0.12.0 printed a kernel proxy's, which it always had, and an RPC
  proxy's first typed cast's descriptor, or an empty string before any.
- **A typed cast does not ask a proxy for its interface.**
  `SIBinder::into_interface`, `FromIBinder::try_from`, `Strong::try_from`
  and reading a `Strong<dyn I>` from a parcel convert a remote binder, kernel
  or RPC, of any interface, as AOSP C++ `interface_cast` and Java
  `asInterface` do; so do `Strong::clone`, `Weak::upgrade`, `into_sync` and
  `try_into_async`, which cast again. A remote object of another interface
  refuses the first two-way call: an AIDL-generated C++, NDK, Rust or
  rsbinder server answers `BadType`, a Java server `EX_SECURITY`
  (`Parcel.enforceInterface` throws `SecurityException`). A one-way call to
  it is dropped without an error. 0.12.0 compared a kernel proxy's
  descriptor, asked for when the binder was read, and returned `BadType`
  from the cast; an RPC proxy took the interface of its first typed cast,
  for every later reader of that address, and refused a cast to another
  with `BadType`. Code that cast to learn whether a binder implements an
  interface calls the new `SIBinder::into_interface_checked`. The service
  manager lookups (`hub::check_interface`, `try_get_interface`,
  `wait_for_interface`, their async forms), `connect`, `Client::get` and
  `try_get`, `RpcSession::get_interface`, `Reconnecting` and
  `rsbinder_topic::registry::RegistryResolver::new` cast with it and keep
  their `BadType`. Over RPC they now ask the remote, so a root or named
  service of another interface is refused where 0.12.0 accepted it and
  failed its calls: `connect` and `RpcSession::get_interface` return
  `BadType`, and `Reconnecting` treats it as a permanent error, so `build`
  fails or the helper closes, where 0.12.0 connected and each call returned
  `BadType`.
- `FromIBinder` has a required associated function,
  `interface_descriptor`, which `into_interface_checked` compares with the
  remote's answer. Generated code implements it; a hand-written `impl`
  returns its interface's descriptor.
- `Binder::<B>::try_from` of a remote binder returns `BadValue` (not a local
  binder) unless the proxy's descriptor is already known and differs, which
  is `BadType`. 0.12.0 returned `BadType` whenever the descriptor the proxy
  held differed: a kernel proxy's, asked for when it was read, or an RPC
  proxy's from its first typed cast, `""` before any, so an RPC proxy no
  typed cast had reached was `BadType` for every `B`.
- `cancel::cancel_remote` sends `cancel()` to a proxy, kernel or RPC, whose
  descriptor nothing has asked for; a binder of another interface drops
  that one-way call. A known other interface is still `BadType`. 0.12.0
  refused a kernel proxy of another interface, and an RPC proxy a typed
  cast had taken for another interface, with `BadType`. For the same
  binders, `BpMemoryHeap::map` and `BpMemory::resolve` get the server's
  refusal of the interface token (see the typed-cast item) rather than
  `BadType` from this process. A binder whose remote answered an empty
  descriptor is refused with `BadType` once that answer is known, where
  0.12.0 sent the call (shared memory over kernel or RPC; `cancel_remote`
  and streams over RPC).
- `descriptor()` of a kernel proxy (`IBinder::descriptor`,
  `ProxyHandle::descriptor`) and `ProxyHandle::prepare_transact(true)`, which
  writes it as the token, make a two-way `INTERFACE_TRANSACTION` when
  nothing has asked for the descriptor yet (CHANGELOG *Changed*). That call
  follows the calling thread's `CallRestriction`, so on a `FatalIfNotOneway`
  thread it panics. A query that fails makes `descriptor()` return `""` and
  `prepare_transact(true)` return the error, such as `DeadObject`. 0.12.0
  asked when the binder was read, with the restriction lifted for that
  call, and these functions read the stored answer: they neither panicked
  nor failed. Ask for the descriptor before setting `FatalIfNotOneway`, and
  call `try_descriptor()` where the error matters.
- `descriptor()` of an RPC proxy (`IBinder::descriptor`) and
  `RpcProxy::prepare_transact(true)` make a two-way `INTERFACE_TRANSACTION`
  when nothing has asked for the descriptor yet, as on kernel binder but
  without the `CallRestriction` (as in AOSP). Like any two-way RPC call it
  needs a connection to send on: when none becomes free within the session's
  timeout, or the session has no outgoing connection, it fails with
  `WouldBlock`: `descriptor()` returns `""` and logs it, and
  `try_descriptor()` and `prepare_transact(true)` return the error. A reply
  deadline that passes ends the session. 0.12.0 made no call: `descriptor()`
  returned the descriptor of the proxy's first typed cast or `""`, and
  `prepare_transact(true)` wrote that as the token.
- `__rpc_stamp_descriptor` is removed. It was `#[doc(hidden)]`, called by
  generated code from the same rsbinder; code that called it by hand to
  have `prepare_transact(true)` write a token drops the call, and
  `prepare_transact(true)` asks the remote for it.

## Service manager lookups

- **`hub::register_for_notifications`, `unregister_for_notifications`,
  `register_client_callback` and `try_unregister_service` return
  `BinderResult<()>`**, as `add_service` does, and so do the
  `ServiceManager` methods and the `hub::android_N` functions of those
  names. The service manager's exception reaches the caller: a refused name
  is `EX_SECURITY` rather than `StatusCode::FailedTransaction`. `?` into a
  `StatusCode` or `Box<dyn Error>` function still compiles; a `match` on a
  `StatusCode` becomes one on `exception_code()` or `transaction_error()`.
  On Android 10 all four return `EX_UNSUPPORTED_OPERATION`.
- **`hub::try_get_service` and `try_get_interface` return
  `BinderResult<Option<_>>`** (`Status`, not `StatusCode`), as every
  `hub::try_*` lookup does; `Err(StatusCode::X)` patterns become
  `Err(s) if s.transaction_error() == StatusCode::X`, and a denied lookup
  shows as `EX_SECURITY`. **`get_interface_async` is deprecated** in favour
  of `try_get_interface_async`, which is `Ok(None)` for a name that is not
  registered: `try_get_interface_async::<T>(name).await?.ok_or(StatusCode::NameNotFound)?`.
- The service manager lookups return the binder of a service whose process
  has died: `check_service` and `wait_for_service` return `Some`,
  `try_get_service` `Ok(Some)`, and the binder's first call returns
  `DeadObject`; `check_interface`, `wait_for_interface` and
  `try_get_interface` return `Err(DeadObject)` from the checked cast.
  0.12.0 failed reading the binder from the reply: `check_service` and
  `wait_for_service` returned `None`, `try_get_service` and
  `try_get_interface` the read's error (`Ok(None)` on Android 10), and
  `check_interface` and `wait_for_interface` `Err(NameNotFound)`.
- `hub::default` takes the context object (handle 0) as the service manager
  without asking for its descriptor, so a context manager that is not an
  `IServiceManager` is accepted and its calls fail as that object answers
  them. 0.12.0 returned `BadType` from `hub::default`.

## Binder objects and process state

- **`IBinder` no longer has `inc_strong`, `attempt_inc_strong`,
  `dec_strong`, `inc_weak` and `dec_weak`.** They counted nothing that
  decided a lifetime (the `Arc` does), and a stray `dec_strong(None)`
  panicked the next `SIBinder` drop in a debug build. Delete them from a
  hand-written `IBinder` impl; to release a binder, drop it
  (`service.dec_weak()?;` becomes `drop(service);`). `rpc_transact`, which
  skips `check_interface`, now takes an argument only rsbinder can build.
  `set_parceled` stays.
- **`ProcessState::init`, `init_with_mmap_size` and `init_default` return
  `Result<&'static ProcessState, StatusCode>`** instead of
  `Box<dyn Error>` (`?` into `Box<dyn Error>` still works). A second call
  with a different driver path, `max_threads` or mmap size returns
  `BadValue` instead of `Ok` with the earlier state (AOSP `initWithDriver`
  aborts); the same settings again stay `Ok`. `driver_name()` and
  `max_threads()` read the settings in force. A missing driver node is
  `NameNotFound`, a failed ioctl or mmap its errno status, a protocol
  version mismatch `NoInit`.
- **`rsbinder::thread_state` and `shared_memory::{heap, dealer, shared,
  wire}` are private modules**: `rsbinder::thread_state::X` becomes
  `rsbinder::X` (`rsbinder::CallingContext`, `rsbinder::get_calling_uid`),
  `rsbinder::shared_memory::heap::MemoryHeapBase` becomes
  `rsbinder::shared_memory::MemoryHeapBase`. The generated
  `cancel::android::os::…` and `permission_controller::android::os::…`
  modules are private too; their items stay at `rsbinder::cancel::*` and
  `rsbinder::permission_controller::*`. `__declare_binder_interface!` is
  `#[doc(hidden)]`.
- **`impl From<rustix::io::Errno> for StatusCode` is removed**, so `rustix`
  is in neither crate's public API. Convert an errno with
  `StatusCode::from(std::io::Error::from_raw_os_error(n))`; the mapping is
  unchanged.
- **`ParcelableHolder::new(Stability)` is deprecated; use
  `ParcelableHolder::local()` or `vintf()`.** A holder's wire stability is
  LOCAL or VINTF only: `new(Stability::Vendor)` and `new(Stability::System)`
  now panic (they wrote LOCAL while behaving otherwise, and `Vendor` refused
  non-VINTF parcelables). Generated code uses `vintf()`.
- **`TransactionObserver`'s tag is `Option<Box<dyn Any>>`**, not
  `Option<Box<dyn Any + Send>>`: `on_transact` and `on_reply` run on the
  serving thread, so the tag may be a thread-bound guard; a
  `Box<dyn Any + Send>` still coerces. `TracingObserver` keeps the entered
  span as its tag.
- **`ServeOptions::threads` and `binder://?threads=` take effect on a
  running process**: `run` / `spawn` apply a ceiling that differs from the
  one in force through the new `ProcessState::set_max_threads`
  (`BINDER_SET_MAX_THREADS`, AOSP `setThreadPoolMaxThreadCount`), where
  0.12.0 returned `BadValue`. A started pool may grow but not shrink
  (`InvalidOperation`), and the two given with different values is
  `BadValue`. `ServeOptions::mmap_size` is still compared only, as a
  mapping cannot be resized.
- `RemoteProxy` has a required method, `prepare_request`. A type outside
  rsbinder that implements the trait must add it. Generated proxies reach
  the trait only through `as_remote`, which returns `ProxyHandle` and
  `RpcProxy` alone, so such a type was never driven by them.
- **The `Binder` constructors panic on a scheduler floor AOSP refuses.**
  `BinderFeatures::min_sched_policy` must be `SCHED_NORMAL` (0),
  `SCHED_FIFO` (1) or `SCHED_RR` (2), and `min_priority` within `-20..=19`
  for `SCHED_NORMAL` or `1..=99` for the two real-time policies; `None`
  stands for `SCHED_NORMAL` and `0`. AOSP `BBinder::setMinSchedulerPolicy`
  aborts on the same values. 0.12.0 masked the policy to two bits and the
  priority to one byte, so `SCHED_IDLE` (5) reached the driver as
  `SCHED_FIFO` and a priority of 300 as 44; `SCHED_BATCH`, which the
  0.12.0 documentation allowed, is now refused.
- **`ProcessState::start_thread_pool` leaves the pool unstarted when its
  first worker cannot be created**, so a later call tries again; 0.12.0
  marked the pool started and every later call did nothing. Opening a
  `binder://` `Client` and starting a kernel `Server` now return the
  failure as an `Err` (`try_start_thread_pool`, CHANGELOG *Added*) instead
  of succeeding with no pool.

## Calling identity and permissions

- **`CallingContext::default()` is gone; use `CallingContext::current()`.**
  `Default` read the caller at construction, so a `#[derive(Default)]`
  struct or an `unwrap_or_default()` recorded whichever caller happened to
  be current. `CallingContext` is `#[non_exhaustive]`.
- **`permission_controller::check_permission` decides kernel or RPC from
  `calling_caller()`, not from the parcel** (CHANGELOG *Security*). Its
  `reader` argument is no longer consulted. Inside an RPC transaction the
  check denies whatever parcel it is given; inside a kernel transaction it
  asks `PermissionManagerService` about that transaction's caller.
  Generated `@EnforcePermission` code is not affected: it passes the
  inbound parcel.
- **`clear_calling_identity()` outside a kernel transaction, and during an
  RPC dispatch, returns the token of this process's own uid and pid**, as
  AOSP does, instead of `0` (CHANGELOG *Security*). It still changes nothing
  there. Code that compared the token with `0` to tell whether it cleared
  anything compares `has_explicit_identity()` instead.

## Local binders and async

- **A sync-only local service converts to its async view.** `into_async`,
  `try_into_async` and the async `FromIBinder` cast of a binder published
  with `Bn*::new_binder` give the generated async proxy over the local
  binder, as AOSP's Rust backend does: each call is serialized into a
  parcel and runs the service's dispatch in this process. 0.12.0 refused
  the cast with `BadType` and `into_async` panicked. In a process without
  kernel binder (`ProcessState` not initialized) the cast is
  `InvalidOperation` and `into_async` panics saying so. A local binder of
  another `Remotable` that answers with the interface's descriptor gets
  the same in-process proxy, sync or async, where 0.12.0 refused it with
  `BadValue`. Off any transaction such a call has the new caller
  `Caller::InProcess { uid, pid }`, this process's own, and
  `@EnforcePermission` decides it for that identity; inside a handler it
  keeps the outer caller. The calling thread's strict-mode policy and work
  source are restored after the dispatch. `Strong::into_async` lists the
  four call semantics of one local object and what the in-process path
  does not do (no `DeadObject` mapping, no observer, `link_to_death` is
  `InvalidOperation`).
- **A generated async proxy runs its transact inline on a thread that is
  handling a transaction, whatever its `BinderAsyncPool`**, and
  `Tokio::spawn` always hands off to `spawn_blocking`. A pool written as
  the trait doc says keeps the kernel's deadlock avoidance and the RPC
  nested-call pin. Code that called `<Tokio as BinderAsyncPool>::spawn`
  directly inside a handler and relied on it running inline checks
  `is_handling_transaction()` itself.

## Code generation (`rsbinder-aidl`, macros)

- **`rsbinder` requires `rsbinder-aidl` with `=`.** Generated code calls
  hidden items of the `rsbinder` it was generated for, so another
  `rsbinder-aidl` patch in the same build no longer resolves. Interface
  code from an older generator keeps 0.12.0's `BadType` for the async cast
  of a sync-only local service.
- **A published service never reports a version above its generated
  module's `VERSION`.** The generated `on_transact` for
  `getInterfaceVersion` and `impl IFoo for Binder<BnFoo>` answer
  `min(service, VERSION)`, and `getInterfaceHash` answers the module's own
  `HASH` when the service's version was above it. A gateway built from an
  older `.aidl` than its upstream reported the upstream's version, whose
  newer methods its own `on_transact` cannot dispatch. The client-side
  `impl IFoo for Strong<dyn IFoo>` still reports the upstream's values.
- **A nested `.aidl` type named `{Interface}Delegate` is refused**: it
  collides with the generated delegate trait (CHANGELOG *Added*).
- **`declare_binder_interface!` is hidden from the docs**: it is for
  generated code; write interfaces with `#[rsbinder::interface]` or
  `.aidl`. Its `native` block takes `adapter:` and `r#async:` together or
  neither; one of them alone, or an async interface without them, is a
  `compile_error!` naming what is missing.

## RPC transport

- **The items deprecated in 0.12.0, and `Parcel::is_for_rpc` (deprecated in
  0.11.0), are removed**, as the 0.12.0 CHANGELOG announced. Each has a
  replacement 0.12.0 already had:

  | Removed | Use |
  |---|---|
  | `rpc::RpcUnixClientConfig::path(p, v)` / `abstract_name(n, v)` | `RpcClientConfig::unix(p, v)` / `unix_abstract(n, v)` |
  | `RpcSession::setup_unix_client_android13plus_with_config(cfg)` | `RpcSession::setup_client_android13plus_with_config(RpcClientConfig::unix(..))` |
  | `RpcSession::setup_unix_client_android13plus_fan_out(p, v, n)` | `setup_client_android13plus_with_config(RpcClientConfig::unix(p, v).outgoing_connections(n))` |
  | `RpcSession::setup_unix_client_android13plus_with_id(p, v, id)`, `ClientOptions::session_id` | set up a new session, then `add_outgoing_connection()` on it |
  | `RpcSession::connect_android13plus_fd_with_id(t, v, mode, id)` | `RpcSession::connect_android13plus_fd(t, v, mode)` |
  | `RpcSession::add_outgoing_connection_android13plus(..)` and `add_outgoing_connection_android13plus_with_config(cfg)` | `add_outgoing_connection()` on a session set up from an `RpcClientConfig` |
  | `RpcSession::add_incoming_connection_android13plus_with_config(cfg)` | `add_incoming_connection()` |
  | `RpcClientConfig::handshake_timeout(d)`, `ClientOptions::handshake_timeout` | `timeout`, which bounds each connect and handshake step too |
  | `rpc::EndReason::Unreadable`, `EndReason::Retired` | nothing: neither was produced; a serve loop reports `EndReason::SessionEnded` |
  | `Parcel::is_for_rpc()` | `!parcel.is_kernel_backed()` |

  A session id was already refused with `BadValue` by every setup call that
  took one, so only code that named these items stops compiling.

- **`RpcServer::run_background` returns `Result<RpcServerHandle>`, and
  dropping the handle stops the server.** The handle is `#[must_use]`; its
  drop, or `stop_and_join()` (which also returns how the accept loop
  ended), raises `stop_accepting`, joins the accept thread and calls
  `terminate`, which ends every session and joins its workers. 0.12.0
  returned a `JoinHandle<()>` whose drop detached the loop, and the server
  kept accepting after its last `Arc` went. `let bg =
  server.run_background();` becomes `let bg = server.run_background()?;`,
  and `server.stop_accepting(); bg.join()` becomes `bg.stop_and_join()?`.
  **`let _ = server.run_background();` still compiles without a warning
  and now stops the server on that line**: bind the handle to a named
  variable or keep it beside the server. A thread the OS refuses is an
  `Err` instead of a panic; `Server::spawn` returns it too, and
  `ServerGuard` wraps the handle.
- **A zero RPC deadline is refused with `BadValue`.**
  `RpcSession::set_reply_timeout` and `RpcServer::set_reply_timeout`,
  `set_idle_timeout` and `set_handshake_timeout` return `Result<()>` and
  refuse `Some(Duration::ZERO)`, keeping the value in force. A zero
  `RpcClientConfig::timeout` is
  refused by the setup call, a zero `ClientOptions::timeout` by
  `Client::open`, and a zero `ServeOptions` deadline by `Server::run` /
  `spawn`, all before connecting or registering a name. 0.12.0 read a zero
  reply or idle deadline as "no deadline" and a zero handshake deadline as
  "keep the 10 s default". `None` is still no deadline.
- **`RpcSession::set_timeout` is `set_reply_timeout`**, the name the server
  gives the same deadline; `set_timeout` is a deprecated alias, also
  returning `Result<()>`.
- **`RpcServer::set_max_threads` returns `Result<()>`.** It, `set_root` and
  `add_service` return `InvalidOperation` once `run` or `run_background`
  has started (AOSP `RpcServer` aborts there), and a server has either a
  `set_root` root or an `add_service` directory: whichever comes second is
  `InvalidOperation`. 0.12.0 let `add_service` replace a root silently and
  took changes that sessions already accepted never saw.
  `set_max_connections` still changes while the server runs.
- **`RpcClientConfig` has no lifetime parameter and owns its source**
  (`unix(impl AsRef<Path>, ..)`, `unix_abstract(impl AsRef<[u8]>, ..)`,
  `tls(impl Into<String>, port, impl Into<String>, ..)`), and the
  connector of `new(.., connect)` is `'static`. The session keeps it:
  **`RpcSession::add_outgoing_connection()` and
  `add_incoming_connection()`** open one more connection as the founding
  one was opened, with the session's own id, wire version and fd mode.
  `add_outgoing_connection_with_config`,
  `add_incoming_connection_with_config` and `RpcClientConfig::session_id`
  are deprecated. A session built from a transport (`connect_android13plus`,
  `from_preconnected_fd`, r34) has no connector, so the new calls return
  `InvalidOperation` there.
- **`RpcSession::session_id()` and `serve_blocking_on` are no longer
  public.** The client-local id was never the one to echo; the server's is
  `get_session_id()`.
- **`RpcTransport::send_raw` and `recv_raw` are required; `send_frame` and
  `recv_frame` have default implementations** (a `u32` little-endian
  length and the body, through the raw calls). Every session moves its
  bytes through the raw calls, so a transport that implemented only the
  frame methods compiled in 0.12.0 and could open no session.
- **`RpcSession::new` and the bundled transport constructors
  (`UnixTransport`, `TcpDebugTransport`, `TlsTransport`, `VsockTransport`)
  return `rsbinder::Result`**, not `RpcResult`; a `.map_err(StatusCode::from)`
  after them can go, and a TLS handshake cut by its deadline is `TimedOut`.
  The `RpcTransport` methods keep `RpcResult`.
- **`RpcProxy::build_request` and `transact` are deprecated** in favour of
  the `RemoteProxy` methods they duplicated, `prepare_request` and
  `submit_transact`.
- **`rsbinder::rpc::{address, end, fd_mode, proxy, server, session}` are
  private modules**; everything public in them is reached through
  `rsbinder::rpc::*` (`rpc::session::RpcSession` becomes `rpc::RpcSession`).
  `RPC_SESSION_ID_NEW`, `SpecialTransaction`, `RpcAddress::unique` and
  `transport::insecure_warning_emitted` are gone from the API, and
  `RpcSession::accept_android13plus`, `accept_android13plus_fd`,
  `local_node_count` and `RpcServer::{live_session_node_count,
  session_live_conns, session_slot_count}` are `#[doc(hidden)]` behind the
  `test-util` feature.
- **A refused TLS certificate has its own status code** on Linux and
  Android: `StatusCode::Errno(-EKEYREJECTED)` (untrusted issuer, bad
  signature, wrong name, no certificate; the peer's `unknown_ca`,
  `access_denied`, `certificate_required` or `unsupported_certificate`
  alert), `-EKEYREVOKED` (revoked) or `-EKEYEXPIRED` (expired, not yet
  valid, an expired CRL; `certificate_expired`), where 0.12.0 returned
  `Unknown`, as AOSP still does. Other TLS failures stay `Unknown`. A rustls
  failure after the handshake is `RpcError::Io(InvalidData)` carrying the
  `rustls::Error`, as a handshake failure is, not `RpcError::Protocol`. An
  rsbinder TLS server half-closes and drains the client (at most 500 ms and
  64 KiB) after a failed handshake, so a TLS 1.3 client whose certificate
  was refused reads the alert instead of `DeadObject`. `Reconnecting::build`
  fails on a rejected or revoked certificate; the background thread keeps
  retrying it and reports it in `last_error()`; an expired one is never
  permanent.
- **With `rpc-tcp-debug`, an `IAccessor` connection that is an inet socket
  is refused with `BadType`** unless the process calls
  `rpc::set_accessor_plaintext_inet(true)`. The feature is unified across
  the build, so a debug feature enabled anywhere changed what a plain
  `get_service` accepted. `RpcSession::from_preconnected_fd` called
  directly is unaffected: its caller picked the socket.
- **A call from another thread on an RPC session whose every outgoing
  connection is held by a serve loop fails at once with `WouldBlock`.**
  That is a client session served with `serve_blocking` or `spawn_serve`
  and no fan-out connections. 0.12.0 waited for the slot under the session
  timeout, without end when none was set, although the loop gives the slot
  up only after a frame from the peer. A nested call from inside a handler
  still uses the serve loop's connection. A client that keeps calling opens
  incoming connections (`RpcClientConfig::incoming_connections`,
  `ClientOptions::incoming_connections`); the `link_to_death` refusal
  message now says when `spawn_serve` fits.

## Streams and topics

- **`stream::Receiver::recv_timeout` returns `Result<T, RecvTimeoutError>`
  and `try_recv` returns `Result<T, TryRecvError>`**, the shape of
  `std::sync::mpsc`: `Err(Timeout)` / `Err(Empty)` while the stream runs,
  `Err(Ended)` on every call once it is over, and `Err(Failed(status))`
  once for a stream that ended in error (where `recv` yields
  `Some(Err(status))` once before `None`). 0.12.0 returned `Ok(None)` for
  both a timeout and an end, so a loop that called again on `Ok(None)`
  spun after the end. `recv`, `recv_async` and the `Iterator` are
  unchanged.
- **`stream::ReceiverPolicy` and `stream::SinkPolicy` are
  `#[non_exhaustive]`** with a `with_*` setter per field:
  `ReceiverPolicy { ring_use: x, ..Default::default() }` becomes
  `ReceiverPolicy::default().with_ring_use(x)`. `entry::WireProfile` is
  `#[non_exhaustive]`, and so is the `PeerIdentity::Local` variant: match
  `PeerIdentity::Local { uid, .. }`, build it with
  `PeerIdentity::local(uid, pid)`.
- `stream::Sink::open` asks no sink for its interface, so opening a stream
  in a binder handler never waits on the consumer. A sink whose interface
  is not known and is another one is accepted: its server refuses the
  one-way `onStart` and batches, and the producer waits for credit that does
  not come. 0.12.0 refused with `BadType` a kernel sink of another
  interface, having asked for its descriptor when the endpoint was read, and
  an RPC sink a typed cast had taken for another interface.
- A stream consumer accepts an `onStart` from a source whose interface
  nothing has asked for and is another one: that server drops the
  consumer's one-way `request` and `cancel`, and the consumer waits for
  batches that do not come. 0.12.0 ended the consumer's stream with
  `BadType` for a kernel source of another interface, having asked for its
  descriptor when the source was read, and for an RPC source a typed cast
  had taken for another interface.
- A topic registry (`rsbinder_topic::registry::Registry`) lists a binder of
  another interface that a client advertises; a subscriber of that name is
  refused with `BadType`. 0.12.0 refused the `advertise`.

## Fast Message Queue (`rsbinder-fmq`)

- **`MessageQueue::begin_write`, `begin_write_cached` and `begin_read`
  return a transaction, `WriteTxn` or `ReadTxn`; `commit_write`,
  `commit_write_cached` and `commit_read` are gone.** The transaction gives
  `write_at` (a `WriteTxn`) or `read_at` (a `ReadTxn`) and is committed with
  `commit()` (all of it) or `commit_first(n)` (a prefix):
  `if let Some(mut r) = q.begin_write(n)? { r.write_at(0, x)?; } q.commit_write(n)?;`
  becomes
  `if let Some(mut txn) = q.begin_write(n)? { txn.write_at(0, x)?; txn.commit()?; }`.
  A transaction dropped without a commit publishes or frees nothing (AOSP's
  Rust `WriteCompletion` commits the whole reservation on drop, which
  publishes elements never written). A commit cannot exceed its own
  reservation (`commit_first(n)` above `len` is `BadValue`), and one that
  finds this side's counter moved since its begin, or the peer's moved
  back, is `Corrupted`; 0.12.0 returned `BadValue("commit exceeds …")`
  there and accepted a commit with no begin.
- **`MessageQueue::write` and `read` return `Result<Transfer>`**
  (`Transfer::Done` / `Transfer::WouldBlock`, `is_done()`), not
  `Result<bool>`. `Transfer` is `#[must_use]`, so `q.write(&x)?;`, which
  loses the items of a write that would block, now warns.
- **`write_blocking` and `read_blocking` take the items and the timeout
  only**, waiting on and waking the default bits; other bits go through
  `write_blocking_with` / `read_blocking_with` and
  `Notify { wait, wake }`. `write_blocking(items, NOT_FULL, NOT_EMPTY, t)`
  becomes `write_blocking(items, t)`, `read_blocking(out, NOT_EMPTY,
  NOT_FULL, t)` becomes `read_blocking(out, t)`, and other masks become
  `*_with(.., Notify::new(wait, wake), t)` with `wait` a `NonZeroU32`.
  libfmq's `readBlocking` lists the wake mask before the wait mask, the
  reverse of the 0.12.0 positions; named fields cannot be swapped by
  position. `Some(Duration::ZERO)` tries once and returns `TimedOut`
  without touching the EventFlag word; 0.12.0 waited on a deadline already
  past, which cleared the wait bits it found set.
- **`shm::futex_wait`, `shm::futex_wake`, `shm::RoWord32::wait` and `wake`
  take the bit mask as `NonZeroU32`**; the `BadValue("empty bit mask")`
  for `0` is gone. `EventFlag::wait(0)` is still `BadValue` and
  `EventFlag::wake(0)` a no-op, as in libfmq.
- **`AttachPolicy` is `#[non_exhaustive]`**: `AttachPolicy { max_capacity:
  n, require_seal: true, require_event_flag: e }` becomes
  `AttachPolicy::new(n).require_event_flag(e)`. `new` sets `require_seal`
  and leaves `require_event_flag` off; `.require_seal(false)` turns the
  seal check off. The fields stay readable.
- **A descriptor that `MessageQueue::attach` or `UnsyncReader::attach`
  refuses is `Error::InvalidDescriptor`, not `BadValue`**: flavor, quantum,
  grantor geometry and overlap, an fd that is unsealed, too small or a
  hugetlbfs file, a ring mapping over `isize::MAX` on 32-bit, and the
  `AttachPolicy` refusals. `BadValue` is for the caller's own arguments.
  `StatusCode::from` maps `InvalidDescriptor` to `BadValue`, so code that
  converts to a binder status sees no change.
- **`Error::Os` carries `rsbinder_fmq::Errno`, not `rustix::io::Errno`**, and
  `impl From<rustix::io::Errno> for rsbinder_fmq::Error` is removed, so a
  rustix major release is no longer a semver break of this crate.
  `Errno::raw()` gives the number and `Errno::from_raw` builds one;
  `std::io::Error::from` takes both `Errno` and `Error`.
- **`rsbinder_fmq::Regions::write_at` is no longer public**; write through
  `WriteTxn::write_at` / `UnsyncWriteTxn::write_at`. No transaction hands
  out a `&mut Regions`, so it could not be called.
- On a 32-bit target `MessageQueue::attach` refuses, with
  `InvalidDescriptor`, a data grantor whose `extent` plus `offset` modulo the page size is more than
  `isize::MAX` (2 GiB − 1). That sum is the length mapped for the ring, and
  `shm::Mapping` maps no more than a Rust slice may span. 0.12.0 passed the
  length to `mmap`: on 32-bit Linux it attached the queue when the address
  space had room for it (it does in an i686 process on an x86_64 kernel);
  on 32-bit Android bionic's `mmap64` refused the length with `ENOMEM`, so
  there only the error changes (`Os(ENOMEM)` to `InvalidDescriptor`). Only a
  descriptor with an `extent` within one page of `i32::MAX` and a data
  `offset` that is not a multiple of the page size can be refused; one from
  `MessageQueue::create` never is, and 64-bit targets are not affected. Make
  the ring smaller; on 32-bit Linux a data grantor on a page boundary
  attaches too.
- `MessageQueue::attach` refuses, with `InvalidDescriptor`, a descriptor in
  which a counter, the data or the EventFlag grantor names an fd that is a hugetlbfs
  file, such as a memfd created `MFD_HUGETLB`; the check is in
  `shm::Mapping::map`, which on Linux and Android calls `fstatfs`
  (`fstatfs64` on a 32-bit target) before each `mmap` and refuses an fd
  whose filesystem the kernel reports as hugetlbfs.
  0.12.0 mapped such a queue where the kernel allowed it and could not
  unmap it when the queue was dropped: the kernel rounds a hugetlbfs
  mapping up to the huge page size and refuses (`EINVAL`) the `munmap` of
  the length that was asked for, so the mappings stayed until the process
  ended. libfmq makes its queues with `ashmem_create_region` (ashmem, or an
  ordinary memfd), so a queue from libfmq or from `MessageQueue::create` is
  not refused. Back the queue with an ordinary memfd or with ashmem.
  No other fd is refused: where the `fstatfs` fails — SELinux denies
  `getattr` on the fd's filesystem, a seccomp filter answers the call with
  an error — the fd is mapped as in 0.12.0. The call is made by
  `MessageQueue::create` and `attach` too, so a seccomp filter that kills
  or traps on a system call outside its list must allow it; Android's
  filters do (bionic `SYSCALLS.TXT` lists `fstatfs64` and `fstatfs`).
- `MessageQueue::attach` refuses a descriptor of the unsynchronized flavor
  with `InvalidDescriptor("descriptor is not of the synchronized flavor")`
  (0.12.0: `BadValue("only the synchronized flavor is supported")`). The
  flavor now has types of its own (CHANGELOG *Added*).
- Off Linux and Android (macOS), `MessageQueue::create` and
  `MessageQueue::attach` return `Error::Unsupported` before they look at
  their arguments or make a system call. 0.12.0 checked `create`'s
  arguments first, so `create(0, ..)` was `BadValue`, and duplicated the
  descriptor's fds in `attach` first, so a failed duplication was `Os`.
  Neither release makes a queue on those targets.
- `Error::Unsupported` displays as "not supported on this target or kernel:
  it lacks memfd, futex, mmap, 64-bit atomics, a read-only 64-bit load or
  F_SEAL_FUTURE_WRITE" (0.12.0:
  "fast message queues need Linux or Android"). The functions of `shm`
  return the same error to callers that have no queue, and it is returned
  on Linux targets without 64-bit atomics as well. Code that compares the
  text needs the new one; matching on the variant is unaffected.
- **`MessageQueue::write`, `read`, `begin_write` and `begin_read` refuse
  more elements than the capacity with `BadValue`**, as the blocking calls
  and the unsync flavor already did; `begin_write_cached` refuses more than
  its `limit` (capped at the capacity) the same way. 0.12.0 returned
  `Ok(false)` / `Ok(None)`, the answer for "not now", and a loop that
  polled until the transfer fit never ended. libfmq returns `false` there;
  only the Rust signalling changes.
- **`rsbinder-fmq/c/rsbinder_fmq.h`'s internal helpers are
  `rsbfmq_internal_*`**, not `rsbfmq__*`, a name C++ reserves; the header
  states its stability (the C API is Provisional) and defines
  `RSBINDER_FMQ_H_VERSION` (1), raised with each change to the C API. A
  copy of the 0.12.0 header that calls an `rsbfmq__*` helper directly
  renames the call.

## Shared memory

- `rsbinder`'s shared memory maps through `shm::Mapping` and so has the same
  two refusals as `MessageQueue::attach` (*Fast Message Queue* above), both
  `BadValue`: `MappedHeap::from_fd` — and what maps a
  received fd through it: `SharedMemory::from_fd`, `Parcel::read_blob`, an
  `IMemory` from a peer — refuses an fd the kernel reports as a hugetlbfs
  file, and on a 32-bit target it and `MemoryHeapBase::new` without
  `FLAG_DONT_MAP_LOCALLY` refuse a region of more than `isize::MAX` bytes.
  0.12.0 passed both to `mmap`. A
  hugetlbfs mapping then could not be unmapped when the heap was dropped,
  and a region that large is more than the slices the heap's accessors
  build may span. AOSP's `MemoryHeapBase` and `SharedMemory` make ordinary
  memfd or ashmem regions, which are not refused. The `fstatfs` of the
  hugetlbfs item under *Fast Message Queue* is made here too, once for each
  mapping.
- **`shm::Mapping::map` refuses, with `BadValue`, a range that reaches past
  the end of a regular file** (a memfd is one). 0.12.0 mapped it, and the
  first access past the end raised `SIGBUS`; the refusal reaches whatever
  maps through it: `MessageQueue::attach`, `MappedHeap::from_fd` and so
  `SharedMemory::from_fd`, `Parcel::read_blob` and a received `IMemory`.
  An fd that is not a regular file (ashmem, a device) is not measured, and
  a failed `fstat` leaves the fd to `mmap` as before.

## `rsb_hub` and `rsbinder-tools`

- `rsb_hub` answers an `addService`, `registerForNotifications` or
  `registerClientCallback` whose binder it cannot link to death with
  `EX_ILLEGAL_STATE`, and AOSP's `servicemanager` message: "Couldn't link to
  death." for `registerForNotifications`, "Couldn't linkToDeath." for the
  other two. 0.12.0 answered the link's own status: `DeadObject` for a binder
  whose death notice the hub already holds, which is also what a caller
  sees when the hub itself has died. The call changes no registration in
  either release.
- `rsbinder-tools`: `config::Config` has a field `topics` (a
  `TopicPolicy`), `config::FileContents` the fields `global_topics` and
  `topic_rules`, and `config::ConfigError` the variants
  `DuplicateGlobalTopics` and `TopicPattern`, with or without the `topic`
  feature. All three are now `#[non_exhaustive]`: outside the crate a
  `Config` is made by `Config::default()` and assigning its fields (a struct
  expression, `..Config::default()` included, no longer compiles), and a
  `match` on `ConfigError` needs a wildcard arm. `PolicyError::EmptyPattern`
  reads "name pattern must not be empty" (0.12.0: "service name pattern
  must not be empty"), as `[[topic]]` names use it too.
  `rsb_hub` logs a loaded configuration as "N rule(s), M service
  declaration(s)" (0.12.0: "N rule(s) and M service declaration(s)"), and
  its supervisor's status line reads "serving; N rule(s), M service
  declaration(s)" (0.12.0: "serving; N rule(s), M declaration(s)").
- **`rsb_hub` tells an accessor by a declaration, not by its descriptor.**
  `getService2` and `checkService2` answer with the `Service::Accessor` arm
  for an instance whose `[[service]]` entry has an `accessor` key, and with
  the binder registered under the accessor name, as AOSP's servicemanager
  does for a VINTF `<accessor>` (CHANGELOG *Added*). 0.12.0 asked every
  binder `addService` received for its descriptor and answered with the
  accessor arm for one registered as `android.os.IAccessor`, under its own
  name. That
  binder is now an ordinary service: `try_get_service` of its name returns
  the `IAccessor` binder, not the bridged root. To keep the name, declare it
  as its own accessor (`name = "a.IFoo/default"`, `accessor =
  "a.IFoo/default"`); a `wait_for_service` of it that the registration
  notice wakes still returns the `IAccessor` binder, as with 0.12.0, since
  the notice carries the binder registered under the name. A name without
  `interface/instance` form cannot be declared and has to change. To follow
  Android, register the accessor under another name and declare that one.
  With an accessor declared,
  - a `getService2` / `checkService2` lookup judges `find` on the accessor
    name too, and so do the calls
    AOSP judges that way (`addService`, `registerClientCallback`,
    `tryUnregisterService` judge `add`; `registerForNotifications`,
    `unregisterForNotifications`, `isDeclared`, `getDeclaredInstances`,
    `updatableViaApex`, `getConnectionInfo` judge `find`);
  - a `getService2` lookup (not `checkService2`) that misses the accessor
    starts the instance's own `start`;
  - nothing registered under the accessor name is `Service::Accessor(None)`,
    which libbinder takes as not found and an rsbinder client follows to its
    process-local accessor providers, where 0.12.0 answered a null
    `ServiceWithMetadata`, on which libbinder too tried its process-local
    accessors; an rsbinder client falls back as it did with 0.12.0;
  - `listServices` and `rsb_service list` show the accessor name, and
    `register_for_notifications` of an instance whose accessor has another
    name is not told when the accessor registers, so `wait_for_service` of
    it polls;
  - `rsb_service dump manager` marks `accessor=yes` on a name a declaration
    names as an accessor (0.12.0 marked a binder registered as
    `android.os.IAccessor`), and lists the instance under *awaiting
    registration* while a client waits on it;
  - a client that uses only `getService` (code 0, the Android 15 and earlier
    protocols) gets what is registered under the instance name, judged on
    the instance name alone, and its miss still runs the instance's `start`.

  `--insecure-allow-all` reads no configuration, so under it no binder is an
  accessor. A hub run with it that served an accessor moves to `--config`
  with a file that grants everything (`[global] list = "any"`, a `[[rule]]`
  with `name = "*"`, `add = "any"` and `find = "any"`) and holds the
  `[[service]]` declaration, as `tests/policy/d8b-accessor.toml` does; a
  hub built with the `topic` feature also needs `[global] topics = "any"`
  and a `[[topic]]` with `name = "*"`, `advertise = "any"` and
  `subscribe = "any"`, as `tests/policy/topic/permissive-topic.toml` has,
  since a `[[rule]]` grants no topic permission. A configuration with an
  `accessor` key fails to load in 0.12.0's `rsb_hub`, which refuses unknown
  keys.
- **`rsb_hub` makes no two-way binder call.** Its threads run
  `CallRestriction::FatalIfNotOneway`, as AOSP's servicemanager does: a
  two-way call from a request handler panics before it is sent and the
  caller gets `UNKNOWN_ERROR`. A panic on any thread but the binder thread
  and a start thread aborts the hub (AOSP's servicemanager aborts on such a
  call from any thread). 0.12.0 ended only the panicking thread, and the
  work it did stopped with no error: retiring the registrations of dead
  binders, `onClients(false)`. With the `topic` feature a panic on the
  thread that sends the notices the registry owes a watcher aborts the hub
  too. Under systemd,
  `Restart=on-failure` brings the hub back up.
- `rsbinder-tools`: `config::Declaration` has a field `accessor`; a struct
  expression of it, or a pattern without `..`, outside the crate needs it. `config::ConfigError` has
  the variants `BadAccessor` and `DuplicateAccessor`.
- `example-hello`'s `rpc_accessor_register_interop_server` takes an
  optional fourth argument, the name to register the accessor under; it
  registers under the instance name without one, as before.
