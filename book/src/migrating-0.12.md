# Migrating from 0.11 to 0.12

Every change in 0.12.0 that can break a program built against 0.11.0: a
compile error, a different value on the wire, or behavior a running program
can observe. Entries are grouped by area; within an area, those that change
the wire say so and need both peers upgraded together.

The additions and fixes themselves are listed in
[`CHANGELOG.md`](https://github.com/hiking90/rsbinder/blob/master/CHANGELOG.md);
a reference such as "(CHANGELOG *Fixed*)" points to the 0.12.0 section there.

## Toolchain and features

- **Minimum supported Rust version is now 1.86** (was 1.85). The local-binder
  cast `Binder::<B>::try_from(SIBinder)` (behind `FromIBinder::try_from` and
  `SIBinder::into_interface`) now downcasts the `Arc` through trait upcasting
  instead of reinterpreting a raw pointer, which needs 1.86. Results are
  unchanged: a remote proxy, another `Remotable` type, or an `IBinder` wrapper
  that forwards `as_any()` to a local binder still fails with `BadValue`.
- **Android builds must link for API 29 (Android 10, the oldest supported
  platform) or newer: pass `--platform 29` to `cargo ndk`.** rsbinder now
  calls bionic's `process_vm_readv`, which cargo-ndk's default API 21 does
  not export, so a build without it fails with `undefined symbol:
  process_vm_readv`. 0.11.0 linked at API 21.
- **The `tokio` feature no longer enables `tokio/full`**, only `tokio/rt`,
  `tokio/rt-multi-thread` and `tokio/sync` (for `death_signal`). A crate that received `tokio/net`, `time`,
  `signal`, `fs`, `io-util` or `macros` through rsbinder must declare them in
  its own `Cargo.toml`.
- **`BinderAsyncRuntime for TokioRuntime<Runtime>` and
  `for TokioRuntime<Arc<Runtime>>` are gone** (an owned runtime in a binder
  panics in `Runtime::drop` when the last `Strong` drops on a worker). Wrap
  `runtime.handle().clone()` in `TokioRuntime<Handle>` instead.

## Status codes

- **`StatusCode::from(ExceptionCode::ServiceSpecific)` is
  `ServiceSpecific(0)`, not `Ok`**, as in AOSP; `StatusCode::from(status)` for
  such a `Status` now yields `ServiceSpecific(0)`, not `FailedTransaction`.
- **On a host whose errno numbering is not Linux asm-generic (Apple
  platforms, and Linux on SPARC, MIPS, Alpha or PA-RISC) the status codes on
  the wire changed; upgrade every such peer together.** 0.12.0 sends AOSP's
  values (`utils/Errors.h` in Linux asm-generic errno numbering) on every host
  (CHANGELOG *Fixed*). Between a 0.11.0 and a 0.12.0 macOS peer six named codes
  decode differently: a 0.11.0 peer reads 0.12.0's `InvalidOperation`,
  `UnknownTransaction`, `BadIndex`, `NotEnoughData`, `WouldBlock` and
  `TimedOut` as `Errno` values (`NotEnoughData`'s `-61` as Darwin
  `ECONNREFUSED`), and a 0.12.0 peer reads 0.11.0's as `Unknown`. A
  `StatusCode::Errno` no longer crosses between two such peers either: it
  arrives as `Unknown`. The same holds for a `StatusCode` serialized into a
  `Parcel`. Between two peers with asm-generic numbering (Linux and Android on
  every other architecture) these codes do not change.
- **On a host whose errno numbering is not Linux asm-generic
  `StatusCode::from(i32)` and `i32::from(StatusCode)` convert wire values, not
  host errnos.** `StatusCode::from(-libc::ECONNREFUSED)` is `NotEnoughData`
  on macOS, since `-61` is AOSP `NOT_ENOUGH_DATA`;
  `StatusCode::from(-libc::ECONNRESET)` is `Unknown`, and
  `i32::from(StatusCode::Errno(e))` is `UNKNOWN_ERROR` (`i32::MIN`). Build a
  status from an errno with `From<rustix::io::Errno>` or `From<std::io::Error>`,
  which keep `StatusCode::Errno` in the host's numbering. On a host with
  asm-generic numbering both conversions are unchanged apart from the sign
  rule in the next entry.
- **A handler's `Err(StatusCode::Errno(x))` with `x >= 0`, or
  `Err(StatusCode::ServiceSpecific(v))` with `v <= 0`, now answers
  `Unknown`** on every host (CHANGELOG *Fixed*), where it answered success
  (`0`) or a status the client decoded as another variant. `i32::from` and a
  `StatusCode` written into a `Parcel` follow the same rule. Send a zero or
  negative service-specific code inside a `Status`
  (`Status::new_service_specific_error`, `Status::service_specific`), which
  carries it unchanged.
- **`ExceptionCode::from(StatusCode::UnexpectedNull)` is `TransactionFailed`,
  not `NullPointer`**, as AOSP `Status::setFromStatusT` maps every non-zero
  `status_t`. A handler returning `Err(StatusCode::UnexpectedNull)` now answers
  with that transaction status instead of an `EX_NULL_POINTER` exception
  header: the client's `Status` has `exception_code() == TransactionFailed`
  and `transaction_error() == UnexpectedNull`, and a branch on
  `ExceptionCode::NullPointer` no longer runs.
- **A service handler's `Err(StatusCode::DeadObject)` reaches a remote caller
  as `FailedTransaction`**, over kernel binder and over RPC; every other
  status goes out as `i32::from` encodes it. An AOSP `BpBinder` that receives
  `DEAD_OBJECT` as a reply status marks itself dead (`mAlive = 0`,
  `BpBinder::transact`) and fails every later call on that proxy without
  sending it, so one handler error cut the caller off from a service that was
  still running. `DeadObject` reaches a handler as an ordinary value: `?` on a
  nested call to a dead binder, or on a `BrokenPipe` write error (`EPIPE` is
  `DEAD_OBJECT`), as in an `on_dump` whose reader exited
  (`dumpsys foo | head`). The substitution also covers a `Status` with
  `ExceptionCode::TransactionFailed` and `DeadObject` as its transaction
  error, which a generated stub sends as the reply status. A local
  (in-process) call still returns the handler's `Err` unchanged; a client
  that matched `DeadObject` from a live rsbinder service now sees
  `FailedTransaction`.
- **`FLAG_COLLECT_NOTED_APP_OPS` is `0x2`, not `0x80`** (CHANGELOG *Fixed*): a
  flag value sent on the wire changes, so a 0.11.0 peer testing or setting it
  disagrees with a 0.12.0 one; rebuild both sides.

## Entry API (`serve` / `connect`)

- **`serve`, `connect`, `connect_binder`, `connect_async`, `Client::open` and
  `Client::open_with` take `impl Into<Uri>`, not `&str`** (CHANGELOG
  *Changed*). Build the destination with constructors, or parse a string
  explicitly:

  ```rust
  // 0.11.0
  rsbinder::serve("binder://")?;
  let h: Strong<dyn IHello> = rsbinder::connect("unix:///tmp/x.sock#hello")?;
  let h: Strong<dyn IHello> = rsbinder::connect(&config.target)?;

  // 0.12.0
  rsbinder::serve(Uri::kernel())?;
  let h: Strong<dyn IHello> = rsbinder::connect(Endpoint::unix("/tmp/x.sock").with_service("hello"))?;
  let h: Strong<dyn IHello> = rsbinder::connect(config.target.parse::<Uri>()?)?;
  ```

  The schemes and query keys are the same, but a malformed string now fails at
  `parse` with a `UriError` instead of at `serve` / `connect`; `?` into
  `rsbinder::Result` still yields `StatusCode::BadValue` and logs the reason.
  Replace a URI built with `format!("unix://{}", path.display())` by
  `Endpoint::unix(&path)`: the string form is percent-decoded, so a path
  holding `%`, `#` or `?` named a different socket or failed to parse.
- **`Endpoint` changed shape.** `Endpoint::Kernel(KernelEndpoint)` replaces
  the 0.11.0 struct variant `Kernel { driver, threads }`; read the settings with
  `KernelEndpoint::driver()` / `threads()` / `mmap_size()` and build one with
  `KernelEndpoint::default().with_threads(..)`. `Endpoint::Vsock { cid, port }`
  and `Endpoint::Tls { host, port }` replace the tuple variants; build them
  with `Endpoint::vsock(cid, port)` and `Endpoint::tls(host, port)`.
- **`Uri`'s fields are private**: use `endpoint()`, `service()` and `wire()`.
  `wire_max_version: Option<u32>` is now a `WireProfile` — `R34`, or
  `Android13Plus(WireVersion)` for `?profile=android13plus[-vN]`.
  **`rsbinder::entry::uri::parse` is removed**; use `Uri::parse` or
  `str::parse`.
- **A `tls://` host is percent-decoded, its `[ ]`, if any, must be one pair
  enclosing the whole host, and a host holding `:` needs them.** 0.11.0
  took the host text literally, split it from the port at the last `:` and
  stripped any number of brackets from its ends: `tls://[[::1]]:9000`,
  `tls://[::1:9000` and `tls://::1:9000` reached `::1`, `tls://a]:9000`
  reached `a`, and `tls://fe80::1`, with no port, became host `fe80:` and
  port 1. All of these are now `UriErrorKind::BadBrackets`; write
  `tls://[::1]:9000`. Write an IPv6 zone id as `%25`
  (`tls://[fe80::1%25eth0]:9000`): `tls://[fe80::1%eth0]:9000` is now
  `BadPercentEscape`, and a numeric zone such as `%12` decodes to a control
  character instead of reaching the resolver. A client connecting to a host
  with a zone id must also set `ClientOptions::tls_server_name`: the host is
  the default TLS server name, a server name cannot hold a zone id, and
  without it the connect fails with `StatusCode::RpcError`. `tls://[]:9000`,
  which 0.11.0 took as an empty host, is `EmptyHost`.
- **A query key given twice is refused** (`UriErrorKind::DuplicateQueryKey`).
  0.11.0 used the last value, so
  `unix:///a?profile=android13plus-v2&profile=android13plus-v0` spoke v0.
- **A `?query` after `#service` is refused** (`UriErrorKind::QueryAfterService`).
  0.11.0 made it part of the service name: `binder://#svc?driver=/dev/x` looked
  up `"svc?driver=/dev/x"` on the default driver. Put the query first.
- **A `unix://` path or `?driver=` may hold percent-escaped bytes that are not
  UTF-8**; 0.11.0 refused `unix:///tmp/%FF.sock`. A kernel driver path that
  is not UTF-8 parses but is `BadValue` at `serve` / `connect`, since
  `ProcessState` opens the driver by a `&str`.
- **`?profile=android13plus-vN` accepts any decimal spelling of `N`**, so
  `-v02` is version 2; 0.11.0 matched only `-v0`, `-v1` and `-v2`.
- **A kernel `Server::add` no longer registers the name; `run` / `spawn`
  do**, in this order: check the `ServeOptions`, start the thread pool,
  register the names. 0.11.0 registered at `add`, so an option refused at
  `run` / `spawn` left the name published with no thread to serve it, and
  its callers blocked. Two consequences:
  - A readiness signal printed between `add` and `run` now comes before the
    name exists, and a client that looks it up once (`check_service`) can
    miss it. Signal after `spawn` instead:

    ```rust
    // 0.11.0
    let server = rsbinder::serve(Uri::kernel())?.add(NAME, svc)?;
    println!("READY");
    server.run()

    // 0.12.0
    let _server = rsbinder::serve(Uri::kernel())?.add(NAME, svc)?.spawn()?;
    println!("READY");
    ProcessState::join_thread_pool()
    ```

  - A registration error — a name the service manager refuses (invalid, or
    held by another uid under `rsb_hub`), a policy denial, an RPC proxy — is
    returned by `run` / `spawn`, not `add`. Names registered
    before the failing one stay published and served until the process
    exits.

## Kernel binder, `Parcel` and process state

- **A kernel option `serve` / `Client::open` cannot honor is now `BadValue`.**
  The `KernelEndpoint` thread count and driver (`binder://?threads=`,
  `?driver=`), `ServeOptions::threads` and `ClientOptions::driver` are fixed
  by whoever initializes `ProcessState` first; a later *different* value used
  to log a warning and now returns `StatusCode::BadValue` (omitting it or
  repeating the value still works). Likewise `ClientOptions::driver` /
  `mmap_size` must agree with the `KernelEndpoint`'s driver and mapping size.
  A driver path that is not UTF-8 is `BadValue` too; 0.11.0 opened its lossy
  UTF-8 conversion, a different path.
- **`ServerGuard` and `Server` are `#[must_use]`**: `serve(uri)?.spawn()?;`
  stopped an RPC server at once. Bind the guard to a named variable.
- **A kernel transaction dispatched inside an RPC handler now reports the
  kernel caller**: `calling_caller()`, `get_calling_uid()` / `pid()` / `sid()`
  and `CallingContext::default()` return `Caller::Kernel` and the kernel
  sender's values, not the suspended RPC peer's. Re-check handlers that pick
  an authorization rule by `match`ing on the `Caller` arm.
- **Outside a transaction `get_calling_uid()` / `get_calling_pid()` return
  this process's own uid and pid** instead of `0` (root), as AOSP's
  `IPCThreadState` does. Inside a transaction nothing changes.
- **`ProcessState::set_call_restriction` now restricts the calling thread
  too** (CHANGELOG *Fixed*). A thread that had already made a binder call kept
  its previous value; it now takes the new one at once, so a twoway call it
  makes afterwards logs an error (`ErrorIfNotOneway`) or panics
  (`FatalIfNotOneway`). The main looper of `ServeOptions::call_restriction`
  is such a thread: a handler running on it that makes a twoway call now hits
  the restriction.
- **A kernel `Client::get` / `connect` reports the service manager's own
  failure** instead of `NameNotFound` when it cannot be reached, and `hub`
  logs why once.
- **`get_calling_sid()` is `None` where SELinux is not available** (CHANGELOG
  *Fixed*): on Linux without a mounted selinuxfs, Smack and AppArmor hosts
  included, a binder with `BinderFeatures::set_requesting_sid` no longer asks
  the driver for the caller's context. Under Smack or AppArmor 0.11.0 returned
  the label, read on to the next NUL byte.
- **`Parcel` is no longer `RefUnwindSafe` without the `rpc` feature**: a
  kernel parcel now holds the binder proxies written into it (CHANGELOG
  *Fixed*). With `rpc` it already was not. Wrap a `&Parcel` in
  `AssertUnwindSafe` where `catch_unwind` needs it.
- **A parcel write over a binder or fd object it recorded is
  `PermissionDenied`** (CHANGELOG *Fixed*): rewinding with `set_data_position`
  and writing over an object fails where 0.11.0 overwrote it.
- **`Parcel::from_ipc_parts` has one more `# Safety` condition**: the handle
  of every `BINDER_TYPE_FD` object at an offset in `objects` must be an open
  file descriptor that stays open until the parcel drops. Reading such an
  object duplicates that fd, as 0.11.0 already did, and the parcel closes
  none of them; the signature and behavior are unchanged. Under the 0.11.0
  conditions a buffer naming a closed fd number was allowed, and a read then
  duplicated whatever fd that number named by then.
- **`Parcel::__set_for_rpc` (`rpc` + `test-util`) returns `Result<()>`** and
  refuses, with `InvalidOperation` and the mode left unchanged, a parcel that
  holds bytes or was built over a received buffer (CHANGELOG *Fixed*). Call it
  on a new parcel before writing to it.
- **Over kernel binder, a service's `on_dump` writes to a duplicate of the
  caller's fd**, not to the received fd itself (CHANGELOG *Fixed*). The
  duplicate is made before the arguments are read; a process out of file
  descriptors now answers the `DUMP_TRANSACTION` with that error instead of
  running `on_dump`. Over RPC, `on_dump` now runs as well (CHANGELOG *Fixed*).
- **`ProxyHandle::dump` takes `F: Into<OwnedFd>`**, not `F: IntoRawFd`. A
  `File`, an `OwnedFd`, a `ParcelFileDescriptor`, a `ChildStdin` and the
  other owning std types compile unchanged. A raw fd number, or a type that
  implements only `IntoRawFd`, has to become an `OwnedFd` first
  (`unsafe { OwnedFd::from_raw_fd(fd) }`, with the caller vouching that it
  owns `fd`). `IntoRawFd` is a safe trait that need not hand over
  ownership, and `dump` closes the fd it takes, so 0.11.0 would close an fd
  such an implementation had only lent it.
- **`IMemoryHeap` has a required `heap_fd(&self) -> Option<BorrowedFd<'_>>`**,
  and `heap_id` is now a provided method returning that fd's number (`-1`
  for `None`). An implementation outside rsbinder no longer compiles until it
  adds `heap_fd`: return the heap's fd (`Some(self.fd.as_fd())`), or `None`
  while it has none. An own `heap_id` may stay, but nothing reads it for the
  reply: `BnMemoryHeap` answers `HEAP_ID` with a duplicate of `heap_fd` (and
  `BadValue` for `None`). In 0.11.0 it borrowed whatever number the safe
  `heap_id` method returned as an fd, so an implementation returning `0` or a
  closed number let one remote `HEAP_ID` request send an unrelated fd of the
  process (stdin, say) to the peer. Callers of `heap_id`, and the wire, are
  unchanged.

## RPC wire

- **The r34 RPC wire (the default profile) now frames messages as android-12
  libbinder does; a 0.12.0 peer and an older one no longer connect.** A
  client writes the `int32` session id `-1` when it connects, then every
  message as a bare `RpcWireHeader` and its body, with no `u32` length prefix.
  An older server reads the `-1` as a frame length past `MAX_FRAME_LEN`, and a
  0.12.0 server reads an older client's first length as a session id it does
  not have; both close the connection. Upgrade both ends together. The new
  wire was checked against android-12 libbinder on an SDK 31 emulator, with
  each side as client and as server (`example-hello/cpp/run_rpc_r34_interop.sh`).
  Every binder in an r34 parcel, a null one included, is now followed by its
  stability as android-12 writes it, a `Category` (`0x0c000001` for System,
  `0x00000001` for null) whatever the host's SDK; a null binder with a
  declared level or a `Category` of version 0 is refused with `BadType`.
  `GET_SESSION_ID` on r34 answers android-12's `int32`, so
  `RpcSession::get_session_id` returns 4 bytes there (was 32); a session not
  accepted by an `RpcServer` has no id and answers `UnknownTransaction`. A custom
  `RpcTransport` that implements only `send_frame` / `recv_frame` can no
  longer carry any session: `RpcSession::new` as a client fails with
  `RpcError::Protocol`, as the android-13+ profile always did; implement
  `send_raw` / `recv_raw`. `RpcSession::new` as an acceptor reads the id in
  its first `serve_blocking` read and ends the session on any id but `-1`.
- **RPC on the android-13+ wire: a null binder now carries a stability
  `int32`, as libbinder's does** (CHANGELOG *Fixed*). An rsbinder 0.11.0 peer
  on that wire reads the parcel 4 bytes off after a null binder or a null
  interface, so update both ends together. Parcels without a null binder are
  unchanged.
- **An android-13+ RPC session takes its fd mode from the connection header
  only**, as AOSP does. There `RpcSession::negotiate_fd_transport` sends
  nothing: it returns the header's mode, and a `Unix` request the header did
  not agree is `InvalidOperation`; 0.11.0 sent `GET_FD_MODE` and could switch
  the session to `Unix` after connecting. Request fd passing before
  connecting with `RpcClientConfig::fd_mode` (the `fd_mode` connect option of
  the entry API), and on the server with `RpcServer::set_supported_fd_modes`.
  A 0.12.0 server answers an android-13+ `GET_FD_MODE` with
  `UNKNOWN_TRANSACTION`, as libbinder does, so a 0.11.0 client calling
  `negotiate_fd_transport` on such a session gets `UnknownTransaction`. On the
  r34 wire `negotiate_fd_transport` still negotiates; against a peer without
  `GET_FD_MODE` (AOSP r34 libbinder answers `UNKNOWN_TRANSACTION`) it now
  returns `Ok(FileDescriptorTransportMode::None)` where 0.11.0 returned
  `Err(UnknownTransaction)`, so the first fd write fails (`FdsNotAllowed`)
  instead of the negotiation. An android-13 (v0) session has no fd mode in
  its header and stays `None` whatever both ends request.
- **An android-13+ RPC server refuses a client that requests an fd mode it does
  not support**, as AOSP does (`RpcServer.cpp` "Rejecting connection:
  FileDescriptorTransportMode is not supported"): `Unix` without
  `RpcServer::set_supported_fd_modes(&[Unix])` (the `fd_modes` serve option),
  or `Trusty`. The server closes the connection after its handshake response,
  so the client's setup succeeds and its first call fails with `DeadObject`.
  0.11.0 accepted the client and left only the server at `None`: the client's
  fd writes went out over `SCM_RIGHTS` and the server dropped the descriptors.
  Opt in on the server, or connect without `fd_mode`.
  `RpcSession::accept_android13plus_fd` with `server_fd_unix == false` (and
  `accept_android13plus`) refuses the same way. The r34 wire is unchanged: its
  `GET_FD_MODE` still agrees `None`.

## RPC sessions and connections

- **An RPC session refuses a connection whose transport differs from its
  founding one** (fd passing or local peer) with `BadType` at attach. A manual
  attach (`add_{outgoing,incoming}_connection_with_config`) over such a
  transport is refused before its header goes out, so the session stays up; a
  hand-assembled session (e.g. `RpcServer::serve_connection`) can hit it too.
- **An RPC session ends as a whole when any of its connections fails** (AOSP
  `RpcState::handleRpcError`). A send or receive failure, a cut frame or an
  expired deadline on one connection shuts every connection down: calls in
  flight on the others return `DeadObject`, every death recipient fires, and
  every local object the peer held is released. 0.11.0 retired the failing
  connection and let a fan-out session go on. After a session ends, reconnect,
  fetch the root and register callbacks again — `rsbinder::Reconnecting`
  (CHANGELOG *Added*) does this for one service. A client's incoming connection
  whose serve thread cannot be spawned ends the session too, since the server
  already holds it as a callback connection.
  So does every other failed incoming attach, except one whose header never
  went out (a refused pre-check, a failed connect or header write) or whose
  connection closed, a reset included, before any byte of the server's `"cci"`
  arrived (a refused attach): those leave the session up.
  A failed outgoing attach (`add_outgoing_connection_with_config`) ends the
  session too, since a libbinder server holds the connection once it has the
  header, except one whose header never went out (a refused pre-check, a
  `max_version` below the session's, a transport unlike the founding
  connection's, an id other than the session's `get_session_id()`, a failed
  connect or header write). A refused attach ends it as well: libbinder
  `android-16.0.0_r3` and later end their session when they refuse an attach
  at the `setMaxThreads` cap, and the close that refusal produces cannot be
  told from one that left the server's session up. Stay within `negotiate()`
  connections and echo `get_session_id()`.
- **An expired reply deadline ends the session** (`RpcSession::set_timeout`,
  `RpcServer::set_reply_timeout`, the `timeout` / `reply_timeout` options). The
  call still returns `TimedOut`; the session no longer survives it or skips the
  late reply. Set it above the slowest legitimate handler; to abandon one call
  and keep the session, run it on another thread and stop waiting. A wait for
  a free connection still fails only that call, with `WouldBlock`.
- **The session timeout bounds more than the reply wait**: `SO_SNDTIMEO`
  (a send that makes no progress for the period ends the session; a steady
  slow reader is not cut) on every connection of a bundled socket transport,
  and on a transport of your own only if it implements
  `RpcTransport::set_write_timeout` (without it a stalled send has no bound),
  the kernel's keepalive check on TCP and TLS over TCP
  (`RpcTransport::set_liveness` has the values and platform differences), and
  on a client each connect and handshake step. A send deadline that expires
  part-way through a frame returns `TimedOut` (0.11.0: `WouldBlock`, which
  means nothing was sent).
- **TCP connections, TLS over TCP included, have keepalive on by default**, at
  the system's intervals without a session timeout (hours), so a session
  whose peer host vanished ends and fires its death recipients. No wire byte
  changes.
- **`RpcServer::set_idle_timeout` judges the session, not the connection**: a
  session ends once no byte has crossed any of its connections and no call has
  been open for the period, so a fan-out client with one quiet connection is
  not evicted, and the eviction can come up to one period later than that; the
  `set_idle_timeout` rustdoc states the rule.
- **A kernel `ETIMEDOUT` on an RPC connection is a dead connection**, not a
  deadline of this end's (keepalive or `TCP_USER_TIMEOUT` gave up): the session
  ends, and a serve loop reports `NotLocal` / `Lost` instead of an idle
  eviction.
- **RPC `link_to_death` (and `death_signal`) is refused with
  `InvalidOperation` on a session that would not notice its connection
  dropping** (a client with no incoming connections and no serve loop). Open
  incoming connections (`incoming_connections(n)`), or call the new
  `RpcSession::spawn_serve` before linking; the server side is unaffected.
- **A `oneway` call made inside an RPC handler needs a connection this end
  opened** (CHANGELOG *Fixed*); without one it is `WouldBlock`, so a server's
  oneway callback needs the client's incoming connections. Twoway is
  unchanged.
- **`RpcServer::setup_unix_server` removes only a stale socket**: a regular
  file is refused with `AlreadyExists`, a listening socket with `EADDRINUSE`.
  Drop removes the socket file only while it is still the one bound. On Apple
  platforms it removes no socket at all and refuses every existing one with
  `EADDRINUSE`, as AOSP `setupUnixDomainServer` does, so delete a crashed
  server's socket before restarting: XNU refuses a connect to a live listener
  whose backlog is full with the same `ECONNREFUSED` a stale socket gives, so
  probing could remove a running server's path. 0.11.0 removed whatever was at
  the path on every platform.
- **An RPC `Parcel` is sent once** (CHANGELOG *Fixed*): build a new request per
  call. Sending the same parcel again, or a reply or incoming parcel as a
  request, returns `InvalidOperation`; a parcel built on another session's
  proxy returns `BadType` (AOSP `RpcState::validateParcel`), as do a local
  binder written into a sent parcel and a `ParcelableHolder` read on one
  session written into a parcel of another. In 0.11.0 a scalar-only request
  could be resent. `RpcProxy::transact` refuses a parcel built for no session
  (`Parcel::new()`) with `BadType`; build it with `build_request`. A handler
  that replaces its reply with such a parcel, or with one built on another
  session, sends the caller `BadType`; with a received or already-sent parcel,
  `InvalidOperation`. 0.11.0 sent their bytes.
- **An unresolved `ParcelableHolder` cannot be relayed over an r34 or
  android-13 v0/v1 RPC session; relay requires wire v2** (android-16). Writing
  a holder that was read but not decoded with `get_parcelable` into a parcel
  of such a session is `BadType`, even when the payload holds only data: those
  wires do not record where a binder sits, so the copy could neither take the
  references its binders need nor prove there are none. Decode the payload
  first and write the typed value. On v2 the relay carries binders and fds.
  In 0.11.0 the bytes were copied as they were, and relaying a holder that held
  one of the peer's own references freed the node early.
- **A session parcel takes a holder's bytes only from its own session** (AOSP
  `Parcel::appendFrom`): a holder read from a kernel parcel, decoded with
  `from_bytes` or read on another session, written undecoded into an RPC
  session parcel, is `BadType`. It was copied as is.
- **An android-13+ RPC client setup refuses a session id, and an attach
  refuses one other than its session's own**, with `BadValue`. The setup
  refuses before any handshake a non-empty id passed to
  `setup_unix_client_android13plus_with_id`,
  `setup_unix_client_android13plus_with_config`
  (`RpcUnixClientConfig::session_id`), `connect_android13plus_fd_with_id` or
  `ClientOptions::session_id`, all now deprecated, and likewise the new
  `setup_client_android13plus_with_config` with `RpcClientConfig::session_id`.
  In 0.11.0 they built a second client `RpcSession` on the server session
  another one founded; the two kept separate oneway numbering, binder
  addresses and lifetimes, so the server dropped one side's oneway calls as
  stale, could dispatch a call for one side's binder to the other side's
  binder at the same address, and ended the shared session when either side
  closed. AOSP has no public entry for this either (`RpcSession::setupClient`
  is private). To add a connection to a session, call
  `add_outgoing_connection_with_config` (or
  `add_incoming_connection_with_config`) on that session with
  `RpcClientConfig::session_id(&session.get_session_id()?)`; attaching from
  another process is not supported, as in AOSP. The wire is unchanged: a
  server still admits an attach that echoes its id.
  A manual attach (`add_outgoing_connection_with_config`,
  `add_incoming_connection_with_config` and the deprecated
  `add_{outgoing,incoming}_connection_android13plus*` forms) likewise refuses,
  with `BadValue` before connecting, every id other than its own session's
  server-minted one (`get_session_id()`, which the session now keeps; an
  attach on a session that never called it makes that round trip itself).
  0.11.0 let one `RpcSession` attach a connection to another server session,
  which split that server session across two client states the same way and
  pooled connections to two server sessions in one client session.
- **Several RPC client setup calls are deprecated** in favor of
  `RpcClientConfig` (CHANGELOG *Deprecated*); they still work, except for the
  session ids now refused (see above), but `-D warnings` flags them.

## `rsbinder-aidl`

- **`rsbinder-aidl` now rejects `.aidl` that AOSP's `aidl` also rejects**
  (CHANGELOG *Added*). An rsbinder-only contract relying on the missing
  validation now fails, with a diagnostic naming the rule.
- **`error::SemanticError` is now `#[non_exhaustive]`** and gained
  `FixedSizeNonFixedField`, `VintfStabilityLeak` and `DirectionNotSpecified`;
  add a `_ => …` arm. `DirectionPrimitive` is replaced by `InvalidDirection`,
  which covers every refused direction and adds the argument name and the
  permitted directions; its `type_kind` is the AIDL type (`int`, `String`),
  not `"a primitive type"`.
- **`render::ConstMember` and `render::EnumMember` gained a trailing
  deprecation element**; pass `String::new()` to keep the previous output. The
  `#[non_exhaustive]` render structs' new `deprecated` field needs no change.
- **`rsbinder-aidl` parser API: `ParcelableDecl::type_params` and
  `UnionDecl::type_params` are `Vec<TypeParam>`** (name, span, annotations)
  instead of `Vec<String>`; read `TypeParam::name`. New `Generic::type_args()`
  returns a `Foo<A, B>` reference's arguments as written.
- **`#[deprecated]` codegen can break a downstream `-D warnings` build**: an
  existing `/** @deprecated … */` javadoc now generates `#[deprecated]`. Drop
  the tag, or allow the lint at the call site.
- **A declaration nested in a `@VintfStability` type is now VINTF-stable**
  (CHANGELOG *Fixed*): a `ParcelableHolder` records VINTF stability for it, and
  `ParcelableHolder::set_parcelable` returns `BadValue` for a payload without
  VINTF stability. Nothing in the build warns.
- **`rsbinder-aidl`: a `@FixedSize` union's implicit `Tag` is `Tag(pub i8)`**
  (CHANGELOG *Fixed*): code that builds or reads it as `i32` no longer
  compiles, and a `Tag[]` is `byte[]` on the wire. A `@FixedSize` union with
  more than 128 fields is rejected, because its `byte` tag cannot number the
  129th; AOSP's `aidl` rejects it as well.
- **`rsbinder-aidl`: a `u32`/`u64` suffix on a hex literal no longer sets its
  width** (CHANGELOG *Fixed*), as in AOSP `ParseIntegral`: `0xFFFFFFFFu64` is
  the `int` `-1`, so a `long` constant set to it is `-1`, not `4294967295`, and
  `0x1FFFFFFFFu32` is a `long` instead of a parse error. Nothing in the build
  warns; a constant sent on the wire changes value.
- **`rsbinder-aidl`: `@nullable(heap=true)` is no longer ignored.** A
  parcelable or union field marked with it is `Option<Box<T>>` wherever it
  is, as AOSP's Rust backend renders it (`aidl_to_rust.cpp:305-306`); only
  one on a reference cycle was boxed before, so code building or matching
  such a field off a cycle as `Option<T>` stops compiling (wrap the value in
  `Box::new`). Such a field no longer counts as an inline edge either, so a
  bare `@nullable` field it cuts off a cycle is `Option<T>`, not
  `Option<Box<T>>`: in `parcelable A { @nullable(heap=true) B b; }
  parcelable B { @nullable A a; }`, `B.a` is now `Option<A>`. `heap=true` on
  anything but a parcelable or union type (a `String`, an array, an
  interface) and any `@nullable` parameter but `heap` are refused, with
  AOSP's messages. The wire is unchanged. Method arguments and returns keep
  their types.
- **`rsbinder-aidl`: a union whose first field is an enum without an
  initializer defaults to the enum's `Default` (backing value `0`)**, not to
  its first enumerator (CHANGELOG *Fixed*). For `enum E { A = 5, B = 6 }` and
  `union U { E e; … }`, `U::default()` is `U::E(E::default())`, which is
  `E(0)`, not `U::E(E::A)`; a default-constructed union sent as is carries
  `0` on the wire. An enum whose first enumerator is `0` is unaffected.
  Nothing in the build warns.
- **`rsbinder-aidl` / `rsbinder-macros`: fixed-size array arguments and returns
  have AOSP's element types** (CHANGELOG *Fixed*). A `@nullable T[N]` `in`
  argument or return value whose element is not a primitive or enum now wraps
  each element: `Option<&[Option<T>; N]>` and `Option<[Option<T>; N]>`, not
  `Option<&[T; N]>` and `Option<[T; N]>`. An `inout T[N]` of a binder,
  interface or `ParcelFileDescriptor` is `&mut [T; N]`, not
  `&mut [Option<T>; N]`. Service implementations and callers of such methods
  stop compiling until updated, and `#[rsbinder::interface]` refuses the old
  spellings and names the new one. The bytes on the wire do not change.
- **`rsbinder-aidl`: a union variant is its field's name with only the first
  letter uppercased** (CHANGELOG *Fixed*), as AOSP names it: `nullable_iface`
  is `Nullable_iface` (was `NullableIface`) and `URL` is `URL` (was `Url`). A
  single lowercase word (`ns` → `Ns`) or a lowerCamel name (`myField` →
  `MyField`) keeps the variant it had. Code naming any other variant stops
  compiling. The wire carries the tag number only, so it does not change.
- **`rsbinder-aidl`: one statement may hold at most 12 `<` that are followed
  by a name and not closed by a `>`** (CHANGELOG *Fixed*); 0.11.0 allowed 256.
  A comparison counts, so a constant such as `A < B || C < D || …` with 13 or
  more such comparisons is now rejected; split it across several constants
  (each `;` resets the count).
- **`rsbinder-aidl` checks argument directions as AOSP `aidl` does**
  (`GetArgumentAspect`, `AidlArgument::CheckValid`); 0.11.0 generated code for
  all of the following. Refused: `out` or `inout` on an `IBinder`, an
  interface or an enum (`in` only, `@nullable` or not) and on a
  `@JavaOnlyImmutable` parcelable or union; `out ParcelFileDescriptor` (`in`
  or `inout` only); and an omitted direction on a type that takes more than
  `in` — any array (fixed-size too, whatever the element), `List`, a
  parcelable or union (generic too) and `ParcelFileDescriptor`. Write `in`
  where the direction was omitted; the generated Rust does not change. For an
  `out` binder, interface or enum, return it, or use an array (`out IBinder[]`
  is `&mut Vec<Option<SIBinder>>`, and an element left `None` goes back as a
  null binder); for an `out` fd, use `inout`. An `out` or `inout` primitive or
  `String` stays refused, now as `SemanticError::InvalidDirection`.
- **`#[rsbinder::interface]` and `.aidl` refuse AOSP's reserved method
  signatures** — `asBinder()`, `getInterfaceHash()` and
  `getInterfaceVersion()` with no argument, and `getTransactionName(int)` — as
  AOSP `aidl` does (`AidlInterface::CheckValid`). Rename such a method; its
  transaction code, set by declaration order, does not change.
- **`rsbinder-aidl`: an enum reference in a constant expression takes the type
  of the enumerator's value, not the enum's `@Backing` width**, as AOSP
  `AidlConstantReference::evaluate` copies the referenced value's type
  (`@Backing` only bounds the value, `AidlEnumerator::CheckValid`). 0.11.0
  widened to `@Backing`, so in a `long`-backed enum `BIT = 1` then
  `BIT << 40`, and an implicit member after `2147483647`, folded; both are
  now `int` overflows. Write the literal at the width you need (`BIT = 1L`).
  A unary operator applies at that type too: with `A = true`, `-E.A` is
  `true` and `~E.A` is an error; with `A = 128u8`, `-E.A` overflows `byte`.
- **`rsbinder-aidl` binds a dotted name as AOSP `ResolveName` does**, for
  constant references and type names alike: the first segment is looked up
  in the enclosing types (innermost first), then the imports, then the
  document's own top-level types, and the rest of the name must exist inside
  what it found; a first segment found nowhere makes the name fully
  qualified. A type of the same package declared in another file without an
  import (an rsbinder extension; AOSP rejects it) is tried only after that,
  so it never outranks an import or a fully-qualified name. In a
  package-less file an import now outranks a package-less type of the same
  simple name. So `Q.X` looks for
  `X` in the type `Q` names and nowhere else. 0.11.0 also tried shorter
  suffixes of a wrong qualifier (`wrong.p.I.X` folded `p.I.X`), kept
  searching outer scopes after an inner type took the first segment
  (`X.Y.Z.C` beside a nested `X` folded the top-level one), accepted a
  qualifier that repeats a type's name (`X.X.C`, `E.E.A`, a field of type
  `X.X`), let an unqualified `X` find a constant inside a nested type also
  named `X`, and could fold `P.X` to a nested type's constant instead of
  `P`'s. These names are now errors and `P.X` folds to `P`'s constant. Name
  a shadowed outer type fully qualified (`p.X.Y`).
- **`rsbinder-aidl` looks an unqualified constant up in the current type
  only**, as AOSP `AidlConstantReference::Resolve` does. 0.11.0 also searched
  enclosing interfaces, so `interface I { const int K = 5; enum E { A = K } }`
  built with rsbinder but fails in AOSP `aidl` ("Can't find K in E"). Qualify
  the reference: `A = I.K`. This includes a field default of enum type:
  `E e = B;` is an error (AOSP "Can't find B in P"); write `E.B`.
- **`rsbinder-aidl` and `#[rsbinder::interface]` reserve the `__Rsb` name
  prefix** (CHANGELOG *Fixed*). An `.aidl` interface, parcelable, enum or union
  whose name starts with it, an interface trait so named, and a signature
  path whose first segment starts with it are now rejected; AOSP's `aidl`
  accepts such names. Rename the type, or reach it in a signature through a
  path such as `crate::…`.
- **`rsbinder-aidl`: an AIDL keyword is a whole word and never a name**, as
  AOSP's lexer (`aidl_language_l.ll`) tokenizes it. A field, argument or
  enumerator named `in`, `out`, `inout`, `package`, `import`, `const`,
  `interface`, `parcelable`, `enum`, `union` or `oneway` is now a syntax error
  (`int in;` used to pass when `;` or `,` followed the name); rename it.
  Text such as `interfaceIFoo {`, `packagep;` or `oneway oneway interface` is
  rejected too. AOSP's `aidl` rejects all of these, so an `.aidl` shared with
  an Android build is unaffected.

## `rsbinder-macros`

- **`#[rsbinder::interface]` and the derives now refuse shapes `.aidl` cannot
  express**, so moving an interface to `.aidl` stays a no-op. The diagnostic
  names the `.aidl` form to use instead. Refused: directions and nullability
  AIDL lacks (`out String`, `Option<i32>`, a bare `out` binder or fd); `in`
  spellings `.aidl` never renders (`&String`, `&Vec<T>`, `&i32`, a by-value
  `String` or binder); array-element `Option`s spelled the other way
  (`&mut Vec<Option<Cfg>>`, `Option<&[String]>`) and `i8` in an array element;
  scalars AIDL has no type for (`u32`, `u64`, `usize`, `u128`, `char`); and
  other shapes such as `BinderResult<T, E>`, `()` or `&mut [T]` arguments,
  duplicate names, receiver attributes, an empty `descriptor`, and on
  `#[derive(Parcelable)]` a bare binder or fd field.
- **`#[rsbinder::interface]` refuses an `out` or `#[inout]` binder object and
  an `out` `ParcelFileDescriptor`**, `Option` or not, as AOSP `aidl` does
  (`GetArgumentAspect`: `IBinder` and an interface go `in` only, a fd `in` or
  `inout`). `&mut Option<rsbinder::SIBinder>`, `&mut Option<Strong<dyn IFoo>>`
  and `&mut Option<ParcelFileDescriptor>` compiled in 0.11.0 and had no
  Android `.aidl` counterpart. Return the object instead, pass the fd
  `#[inout]`, or use an array, which takes every direction (`out`:
  `&mut Vec<Option<_>>`, `#[inout]`: `&mut Vec<_>`).
- **`#[rsbinder::interface]` refuses two interfaces in one module whose names
  share a stem** — `IFoo` and `Foo` both name `BnFoo`/`BpFoo` (AOSP
  `ClassName` drops a leading `I` before an uppercase letter). This failed
  with E0659 only once `BnFoo` or `BpFoo` was used; it now fails at the
  second declaration with E0428 on `__rsbinder_one_interface_per_stem_Foo`.
  Move one of the interfaces to another module or rename it.

## `rsbinder-tools`

- **`rsb_hub` checks every directory above its configuration, not only the
  configuration itself** (CHANGELOG *Changed*). A group- or world-writable
  (sticky or not) or foreign-owned directory anywhere on the path refuses the
  load: one kept under `/tmp` no longer loads, nor does `~/rsbinder/hub.d`
  when `~/rsbinder` was made `0775` under umask `002`. For an unprivileged
  `rsb_hub`, move it under `$XDG_RUNTIME_DIR` or `$HOME`; a root `rsb_hub`
  needs a root-owned path such as `/etc/rsbinder/hub.d`. Either way
  `chmod g-w,o-w` the directories on the way.
  `rsbinder_tools::config::check_path` is removed: the check now runs on
  held descriptors, and a path-based probe would report a different inode
  than the one read.
- **`rsb_hub` refuses configurations 0.11.0 loaded**: it exits 1 at start,
  and a SIGHUP reload keeps the policy in force. A `[[service]]` name must be
  `interface/instance` with both halves non-empty, within `addService`'s name
  rule (at most 127 bytes of `[A-Za-z0-9._/-]`); a bare interface name, which
  0.11.0 read as `<name>/default`, is refused. A `systemd` unit must not be
  empty, an `exec` program must be an absolute path, and `connection.port`
  must be 1–65535 (`ConfigError::Toml` otherwise). An `exec` program must
  exist and pass the check the configuration itself gets (CHANGELOG
  *Security*) — the program and every directory on the way to it from `/`
  owned by root or `rsb_hub`'s uid and not group- or world-writable — so a
  program under `/tmp` or under a group-writable directory is refused.
  `--config` must name a regular file or a directory; in a directory, a
  `*.toml` entry that cannot be opened (a dangling symlink) fails the load,
  where 0.11.0 skipped it, and a file whose name starts with `.` (an editor
  lock or backup file) is no longer read.
  `rsbinder_tools::config::ConfigError` gained `EmptyUnit`, `RelativeExec`,
  `UntrustedExec`, `ExecIo` and `BadServiceName`; an exhaustive `match` needs
  the new arms.
  `config::InstanceName::parse` returns `Option<InstanceName>`: `None` without
  a `/` or with an empty half.
- **`rsb_hub` answers differently in three cases.** `getDeclaredInstances`
  returns `Security` when the interface has declared instances and the caller
  may `find` none of them, as AOSP `ServiceManager.cpp` does; 0.11.0 returned
  an empty list, so `rsb_service instances` exits 2, not 1. `addService`
  succeeds once the service is registered even if an `onRegistration`
  callback fails; 0.11.0 returned that callback's error. `rsb_hub` exits 1
  when its signal, death-notification or client-callback thread cannot start,
  or `sigwait` fails, where 0.11.0 kept serving without that thread.
- **`rsbinder_tools::nss::gids_for_uid` returns `Option<BTreeSet<u32>>`**:
  `None` is a failed lookup, not an unknown uid (that is `Some` of an empty
  set). `nss::GroupCache` and `config::Enforcer` hold a boxed resolver and are
  no longer `UnwindSafe` / `RefUnwindSafe`; wrap them in `AssertUnwindSafe`
  where `catch_unwind` needs it.
- **`rsb_service` exits 2, not 1, when the device has no service manager**:
  0.11.0 `check` and `dump` reported every name as not registered there.
  **`rsb_device` refuses a device name over 255 bytes or naming a binderfs
  entry** (`binder-control`, `features`, `binder_logs`) before it mounts
  anything; 0.11.0 applied `--group` / `--mode` to that entry.
