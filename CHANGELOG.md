# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor releases may add features and occasionally
change APIs; patch releases are backward-compatible fixes.

This changelog starts at 0.9.0. For earlier releases, see the
[Git tags](https://github.com/hiking90/rsbinder/tags) and
[GitHub releases](https://github.com/hiking90/rsbinder/releases).

## [Unreleased]

## [0.13.0] - 2026-10-31

### Migrating from 0.12.0

Each item below breaks a 0.12.0 build or changes what a 0.12.0 program
observes. The book's
[Migrating from 0.12 to 0.13](https://hiking90.github.io/rsbinder/migrating-0.13.html)
says what to change for each, under the same headings. Items marked
**wire** change bytes between peers; an rsbinder peer of either release
accepts both the 0.12.0 bytes and the new ones; the oneway number's entry
says what a peer does with the 0.12.0 value.

**Typed casts and interface descriptors**

- `Debug` of an `SIBinder`, `dyn IBinder` or `ProxyHandle` prints a proxy
  descriptor nothing has asked for as `(not cached)`.
- A typed cast (`into_interface`, `FromIBinder::try_from`, `Strong::try_from`,
  a parcel read, `Strong::clone`, `Weak::upgrade`) accepts a remote binder of
  any interface, not `BadType`; `into_interface_checked` asks the remote.
  Over RPC, `connect`, `RpcSession::get_interface` and `Reconnecting` refuse
  a service of another interface with `BadType`.
- `FromIBinder` has a required associated function, `interface_descriptor`.
- `Binder::<B>::try_from` of a remote binder is `BadValue`; `BadType` only
  when the proxy's descriptor is known and differs.
- `cancel::cancel_remote`, `BpMemoryHeap::map` and `BpMemory::resolve` send to
  a proxy whose interface nothing has asked for, instead of `BadType`.
- A kernel proxy's `descriptor()` and `prepare_transact(true)` ask the remote
  on first use: on a `FatalIfNotOneway` thread that panics, and a failed
  query is `""` or the error.
- An RPC proxy's `descriptor()` and `prepare_transact(true)` ask the remote on
  first use and can fail with `WouldBlock`.
- `__rpc_stamp_descriptor` is removed.

**Service manager lookups**

- `hub::register_for_notifications`, `unregister_for_notifications`,
  `register_client_callback` and `try_unregister_service` return
  `BinderResult<()>`; a refused name is `EX_SECURITY`, not
  `FailedTransaction`.
- `hub::try_get_service` and `try_get_interface` return
  `BinderResult<Option<_>>`; `get_interface_async` is deprecated for
  `try_get_interface_async`.
- A lookup of a service whose process died returns its binder, whose first
  call is `DeadObject`; the `*_interface` lookups return `Err(DeadObject)`.
- `hub::default` accepts a context manager that is not an `IServiceManager`
  instead of returning `BadType`.
- On Android 10, `wait_for_service` looks again a second after its first
  miss, not at once, and logs "still waiting" every ten seconds.

**Binder objects and process state**

- `IBinder` loses `inc_strong`, `attempt_inc_strong`, `dec_strong`, `inc_weak`
  and `dec_weak`; `rpc_transact` takes an argument only rsbinder can build.
- `ProcessState::init`, `init_with_mmap_size` and `init_default` return
  `Result<&'static ProcessState, StatusCode>`; a second call with other
  settings is `BadValue`.
- `rsbinder::thread_state` and `shared_memory::{heap, dealer, shared, wire}`
  are private; their items are at `rsbinder::*` and
  `rsbinder::shared_memory::*`.
- `impl From<rustix::io::Errno> for StatusCode` is removed.
- `impl From<Stability> for i32` is removed; a parcel writes a binder's
  stability word with the binder, `Stability::includes` / `==` compare
  levels, and `Stability::try_from(i32)` stays.
- `ParcelableHolder::new(Stability)` is deprecated for `local()` / `vintf()`;
  `new(Stability::Vendor)` and `new(Stability::System)` panic.
- **wire**: a binder built with `Stability::Local`
  (`Binder::new_with_stability`, or the `stability:` argument of
  `declare_binder_interface!`) takes this process's level, `System`, when
  it is built, as AOSP Rust does. `stability()` on it returns `System`, not
  `Local`, and it goes out with `System`'s stability word (`0x0c`;
  `0x0c000001` on an Android SDK 31/32 kernel and on the r34 RPC wire)
  where 0.12.0 wrote level 0, which libbinder refuses with `BAD_TYPE` on
  either wire. An rsbinder peer accepts both. Over Android 10 kernel
  binder, whose parcel carries no stability word, the bytes stay the same
  and only `stability()` changes.
- **wire**: a kernel proxy reports and sends back the first declared level
  (vendor, system or VINTF) it was received with, also when the proxy
  existed before that word was read: a binder inside a `ParcelableHolder`
  payload, or a handle first looked up with
  `ProcessState::strong_proxy_for_handle`. 0.12.0 kept such a proxy at
  `System` and sent it back as `System`. A kernel proxy that received
  `UNDECLARED` (0) reports `System`, not `Local`, and goes back with
  `System`'s word; on Android 10, which has no stability word, every
  kernel proxy reports `System`, not `Local`.
- `TransactionObserver`'s tag is `Option<Box<dyn Any>>`, without `Send`.
- `ServeOptions::threads` and `binder://?threads=` raise the ceiling of a
  running process instead of returning `BadValue`; shrinking a started pool
  is `InvalidOperation`.
- `RemoteProxy` has a required method, `prepare_request`.
- The `Binder` constructors panic on a `min_sched_policy` / `min_priority`
  AOSP refuses, `SCHED_BATCH` included.
- `ProcessState::start_thread_pool` leaves the pool unstarted when its first
  worker cannot be created; a `binder://` `Client` and a kernel `Server`
  return the failure.
- A kernel proxy lookup fails on a flush only when the driver never took
  the proxy cache's `BC_INCREFS`: with the driver's error on every thread,
  or `InvalidOperation` when the flush succeeded without the driver taking
  it. Any other failed flush, of a `BC_INCREFS` the driver took or of the
  proxy's `BC_ACQUIRE`, is logged and the proxy is returned (AOSP
  `incStrongHandle`). 0.12.0 failed the lookup on a failed flush of either
  command off a binder thread (the driver's error) and of `BC_INCREFS` on
  one (`DeadObject`). A lookup made from a TLS destructor after the binder
  thread state's is `InvalidOperation`, where 0.12.0 aborted.

**Calling identity and permissions**

- `CallingContext::default()` is removed; use `CallingContext::current()`.
  `CallingContext` is `#[non_exhaustive]`.
- `permission_controller::check_permission` decides kernel or RPC from
  `calling_caller()`; its `reader` argument is not consulted.
- `clear_calling_identity()` outside a kernel transaction or during an RPC
  dispatch returns a token of this process's own uid and pid, not `0`.

**Local binders and async**

- `into_async`, `try_into_async` and the async cast of a sync-only local
  service give an in-process async proxy instead of `BadType` or a panic;
  off a transaction such a call has the caller `Caller::InProcess`.
- A generated async proxy transacts inline on a thread handling a
  transaction, whatever its `BinderAsyncPool`; `Tokio::spawn` always uses
  `spawn_blocking`.
- In a handler, `wait_for_interface_async` runs inline and holds the
  binder thread until the service appears; `timeout`, `select!` and a drop
  cannot end it. A bounded wait there is a `try_get_interface_async` loop.

**Parcel reads**

- A nullable or structured parcelable, a `ParcelableHolder` and a default
  `Option<T>` read any nonzero presence word as present, as AOSP Rust and
  Java do; 0.12.0 refused all but `1` with `UnexpectedNull`. A
  `ParcelFileDescriptor` still refuses it, as AOSP's does.
- `Option<String>` reads a malformed `String16` (a negative length other
  than `-1`, a short parcel, no NUL terminator) as `None`, as the NDK does,
  and `String` as `UnexpectedNull`; an unpaired surrogate is skipped, not
  `BadValue`. A `Status` message keeps the strict read: only `-1` is null,
  anything else malformed is `UnexpectedNull`.
- An array length below `-1` is `BadValue`, not `UnexpectedNull`. A length
  past the bytes left is `NoMemory`, not `NotEnoughData`, except when a
  `char` array's words run out, which stays `NotEnoughData`.

**Code generation (`rsbinder-aidl`, macros)**

- `rsbinder` requires `rsbinder-aidl` with `=`.
- A published service's `getInterfaceVersion` answers at most its generated
  module's `VERSION`, and `getInterfaceHash` the module's `HASH` above it.
- A nested `.aidl` type named `{Interface}Delegate` is refused.
- `declare_binder_interface!` is hidden from the docs; its `native` block
  takes `adapter:` and `r#async:` together or neither.
- A constant expression whose `||` or `&&` the left operand decides
  (`const boolean X = true || TYPO;`, `false && TYPO`) is a diagnostic when
  the right operand is an unresolved name: AOSP's `aidl` evaluates both
  operands and rejects the name, so the file never built for Android. Fix
  the name or add the missing `import`.

**RPC transport**

- The items deprecated in 0.12.0 are removed: `RpcUnixClientConfig`,
  `setup_unix_client_android13plus_{with_config,with_id,fan_out}`,
  `connect_android13plus_fd_with_id`,
  `add_outgoing_connection_android13plus{,_with_config}`,
  `add_incoming_connection_android13plus_with_config`,
  `RpcClientConfig::handshake_timeout`, `ClientOptions::{session_id,
  handshake_timeout}`, `EndReason::{Unreadable, Retired}`; so is
  `Parcel::is_for_rpc` (deprecated in 0.11.0).

- `RpcServer::run_background` returns `Result<RpcServerHandle>`, not
  `JoinHandle<()>`, and dropping the handle stops the server:
  `let _ = server.run_background();` stops it on that line.
- A zero RPC deadline (the `set_*_timeout` setters, `RpcClientConfig::timeout`,
  `ClientOptions::timeout`, `ServeOptions`) is `BadValue`, not "no deadline";
  the setters return `Result<()>`.
- `RpcSession::set_timeout` is deprecated for `set_reply_timeout`.
- `RpcServer::set_max_threads` returns `Result<()>`; it, `set_root` and
  `add_service` are `InvalidOperation` once the server runs, as is
  `set_root` together with `add_service`.
- `RpcClientConfig` has no lifetime parameter and owns its source;
  `add_*_connection_with_config` and `RpcClientConfig::session_id` are
  deprecated for `add_outgoing_connection()` / `add_incoming_connection()`.
- `RpcSession::session_id()` and `serve_blocking_on` are not public.
- `RpcTransport::send_raw` and `recv_raw` are required; `send_frame` and
  `recv_frame` are provided.
- `RpcSession::new` and the bundled transport constructors return
  `rsbinder::Result`, not `RpcResult`.
- `RpcProxy::build_request` and `transact` are deprecated for
  `RemoteProxy::prepare_request` and `submit_transact`.
- `rsbinder::rpc::{address, end, fd_mode, proxy, server, session}` are
  private (use `rsbinder::rpc::*`); some items are removed or moved behind
  `test-util`.
- A refused TLS certificate is `Errno(-EKEYREJECTED)`, `-EKEYREVOKED` or
  `-EKEYEXPIRED`, not `Unknown`; a rustls failure after the handshake is
  `RpcError::Io(InvalidData)`, not `RpcError::Protocol`.
- With `rpc-tcp-debug`, an `IAccessor` connection over an inet socket is
  `BadType` unless `rpc::set_accessor_plaintext_inet(true)` is called.
- A call from another thread on a session whose every outgoing connection is
  held by a serve loop fails at once with `WouldBlock` instead of waiting.
- **wire**: on an Android SDK 31/32 host, an RPC session on the android-13+
  wire writes a non-null binder's stability as the bare level (`0x0c` for
  a system binder), not the android-12 `Category` (`0x0c000001`). A
  libbinder 13+ peer accepts a vendor, system or VINTF binder where it
  refused it with `BAD_TYPE`; an rsbinder peer accepts both forms. Other
  hosts, the r34 wire and kernel binder write the same bytes as 0.12.0.
- An RPC session on the android-13+ wire accepts a null binder whose
  stability word has bits above the low 16 set and the low 16 zero (e.g.
  `0x00010000`); 0.12.0 refused it with `BAD_TYPE`. libbinder 13+ reads
  the word as an `int16_t` and accepts it too.
- **wire**: an RPC proxy reports and sends back the stability level it
  arrived with. `SIBinder::stability()` / `IBinder::stability()` on it (an
  android-16 accessor's root included) return that level, the first
  declared one (vendor, system or VINTF) if it arrived more than once, and
  the proxy goes back to its peer with it (the bare level on the
  android-13+ wire, the android-12 `Category` on r34); 0.12.0 returned and
  wrote `System` for every RPC proxy. So `includes(Stability::System)` is
  `false` on a `Vendor` proxy, and a
  libbinder peer whose binder is vendor or VINTF now accepts it coming home
  where it refused it with `BAD_TYPE`. A proxy that has received only
  `UNDECLARED` (0) or levels naming no `Stability` reports and goes back as
  `System`. Kernel proxies follow the same rule, except that a level naming
  no `Stability` is `BadValue` there (*Binder objects and process state*).
- An RPC session admits a further connection only from its founding
  connection's peer: the same uid for a Unix peer, the same certificate
  fingerprint for an mTLS peer, the same cid for a vsock peer; an anonymous
  peer (plaintext TCP, or on the android-13+ wire a TLS server that asks
  no client certificate) joins an anonymous founder on the id alone. This
  covers an r34 join, an android-13+ attach or callback connection and a
  client's own fan-out.
  0.12.0 compared the uid on an r34 join only, and otherwise admitted a
  join or attach on the session id and the transport kind alone.
- An `RpcServer` set up with TLS (`setup_*_tls`) refuses every r34 join;
  the r34 founding connection serves as before.
- An RPC server reads the interface token only before an AIDL call code
  (`FIRST_CALL_TRANSACTION..=LAST_CALL_TRANSACTION`), as the kernel path
  does: `EXTENSION_TRANSACTION`, `DEBUG_PID_TRANSACTION`, `DUMP_TRANSACTION`
  and a custom code outside that range reach the binder at position 0.
  0.12.0 read a token first and failed a parcel without one.
- `link_to_death` on an RPC proxy refuses a recipient with no strong
  reference left with `BadValue`, as a kernel proxy does; 0.12.0 returned
  `Ok` and never notified it.
- `send_frame` on `UnixTransport`, `TcpDebugTransport` and
  `VsockTransport` reports a send deadline that expired before the first
  byte as `RpcError::Timeout`, not `Io(WouldBlock)`; their `recv_frame`
  extends its read deadline while the peer takes this end's bytes.
- Two transport errors carry another code. Fds that arrive with no byte
  are `RpcError::Truncated` in the android-13+ reader too; 0.12.0 read
  them as a clean end (`EndOfStream`). A `sendmsg` with fds that takes
  nothing is `WriteZero` on Apple too; 0.12.0 returned `EndOfStream`
  there.
- The last drop of an RPC proxy does not wait for the peer on a plain Unix
  socket (`UnixTransport`) on Linux and Android, and on no transport runs death
  recipients on the dropping thread. With no room, a handler's own connection
  holds the `DEC_STRONG` for its next reply, and any other waits for the session's
  reaper thread, so it can come after the next reply. The frame's format and the
  amounts' sum are the same; the reaper's releases of one address can share a frame.
- A frame's fds refused for too many in flight for the uid (`ETOOMANYREFS`) are
  `RpcError::Protocol` and fail that call with `StatusCode::RpcError`, and a reply
  that had them fails the peer's call with `FAILED_TRANSACTION`, where 0.12.0 ended
  the session on the transport error.
- A oneway refused before any byte (`FrameTooLarge`, `Protocol`) ends the session
  if another thread has already reserved the proxy's next async number, where
  0.12.0 left a gap that held back later oneways to that address at the peer.
- `RpcSession::new` and the session setups fail when the process cannot spawn
  the session's thread, where 0.12.0 logged it and lost deferred `DEC_STRONG`s.
- **wire**: a oneway on an RPC proxy of an address received again before the
  last proxy's `DEC_STRONG` was written carries the number the peer's node
  expects, not 0, and runs at the peer. The frame's format is the same. An
  rsbinder peer of either release runs the new number and still drops a 0.12.0
  sender's, as 0.12.0 did.
- **wire**: a `DEC_STRONG` a oneway handler holds for lack of a free connection
  leaves through the reaper thread as soon as the serve loop is back at its read,
  on another connection than the one the oneway came on, where 0.12.0 held it for
  that connection's next reply. A session with no such connection still holds it.
- Below wire v2 (r34, android-13 to android-15) a proxy flattened into a parcel
  counts as a send, as libbinder counts it, where 0.12.0 counted only a
  transaction's target. Its release waits for the peer to pay, so a binder argument
  the peer never reads keeps the release, and the peer's node, until the session ends.
- **wire**: from wire v1 on, a received parcel that names other fds than it came
  with, or a binder below v2, is `BadValue`: a handler does not run, a reply's call
  fails. 0.12.0 ran the handler, whose read of a missing fd failed.
- **wire**: a session that dropped a `DEC_STRONG`, having no connection to write it
  on, numbers on a later proxy's oneways after the peer attaches a connection, where
  0.12.0 restarted at 0. The frame's format is the same.

**Streams and topics**

- `stream::Receiver::recv_timeout` and `try_recv` return
  `Result<T, RecvTimeoutError>` / `Result<T, TryRecvError>`, not `Ok(None)`
  for both a timeout and an end.
- `stream::ReceiverPolicy`, `stream::SinkPolicy`, `entry::WireProfile` and
  `PeerIdentity::Local` are `#[non_exhaustive]`; use the `with_*` setters and
  `PeerIdentity::local`.
- `stream::Sink::open` accepts a sink of another interface nothing has asked
  for, instead of `BadType`.
- A stream consumer accepts an `onStart` from a source of another interface
  nothing has asked for, instead of ending with `BadType`.

**Fast Message Queue (`rsbinder-fmq`)**

- `MessageQueue::begin_write`, `begin_write_cached` and `begin_read` return
  `WriteTxn` / `ReadTxn`, committed with `commit()` / `commit_first(n)`;
  `commit_write`, `commit_write_cached` and `commit_read` are removed.
- `MessageQueue::write` and `read` return `Result<Transfer>`, not
  `Result<bool>`.
- `write_blocking` and `read_blocking` take the items and the timeout only;
  other bits go through `*_blocking_with` and `Notify`.
- `shm::futex_wait`, `futex_wake`, `RoWord32::wait` and `wake` take the mask
  as `NonZeroU32`.
- `AttachPolicy` is `#[non_exhaustive]`; build it with `AttachPolicy::new`.
- A descriptor `attach` refuses is `Error::InvalidDescriptor`, not
  `BadValue`.
- `Error::Os` carries `rsbinder_fmq::Errno`, not `rustix::io::Errno`, and
  `Error` has no `source()`; match `Error::Os(errno)` for the errno.
- `Regions::write_at` is not public.
- On a 32-bit target `attach` refuses a ring mapping over `isize::MAX` with
  `InvalidDescriptor`.
- `attach` refuses an fd on hugetlbfs with `InvalidDescriptor`; `create` and
  `attach` call `fstatfs`, which a seccomp filter must allow.
- `attach` of an unsynchronized-flavor descriptor is `InvalidDescriptor`, not
  `BadValue`.
- Off Linux and Android, `create` and `attach` return `Error::Unsupported`
  before checking their arguments.
- `Error::Unsupported` displays a new text.
- `write`, `read`, `begin_write` and `begin_read` refuse more elements than
  the capacity with `BadValue`, not `Ok(false)` / `Ok(None)`.
- `rsbinder_fmq.h`'s internal helpers are `rsbfmq_internal_*`, not
  `rsbfmq__*`.

**Shared memory**

- `MappedHeap::from_fd` (so `SharedMemory::from_fd`, `Parcel::read_blob`, a
  received `IMemory`) refuses a hugetlbfs fd, and on a 32-bit target it and
  `MemoryHeapBase::new` refuse a region over `isize::MAX`, with `BadValue`.
- `shm::Mapping::map` refuses a range past the end of a regular file with
  `BadValue` instead of mapping it.

**`rsb_hub` and `rsbinder-tools`**

- `rsb_hub` answers a binder it cannot link to death with `EX_ILLEGAL_STATE`,
  not `DeadObject`.
- `rsbinder-tools`: `config::Config`, `FileContents` and `ConfigError` are
  `#[non_exhaustive]` and have topic fields and variants; three messages
  change wording, and `rsb_service`'s hint after a denial begins "The hub's
  policy denies this to uid" (0.12.0: "The service manager's policy").
- `rsb_hub` treats a service as an accessor by its `[[service]]` `accessor`
  key, not by the `android.os.IAccessor` descriptor; under
  `--insecure-allow-all` no binder is an accessor.
- `rsb_hub` threads run `FatalIfNotOneway`, and a panic on a thread other
  than the binder and start threads aborts the hub.
- `config::Declaration` has a field `accessor`; `ConfigError` has
  `BadAccessor` and `DuplicateAccessor`.
- `example-hello`'s `rpc_accessor_register_interop_server` takes an optional
  fourth argument.
- `config::SystemRunner` starts every command without `$NOTIFY_SOCKET`,
  whether or not the program called `Notifier::from_environment`.
- `notify::Notifier::from_environment` leaves `$NOTIFY_SOCKET` set; a child
  the program spawns itself, outside `config::SystemRunner`, inherits it.
- `rsb_hub`'s filtered `dump` counts in `hidden by policy` every name the
  policy withholds from the caller, not only the names the filter matches.
- `rsb_hub` answers a `dump` before the dump is written, so the call's return
  is not the dump's end, and answers a `dump` during another `WOULD_BLOCK`.
- A service `rsb_hub` starts runs with no signal blocked: a `SIGHUP`, `SIGINT`
  or `SIGTERM` it has no handler for ends it.

### Added

- **`Status::ok()`**: the success status (`EX_NONE`), as AOSP Rust binder
  has it.
- **`DeathLink`**: a death notification that unregisters when it drops.
  `DeathLink::new(&binder, recipient)` links and keeps both strongly; the
  drop unlinks and drops the binder and the recipient under `catch_unwind`,
  so a panic there is logged instead of unwinding (an abort when the link
  drops during another unwind), and an unlink error such as `DeadObject`
  after the obituary is logged at debug level.
- **`hub::is_valid_service_name`**: whether a name is one the service
  manager accepts (AOSP `isValidServiceName`: 1 to 127 bytes of
  `[A-Za-z0-9._/-]`). `rsb_hub`'s configuration and `rsbinder-topic`'s
  registry check their names with it.
- **`impl TryFrom<i32> for ExceptionCode`**: the variant for a wire value,
  every variant included (the reply-header markers, `TransactionFailed`,
  `JustError`); any other value is the error. `Deserialize` still reads a
  value with no variant as `JustError`.
- **`bridge::Delegating` and a generated `IFooDelegate` trait per
  interface**: a gateway overrides only the methods it changes (typically
  the one that re-wraps a binder argument) and publishes
  `BnFoo::new_binder(Delegating::new(upstream, hooks))`; every other
  method, `getInterfaceVersion` and `getInterfaceHash` forward to the
  upstream. Sync only; binder arguments are not re-wrapped automatically,
  and a method that carries a binder is not forced to be overridden (a
  same-stack gateway may forward it as it is).
- **`Caller::InProcess`**, **`ProcessState::set_max_threads`**, and with
  `rpc-tcp-debug` **`rpc::set_accessor_plaintext_inet` /
  `accessor_plaintext_inet`**: see Migrating.
- **`ProcessState::try_start_thread_pool()`**: `start_thread_pool` that
  returns the failure to create the first worker.
- **`AccessorProviderHandle::keep_for_process()`**: keeps the provider
  registered until the process exits, as AOSP's `addAccessorProvider` does
  when its caller discards the returned weak pointer.
- **`ProcessState::freeze(pid, timeout)` and `thaw(pid)`**, which name the
  direction; `freeze_process(pid, enable, timeout)` is deprecated (a
  positional `bool`, and a `timeout` a thaw ignores).
- **The process-wide slots can be read and swapped**:
  `observe::observer()` and `replace_observer()`,
  `permission_controller::permission_authority()` and
  `replace_permission_authority()`. A library that installs into a process
  it does not own keeps what it replaced, or installs an `observe::Fanout`,
  which forwards to several observers, each with its own tag, a panic in
  one not reaching the others. The replaced authority is dropped after the
  slot's lock is released, so a panicking `Drop` no longer poisons it.
- **`TxnContext::new(descriptor, code)`** and `with_*` setters, to
  unit-test a `TransactionObserver` without a transaction.
- **`bridge::Rewrap::remote_of(&wrapper)`**: the remote a wrapper was made
  for, for an upstream that hands a wrapper back.
- **`rpc::TlsTransport::connect_stream_with` / `accept_stream_with`**
  (`rpc-tls`): the hand-assembled TLS handshake with a deadline, set for
  the handshake as a whole and cleared after it, as the built-in `tls`
  client path sets one. `connect_stream` / `accept_stream` keep no
  deadline; a zero one is `BadValue`.
- **`pub use rsbinder::rpc::vsock`** (`rpc-vsock`): the `vsock` crate that
  `VsockTransport::from_stream` and the `TlsStream` impl name.
- **`hub::android_16::try_resolve_accessor` and
  `try_resolve_via_process_local`** return why a resolve failed: `BadType`
  for a binder that is not an `IAccessor`, `NameNotFound` for one serving
  another instance, `addConnection`'s status, the handshake's error; a
  name no provider claims is `Ok(None)`. The `Option` forms
  `resolve_accessor` and `resolve_via_process_local`, which logged each
  failure and returned `None`, are deprecated.
- **`fmq::MQDescriptor::try_from_sized(descriptor)`**: `TryFrom<Descriptor>`
  that also refuses a queue whose element size is not `size_of::<T>()`,
  which the untyped conversion leaves for the peer's libfmq to refuse.
- **`Debug` for the public types that had none**: `ProcessState`,
  `Binder<T>`, `Reconnecting<T>`, `WeakReconnecting<T>`,
  `ReconnectBuilder<T>`, `LazyServiceRegistrar`, `hub::ServiceManager`,
  `DeathSignal`, `entry::{Server, ServeOptions}`, and with `rpc`
  `RpcSession`, `RpcServer`, `RpcClientConfig`, `RpcProxy` and the
  transports. None blocks, takes a lock a binder thread
  holds (a busy one prints `<locked>`), makes a call, or prints key
  material or a session id.

- **`[[service]] accessor` in `rsb_hub`'s configuration**: the name the
  `IAccessor` of an instance is registered under, AOSP's VINTF `<accessor>`
  (see Migrating). The name must be one `addService` takes and not one the
  hub holds (`manager`, and with the `topic` feature
  `rsbinder.topic.registry`); two instances may not
  name the same accessor, as libvintf refuses. `rsbinder-tools` adds
  `config::Declarations::accessor` and `accessor_for`,
  `config::Enforcer::check_uid_in` and `check_caller_in` (a judgement
  against a snapshot of the configuration, so the several of one
  transaction see one configuration), and the constants
  `config::SELF_SERVICE_NAME`, `config::RESERVED_SERVICE_NAMES` and
  `config::START_THREAD_NAME`.
- **`Interface::dump_fd` and `Remotable::on_dump_fd`**: the dump handler
  that owns the caller's fd, so a service can write the dump on another
  thread and return, and a caller that never reads its pipe holds that
  thread rather than the binder thread. `DUMP_TRANSACTION` now calls
  `on_dump_fd`, which generated `Bn*` types forward to `dump_fd`; both
  default to the writer handlers (`on_dump`, `dump`), so a service that
  implements only `dump` behaves as in 0.12.0.

- **Generated proxies write their own interface descriptor as the request
  token**, as every AOSP backend does, through the new
  `RemoteProxy::prepare_request(interface)`, also on `ProxyHandle`.
  0.12.0 wrote the descriptor the proxy held for its remote, which a
  successful typed cast had already made equal, so the bytes on the wire do
  not change. `prepare_transact(true)` still writes the remote's.
- **`IBinder::try_descriptor`**, also on `ProxyHandle` and `RpcProxy`: the descriptor with
  the error of a query that failed, where `descriptor()` returns `""` for
  both that and a binder whose interface states none.
- **`SIBinder::into_interface_checked`** (AOSP C++ `checked_interface_cast`):
  `into_interface` that also asks a remote binder for its descriptor, once
  per proxy, and refuses another interface with `BadType`; a query that
  fails returns its error, such as `DeadObject`.

- **A Unix-socket RPC server and client reached by descriptors**:
  `RpcServer::setup_unix_server_from_bound_fd` serves on a socket another
  party bound (AOSP `setupRawSocketServer`), and makes it listen, so a
  client's `SO_PEERCRED` names this process. `RpcClientConfig::unix_fd`
  connects through an `O_PATH` descriptor of the socket's file, which may be
  unlinked: the session owns the descriptor, so all its connections go to one
  socket. `RpcSession::peer_identity` is who is at the other end of the
  founding connection. `RpcServer::set_receive_fds(false)` has the kernel close
  the fds a peer sends, for a server whose calls take none, while it goes on
  sending fds.
- **`rsbinder-topic`, a new crate (Experimental)**: one publisher and many
  subscribers over kernel binder, on Linux and Android. A binder call sets
  each subscription up; the records then travel through a ring in a sealed
  memfd that every subscriber maps read-only, so one copy serves all of them
  and receiving makes no binder call. See the book's
  [Topics](https://hiking90.github.io/rsbinder/topics.html) chapter and the
  crate's `examples/`.
  - Its feature `uds` (off by default) makes the subscription call over RPC
    on a Unix domain socket, where a binder device cannot be had:
    `uds::subscribe` and `uds::serve`, with `uds::Options::expect_uid` to
    check who listens by `SO_PEERCRED`. `registry::uds::UdsResolver` finds a
    topic by name through a registry on a socket, `registry::uds::UdsRegistry`
    with a `SocketDir` to serve one. `Resolver::watch_resolved` and `Resolved`
    carry what a binder needs kept alive, and a publisher given up on with no
    binder; the default keeps nothing, so a resolver of the application's own
    need not implement `watch_resolved`.
  - `Publisher<T>` and `Subscriber<T>` for any parcelable or `Bytes`
    (`Payload`). A publish never waits: a subscriber that falls a ring
    behind gets `RecvError::Lagged` with the number it lost. With
    `PublisherPolicy::backpressure` a publish waits up to
    `Backpressure::publish_timeout` (which also bounds a loan and the end
    record of `close` or a drop) and then takes the subscribers in the way
    out (`Closed::Evicted`); a stopped subscriber is taken out only once it
    holds a publish up. `Publisher::close()` ends the topic with
    `Closed::End`, `close_with(&Status)` with a status. `Publisher::new`
    returns `BinderResult` as the other constructors do
    (`EX_ILLEGAL_ARGUMENT` with the failed check for a policy out of range).
    A receive that ends with `Closed::Corrupted`, and a subscription refused
    for it, logs once per subscription which check of the topic's memory or
    pool failed.
  - `Publisher::advertise` and `NamedSubscriber`: a topic found by a service
    manager name, the subscriber reconnecting to whichever publisher holds
    it (`RecvError::Disconnected` between connections). A registration
    notice the subscriber missed (a oneway call, lost when the process has no
    room for it) is made up by a `check_service` when a publisher ends, or
    refuses as ended, with no other binder for the name; it asks once more
    when a notice during the call gives the name a binder it cannot use. `Resolver`
    (`Advertisement`, `Watch::with_lookup`, `host`) puts the names somewhere
    else; a `Publisher` drops the `Advertisement` its resolver returned in
    `close`, or as it drops, before its end record, and
    `Publisher::advertise_on` gives a name after the topic was made.
    `NamedSubscriber::state` reports, at every ask, a refusal a retry cannot
    change, a corrupt ring, or `Closed::ResolverDied` once a registry's host
    died and no connection is left. A `NamedSubscriber<Bytes>` receives
    samples too (`recv_sample`, `try_recv_sample`, `try_latest_sample`,
    `recv_sample_timeout`, `recv_sample_async`), with the value receives'
    connection and drain rules; `unsafe` `trust_publisher` makes a
    `NamedSubscriber<Bytes, Trusted>` that vouches for every publisher of
    the name.
  - `registry`: a registry of topic names apart from service names, which
    `rsb_hub` serves with its `topic` feature. `RegistryResolver` is its
    client (`find`, `list`), taken by `Publisher::advertise_with` and
    `NamedSubscriber::subscribe_with`: a name is taken back as its publisher
    closes or its process dies, and a notice a watcher had no room for is
    sent again. `Registry`, `RegistryAccess` and `RegistryStats` are the host
    side. `RegistryResolver::list` returns `registry::TopicListing` (`uid:
    u32`, `age: Duration`). The AIDL is rsbinder's own; the wire-only types
    (`ITopicWatcher`, `TopicInfo`, `TopicRef`, `ISubscription`, `Pool`) are
    hidden from the docs, and `ITopic`, `ITopicRegistry`, `SubscribeRequest`
    and `TopicEndpoint` stay documented. See the book's Topics chapter.
  - Waits on a futex word of the topic (`Wake::Futex`, one wake system call
    per publish for every sleeping subscriber) or on a socket of the
    subscription (`Wake::Fd`, with `Wait` and `wait_any` for event loops);
    `recv_async` with the `tokio` feature, `RecvError::Runtime` when the
    runtime cannot run a wait; `Interrupter`, the only source of
    `RecvError::Cancelled`; `recv_latest_timeout`. Each futex subscriber
    sleeps on one of 32 futex bits, and a wake this process sends for it
    (an interrupt, a death, an eviction) names that bit alone, so it ends
    about 1 in 32 of the topic's other futex sleeps instead of all of them;
    a publish still wakes every bit. The layout and `VERSION` do not change.
  - Loans (`PoolPolicy`, `Publisher::loan_slice`, `LoanMut`): a `Bytes` value
    written straight into a slot of a second memfd and received in place as
    a `Sample` (`recv_sample`, `try_latest_sample`, `recv_sample_async`).
    An `Untrusted` subscriber copies a slot with atomic loads; `unsafe`
    `trust_publisher` makes a `Trusted` one, whose samples deref to `[u8]`.
    `LoanMut` is `Send` (not `Sync`): a loan can be published on another
    thread or held across an `.await` in a spawned future. With
    backpressure, `loan_slice` waits for a free slot and `LoanMut::publish`
    for room in the ring, each up to `publish_timeout` holding the topic's
    lock, which blocks the thread they run on. A `loan_slice` while
    `PoolPolicy::max_loans` loans are held returns
    `PublishError::TooManyLoans` at once, without that wait.
  - `PublisherPolicy::authorize` and `Acceptor` decide who may subscribe;
    `Publisher::stats` and `Subscriber::stats` count what went through, and
    `Publisher::stats` reports each subscription's pinned pool slot
    (`SubscriberInfo::pinned`) and whether the publisher holds a name
    (`PublisherStats::advertised`).
  - The error enums `RecvError`, `Closed`, `Disconnected` and `PublishError`,
    the enums `Start` and `Wake`, and `PublisherStats`, `SubscriberInfo`,
    `SubscriberStats` and `registry::TopicListing` are `#[non_exhaustive]`.
    The subscription AIDL (`aidl/rsbinder/topic/`)
    and the memory layout are rsbinder's own; a publisher and a subscriber
    of different layout versions refuse each other
    (`ITopic::ERROR_VERSION`).
- **`rsb_hub`'s `topic` feature (off by default, Experimental)**: the hub
  serves `rsbinder-topic`'s registry of topic names under
  `rsbinder.topic.registry`, registered before it reports `READY=1`, and
  refuses that name to every transaction's `addService`,
  `registerClientCallback` and `tryUnregisterService`, under
  `--insecure-allow-all` too. Its policy is `[[topic]]` rules (`name`,
  `advertise`, `subscribe`) and a `[global] topics` gate in the hub's
  configuration files, a namespace apart from `[[rule]]`; a hub built
  without the feature refuses a file holding either. `rsb_service dump
  manager` reports the registry's counters, and `rsb_service topics
  [PREFIX]`, built with the same feature, lists the names a caller may
  subscribe to with their uid, pid, age and watchers. A hub whose `[[topic]]`
  rules have no `[[rule]]` letting anyone find the registry warns as it loads
  and reloads. With the feature `rpc` too, `rsb_hub --topic-socket PATH` serves
  the registry on a Unix socket with no binder device, under the same rules,
  and warns of the `[[rule]]`, `[[service]]` and `[global] list` it does not
  use; `rsb_service topics --topic-socket PATH` lists its names. The hub starts
  only on a directory of its own for the socket's files (mode 0700, nothing in it
  but what a crash of a hub left), and runs at most 1024 serve workers, one to a
  session and five descriptors to each, fewer once half of `RLIMIT_NOFILE` does
  not hold them (under 10 240), with a warning from under 15 152, which also holds
  its names. In the library:
  `config::TopicPolicy`, `TopicRule`, `TopicPermission`,
  `config::registry_out_of_reach`, `config::unused_on_a_topic_socket` and
  `Enforcer::check_topic`.
- **The unsynchronized FMQ flavor in `rsbinder-fmq`** (also `rsbinder::fmq`):
  `UnsyncWriter` and `UnsyncReader`, libfmq's
  `MessageQueue<T, kUnsynchronizedWrite>`. One writer, any number of
  readers; a write never waits, a full ring overwrites its oldest elements,
  and a reader that had not read them gets `UnsyncRead::Lost` with the
  number it skipped and resumes half a ring behind the writer. 0.12.0
  modelled the flavor in `Flavor` and refused to attach it.
  - The layout and counter discipline are those of `android-17.0.0_r1`:
    grantor 0 is the write region end (`Descriptor::WRITE_REGION_END`,
    libfmq from `android-16.0.0_r4`), which the writer stores before it
    copies and a reader loads after its own copy to find out that it was
    overwritten. `UnsyncReader::read_latest` follows libfmq's `readLatest`,
    except that a write in progress on a full ring makes it copy nothing
    (`UnsyncLatest { copied: 0 }`) only when that write reserves the whole
    ring.
  - `UnsyncWriter::begin_write` returns an `UnsyncWriteTxn`, committed with
    `commit()` or `commit_first(n)` as the synchronized flavor's
    transactions are. `UnsyncReader::attach` maps no EventFlag word, so it
    ignores `AttachPolicy::require_event_flag`.
  - `UnsyncCreate::read_only_readers` seals the memfd `F_SEAL_FUTURE_WRITE`
    after the writer mapped it, so no reader can write the counters or the
    ring. On a kernel without that seal (mainline Linux before 5.1)
    `UnsyncWriter::create` returns `Error::Unsupported` and makes no queue.
    `UnsyncReader` maps read-only in any case, and stores nothing when
    it attaches. A libfmq reader of a queue in use must be constructed with
    `resetPointers = false`: the default resets the writer's counters, which
    on a read-only queue is a `SIGSEGV` in that reader.
  - `UnsyncStart::Now` starts a reader at the write counter instead of at
    zero, where a libfmq reader starts.
  - Unlike libfmq, `UnsyncWriter` does not lower the write region end when
    fewer elements are committed than were reserved, so a reader never
    returns elements a partly committed write overwrote; and one write is at
    most `capacity - 1` elements, which libfmq accepts at `capacity` with a
    warning.
  - Checked against AOSP's libfmq on the host in both roles
    (`tests/libfmq_host.rs`). Not yet run on an Android device or on
    AArch64. `UnsyncReader::attach` is `Unsupported` where `shm::RoWord64`
    does not exist.
- **Shared-memory primitives in `rsbinder_fmq::shm`** (also
  `rsbinder::fmq::shm`): what the queue is built from, for protocols other
  than FMQ. `create` with `CreateOptions`, `add_seals`, `seals` and `Seals`
  (a memfd and its seals; `add_seals` returns the kernel's error unchanged,
  so `EINVAL` for an unsupported `FUTURE_WRITE` is the caller's to handle;
  an owner that writes through its own mapping maps it before that seal);
  `Mapping` and `RoMapping` (a shared mapping at any byte offset, read-write
  or read-only, on macOS as well as Linux and Android), and `Mapping::heap`
  (the same over heap memory, for tests and Miri); `futex_wait`,
  `futex_wake` and `Deadline` (non-private bitset futex against one absolute
  `CLOCK_MONOTONIC` deadline). The four functions the module had in 0.12.0
  are unchanged, and `ashmem_write_protected` joins them: libcutils
  `ashmem_get_prot_region` without `PROT_WRITE`, which is to an ashmem
  region what the write seals are to a memfd.
- **A mapping is reached through the pieces it is carved into, never through
  an address.** `Mapping::carve` / `RoMapping::carve` consume the mapping and
  hand out `Word` / `Words` (4- and 8-byte words, deref to `AtomicU32` /
  `AtomicU64`, or to `RoWord32` / `RoWord64` read-only), `Bytes` / `RoBytes`
  (an area copied in and out through atomics) and `Lendable` / `RoLendable`
  (an area whose bytes may also be lent in place through `unsafe` slices,
  for a protocol that gives a region to one party at a time). A piece off its
  alignment, past the mapping or over a byte another piece holds is
  `BadValue`. No two pieces share a byte, and a piece's kind fixes the size
  of every access to its bytes, so no byte is accessed at two sizes — which
  Rust's memory model makes undefined behavior unless both are loads — and
  no plain access is made to memory a peer can write; pieces keep the
  mapping alive, and are `Send + Sync`. `Bytes` gives each byte one size
  from the area's address and length: the bytes before the first `usize`
  boundary and after the last whole word one at a time, every other byte as
  part of its word, and a copy that covers part of a word stores it with a
  compare-exchange that writes the other bytes back. Two threads may copy
  overlapping ranges of one area at once.
  `Mapping::populate_huge_pages` asks for transparent huge pages before the
  mapping is carved. `Element` lives in `shm` now and is still exported at
  the crate root.
- **For memory a peer owns, in `rsbinder_fmq::shm`:**
  - `verify_received` checks an fd from a peer before it is mapped — a
    tmpfs memfd, sealed against shrinking and writing, every page holding
    data — so that the peer cannot shrink the file or leave a page missing
    under the mapping. A swapped-out page counts as data; a caller that
    cannot accept a swap-in under its access locks the mapping (`mlock`). It
    finds holes with `lseek(SEEK_HOLE)`, not with the block count, which
    also counts pages allocated past the end of the file.
  - `CreateOptions::populate` makes the memfd that passes: it writes zeros
    over the whole file. `allocate` (`fallocate`) alone does not, because
    the kernel reports a reserved page nothing has touched as a hole.
  - A `RoMapping`'s pieces view words as `RoWord32` / `RoWord64` and areas
    as `RoBytes` / `RoLendable`. They have loads, and `RoWord32` the futex
    wait and wake, and no method that stores, so code that holds one cannot
    fault on a write — `AtomicU32::store` and `Bytes::store` are safe
    methods that do on a read-only page. `RoWord64` exists where an 8-byte
    load does not write: the 64-bit architectures, for which the standard
    library defines it on read-only memory, and 32-bit x86 with SSE2 and
    32-bit ARM with 64-bit atomics, where it is the instruction the compiler
    emits (`movq`, `ldrexd`) and a test loads through a read-only mapping.
    On every other target `RoWord64` is a type with no value, with the same
    methods, so that code naming it compiles everywhere:
    `RoWord64::PRESENT` tells the two apart, and there `RoCarve::word64`
    returns `Unsupported`.
  - `Bytes::store_release` and `load_acquire` copy with an order: a reader
    that got any byte of a `store_release` is ordered after what the writer
    did before it, which lets a reader detect that its copy was overwritten.
    `load_acquire` is relaxed loads and one acquire fence, so it is defined
    through a read-only mapping.
  - `Bytes::load_extend` and `RoBytes::load_extend` append bytes of the
    area to a `Vec<u8>`: the loads of `load`, relaxed and with no fence,
    written into the vector's spare capacity without zeroing it first. The
    vector's length grows by the count, and the bytes it held stay in front.
    A caller that receives into a new or reused buffer no longer writes
    every byte twice, once with zeros and once with the copy.
- **`rsbinder_fmq::shm::spin`** (workspace-internal, `#[doc(hidden)]`, not
  covered by the stability tier): how long a side of a shared-memory
  channel looks at the memory before it parks on a futex. This is the
  policy `rsbinder::stream`'s ring has used since 0.12.0, moved here so
  that every channel in a process, `stream`'s and `rsbinder-topic`'s,
  counts against one limit on concurrent looks (half the cores).
- **`MessageQueue::write_blocking_with` / `read_blocking_with` and
  `Notify`** (with `Notify::WRITE` / `Notify::READ`, the default bits),
  **`Transfer`**, **`WriteTxn` / `ReadTxn`**, **`AttachPolicy::new`** with
  `require_seal` / `require_event_flag`, **`Error::InvalidDescriptor`** and
  **`rsbinder_fmq::Errno`**: see Migrating.
- **`to_bytes_into` and `from_bytes_in`**: `to_bytes` and `from_bytes` on a
  buffer the caller keeps. `to_bytes_into(&value, &mut out)` appends to `out`
  the bytes `to_bytes` returns, wherever `out` ends, and on an error leaves
  `out` as it was. `from_bytes_in(&mut bytes)` reads the value from `bytes`
  themselves, where `from_bytes` decodes a copy, and leaves them unchanged.
  A caller that encodes or decodes many values allocates only when its
  buffer grows; `to_bytes` and `from_bytes` are these two on a new buffer.
- **`hub::watch`**: calls a closure with the binder each time the default
  service manager registers a service under a name, until the returned
  **`hub::ServiceWatch`** drops; the drop unregisters the callback. It is
  `register_for_notifications` with the unregister held by a guard, the
  pairing `wait_for_service` and `rsbinder-topic`'s service-manager
  resolver now use.
- **`rsbinder_fmq::shm::seal_unsupported`**: whether an `add_seals` error
  means the kernel lacks a seal of the mask (`EINVAL` for a mask with
  `FUTURE_WRITE`, Linux before 5.1). It reads the answer right only for a
  memfd the caller created, since a file that takes no seals answers
  `EINVAL` too.
- **`rsbinder_fmq::EventFlag::wake_committed_lazy`**: `wake_lazy` on the
  targets whose code for a libfmq waiter orders it too (x86, x86-64,
  AArch64, 32-bit ARM, RISC-V 64), `wake` elsewhere; the wake the stream
  ring sends after each committed record.
- **`rsbinder_fmq::shm::Deadline::until`**: the futex deadline at an
  `Instant`, as `Deadline::after` the time left until it.
- **`rsbinder_fmq::shm::ring_parts`**: the one or two parts of a copy
  that wraps at a ring's end, handed to a closure that makes the copy
  with whichever `Bytes`/`RoBytes` method the protocol needs. The FMQ
  queues and `rsbinder-topic` copy through it.

### Changed

- **The last drop of an RPC proxy on a plain Unix socket waits for no peer**
  (see *Migrating*). On Linux and Android a `DEC_STRONG` on a `UnixTransport` is
  one `sendmsg` that puts the whole frame on the socket or nothing. With no room
  a handler's own connection holds it for its next reply, and for any other the
  session's reaper thread writes it once the peer reads, and a peer that takes
  nothing for the send deadline loses the session there; 0.12.0's drop waited in
  the write, for as long as a session with no deadline lasts. The reaper's wait
  keeps no session alive: with no incoming connection, the last handle and proxy
  close the connection. What the reaper owes is one sum per address, so no burst of
  drops ends a session. Other transports (TLS over a Unix socket among them) and
  Apple keep the write that waits. A write that fails on any transport stops the
  session at once, so nothing follows a frame it cut, and the reaper thread runs the
  death recipients, so a drop runs none. The frame format and the amounts' sum are
  unchanged; the reaper's releases of one address can share a frame.
- **An RPC handler that panics releases what its reply held before the
  reply.** The partial reply is discarded inside the handler step, so the
  `DEC_STRONG` of a proxy it held goes out ahead of the `Unknown` reply, the
  order the session keeps for every other release; it followed the reply.
  The kernel and RPC handler steps share one function, and a handler panic
  logs one line on both: `a transaction handler panicked for code N: …`.
- **Every caught panic logs its message.** The death-link,
  freeze-notification, reply-path, `Rewrap` and `Reconnecting` catches
  logged a fixed line; they now append the panic's text.
- **The frame API of the socket transports follows their raw sends and
  reads** (see *Migrating*). `UnixTransport`, `TcpDebugTransport`,
  `VsockTransport` and `TlsTransport` keep the trait's `send_frame` /
  `recv_frame`, which frame over `send_raw` / `recv_raw`, and every socket
  send but TLS's goes through one loop, where a send deadline that expires
  before the frame's first byte is `RpcError::Timeout` and after it
  `Io(WouldBlock)`, counted from the peer's last progress; `send_frame`
  on unix, tcp_debug and vsock reported `Io(WouldBlock)` at any position.
  `recv_frame` keeps its read deadline open while the peer is still taking
  this end's bytes, as a session's reads already did.
  `UnixTransport::send_frame_with_fds` waits for room in `poll` on Linux
  and Android, where it blocked. Every frame reader classifies a short
  read by one rule: fds that arrive with no byte are a cut frame
  (`Truncated`) in the android-13+ reader too, where they read as a clean
  end. A `sendmsg` that takes nothing is `WriteZero` on every send.
- **Receiving a kernel binder makes no call to the process behind it.**
  Reading a handle from a parcel sends no `INTERFACE_TRANSACTION`; a kernel
  proxy asks for its descriptor when `descriptor()`, `try_descriptor()` or a
  checked cast first needs it, caches the first answer, and caches no
  failure (AOSP `BpBinder::getInterfaceDescriptor`). A service that receives
  a binder from a stopped or slow process no longer waits for that process,
  and a process looking up the service manager no longer asks it for its
  descriptor. So `descriptor()` on a kernel proxy may make a two-way call
  the first time, returns `""` if it fails, and is subject to the thread's
  `CallRestriction` (a `FatalIfNotOneway` thread panics); do not call it
  under a lock a binder thread needs. Reading the binder of a dead process
  succeeds, and its first call returns `DeadObject`. A `stream::Sink` that
  streams by calls asks its sink nothing, and a consumer's binder thread
  receiving `onStart` asks nothing. An RPC proxy follows the same rules,
  without the `CallRestriction`: it asks when its descriptor is first
  needed, and a typed cast neither asks nor fixes its interface.
- **`Regions` is `Send + Sync`, and `WriteTxn`, `ReadTxn` and `UnsyncWriteTxn`
  are `Send`.** They were not so that no two copies through one mapping ran
  at once: two copies that start at different bytes split them into
  different access sizes. A queue's ring is a `shm::Bytes` now, which gives
  every byte one size, so the types take the auto traits of their fields.
  A transaction borrows the queue's ring apart from its counters, and
  `MessageQueue`, `UnsyncWriter` and `UnsyncReader` are `Send` whatever the
  element type. The queues, `rsbinder::shared_memory` and `rsbinder-topic`'s
  memory layouts reach shared memory only through `shm` pieces;
  `rsbinder-fmq` denies `unsafe` code outside `shm`.
- **The `tokio` floor is 1.53.3**, the first release with
  `AsyncFd::register_with_interest`; the constructors it replaces are
  deprecated there as unsound (tokio #8587). `rsbinder` itself makes no new
  call; the workspace shares one floor.
- **Types whose drop ends something are `#[must_use]`**: `stream::Sink`
  (ends the stream as failed), `stream::Receiver` (cancels it),
  `DeathSignal` (unregisters the notification), `hub::AccessorProviderHandle`
  (un-registers the provider), `reconnect::Reconnecting` (closes the helper)
  and `bridge::Rewrap` (unlinks its wrappers). A value of one of them left
  unused, `add_accessor_provider(set, f)?;` for one, now warns; the
  attribute reaches through `?`. A provider meant to stay registered calls
  `keep_for_process`.
- **A binder built with `Stability::Local` holds this process's level,
  `System`**, as AOSP Rust's constructor marks it (`mark_local_stability`).
  `stability()` reports it and both wires write it; 0.12.0 kept `Local`
  and wrote level 0 (`UNDECLARED`), which libbinder refuses on a non-null
  binder with `BAD_TYPE`, so such a binder could not reach a libbinder
  peer (see *Migrating*). Over Android 10 kernel binder, whose parcel
  carries no stability word, it could, and only `stability()` changes there.
  `Stability::Local` stays: for a binder it names
  the compilation-unit level, for a `ParcelableHolder` `STABILITY_LOCAL`.
- **`rsbinder_fmq::Error::Os` has no `source()`.** Its `Display` already is
  the errno's message, so a reporter that walks the source chain (`anyhow`'s
  `{:#}`, `eyre`, `tracing`) printed it twice.
- **`wait_for_service` on Android 10** waits in the same loop as on later
  versions, minus the registration notice: after the first lookup misses,
  the next comes a second later rather than at once, and a missing service
  logs the same "still waiting" warning every ten seconds (see *Migrating*).

### Fixed

- **Interface code generated with `Builder::set_async_support(false)`
  compiles against rsbinder with its `async` feature on** (the default); it
  failed with `cannot find type BnFoo`. Code generated with async support
  and built against rsbinder without `async` fails with one error that
  names the fix (enable `async`/`tokio`, or `set_async_support(false)`).
- **A `dump` of `rsb_hub` no longer waits on the caller.** The hub has one
  binder thread, and 0.12.0 wrote the dump, or the reason it was denied, to
  the caller's fd on that thread: a caller that passed a pipe and did not
  read it stopped every request to the hub once the pipe was full. The hub
  now writes on a thread of its own (`rsb_hub:dump`) and replies at once;
  the dump ends when that thread closes the fd. One dump is written at a
  time, and a dump that arrives while one is still being written is
  answered `WOULD_BLOCK` with nothing written, which a caller that never
  reads makes the answer to every later dump until the hub restarts.
- **On 32-bit Android, `rsbinder-fmq`'s futex wait and wake call `futex`, not
  `futex_time64`** (`EventFlag`, the blocking queue calls, `stream` over
  kernel binder). Android 10's seccomp allowlist (bionic
  `SECCOMP_WHITELIST_COMMON.TXT`) has no `futex_time64`, and its policy
  (`seccomp_policy.cpp`) traps a call outside the list (`SECCOMP_RET_TRAP`,
  a `SIGSYS`) instead of returning `ENOSYS`. libfmq's `EventFlag` calls
  `futex` as well. A wake is one system call on every kernel.
- **`rsbinder-fmq` compiles for Linux targets without 64-bit atomics and for
  WASI** (checked: `armv5te-unknown-linux-gnueabi`,
  `powerpc-unknown-linux-gnu`, `wasm32-wasip1`). 0.12.0 did not compile
  there — the queue imports `AtomicU64`, and rustix has no `clock_gettime`
  for WASI — although its documentation said the crate compiles
  everywhere. On these targets `MessageQueue::create` and
  `MessageQueue::attach` return `Error::Unsupported`. On the Linux ones the
  `shm` module is complete, with `RoWord64` a type that has no value (see
  Added), except that
  `shm::futex_wake` needs Linux 5.1 on a 32-bit target that rustix serves
  through libc, where it calls `futex_time64` (by default
  `powerpc-unknown-linux-gnu`, 32-bit MIPS and the architectures rustix has
  no inline assembly for); on WASI
  `shm::Mapping::map` returns `Unsupported` too and
  `shm::Deadline::after` returns `None`.
- **`rsbinder-aidl` reads `@Backing(type=...)` as a folded constant string**,
  as AOSP's `aidl` does, so `@Backing(type="in" + "t")` selects `int` instead
  of failing with an invalid backing type.
- **An RPC session on the android-13+ wire writes a binder's bare stability
  level on an Android SDK 31/32 host.** 0.12.0 wrote the android-12
  `Category` there (`0x0c000001` for a system binder), the encoding that
  host's kernel binder uses; libbinder 13+ reads the word as an `int16_t`
  and refused every non-null binder the session sent (the root, arguments
  and replies) with `BAD_TYPE`. Other hosts, the r34 wire and the kernel
  binder wire are not affected; an rsbinder peer accepts both encodings
  (see *Migrating*).
- **An RPC session on the android-13+ wire reads a null binder's stability
  from the word's low 16 bits, as libbinder 13+ does.** 0.12.0 compared
  all 32 bits with `UNDECLARED` and refused a word such as `0x00010000`
  that libbinder accepts (see *Migrating*).
- **An RPC proxy reports and sends back the stability it arrived with, as
  libbinder's `BpBinder` does.** 0.12.0 returned `System` from
  `IBinder::stability()` on every RPC proxy (and on an android-16
  accessor's root) and wrote `System` for it. The wire now writes what the
  getter returns, for every kind of binder, so the two cannot differ.
  libbinder refuses its own binder coming home with a level other than its
  own (`Stability::setRepr`), so passing a vendor or VINTF binder of a
  libbinder peer back to it, such as a callback or token handed on in a
  later call, failed with `BAD_TYPE` (see *Migrating*).
- **A kernel proxy made before its stability word was read takes the level
  that word carries.** Reading a `ParcelableHolder` from a kernel parcel
  looks up every handle in its payload before the payload's binders are
  read, and 0.12.0 created those proxies as `System` and dropped the word
  the later read carried; a handle first looked up with
  `ProcessState::strong_proxy_for_handle` behaved the same. A vendor or
  VINTF binder received inside a holder therefore reported `System` and was
  sent on as `System`: back to its libbinder owner, which refuses it with
  `BAD_TYPE` (`Stability::setRepr`), or to a third process, which recorded
  the wrong level. Every proxy, kernel and RPC, now starts with no level,
  takes the first declared one a read of its handle carries, and reports
  this process's level (`System`) until then; a received `UNDECLARED` (0)
  fills nothing (see *Migrating*). A kernel proxy revived after its last
  strong reference dropped, while a `WIBinder` kept the handle, keeps the
  level the handle was received with, as AOSP revives the same `BpBinder`.
- **`wait_for_interface_async` called while handling a transaction no
  longer needs a Tokio runtime.** It runs on the handler's thread, as
  `get_interface_async` (now `try_get_interface_async`),
  `check_interface_async` and `connect_async` already did in 0.12.0 and as
  AOSP `binder_tokio::wait_for_interface` does. 0.12.0
  handed it to `spawn_blocking`, which panics ("must be called from the
  context of a Tokio 1.x runtime") when the handler is polled outside a
  Tokio runtime (see *Migrating*).
- **The calls `wait_for_interface_async` makes inside a handler stay in the
  handler's transaction chain**: the interface check's
  `INTERFACE_TRANSACTION` to the service found and, on the Android 16
  protocol with `rpc`, the `getInstanceName` / `addConnection` calls to its
  accessor. 0.12.0 made them from a blocking-pool thread, so the kernel did
  not hand them to a thread of the peer that waited for this handler's
  reply: a peer whose binder threads all waited for that reply and whose
  thread pool could not grow (or had none) never answered, and the handler
  never returned.
- **`rsbinder_tools::notify::Notifier::from_environment` no longer removes
  `$NOTIFY_SOCKET` from the process environment.** Removing it called
  `std::env::remove_var`, which races libc `getenv` in any other thread, so
  the safe function was sound only when called before the first thread was
  spawned. The variable now stays set; instead every command that
  `config::SystemRunner` spawns (`systemctl start`, an `exec` start) is built
  with `env_remove("NOTIFY_SOCKET")`, so a service `rsb_hub` starts still
  cannot report `rsb_hub`'s unit ready or stopping. A child that other code
  in the process spawns inherits the variable (see *Migrating*).
- **A `char` array that would reach a recorded object writes no element.**
  It was refused with `PermissionDenied` only at the element that reached
  the object, after the elements before it were written and with the
  position left inside the array; it is now refused before the first
  element, as every other array is (AOSP `writeInplace`).
- **A panic while a kernel binder's death is delivered still answers the
  driver.** A panic in that step (a death recipient's `Drop`, or rsbinder's
  own lock checks) unwound past `BC_DEAD_BINDER_DONE`, so the driver held
  the death's clear notification and the pin release behind it. The panic
  is now caught and logged and the done is queued, as the frozen-binder
  handshake already did, and on a kernel or RPC proxy the recipients after
  a panicking one's `Drop` are still notified. A kernel proxy's recipient
  lock recovers from poison instead of panicking in `link_to_death`,
  `unlink_to_death` and the obituary, and an obituary whose unlink panics
  still reaches the recipients.
- **An RPC proxy dropped by a `thread_local!` destructor no longer aborts
  the process.** When the destructor ran after the RPC session's own
  thread-local, the release panicked inside it, which aborts. The release is
  now sent on a free connection, and a call (two-way or oneway) or a reply
  made from such a destructor is refused with `InvalidOperation`, as a
  kernel death or freeze request from one already was.
- **A panic that escapes an RPC frame's dispatch ends the session.** A
  panic from user code that a release runs outside the handler, such as the
  `Drop` of a binder the session lets go when an error reply's parcel
  drops, unwound the serve thread past the session's end: the session stayed
  live with no thread reading that connection, so a peer's call on it
  waited without end. The serve loop now catches it, logs it and ends with
  `EndReason::Dispatch(StatusCode::Unknown)`, which ends the session as any
  other loop end does; `serve_blocking` returns instead of unwinding.
- **A panic in a kernel transaction outside the handler restores the
  thread's calling identity.** A panic between the dispatch and the reply,
  such as the `Drop` of a binder the handler had unpublished, left the
  transaction's calling identity and strict-mode policy on the thread; under
  a `catch_unwind` of the caller's or tokio's `spawn_blocking`, the thread
  then answered `get_calling_uid` with that caller and
  `is_handling_transaction` with `true`. They are now restored on every exit.
- **A panic in the drop of a panicked handler's reply is caught.** The
  partial reply of a handler that panicked is discarded under its own
  `catch_unwind`, kernel and RPC alike, so a `Drop` that panics there no
  longer unwinds the binder thread or the RPC serve thread.
- **A kernel binder proxy made from a TLS destructor** (one that runs
  after the binder thread state's own) fails with `InvalidOperation`
  instead of aborting the process; a proxy dropped there still sends its
  decrements, as it did.
- **A failed flush no longer leaks a kernel reference count.** The proxy
  cache's `BC_INCREFS` is taken back when the driver never consumed it
  and counts as sent when it did; a `BC_ACQUIRE` counts as sent once
  queued, as in AOSP `incStrongHandle` (what a lookup returns: see
  *Migrating*). Before, either failure left a count the matching decrement
  never undid.
- **A service `rsb_hub` starts no longer inherits its signal mask.** The hub
  blocks `SIGHUP`, `SIGINT` and `SIGTERM` for its `sigwait`, and a child kept
  that mask: an `exec` service ignored `SIGTERM`, so `systemctl stop` of a
  unit holding one waited for its timeout and ended in `SIGKILL`. A child now
  starts with no signal blocked.
- **A dropped `recv_async` no longer holds up the stream's next read when the
  blocking pool is full.** On the ring, a wait the pool had not started stayed
  queued, and the next `recv` or `recv_timeout` waited for it before reading:
  `recv_timeout` returned nothing with a record in the ring, and `recv` waited
  until a pool thread came free. The dropped future now takes such a wait back.
- **A oneway on an RPC proxy of an address received again before the last
  proxy's `DEC_STRONG` was written runs at the peer.** The drop of a proxy reset
  the address's oneway number before that write, which can wait for a free
  connection. A proxy made for the address meanwhile numbered from 0, below what
  the peer's node, alive at that point, expects: the call returned `Ok`, and an
  rsbinder peer dropped the oneway where a libbinder peer parked it. The number
  now lasts until the address's last `DEC_STRONG` is on the wire. A peer that
  sends the address again before reading that frame meets the same result, as
  with libbinder on this side (android-17.0.0_r1 `RpcState.cpp:871-905`).
- **A oneway on an RPC proxy also sent as an argument is not overtaken by its
  release.** Below wire v2 a peer's payment for a read argument could pay for a
  oneway still waiting on another connection: the release went out, the peer
  dropped the oneway, and the call had returned `Ok`.
- **A `DEC_STRONG` held on a connection with no reply to come goes out.** A client
  whose one outgoing connection was busy held it on a callback connection the peer
  sends only oneways on, so the peer's entries and this end's service object stayed
  until the session ended.
- **A `DEC_STRONG` with no connection to go on is kept for one.** A session with no
  outgoing connection dropped it, which kept the peer's node until the session
  ended, and after a late attach a later proxy could number its oneways from 0 below
  what the node expects. It now goes out before the next reply on any connection, or
  on the attach, and the number lasts until then.
- **A frame that names fds it did not bring is refused**, as AOSP
  `rpcSetDataReference` refuses it, before any handler runs.

### Security

- **`check_permission` no longer grants an RPC peer through a kernel
  parcel.** An RPC handler that passed `check_permission` a
  kernel-marshalled parcel (a fresh `Parcel::new()`, or an outer kernel
  reader it captured) got past the RPC deny, and `PermissionManagerService`
  was asked about the RPC peer's uid; a Unix peer running as root was
  granted unconditionally. The transport is now taken from
  `calling_caller()`.
- **A calling-identity token taken outside a transaction no longer makes
  the caller root.** `clear_calling_identity()` returned `0` there, which
  decodes to uid 0; restored inside a later kernel transaction, it made
  every following `get_calling_uid()` check see root. The token now names
  this process.
- **The `rustls` floor is 0.23.46** for RUSTSEC-2026-0346 (client session
  `Debug` impls print TLS secrets), RUSTSEC-2026-0347 (application data
  sent to the outer server after an ECH rejection), RUSTSEC-2026-0348
  (Brotli certificate decompression allocates up to 1 GiB before
  authentication) and RUSTSEC-2026-0349 (an unauthenticated ServerHello
  downgrades later connections to classical key exchange); `rpc-tls` only.
- **The `rustls-webpki` floor is 0.103.16** for RUSTSEC-2026-0382
  (certificate verification may accept distrusted intermediates). No
  `rustls` release requires the fixed version yet, so `rpc-tls` now names
  `rustls-webpki` itself, for its floor alone; `rpc-tls` only.
- **An RPC session no longer admits a connection from a peer of another
  uid, certificate or VM on its id alone.** A peer that knew a session's
  id (r34's is 31 random bits) joined an r34 session over vsock or TLS, or
  attached to an android-13+ session, from another uid, certificate or VM,
  and could transact on every object the session's client had been handed
  (RPC binder addresses count up per session). A further connection must
  now come from the founding connection's peer (uid, certificate
  fingerprint or vsock cid), and a TLS server takes no r34 join. A
  transport that reports no peer identity leaves nothing to compare: a
  session founded over plaintext TCP (`rpc-tcp-debug`), or on the
  android-13+ wire through a TLS server that asks no client certificate,
  still admits a further anonymous connection on the id alone (see
  *Migrating*).
- **`rsb_hub`'s dump no longer tells a caller which names `find` denies
  it.** The `hidden by policy` count was taken after the caller's
  substring filter, so a caller with `list` could send the hub a `dump`
  with a substring (`rsb_service dump manager <substring>`, which also
  needs `find` on `manager`) and read from the count whether a name it may
  not find contains that substring. The count now covers every name the
  policy withholds, whatever the filter (see *Migrating*).

## [0.12.0] - 2026-10-11

### Migrating from 0.11.0

Each item below breaks a 0.11.0 build or changes what a 0.11.0 program
observes. The book's
[Migrating from 0.11 to 0.12](https://hiking90.github.io/rsbinder/migrating-0.12.html)
says what to change for each, under the same headings. Items marked **wire**
change bytes between peers: upgrade both ends together.

**Toolchain and features**

- MSRV is 1.86 (was 1.85).
- Android builds link for API 29 or newer: pass `--platform 29` to `cargo ndk`.
- The `tokio` feature enables only `tokio/rt`, `tokio/rt-multi-thread` and
  `tokio/sync`, not `tokio/full`.
- `BinderAsyncRuntime` for `TokioRuntime<Runtime>` and
  `TokioRuntime<Arc<Runtime>>` is removed; use `TokioRuntime<Handle>`.

**Status codes**

- `StatusCode::from(ExceptionCode::ServiceSpecific)` is `ServiceSpecific(0)`,
  not `Ok`.
- **wire**: on a host whose errno numbering is not Linux asm-generic (Apple;
  Linux on SPARC, MIPS, Alpha, PA-RISC) statuses go out in AOSP's numbering.
- On such a host `StatusCode::from(i32)` and `i32::from(StatusCode)` convert
  wire values, not host errnos.
- A handler's `Err(Errno(x))` with `x >= 0` or `Err(ServiceSpecific(v))` with
  `v <= 0` answers `Unknown`.
- `ExceptionCode::from(StatusCode::UnexpectedNull)` is `TransactionFailed`, not
  `NullPointer`.
- A handler's `Err(StatusCode::DeadObject)` reaches a remote caller as
  `FailedTransaction`.
- **wire**: `FLAG_COLLECT_NOTED_APP_OPS` is `0x2`, not `0x80`.

**Entry API (`serve` / `connect`)**

- `serve`, `connect`, `connect_binder`, `connect_async`, `Client::open` and
  `Client::open_with` take `impl Into<Uri>`, not `&str`; parse a string with
  `.parse::<Uri>()?`.
- `Endpoint::Kernel` holds a `KernelEndpoint`; `Endpoint::Vsock` and
  `Endpoint::Tls` have named fields.
- `Uri`'s fields are private and `wire_max_version` is `wire()`
  (`WireProfile`); `rsbinder::entry::uri::parse` is gone.
- A `tls://` host is percent-decoded, so an IPv6 zone id is written `%25`
  (`tls://[fe80::1%25eth0]:9000`; a client also sets
  `ClientOptions::tls_server_name`, since the host is the default TLS server
  name and a server name cannot hold a zone id). Its `[ ]`, if any, must be
  one pair enclosing the whole host, and a host holding `:` needs them
  (`tls://[[::1]]:9000`, `tls://[::1:9000`, `tls://::1:9000` are
  `BadBrackets`); `tls://[]:<port>` is `EmptyHost`.
- A query key given twice is refused (`DuplicateQueryKey`), not
  last-value-wins.
- A `?query` after `#service` is refused (`QueryAfterService`), not part of
  the service name.
- A `unix://` path or `?driver=` may hold percent-escaped bytes that are not
  UTF-8.
- `?profile=android13plus-vN` accepts any decimal spelling of `N` (`-v02`).
- A kernel `Server::add` queues the name; `run` / `spawn` register it, after
  the options are checked and the thread pool is started. Print a readiness
  line after `spawn()?` (then `ProcessState::join_thread_pool()`), not
  between `add` and `run`. A registration error is returned by `run` /
  `spawn`, not `add`.

**Kernel binder, `Parcel` and process state**

- A kernel option `serve` / `Client::open` cannot honor is `BadValue`, not a
  warning; so is a kernel driver path that is not UTF-8.
- `ServerGuard` and `Server` are `#[must_use]`.
- A kernel transaction dispatched inside an RPC handler reports the kernel
  caller.
- Outside a transaction `get_calling_uid()` / `get_calling_pid()` return this
  process's own ids, not `0`.
- `ProcessState::set_call_restriction` restricts the calling thread too.
- A kernel `Client::get` / `connect` reports the service manager's failure, not
  `NameNotFound`.
- `get_calling_sid()` is `None` where SELinux is not available.
- `Parcel` is no longer `RefUnwindSafe` without the `rpc` feature.
- A parcel write over a binder or fd object it recorded is `PermissionDenied`.
- `Parcel::from_ipc_parts` has one more `# Safety` condition.
- `Parcel::__set_for_rpc` returns `Result<()>` and refuses a parcel that holds
  bytes.
- Over kernel binder `on_dump` writes to a duplicate of the caller's fd.
- `ProxyHandle::dump` takes `F: Into<OwnedFd>`, not `F: IntoRawFd`.
- `IMemoryHeap` requires `heap_fd`; `heap_id` is a provided method.

**RPC wire**

- **wire**: the r34 profile (the default) frames messages as android-12
  libbinder does; 0.11.0 and 0.12.0 peers no longer connect.
- **wire**: on the android-13+ wire a null binder carries a stability `int32`.
- An android-13+ session takes its fd mode from the connection header only.
- An android-13+ server refuses a client that requests an fd mode it does not
  support.
- `RpcSession::accept_android13plus[_fd]` refuses a connection header whose
  session id is neither empty nor 32 bytes.

**RPC sessions and connections**

- A connection whose transport differs from the session's founding one is
  refused with `BadType`.
- A session ends as a whole when any of its connections fails.
- An expired reply deadline ends the session.
- The session timeout also bounds sends, connect and handshake steps (a
  `unix` or `vsock` `connect(2)` excepted), and TCP liveness.
- A `tls` client's connect deadline (`timeout`, the deprecated
  `handshake_timeout`) is shared by every address the host name resolves to;
  0.11.0's `ClientOptions::handshake_timeout` gave each address the whole
  deadline.
- `RpcServer::set_handshake_timeout` bounds the whole admission phase (on r34,
  through a new session's first transaction; a joining connection's ends with
  its session-id preamble), a client's handshake deadline (`timeout`, the
  deprecated `handshake_timeout`) each TLS or android-13+ handshake step as a
  whole, not each read, and `RpcSession::from_preconnected_fd` its handshake
  by a fixed 10 s as a whole.
- An expired TLS handshake is `RpcError::Timeout`, not `Io(WouldBlock)`.
- On Linux and Android a send or read deadline counts the peer taking this
  end's bytes as progress; Apple keeps the 0.11.0 rule.
- TCP connections, TLS over TCP included, have keepalive on by default.
- `RpcServer::set_idle_timeout` judges the session, not the connection.
- A kernel `ETIMEDOUT` on a connection ends the session.
- `link_to_death` / `death_signal` is refused on a session that would not
  notice its connection dropping.
- A `oneway` call inside an RPC handler needs a connection this end opened.
- `RpcServer::setup_unix_server` removes only a stale socket, and none on Apple
  platforms.
- An RPC `Parcel` is sent once; one built on another session is `BadType`.
- An undecoded `ParcelableHolder` can be relayed only over wire v2.
- A session parcel takes an undecoded holder's bytes only from its own session.
- A client setup refuses a session id; an attach refuses one other than its
  session's own.
- Several RPC client setup calls are deprecated (see *Deprecated*).

**`rsbinder-aidl`**

- `.aidl` that AOSP's `aidl` rejects is rejected (see *Added*).
- `error::SemanticError` is `#[non_exhaustive]`; `DirectionPrimitive` became
  `InvalidDirection`.
- `render::ConstMember` and `render::EnumMember` gained a deprecation element.
- `ParcelableDecl::type_params` and `UnionDecl::type_params` are
  `Vec<TypeParam>`.
- A `/** @deprecated */` javadoc generates `#[deprecated]`.
- A declaration nested in a `@VintfStability` type is VINTF-stable.
- **wire**: a `@FixedSize` union's `Tag` is `Tag(pub i8)`, and a `Tag[]` is
  `byte[]`.
- **wire**: a `u32`/`u64` suffix no longer sets a hex literal's width, so some
  constants change value.
- A `@nullable(heap=true)` field is `Option<Box<T>>` wherever it is.
- **wire**: a union whose first field is an enum without an initializer
  defaults to the enum's `Default` (`0`).
- Fixed-size array arguments and returns have AOSP's element types (also
  `rsbinder-macros`).
- A union variant is its field's name with only the first letter uppercased.
- One statement may hold at most 12 unclosed `<` followed by a name.
- Argument directions are checked as AOSP `aidl` checks them.
- AOSP's reserved method signatures are refused (also `#[rsbinder::interface]`).
- An enum reference in a constant expression has its value's type, not the
  `@Backing` width.
- A dotted name binds as AOSP `ResolveName` binds it.
- An unqualified constant is looked up in the current type only.
- The `__Rsb` name prefix is reserved (also `#[rsbinder::interface]`).
- An AIDL keyword is a whole word and never a name.

**`rsbinder-macros`**

- `#[rsbinder::interface]` and the derives refuse shapes `.aidl` cannot
  express.
- `#[rsbinder::interface]` refuses an `out` or `#[inout]` binder and an `out`
  `ParcelFileDescriptor`.
- Two interfaces in one module whose names share a stem fail at the second
  declaration.

**`rsbinder-tools`**

- `rsb_hub` checks every directory above its configuration;
  `config::check_path` is removed.
- `rsb_hub` refuses configurations 0.11.0 loaded; `ConfigError` gained
  variants.
- `rsb_hub` answers `getDeclaredInstances`, `addService` and a failed thread
  start differently.
- `nss::gids_for_uid` returns `Option<BTreeSet<u32>>`; `GroupCache` and
  `Enforcer` are not `UnwindSafe`.
- `rsb_service` exits 2 without a service manager; `rsb_device` refuses
  binderfs entry names.
### Added

- **`rsbinder::Reconnecting`** (`rsbinder::reconnect`): a handle to one
  service, named by a `Uri`, that looks it up again after its process dies or
  its RPC session ends and reruns an `on_connect` hook. Book: "Reconnecting
  to a Service".
- **Streaming with back-pressure** (`rsbinder::stream`): `Sink<T>` and
  `Receiver<T>`, over an FMQ ring on kernel binder and credited `oneway`
  calls on RPC; async variants with `tokio`. Book: "Streaming".
- **New crate `rsbinder-fmq`**: Android's Fast Message Queue (`libfmq`) in
  Rust, wire-compatible, synchronized flavor, Linux and Android.
  `rsbinder::fmq` re-exports it with the `MQDescriptor` family.
- **C reference headers for an NDK peer**: `rsbinder-fmq/c/rsbinder_fmq.h`
  and `contrib/ndk/rsbinder_stream.h`.
- **`rsbinder-aidl`: generic parcelables** (`parcelable Foo<T, U>`), and the
  `android.hardware.common` FMQ types and `NativeHandle` built in.
- **An r34 `RpcServer` serves android-12 libbinder's multi-connection
  clients**, one session id per client.
- **`rpc::RpcClientConfig`**: one android-13+ client config for every
  transport, with `RpcSession::add_{outgoing,incoming}_connection_with_config`.
- **`RpcSession::is_ended`**, **`rpc::EndReason::SessionEnded`**,
  **`RpcTransport::peer_closed`** and **`RpcTransport::set_liveness`** (both
  also on `TlsStream`).
- **`RpcTransport::shutdown_handle`**: cuts a connection from another thread,
  for the whole-handshake deadline; a transport of your own that returns
  `None` (the default) has its handshake bounded per wait only, by the
  `set_read_timeout` and `set_write_timeout` it implements (neither: no bound).
- **`rpc::transport::MemTransport` carries a raw byte stream**, so an
  android-13+ session runs over `mem` in hermetic tests.
- **Work source API** (`set_calling_work_source_uid` and friends), carried on
  outgoing kernel calls.
- **Transaction names** (`Builder::trace(true)`,
  `Remotable::transaction_name`), **transaction observers**
  (`rsbinder::observe`) and a **`tracing` feature**.
- **`ParcelFileDescriptor::pipe()`**, and `Read` / `Write` for
  `ParcelFileDescriptor`.
- **`Parcel::write_blob` / `read_blob`**, compatible with Java's
  `Parcel.writeBlob`.
- **Cooperative cancellation** (`rsbinder::cancel`) over
  `android.os.ICancellationSignal`.
- **Typed service-specific errors** (`ServiceSpecificError`,
  `#[derive(ServiceSpecificError)]`) and **`Status::message()`**.
- **A configurable receive mapping**: `ProcessState::init_with_mmap_size`,
  `KernelEndpoint::with_mmap_size` (`binder://?mmap=`),
  `ClientOptions::mmap_size`.
- **`TransportCaps`**: what a transport can do, from `Client::caps()`,
  `RpcSession::caps()` or `calling_caps()`.
- **`wait_for_interface_async`, `check_interface_async` and `death_signal`**
  (`tokio`).
- **`rsbinder-aidl` validates `.aidl` as AOSP's `aidl` does** and warns when a
  type name resolves only without the `import` AOSP requires.
- **`@deprecated` support** in `rsbinder-aidl` and `rsbinder-macros`.
- **`rsbinder-macros`: the signature checks cover every out/inout and `in`
  array shape `.aidl` renders.**
- **rsbinder-tools: `config::is_valid_service_name`**; `config::Config` and
  `config::FileContents` are re-exported.
- **Freeze notifications**: `IBinder::add_frozen_state_change_callback` /
  `remove_frozen_state_change_callback` with `FrozenStateChangeCallback` and
  `FrozenState` (AOSP `addFrozenStateChangeCallback`, libbinder
  android-15.0.0_r6+), on drivers that report
  `features/freeze_notification`; `InvalidOperation` elsewhere and on RPC.
  `ProcessState::freeze_process` and `process_freeze_info` issue
  `BINDER_FREEZE` / `BINDER_GET_FROZEN_INFO` (AOSP `IPCThreadState::freeze` /
  `getProcessFreezeInfo`); Android's SELinux grants both to `system_server`
  only, so elsewhere they fail with `EACCES`. A freeze request goes to the
  driver alone, so a C driver older than Linux 6.13 that refuses it for a
  binder whose process has died (fixed upstream in `ca63c66935b9`, backported
  to 6.12.4 and the GKI branches in late 2024) fails the add instead of
  aborting the process.

### Changed

- **The entry API takes a typed destination, `rsbinder::Uri`**, built from
  `Uri::kernel()`, `Endpoint::unix` / `unix_abstract` / `vsock` / `tls`,
  `KernelEndpoint` and `with_service` / `with_wire`, or parsed from a string
  with the same schemes and query keys as before (`Uri::parse`, `str::parse`;
  what the parser now accepts or refuses differently is under *Migrating*,
  "Entry API"). A parse failure is a `UriError` whose `kind()` says why; it
  converts into `StatusCode::BadValue` for `?`, logging the reason. `Display`
  writes the string form back, and parsing it yields the same `Uri` for every
  value `serve` / `connect` accept, so a Unix path or driver path with `#`,
  `?`, `%` or non-UTF-8 bytes is now expressible (a non-UTF-8 driver path is
  still `BadValue` at `serve` / `connect`, since `ProcessState` opens a
  `&str`). `serve` / `connect` refuse with `BadValue` the values the
  string form cannot carry: a relative Unix path, an empty abstract name, TLS
  host or service name, and a kernel endpoint with
  `WireProfile::Android13Plus`. `connect_async` converts its argument at the
  call, so its future no longer borrows it. `KernelEndpoint`, `WireProfile`,
  `WireVersion`, `UriError` and `UriErrorKind` are in `rsbinder::entry`.
- **`rsbinder-aidl` warns on a bare `@nullable` field that closes a reference
  cycle**, which AOSP `aidl` rejects; write `@nullable(heap=true)`.
- **A kernel proxy's cache entry and `BC_INCREFS` reference are released when
  the last `SIBinder` and `WIBinder` for the handle are gone**, as in AOSP
  `~BpBinder`; 0.11.0 kept them until the remote died.
- **`proxy_count` counts a kernel proxy over AOSP's `BpBinder` lifetime**, and
  charges the per-uid count to the uid seen when the handle was first counted.
- **`to_bytes` / `from_bytes` no longer require the `rpc` feature.**
- **`rsb_hub`'s configuration trust check walks from `/` one
  `openat(O_NOFOLLOW)` per component** and reads through the descriptor it
  checked, so a rename between check and read changes nothing.
- **`rsb_hub` re-reads the groups of a caller a group rule denied**, at most
  once every 15 s per uid (`GroupCache::MIN_REVALIDATE`).
- **`rsb_hub` starts a service shared by several declarations once** per
  `exec` argv or `systemd` unit, not once per name.
- **Fewer copies and allocations per transaction and per RPC frame**
  (`String16`, oneway replies, RPC sends, stream items). No wire change.
- **`@EnforcePermission` over kernel binder costs one IPC per check, not two**:
  the `IPermissionController` is kept, as AOSP keeps `gPermissionController`.
- **`rsbinder-aidl` generated code passes `clippy::unwrap_used`,
  `clippy::expect_used` and `clippy::allow_attributes_without_reason`**: a
  `Builder::hash()` proxy keeps the remote hash in a `OnceLock` instead of a
  `Mutex` locked with `unwrap()`, and every generated `allow` carries a
  `reason`. The hash a proxy returns is unchanged.

### Deprecated

- **`rpc::RpcUnixClientConfig`** → `RpcClientConfig::unix` / `unix_abstract`.
- **`RpcSession::setup_unix_client_android13plus_with_config`** →
  `setup_client_android13plus_with_config` with a `RpcClientConfig`.
- **`RpcSession::setup_unix_client_android13plus_with_id`** and
  **`ClientOptions::session_id`** → set up a new session, then
  `add_outgoing_connection_with_config` with
  `.session_id(&session.get_session_id()?)` on it; a non-empty id is refused.
  **`_fan_out`** → the same setup call with `.outgoing_connections(n)`.
- **`RpcSession::connect_android13plus_fd_with_id`** →
  `connect_android13plus_fd`; a non-empty id is refused.
- **`RpcSession::add_outgoing_connection_android13plus`** and
  **`add_{outgoing,incoming}_connection_android13plus_with_config`** →
  `add_{outgoing,incoming}_connection_with_config`.
- **`RpcClientConfig::handshake_timeout`** and
  **`ClientOptions::handshake_timeout`** → `timeout`.
- **`rpc::EndReason::Unreadable`** and **`EndReason::Retired`** are no longer
  produced; a serve loop reports `SessionEnded`.

The deprecated items still work and delegate to the new ones, apart from the
session ids above, and are removed in the release after 0.12.0. The
single-connection setup calls (`setup_unix_client_android13plus`, `_abstract`,
`_fd`, `setup_tcp_client_tls_android13plus`) are not deprecated.

### Fixed

Kernel binder and `Parcel`:

- **A death link survives the previous proxy's late clear.** When the last
  proxy of a handle dropped while its `BC_CLEAR_DEATH_NOTIFICATION` was still
  queued (on a looper, until its handler returned) and another thread
  re-resolved the handle and linked, the driver ignored the new request and
  the old clear removed the registration: the new recipients never heard of
  the death, and the handle's `BC_DECREFS` was held for good. The registration
  is now per handle, cleared by its last linked proxy, and a link during a
  clear requests again after the clear completes. AOSP's `BpBinder` has the
  same order. `link_to_death` now fails when its request never reached the
  driver, instead of reporting a link that cannot fire.
- **A parcel refuses a write that overlaps an object it recorded**
  (`PermissionDenied`); a rewritten fd number could close an fd the parcel did
  not own (see *Migrating*).
- **A kernel parcel closes the fds it holds, not the ones its bytes name**, as
  AOSP `Parcel::freeDataNoInit` does.
- **A big-endian host writes an fd or handle object the driver reads
  correctly**; it read handle or fd `0`.
- **`get_calling_sid()` no longer reads a freed transaction buffer.** The
  context is copied with `process_vm_readv(2)`, which a seccomp filter must
  allow, and kept only where SELinux is available (see *Migrating*).
- **`BinderFeatures::set_requesting_sid` requests a context only where SELinux
  is available**; elsewhere every call to such a binder failed with
  `FailedTransaction`.
- **A handler can return a binder proxy it received in the same transaction**:
  a kernel `Parcel` holds a reference on every proxy written into it.
- **`MappedHeap::from_fd` refuses an ashmem range past the region** with
  `BadValue`, instead of a `SIGBUS` on first access.
- **A `FatalIfNotOneway` thread no longer panics when it reads a proxy new to
  the process.**
- **`ProcessState::set_call_restriction` applies to the calling thread** (see
  *Migrating*).
- **Reading a `ParcelableHolder` whose objects are out of offset order** no
  longer underflows an offset.
- **`get_extended_error()` returns `InvalidOperation`** on a driver without
  `BINDER_GET_EXTENDED_ERROR`, as documented.
- **`list_services` on Android 10 no longer stops at a 127-character name.**
- **On Android 10 a build without the `android_10` feature names the missing
  feature**: `hub::default` fails with `InvalidOperation`, as on the other
  SDKs, where `context_object()` failed with `PermissionDenied`.
- **`connect_async` on a kernel endpoint can be cancelled** by dropping the future.
- **A synchronous handle to a local async service no longer panics when called
  from async code**; see `TokioRuntime`'s docs for the cases left.

Status codes:

- **On a host whose errno numbering is not Linux asm-generic, a status goes on
  the wire in AOSP's numbering** (see *Migrating*).
- **A transaction status never encodes a payload as `0`**, which the peer read
  as success (see *Migrating*).
- **`StatusCode::from(std::io::Error)` no longer panics** on errno `0` or one
  outside the OS's range; it is `Unknown`.
- **`FLAG_COLLECT_NOTED_APP_OPS` is `0x2`**, the AOSP `IBinder.java` value
  (see *Migrating*).

RPC:

- **A slow link no longer trips the session's deadlines while the peer keeps
  up.** On the bundled socket transports a send deadline counted only the
  sends the kernel accepted, and the kernel reports room after a third of a
  send buffer TCP grows to megabytes has drained; a read deadline (a reply
  wait, a server's idle wait) counted only the peer's bytes, while the tail of
  a large frame of ours could still be on its way to it. Over a 1 Mbit/s link
  a 2 MiB call with `set_timeout(1 s)` ended the session after 2.5 s, and a
  server with `set_idle_timeout(1 s)` ended one while its large reply was
  still being delivered. On Linux and Android both now count the peer taking
  this end's bytes (`SIOCOUTQ`) as progress; on Apple neither changes, nor on
  a socket that refuses `SIOCOUTQ` (Android SELinux on another domain's
  socket, a vsock socket on a kernel whose vsock does not report its queue),
  which is asked once. See
  the `transport` module doc "A slow peer".
- **An android-13+ server refuses a session id that is not 32 bytes before
  reading it**, as AOSP `RpcServer::establishConnection` does; it read up to
  65535 bytes first, holding the connection's worker meanwhile.
- **A handshake deadline bounds the whole handshake, not each read.** A peer
  that sent one byte at a time, each inside the deadline, kept a server's
  handshake going indefinitely (holding its worker, and under
  `set_max_connections` an admission slot, so that `stop_and_join` waited on
  it too) and a client's setup call from returning. A thread of the
  deadline's own now cuts the connection once the phase outlives it:
  `set_handshake_timeout` from the accept to admission (the authorizer's run
  left out), the client's
  `timeout` for each TLS or android-13+ handshake step, and
  `RpcSession::from_preconnected_fd`'s 10 s for its handshake. An expired TLS
  handshake reports `TimedOut`, not `WouldBlock`.
- **A `tls` client's connect deadline bounds every resolved address
  together.** In 0.11.0 `ClientOptions::handshake_timeout` gave each address
  the host name resolved to the full deadline, so a host with `N` silent
  addresses held the setup call for `N` times it. `timeout` (and the
  deprecated `handshake_timeout`, `RpcClientConfig`'s too) now bounds them
  together: each try gets an even share of the time left but at least 2 s
  (Go `net`'s floor; all of it when less is left), so an address refused at
  once leaves its share to the next, and one slower to accept than its share
  is given up for the next (see *Migrating*).
  `RpcSession::setup_tcp_client_tls` and
  `setup_tcp_client_tls_android13plus` take no deadline, and their docs now
  say so and name the bounded form.
- **On the android-13+ wire a null binder interoperates with libbinder**: it
  carries the stability `int32` AOSP writes (see *Migrating*).
- **On the r34 wire an android-12 peer's binder is no longer refused one time
  in 256.**
- **On the r34 wire releasing many references to one binder no longer builds a
  frame per reference up front**, an allocation the peer sized.
- **`RpcServer::live_session_node_count` counts the sessions of an r34
  server**; it read 0.
- **Dropping a proxy right after a oneway on it no longer aborts a libbinder
  peer**: the `DEC_STRONG` waits for what the peer pays back, as AOSP
  `sentRef`.
- **An android-13+ libbinder client no longer keeps an rsbinder object alive
  until the session ends**: rsbinder returns the `DEC_STRONG`s it owes.
- **Reading a binder twice no longer returns a reference the peer never
  gave.**
- **A client that only sends oneways no longer deadlocks with its server**: a
  send drains `DEC_STRONG`s while it waits. A custom `RpcTransport` drains by
  implementing `send_raw_draining`, a custom `TlsStream` by returning its
  `socket`, whose `SO_SNDTIMEO` then carries the send deadline: on Linux and
  Android every send on such a stream waits on that socket
  (`TlsStream::socket`).
- **An RPC parcel dropped unsent releases its local binders' reservations**;
  writing a local binder into an ended session's parcel is `DeadObject`.
- **A `ParcelableHolder` read from an RPC transaction decodes the binders and
  v1+ fds in its payload.**
- **A `ParcelableHolder` whose read fails after the stability check is left
  empty**, as in AOSP.
- **A handler reply refused at the send reaches the caller as a status**
  (`BadValue` for more than 64 fds, `FailedTransaction` over `MAX_FRAME_LEN`)
  and the connection keeps serving.
- **A `oneway` call inside a handler no longer rides the incoming connection**
  (see *Migrating*).
- **A `DUMP_TRANSACTION` that arrives over RPC runs `on_dump`.**
- **A send to a closed peer is an error, not `SIGPIPE`**, on the bundled
  Unix-socket, TLS and `tcp_debug` paths. On Apple platforms a `TlsStream`
  over a stream from elsewhere needs the caller to set `SO_NOSIGPIPE`.
- **A TLS send that rustls stopped part-way through a frame ends the
  session**; it returned `Protocol`.
- **On macOS a call whose peer closed while it waited returns `DeadObject`**,
  not `BadValue`.
- **A transaction made inside a reply wait no longer clears the outer call's
  reply deadline.**
- **A session timeout too large for an `Instant` no longer panics.**
- **A TLS or `tcp_debug` connect that hits its deadline returns `TimedOut`**,
  not `Unknown`.
- **The `unix` transport's raw sends no longer count the 16-byte header
  against `MAX_FRAME_LEN`.**

`rsbinder-aidl` and `rsbinder-macros`:

- **An AIDL item named `Ok`, `Err`, `Some`, `None`, `Default`, `Option`,
  `Vec`, `Box` or `String` no longer breaks the generated code**, and the
  macros compile in a module that shadows a prelude name. A `Box` nested in an
  interface still breaks the async trait.
- **A reference cycle cut by a `@nullable` field no longer rejects the other
  fields on it.**
- **A direction followed directly by a comment parses** (`in/*c*/ int[] a`).
- **A bidirectional control character no longer reaches a generated string
  literal raw**; rustc refused the user's crate.
- **A union's implicit `Tag` has its enumerators**, so `U.Tag.b` resolves.
- **A nested declaration inside a `@VintfStability` type inherits that
  stability** (see *Migrating*).
- **A `@VintfStability` interface published through the sync path registers
  with VINTF stability**; peers requiring VINTF refused it.
- **A type nested three or more levels deep can name a type from a grandparent
  scope.**
- **`@Descriptor("")` uses the canonical name**, as AOSP does.
- **A `@FixedSize` union's `Tag` is backed by `i8`**, as in AOSP (see
  *Migrating*).
- **A hex literal ignores a `u32`/`u64` suffix**, as AOSP `ParseIntegral` does
  (see *Migrating*).
- **A union's `Default` for an enum-typed first field is the enum's `Default`**
  (see *Migrating*); a union whose first field is another union's `Tag` no
  longer panics the generator.
- **A name that collides with an item the generator emits is a diagnostic**,
  not a rustc error in `OUT_DIR`; the macros do the same for a method named
  `descriptor`, `dump`, `as_binder` and the like.
- **Fixed-size array arguments match AOSP's Rust backend**; a null element from
  a Java or C++ peer failed the call with `UNEXPECTED_NULL` (see *Migrating*).
- **Union variant names follow AOSP `GetCapitalizedName`** (see *Migrating*).
- **An unclosed `<` counts against the generic nesting limit (12)** (see
  *Migrating*).
- **Codegen `async` can be turned off again by a downstream crate.**

`rsb_hub`:

- **A `dump` that fails writing to the caller's fd answers `OK`**, as AOSP
  `BBinder::dump` does.
- **A `systemd` unit is passed to `systemctl` after `--`.**

### Security

- **`BpMemory` and `BpMemoryHeap` send their calls only with the `IMemory` /
  `IMemoryHeap` interface token.** A reply naming another interface's binder
  as the heap made the client send that binder code 1 under its own identity;
  such a binder is now `BadType`.
- **`rsbinder-aidl`: a permission annotation written after `oneway`
  (`oneway @EnforcePermission("X") void m();`) is enforced.** It was ignored,
  so the generated service ran the method for every caller. Rebuild services
  generated from AIDL of this shape.
- **`rsb_hub`: an `exec` program is checked like the configuration that names
  it**: the program and every directory above it must be owned by root or
  `rsb_hub`'s uid and not group- or world-writable. Whoever could replace one
  of them could run code as `rsb_hub` (normally root).
- **The `rustls` floor is 0.23.45** for RUSTSEC-2026-0285 (TLS 1.3 handshake
  messages accepted across encryption levels); `rpc-tls` only.

## [0.11.0] - 2026-09-10

### Migrating from 0.10.0

This release's breaking changes; some change only behavior or a value, so
nothing in your build will warn. The little-endian wire fix under *Platform
support* belongs here too (it changes bytes only on a big-endian host).

- **`ProcessState::init(path, 0)` no longer means "the default".** `0` now
  asks the kernel for zero binder threads, and nothing warns. Pass the newly
  public `DEFAULT_MAX_BINDER_THREADS` for the default. `init_default()` and a
  `binder://` URI without `?threads=` are unchanged.
- **`FLAG_PRIVATE_VENDOR` is now `0x10000000`, not `0`**, matching AOSP
  `IBinder.h`; passing it to `transact` now sets bit 28 on the wire. Nothing
  warns. `FLAG_PRIVATE_LOCAL` is unchanged (`0`).
- **`RpcServer::set_root` / `RpcSession::set_root` return `Result<()>`**, since
  they now refuse a *remote* binder (see *Changed*). Add `?` or
  `.expect(...)` at every call; without `-D warnings` the refusal goes
  unnoticed.
- **`rpc::transport::TlsStream` gained a required `shutdown_stream()`.**
  Custom stream implementations must add
  `fn shutdown_stream(&self) -> std::io::Result<()>` that shuts the stream down
  in both directions (a no-op would hang `RpcSession::close_session`). The
  bundled `TcpStream`/`UnixStream`/`VsockStream` impls are unaffected.
- **`shutdown` now names one operation; four methods are renamed**, with no
  deprecated aliases. Only the transport half-close keeps the name
  (`rpc::transport::RpcTransport::shutdown`). Only the first was public in
  0.10.0:
  - `RpcServer::shutdown` → `RpcServer::stop_accepting` (drain; nothing is
    joined). To end connected sessions too, use `RpcServer::terminate`.
  - `ServerGuard::shutdown` → `ServerGuard::stop_and_join`.
  - `RpcSession::shutdown` → `RpcSession::close_session`.
  - `TlsStream::shutdown` → `TlsStream::shutdown_stream` (above).
- **`RpcError::PeerClosed` is now `RpcError::EndOfStream`** (`Display`: "RPC
  stream ended"), since it never knew who ended the stream; that is
  `SessionEnd::by`. Match arms need the new name.
- **A TLS stream that ends without `close_notify` is no longer a clean close.**
  It is the new `RpcError::UncleanEndOfStream` (`StatusCode::DeadObject`), and
  a serve loop reaching it returns `Err(DeadObject)` instead of `Ok(())`.
  `TlsTransport::shutdown` now sends `close_notify`, so a deliberate close is
  still a clean `EndOfStream`; after it, this end's sends fail with
  `EndOfStream`. Plain-socket backends are unaffected.
- **`RpcSession::serve_blocking`, `serve_blocking_on` and
  `serve_blocking_clearing_deadline_after_first` return `rpc::SessionEnd`,
  not `Result<()>`.** Add `.into_result()` to the call. Differences from
  before: idle eviction is now `Ok(())` (was `Err(TimedOut)`), every lost
  stream is `DeadObject` (tell causes apart by `reason`), and a local
  `close_session` / `terminate` ends as `EndedBy::Local` with an intact stream.
- **`rpc::transport::RpcTransport::shutdown` is a required method** (its
  post-0.10.0 no-op default hung `close_session`). A custom transport must
  implement it; the rustdoc states the contract.
- **`rpc::transport::MemTransport::shutdown` models a Linux socket**: queued
  frames on either side are still delivered, then end of stream, and sends on
  either side fail. Tests relying on a queued frame vanishing see the
  difference.
- **`rpc::RpcError` gained `DeadlineMidFrame`** (`TimedOut`) for a read
  deadline that elapses mid-frame, previously `Truncated` (`NotEnoughData`);
  a serve loop ends with `EndReason::DeadlineMidFrame`. Code matching
  `RpcError::Truncated` for a deadline case needs the new arm.
- **A transaction that could not be sent fails with `StatusCode::WouldBlock`**
  (safe to retry), as AOSP does: a call on a session with no outgoing
  connection (was `FailedTransaction`) and a wait for a free connection slot
  that hit the deadline (was `TimedOut`). `TimedOut` now means the reply wait
  expired after the request went out, except for an armed android-13+ write
  deadline (`RpcServer::set_idle_timeout`) expiring before the first byte.
- **Dropping a `ServerGuard` now ends the server, connected clients
  included**, through the new `RpcServer::terminate` (stop accepting, end
  every session, join the workers). `RpcServer::stop_accepting` only drains;
  code that relied on it to end a server wants `terminate`.
  `RpcSession::close_session` now ends a session whatever its connection
  count.
- **rsbinder-aidl rejects `.aidl` it used to accept**: a
  `@JavaOnlyStableParcelable` / `cpp_header` / `ndk_header` declaration with
  no `rust_type`, an `in out` argument, `self`/`Self`/`super`/`crate` as an
  identifier or package segment, an enum discriminant outside its `@Backing`
  range, and a `@RustDerive` parameter outside AOSP's schema (`Debug` and
  `Default` are accepted and dropped).
- **rsbinder-aidl resolves names in AOSP's order** (enclosing scopes, imports,
  package): an `import a.IFoo` now outranks a same-package `IFoo`, and
  `Outer.X` no longer reaches `Outer.Inner.X`. Code relying on either fails
  with an unresolved-name diagnostic.
- **`AidlError::Template` gained a `source` field** (the `tera::Error`); a
  `match` arm `AidlError::Template { message }` needs `..`.
- **`out` arguments of `IBinder` / `ParcelFileDescriptor` / an interface are
  now `&mut Option<T>`** in generated traits, as in AOSP. Update
  implementations; the wire and `inout` are unchanged.
- **`@nullable` `out`/`inout` arrays of a primitive or enum are now
  `&mut Option<Vec<T>>`**, not `&mut Option<Vec<Option<T>>>`, matching
  libbinder's wire (fixed-size arrays too). Update implementations.
- **`rsbinder::service` is gone** — `Registry`, `Broker`,
  `service::{kernel,rpc}::{Host,Broker}`. Use `serve` / `connect`; the
  call-by-call mapping is under *Removed*.
- **`rsb_hub` refuses to start without an access-control policy.** Give it
  `--config <PATH>` (default `/etc/rsbinder/hub.d`) or, deliberately,
  `--insecure-allow-all`.
- **`rsb_device` creates the binder node `0600`.** Grant access with
  `--group <group> --mode 0660` and put the users in that group.
- **Low-level `Parcel` accessors are `pub(crate)`** (`as_ptr`, `as_mut_ptr`,
  `capacity`, `set_data_size`, `close_file_descriptors`, `is_empty`,
  `from_vec`), along with the RPC-ops plumbing and three `thread_state`
  helpers. Use `Parcel::from_ipc_parts` for raw buffers.
- **The RPC wire codec is private** (`WireMessage`, `WireCodec`, `R34Codec`,
  `Android13PlusCodec`, `WireReply`, `WireTransaction`, `RpcState`). Use
  `RpcServer` / `RpcSession` / `RpcProxy` and the transport traits.
- **`LazyServiceRegistrar::register_service` now registers, and the process
  exits when its last client goes away.** Drop your own `addService` /
  `registerClientCallback` calls next to it; to keep the process alive use
  `set_active_services_callback` or `force_persist`. It now takes
  `impl Into<SIBinder>` (drop `.as_binder()`) and returns
  `Result<(), Status>`; retype bindings that named `StatusCode`.
- **`WIBinder` has no `Native` fallback for proxies**; only an exhaustive
  `match` on the enum notices.
- **`IMemoryHeap::base` returns `Option<SharedBytes<'_>>`**, not `&[u8]`: a
  read-only view (`load` / `copy_to` / `slice` / `to_vec`). Write through
  `write_at` on the concrete heap types.
- **Nine module paths are gone; the crate root is the only way in.** Drop the
  module segment — every replacement below is the same type:

  | Was | Now |
  |---|---|
  | `rsbinder::parcel::Parcel` | `rsbinder::Parcel` |
  | `rsbinder::parcelable::{Serialize, Deserialize, Parcelable, …}` | `rsbinder::{Serialize, Deserialize, Parcelable, …}` |
  | `rsbinder::parcelable_holder::ParcelableHolder` | `rsbinder::ParcelableHolder` |
  | `rsbinder::proxy::{Proxy, ProxyHandle}` | `rsbinder::{Proxy, ProxyHandle}` |
  | `rsbinder::native::{Binder, BinderFeatures, is_handling_transaction}` | `rsbinder::{Binder, BinderFeatures, is_handling_transaction}` |
  | `rsbinder::error::{Result, StatusCode}` | `rsbinder::{Result, StatusCode}` |
  | `rsbinder::status::{BinderResult, ExceptionCode, Status}` | `rsbinder::{BinderResult, ExceptionCode, Status}` |
  | **`rsbinder::status::Result`** | **`rsbinder::BinderResult`** — see below |
  | `rsbinder::binder_async::{BoxFuture, BinderAsyncPool, BinderAsyncRuntime}` | `rsbinder::{BoxFuture, BinderAsyncPool, BinderAsyncRuntime}` |
  | `rsbinder::file_descriptor::ParcelFileDescriptor` | `rsbinder::ParcelFileDescriptor` |

  `rsbinder::status::Result` (`Result<T, Status>`) collided with
  `rsbinder::Result` (`Result<T, StatusCode>`); use `BinderResult`, the same
  type. Domain modules (`hub`, `rpc`, `thread_state`, `shared_memory`,
  `entry`, `bridge`, …) are untouched.
- **`Parcel::is_for_rpc` is deprecated in favour of `Parcel::is_kernel_backed`,
  with the polarity inverted**: replace with `!p.is_kernel_backed()`.
  `Parcel::set_for_rpc` is now internal; use the RPC session, or
  `rsbinder::to_bytes` for data.
- **`rsbinder::get_interface` is now `rsbinder::get_interface_async`** (it is
  the tokio one), matching `connect_async`. It no longer does the Android 10
  ~5s client-side poll; use `hub::wait_for_interface` to wait.
- **`hub::get_service` / `get_interface` and the two `ServiceManager` methods
  of the same names are removed** (deprecated since 0.10.0). Use
  **`try_get_service` / `try_get_interface`**, the same wire call, which
  return the failure instead of flattening it:

  | Was | Now |
  |---|---|
  | `hub::get_service(n)` | `hub::try_get_service(n).ok().flatten()` |
  | `hub::get_interface::<T>(n)` | `hub::try_get_interface::<T>(n)?.ok_or(StatusCode::NameNotFound)` |

  The Android 10 client-side ~5s poll is gone; use `wait_for_service` /
  `wait_for_interface` to wait on every version.
- **A `ParcelableHolder` no longer moves freely between kernel and RPC
  parcels** (details under *Fixed*); nothing in the build warns:
  - one received over RPC (or from `from_bytes`) is `BadType` in a kernel
    transaction once it carries a payload;
  - one received over kernel binder is `BadType` on RPC or in `to_bytes` when
    its payload carries an object (`FdsNotAllowed` if all are fds);
  - one read from an RPC parcel yields an RPC-mode sub-parcel, so reading a
    binder out of it is `BadType`;
  - one read from an RPC *session*'s parcel is `BadType` in `to_bytes`;
    `get_parcelable::<T>` still resolves it to a value that encodes anywhere.
- **An fd written to an RPC parcel with no negotiated fd mode now fails with
  `FdsNotAllowed`, not `BadType`**, as current AOSP does. A binder written to
  a parcel with no session stays `BadType`.
- **Test- and fuzz-only entry points need a feature.** The `__fuzz_*`
  decoders need `fuzzing`; `RpcSession::__slot_count`, `__incoming_thread_*`,
  `RpcServer::__set_attach_shutdown_probe` and `Parcel::__set_for_rpc` need
  `test-util`. If you called one of the nine 0.10.0 shipped (eight
  `__fuzz_*` and `__set_attach_shutdown_probe`), enable the feature.

### Platform support

- **The parcel wire is little-endian on every host.** It used to follow the
  CPU, so a big-endian host produced parcels no other machine could read.
  Little-endian builds emit identical bytes. The `BC_*`/`BR_*` stream and
  UAPI structs stay host-native, as the kernel driver requires. The RPC path
  is verified on big-endian (s390x under qemu); the kernel path is
  structurally correct but unverified.

### Added

- **rsbinder:** `to_bytes` / `from_bytes` — store a value using its AIDL
  encoding, without a second schema in protobuf or serde:

  ```rust
  std::fs::write("settings.bin", rsbinder::to_bytes(&settings)?)?;
  let settings: Settings = rsbinder::from_bytes(&std::fs::read("settings.bin")?)?;
  ```

  Binders and file descriptors are refused on write, reading is strict
  (leftover bytes are an error), and a parcelable wrapper gives forward
  compatibility. Behind the `rpc` feature. See the book's *Storing Values*
  and `example-hello`'s `serde_demo`.
- **rsbinder:** a **gateway** is now one line. `Strong<I>` implements
  `Interface` and generated interfaces implement themselves for
  `Strong<dyn IFoo>`, so `serve(uri)?.add("foo", BnFoo::new_binder(upstream_proxy))?`
  re-publishes a service onto another transport; version and hash report the
  upstream's values. Binder arguments still need re-wrapping; see the book's
  [cross-transport chapter] and `example-hello`'s `gateway_service`.

  [cross-transport chapter]: https://hiking90.github.io/rsbinder/cross-transport-services.html
- **`rsbinder::bridge::Rewrap`** — one local wrapper per remote binder, so a
  gateway forwarding a binder argument keeps identity across calls
  (`register(cb)` / `unregister(cb)`). `Rewrap::new(BnCallback::new_binder)`
  then `.wrap(cb)`; holds only weak references, pruned on death, drop or
  `purge_dead()`.
- **rsbinder (RPC):** client-side **incoming (callback) connections** —
  `RpcUnixClientConfig::incoming_connections(n)`,
  `RpcSession::add_incoming_connection_android13plus_with_config`, and
  `ClientOptions::incoming_connections` (AOSP `setMaxIncomingThreads`), so a
  server can call a client's callbacks from any thread. Losing the last
  incoming connection is the session's death. Android-13+ profile, Unix
  sockets.
- **rsbinder (RPC):** `ClientOptions::handshake_timeout` and
  `RpcUnixClientConfig::handshake_timeout` — a deadline for the connection
  handshake on `?profile=android13plus` and `tls://` (default `None`,
  unbounded). On a plain r34 endpoint it is `StatusCode::BadValue`.
  Client-side counterpart of `ServeOptions::handshake_timeout`.
- **rsbinder (RPC):** `RpcServer::set_reply_timeout` — bounds how long the
  server waits for a reply to a callback it issued (default `None`).
- **rsbinder (`binder`):** `Strong::try_into_async` — the fallible form of
  `into_async`, returning `BadType` for a sync-only local service where
  `into_async` panics.
- **rsbinder (RPC, `vsock`):** `vsock://…?profile=android13plus` now works
  (it failed at the first handshake byte), e.g. against libbinder on
  Microdroid/AVF.
- **rsbinder (kernel):** a thread that talked to the binder driver now sends
  `BINDER_THREAD_EXIT` when it ends, as AOSP does, instead of leaving a
  `binder_thread` behind in the kernel.
- **rsbinder (`hub`):** an `android_15` feature — the Android 15
  service-manager protocol from `android-15.0.0_r6` on, which shifted
  transaction codes without changing the SDK version. `android_14` still
  serves the initial release; with both enabled `hub::default` probes once
  and picks. Lookups go through `getService` only, so VINTF `<accessor>` is
  not supported on Android 15.
- **rsbinder (`lazy_service`):** `LazyServiceRegistrar::instance`, the
  process-wide registrar (AOSP `getInstance`; `new` remains for an independent
  one), and `set_active_services_callback` (AOSP `setActiveServicesCallback`;
  returning `true` takes over the shutdown decision).
- **example-hello (`hello_callback_demo`):** now a real lazy service that
  exits once the last client goes away, tested against both `rsb_hub` and
  Android's `servicemanager`. `rsb_hub` logs `Unregistering <name>` for an
  honoured `tryUnregisterService`, as AOSP does.
- **rsbinder-tools (`rsb_service`):** a new CLI for querying the service
  manager (Linux counterpart of `service` / `dumpsys -l`): `list`, `info`,
  `check`, `declared`, `instances`, `connection`, `dump <name> [args...]`.
  Exit status `0` yes, `1` no, `2` could not answer; policy denials are
  reported as such.
- **rsbinder-tools (`rsb_hub`):** `dump` support: `rsb_service dump manager`
  prints registrations, subscriptions, access-control mode and names being
  waited on. Gated on `list`, filtered by `find`.
- **rsbinder-tools (`rsb_hub`):** service-supervisor integration: clean exit
  on `SIGTERM` / `SIGINT`, and systemd `Type=notify` (`READY=1` once it holds
  handle 0, `STOPPING=1`, `STATUS=`) without `libsystemd`. A second hub on the
  same device names the cause.
- **rsbinder (hub):** `hub::get_declared_instances` and
  `hub::get_connection_info` (Android 12 / 13 protocols and Linux), and
  error-preserving `try_list_services`, `try_is_declared`,
  `try_get_declared_instances`, `try_get_connection_info` and
  `try_get_service_debug_info` (on `ServiceManager` and as free functions).
- **rsbinder-tools (`rsb_hub`):** on-demand service start: a `[[service]]`
  entry with `start = { systemd = "unit" }` or `start = { exec = [...] }` is
  started when a `getService` misses it (AOSP `tryStartService`), at most one
  attempt per name at a time; `checkService` never starts anything. `rsb_hub`
  now refuses a configuration writable by anyone but its owner, or owned by
  someone other than root or `rsb_hub`.
- **rsbinder-tools (`rsb_hub`):** service declarations: a `[[service]]` entry
  (`pack.age.IFoo/instance`) answers `isDeclared`, `getDeclaredInstances` and
  `getConnectionInfo` in place of VINTF, filtered by `find`.
- **rsbinder:** `WIBinder` now implements `PartialEq<SIBinder>` (and the
  reverse), so a `DeathRecipient` can test `*who == stored`.
- **rsbinder (`macros` feature):** `#[rsbinder::interface]`,
  `#[derive(Parcelable)]` and `#[derive(BinderEnum)]` — declare an interface
  as a Rust trait with no `.aidl` or `build.rs`, generating code identical to
  the equivalent `.aidl`. `&mut T` is out, `#[inout]` round-trips,
  `Option<T>` is nullable, `#[oneway]` drops the reply. `BinderEnum` needs
  `#[repr(i8|i32|i64)]` and is closed (undeclared values are `BadValue`).
  `.aidl` stays canonical for unions, constants, `@VintfStability`,
  `@EnforcePermission`, `ParcelableHolder`, generics and nested types.
- **rsbinder-aidl:** new public `render` module — `FnMembers`,
  `InterfaceRender` / `ParcelableRender` / `EnumRender`, `ParcelableMember`,
  and `render_interface` / `render_parcelable` / `render_enum`, the seam
  `rsbinder-macros` uses. Input structs are `#[non_exhaustive]` with `new()`.
  Generated output is unchanged.
- **rsbinder-aidl:** `Builder::dest_dir` — set the output directory without
  the process-wide `OUT_DIR` variable.
- **rsbinder (entry API):** `rsbinder::serve(uri)` / `rsbinder::connect::<dyn I>(uri)`
  / `rsbinder::Client` — one bootstrap for every transport, selected by a URI
  (`binder://`, `unix://`, `unix-abstract://`, `vsock://`, `tls://`,
  `#service`, `?profile=android13plus`):
  `serve("binder://")?.add(name, BnFoo::new_binder(impl))?.run()?`. Other
  options go through `ServeOptions` / `ClientOptions` (wrong transport is
  `BadValue`); `connect_async` with `tokio`. `ServerGuard::server()` /
  `Client::session()` expose the RPC layer, `Client::open_with`'s closure
  receives the `Endpoint`, and `Endpoint::supports_fd_passing()` is public.
  `CallRestriction` is re-exported at the root, `#[non_exhaustive]` and
  `PartialEq`.
- **rsbinder (rpc):** `RpcSession::close_session()` — declare a session dead
  now (obituaries, release of the peer's local objects, AOSP
  `RpcState::clear`), shutting every connection down and joining the incoming
  threads. It does not join a user-driven `serve_blocking`.
- **rsbinder (shared memory):** `shared_memory` is now a working
  implementation: `MemoryHeapBase` (memfd with seals on Linux/Android,
  `shm_open` on macOS), `MappedHeap` (`from_fd_strict` verifies seals),
  wire-compatible `BnMemoryHeap`/`BpMemoryHeap` and `BnMemory`/`BpMemory`
  (`android.utils.IMemoryHeap` / `IMemory`), and `SharedMemory`
  (`android.os.SharedMemory`, byte-identical to `ParcelFileDescriptor`).
  Also `MemoryHeapBase::seals()` and `seal_future_write` on both heap types.
  Validated against libbinder on an Android 16 emulator. `rustix` gains `fs`
  (and `shm` on macOS).
- **rsbinder (shared memory):** `MemoryDealer` — AOSP's chunk allocator over
  one shared heap; each `Allocation` is an `IMemory` window freed on drop.
  `HeapCache` / `BpMemory::new_with_cache` map a dealer's heap once.
- **example-hello / book:** `shm_service` / `shm_client` and a *Shared Memory*
  chapter.
- **rsbinder (internal):** `ParcelFileDescriptor` serialization is layered on
  crate-private `write_raw_fd` / `read_raw_fd`; bytes are unchanged.
- **rsbinder-tools (`rsb_hub`):** per-name access control: every entry point
  is gated on an `add` / `find` / `list` policy keyed on the caller's uid and
  groups (not pid). TOML via `--config <PATH>` (file or directory), first
  matching rule wins, `SIGHUP` reloads. A denied lookup reports "not
  registered"; other denials are `EX_SECURITY`; listings are also filtered
  per name by `find`. See the
  [Service Manager chapter](https://hiking90.github.io/rsbinder/service-manager.html#access-control).
- **rsbinder-tools (`rsb_device`):** `--group` and `--mode` for the binder
  device node, the only gate on who may speak binder.

### Changed

- **rsbinder:** writing a binder that belongs to the *other* IPC stack — an
  RPC proxy into a kernel parcel, or a kernel proxy into an RPC parcel — is
  now refused at **write time** with `InvalidOperation`, matching libbinder,
  instead of failing on the receiver's first call. `serve(rpc://…).add`,
  `RpcServer::add_service` and `set_root` (which now return `Result<()>`, see
  *Migrating*) reject a remote binder up front; wrap the proxy in a local
  `Bn*` (`BnFoo::new_binder(proxy)`) instead. Kernel `hub::add_service` still
  accepts a *kernel* proxy.
- **rsbinder (RPC):** a non-nested call on a session with no connection to
  send on now fails at once with `WouldBlock` and a log line, instead of
  waiting forever. Connection slots are tagged by direction (AOSP
  `mOutgoing`/`mIncoming`), and a nested call from inside a **oneway**
  handler goes out on a connection the peer reads (or `WouldBlock`).
  **Breaking for `RpcSession::new(t, AddressSpace::Acceptor)`:** such a
  session can no longer transact outside a dispatch (`get_root`,
  `RpcProxy::transact`, `ping_binder` return `WouldBlock`); the peer must open
  incoming connections (`?profile=android13plus`).
- **rsbinder (RPC):** the server's callback-slot budget (`2 × set_max_threads`)
  now counts callback connections only; callback slots no longer get the
  `set_idle_timeout` read deadline (use `RpcServer::set_reply_timeout`), and a
  refused callback attach is an error on the client.
- **rsbinder (RPC):** a `DEC_STRONG` no longer takes a serve-driven slot
  (AOSP `ExclusiveConnection::find`); without an outgoing connection the
  reference is released at session end. Open an incoming connection for
  prompt release.
- **rsbinder (RPC):** on session death every connection is shut down first and
  the pool emptied, so a `binder_died` handler may call
  `RpcSession::close_session` without deadlocking.
- **rsbinder (RPC):** attaching to a session that died during the attach
  handshake now fails with `DeadObject`.
- **rsbinder (`shared_memory`) — breaking:** `IMemoryHeap::base` now returns
  `Option<SharedBytes<'_>>`, a read-only view copying through atomics (a
  `&[u8]` over peer-written memory was unsound). `read_at` / `write_at` use
  the same atomics, making `MemoryHeapBase: Sync` sound.
  `MemoryDealer::allocate` returns a window of the requested size, not the
  granule-rounded one.
- **rsbinder (`shared_memory`):** `MappedHeap::from_fd` refuses a zero-length
  fd unless it is really ashmem (an un-`ftruncate`d memfd caused `SIGBUS`), and
  `SharedMemory` gates `ASHMEM_GET_SIZE` the same way.
  `MemoryHeapBase::seal_future_write` reports `InvalidOperation` for a heap
  created without `FLAG_MEMFD_ALLOW_SEALING`.
- **rsbinder (`proxy`):** `ProxyHandle` equality is `(handle, generation)`,
  and it is now `Eq`.
- **rsbinder (`Parcel`):** `set_data_position` ignores (and logs) a position
  above `i32::MAX`, as AOSP does; see also *Fixed*.
- **rsbinder (entry):** `ClientOptions::tls` / `tls_server_name` and
  `ServeOptions::tls` on anything but `tls://` are `BadValue` instead of
  silently plaintext (TLS over Unix/vsock remains via
  `RpcServer::setup_unix_server_tls` / `setup_vsock_server_tls`).
  `ClientOptions::session_id` on the multi-connection path is now forwarded.
- **rsbinder (RPC):** `GET_FD_MODE` on a session already in `Unix` fd mode
  changes nothing (it used to downgrade it); an unsolicited `REPLY` ends the
  session; `find_conn` waits are bounded by `RpcSession::set_timeout`
  (`WouldBlock`); `close_session` logs when there is nothing to tear down.
- **rsbinder (`lazy_service`):** `force_persist` may **not** be called from
  the active-services callback (it deadlocks); use `try_unregister` /
  `re_register` there.
- **rsbinder-aidl — breaking** (each listed under *Migrating*): a
  `@JavaOnlyStableParcelable` or `cpp_header` / `ndk_header`-only parcelable
  without `rust_type` is an `AidlError::Semantic` instead of an empty struct;
  `out` arguments without `Default` are `&mut Option<T>` (was uncompilable);
  `in out` / `out in` and `self`/`Self`/`super`/`crate` identifiers are
  rejected; `@nullable` `out`/`inout` primitive or enum arrays are
  `&mut Option<Vec<T>>`; `@RustDerive` accepts only AOSP's seven traits
  (`Copy`, `Clone`, `PartialOrd`, `Ord`, `PartialEq`, `Eq`, `Hash`);
  `AidlError::Template` gained `source`.
- **rsbinder-aidl:** generic nesting has its own, tighter limit (12, vs 256
  for brackets), so deep generics report a diagnostic instead of hanging the
  build; the diagnostic names which limit it hit.
- **rsbinder-aidl:** `Builder::hash("")` now panics like `Builder::version(0)`.
- **rsbinder (`lazy_service`) — breaking:** `LazyServiceRegistrar` now does
  what its name says: `register_service` calls `addService` (with
  `FLAG_IS_LAZY_SERVICE`) and `registerClientCallback` itself, tracks
  `onClients`, and once no service has clients unregisters them and exits
  with status 0 (AOSP `tryShutdownLocked`). It takes `impl Into<SIBinder>`,
  returns `Result<(), Status>`, and refuses Android 10. The registrar is
  `Clone`; `re_register` no longer resets `has_clients`; a new service starts
  with no clients. Registering several services in a row can exit mid-loop;
  hold it with `force_persist(true)` first.
- **rsbinder — breaking:** `ProcessState::init`'s `max_threads` is passed to
  `BINDER_SET_MAX_THREADS` **as written**, as AOSP does: `0` no longer means
  "the default" and values `>= 15` are no longer clamped. `init_default()`
  and `binder://` without `?threads=` pass the new `DEFAULT_MAX_BINDER_THREADS`
  and are unchanged; `?threads=0` gets zero. The ceiling is logged at `info`.
- **rsbinder — breaking (internal representation):** a proxy's weak identity
  is stamped into the `ProxyHandle` at construction. `WIBinder` has no
  `Native` fallback for proxies, `downgrade` no longer takes the cache lock or
  needs an initialized `ProcessState`, and weak references to the same
  `(handle, generation)` compare equal.
- **rsbinder-tools (`rsb_hub`) — breaking:** refuses to start without an
  access-control policy and denies what it does not allow (see *Migrating*);
  a policy that fails to load never falls back to permissive.
- **rsbinder-tools (`rsb_device`) — breaking:** the binder node is created
  `0600` instead of `0666`; `sudo rsb_device binder --group binder --mode 0660`.
- **rsbinder-tools (`rsb_hub`):** `listServices` and `getServiceDebugInfo`
  return names in sorted order, as AOSP does.
- **rsbinder-aidl / rsbinder (async):** generated `IFooAsyncService` impls use
  the re-exported `#[rsbinder::__async_trait]`, so consumers no longer need
  their own `async-trait` dependency. Generated source changes, not the wire.
- **rsbinder (rpc):** an RPC proxy now holds its `RpcSession` **strongly**, as
  AOSP does: dropping the session handle no longer invalidates its proxies,
  and the connection closes with the last proxy. Code that dropped the session
  to disconnect must drop the proxies too. Session death now releases the
  peer's local objects, including for a client-only session whose last
  connection is lost (which also fires `DeathRecipient`s).
- **rsbinder (entry API):** `ServeOptions::fd_modes` / `ClientOptions::fd_mode`
  reject Unix fd passing on a transport that cannot carry `SCM_RIGHTS`.
- **rsbinder (AOSP alignment):** `FLAG_PRIVATE_VENDOR` is now `0x10000000`
  (see *Migrating*).
- **rsbinder (`rpc` feature, breaking):** `WireMessage::DecStrong(RpcAddress,
  u32)` and `RpcState::dec_strong_local(.., amount)` carry the decrement
  amount, so a batched `RpcDecStrong` from libbinder no longer leaks nodes.
- **rsbinder (breaking):** no longer public: the RPC wire-codec types
  (`WireMessage`, `WireCodec`, `R34Codec`, `Android13PlusCodec`, `WireReply`,
  `WireTransaction`), `RpcState`, `RpcParcelOps`, `Parcel::attach_rpc_ops`,
  `FnFreeBuffer`, `rpc::RpcSessionInner`, `RpcProxy::stamp_descriptor`,
  `thread_state::{check_interface, is_handling_transaction,
  get_calling_uid_or_self}` and `Parcel::{as_ptr, as_mut_ptr, capacity,
  set_data_size, close_file_descriptors, is_empty, from_vec, set_for_rpc}`.
  `rsbinder::is_handling_transaction` and `Parcel::from_ipc_parts` remain.
- **rsbinder (kernel binder):** a driver that consumed only part of the
  command buffer now prints the diagnosis to stderr and **aborts the process**
  (as AOSP does) instead of `panic!`, which could be caught and continue on a
  desynchronized stream.
- **rsbinder (kernel binder):** a failed `BINDER_WRITE_READ` now logs the
  command the driver refused.

### Removed

- **rsbinder (`service` module):** the `rsbinder::service` facade (`Registry` /
  `Broker`, `service::kernel::{Host, Broker}`, `service::rpc::{Host, Broker}`)
  is gone, replaced by the entry API (`serve` / `connect` / `Client`):
  `kernel::Host::new()? + add_service + serve()` →
  `serve("binder://")?.add(..)?.run()?`; `rpc::Host::unix(p)` →
  `serve("unix://<p>")`; `kernel::Broker::new()?.get_interface(n)` →
  `connect("binder://<n>")`; `rpc::Broker::unix(p)?.get_interface(n)` →
  `connect("unix://<p>#<n>")`; `Host::builder()` options → `ServeOptions`
  via `Server::with`.

### Fixed

- **rsbinder (kernel binder) — a service registered without an AIDL
  interface could not be resolved at all** (its null `INTERFACE_TRANSACTION`
  answer was `UnexpectedNull`; 23 of 280 services on a stock Android 34
  image). The null is now read as empty, as AOSP does.
- **rsbinder (RPC, TLS) — a session's own shutdown read as a truncation on
  its own side**; the transport now reports its own shutdown as a clean end.
- **rsbinder (RPC) — a second `shutdown()` on a socket transport failed on
  macOS** (`ENOTCONN`). `shutdown` is idempotent on every backend; remaining
  failures are logged at `warn!`.
- **rsbinder (RPC) — a slot a nested call had marked unreadable could still
  be handed out for writing.** The slot selectors now refuse it (`DeadObject`
  for its owner, `WouldBlock` if it was the only outgoing slot).
- **rsbinder (RPC) — a local shutdown ended a serve loop as `Ok(())` or
  `Err(DeadObject)` by race.** Every end after a local decision is now
  `EndedBy::Local`, and a frame read after it ends the loop
  (`EndReason::Interrupted`) instead of being dispatched.
- **rsbinder (RPC, fd-mode) — `UnixTransport::shutdown` left buffered bytes
  for the next reader**; the buffer is now cleared after the socket goes down.
- **rsbinder (RPC, `rpc-tcp-debug`) — a preconnected `AF_INET` fd could not
  complete the android-13+ handshake** (`RpcSession::from_preconnected_fd`);
  `TcpDebugTransport` now implements raw byte access.
- **rsbinder (RPC) — `ServerGuard::drop` blocked for as long as a client
  stayed connected.** `RpcServer::terminate` now ends every session the server
  minted (r34 included) and joins the workers, and is safe from a handler.
  `RpcSession::close_session` now ends a session with several connections.
- **rsbinder (RPC, TLS) — a TCP end without `close_notify` read as a clean
  close**, which is what a truncation attack looks like. It is now
  `RpcError::UncleanEndOfStream` (`DeadObject`, logged), and
  `TlsTransport::shutdown` waits for the in-flight send and then sends
  `close_notify` (bounded wait).
- **rsbinder (RPC) — a refused attach was reported to the client as success**,
  breaking a later unrelated call. `add_outgoing_connection_android13plus`
  and `setup_unix_client_android13plus_with_id` (and
  `ClientOptions::session_id`) now confirm admission with one
  `GET_SESSION_ID` round trip. `RpcSession::session_id()` is client-local;
  `get_session_id()` fetches the peer's.
- **rsbinder (RPC) — a nested call that lost track of the stream left the
  connection for the outer call to read**, which could take the nested reply
  as its own. The slot is now marked unreadable and retired; a serve loop
  ending this way reports `DeadObject`.
- **rsbinder (RPC) — a send that stopped part-way left its connection in the
  pool** (replies and `DEC_STRONG` ignored the error). Every partial send now
  retires or poisons its slot. A failure before the first byte (including a
  send deadline, now `RpcError::Timeout` / `TimedOut`) keeps the connection,
  except on `rpc-tls`, which always retires it.
- **rsbinder (RPC) — a zero handshake deadline was neither honored nor
  reported** (`StatusCode::Unknown`); `handshake_timeout` of
  `Duration::ZERO` is now `BadValue` before any connect.
- **rsbinder (RPC) — a write-side disconnect on an android-13+ session
  reported `Unknown` instead of `DeadObject`.** `From<RpcError> for io::Error`
  now maps `EndOfStream` → `BrokenPipe` and `Timeout` → `TimedOut`; the TLS
  R34 adapter is fixed too.
- **rsbinder (RPC) — a wire-profile mismatch never mentioned the profile.**
  Both sides' rejects now name the r34 profile as the likely cause, and an
  attach with too low a `max_version` points at `wire_protocol_version()`.
- **rsbinder (kernel) — a failed reply flush left a `BC_REPLY` pointing at
  freed memory** (a cross-process use-after-free). The reply path retries or
  rewinds the unconsumed command; a `dec_strong` failure no longer skips the
  reply or the calling-identity restore.
- **rsbinder (`Parcel`) — a safe forward seek could make the kernel read past
  the buffer** and send heap bytes to the peer. The kernel now gets the
  initialized length; the byte-copy generics are sealed behind `ParcelPod`.
- **rsbinder (kernel) — `become_context_manager` never asked for the
  caller's security context**, so `get_calling_sid()` was always `None` in
  `rsb_hub`. `FLAT_BINDER_FLAG_TXN_SECURITY_CTX` is now set on Android and on
  Linux with SELinux mounted.
- **rsbinder (`parcelable`) — the array pre-allocation cap bounded the count,
  not the bytes** (~1.5 GiB for a crafted `Vec<String>`); it is now
  `data_avail() / 4` elements.
- **rsbinder (RPC) — a `DEC_STRONG` could deadlock the session** (a node's
  last reference dropped under the state lock). The drop now happens after
  the lock, and a `DEC_STRONG` from inside a dispatch is deferred until after
  the reply, as in AOSP, so a returned binder is not freed early.
- **rsbinder (RPC) — a peer could plant a proxy entry in our own address
  subspace**; such an address is now refused with `BadValue`, as AOSP does.
- **rsbinder (RPC, `tokio`) — a nested callback from an RPC handler
  deadlocked in a pure-RPC process**; `BinderAsyncPool::spawn` now runs it on
  the dispatching thread.
- **rsbinder (RPC, macOS) — a non-`AF_UNIX` fd resolved to a root peer**
  (`getpeereid` succeeds on TCP with `euid = 0`); the socket family is checked
  first. On Linux/Android a peer `pid == 0` no longer yields an invalid value.
- **rsbinder (RPC server) — the r34 (default) profile never lifted the
  handshake *write* deadline**, so large replies to slow readers failed. The
  incoming-slot cap on attach is now atomic and counts serve-driven slots
  only, as AOSP does.
- **rsbinder (`proxy_count`) — a panicking `ProxyCountCallback` escaped a
  `Drop`**; callbacks are now isolated with `catch_unwind`.
- **rsbinder (`lazy_service`) — a failed re-registration could hide a service
  forever** (the entry stayed "registered").
- **rsbinder (`lazy_service`) — a refused `registerClientCallback` came back
  as `EX_TRANSACTION_FAILED`**; the service manager's own `Status` is kept.
- **rsbinder (`hub`, Android 15) — the numbering probe ran on every
  `default()`**; the answer is cached and logged once, and a build with
  neither feature names both.
- **rsbinder (RPC) — a proxy from another session was sent as a local
  address**; it is now refused with `INVALID_OPERATION`, as AOSP does.
- **rsbinder (`hub`, Android 15 r6+) — `check_service` starts lazy
  services**, since it is sent as `getService`; now documented.
- **rsbinder (`file_descriptor`) — the fd null marker accepted any non-zero
  value** (now only `1`), and a Java reliable-pipe `ParcelFileDescriptor` now
  gets the best-effort `DETACHED` notice without `BadType` or `SIGPIPE`.
- **rsbinder (RPC) — hardening and housekeeping:** bounded body allocation
  and strict address option bits in `wire_android13`, `MAX_FRAME_LEN` in the
  in-memory transport, a failed `SCM_RIGHTS` attach is an error, a reaper
  spawn failure is logged, and `unix-abstract` / `binder://name` URIs decode
  percent-escapes (`%+9` is no longer one).
- **rsbinder — comments and tests that said the wrong thing** were corrected
  (e.g. `ParcelableHolder` is `Send + Sync`), and tests that could not fail
  now can.
- **rsbinder-aidl — constant resolution:** an unqualified constant could
  resolve to an unrelated interface's constant, and a constant's expression
  was folded in the referencing scope. Constants are now keyed by namespace,
  resolved through enclosing scopes and imports, and folded where declared.
- **rsbinder-aidl — `@JavaOnlyImmutable` parcelables and unions lost all their
  members** (mistaken for `@JavaOnly…`); they are generated normally.
- **rsbinder-aidl — enum references in constant expressions:** they now
  promote to their integral value at their `@Backing` width, as in AOSP, so
  `Status.OK == 0`, float operands, `E.FOO || false`, unary `-`/`~` and
  `F.BIT << 31` fold correctly.
- **rsbinder-aidl — a non-nullable `out` argument could send a null**; the
  server now fails with `UNEXPECTED_NULL`, as AOSP does, including for
  fixed-size and nested fixed-size `out ParcelFileDescriptor` arrays.
- **rsbinder-aidl — array type fixes:** a `@nullable` fixed-size array follows
  the primitive/enum rule of vectors; `inout` arrays use `Vec<T>` in both the
  trait and the server (was E0308); fixed-size array constants are value
  types, and `@nullable String` array constants are typed correctly.
- **rsbinder-aidl — source discovery:** a symlink cycle no longer loops
  forever, include directories are deduplicated by canonical path (no false
  `AmbiguousImport`), and an empty derived include path no longer collides
  with `include_dir(".")`.
- **rsbinder-aidl — the output directory is created on demand**, and I/O
  errors name the path.
- **rsbinder-aidl — a second `Builder` could inherit the first one's
  declarations**; the parser is reset at parse time.
- **rsbinder-aidl — legitimate AIDL was rejected as "nesting too deep"**
  (`>>` counted only as a shift).
- **rsbinder-aidl — codegen that failed in rustc is now an AIDL diagnostic or
  correct output:** enum discriminants outside their `@Backing` range, two
  union fields mapping to one variant, `@RustDerive(Debug=true)` (E0119) or
  an unknown parameter, unescaped `rust_type` names and keyword package
  segments (incl. `gen`), and a negative shift amount reported by magnitude.
- **rsbinder-aidl — the `#[allow(clippy::all)]` / `#[allow(unused_imports)]`
  header applied only to the first top-level module**; both are now in every
  generated module's inner `#![allow(...)]`.
- **rsbinder (`hub`) — Android 15 QPR builds are now refused instead of
  silently losing registrations.** `android-15.0.0_r6` shifted every
  `IServiceManager` transaction code after `getService` by one without
  changing the SDK version. `hub::default` now probes which numbering the
  device speaks and dispatches to `android_14` or the new `android_15`
  feature; a build without the needed feature refuses the device and names it.
- **rsbinder (hub):** `ServiceManager::get_connection_info` did not compile
  with Android 13 and 14 enabled together.
- **rsbinder (`ProxyHandle::dump`):** a failed `write_object` leaked the
  descriptor.
- **rsbinder:** a `DeathRecipient` could not identify the binder that died:
  weak references taken inside `binder_died` compared unequal to `who` and to
  earlier weaks. Fixed by stamping the weak identity at construction (see
  *Changed*).
- **rsbinder-tools (`rsb_hub`):** a dead service was never removed from the
  registry (same cause); `rsb_hub` now matches `who` against its stored
  binders.
- **rsbinder-tools (`rsb_hub`):** death subscriptions are reference-counted per
  binder; `unregisterForNotifications` leaked one per cycle, and a binder
  under K names took K subscriptions.
- **rsbinder-tools (`rsb_hub`):** `getService2`/`checkService2` now report
  `isLazyService` from the registered `FLAG_IS_LAZY_SERVICE` (was always
  `false`).
- **rsbinder-tools (`rsb_hub`):** exception codes match AOSP: "only a server
  can register client callbacks" / "unregister itself" are
  `EX_UNSUPPORTED_OPERATION` (were `EX_SECURITY`), and `tryUnregisterService`
  on an unknown or mismatched name is `EX_ILLEGAL_STATE` (was
  `EX_ILLEGAL_ARGUMENT`).
- **rsbinder-tools (`rsb_hub`):** a failed self-registration is logged instead
  of stopping the process, and `addService` warns when `dumpPriority` sets no
  `DUMP_FLAG_PRIORITY_*` bit.
- **rsbinder (`macros` feature) — shapes now refused:** a `#[oneway]` method
  with an out/inout parameter, a `ParcelableHolder` field, nested borrowed
  types, `unsafe`/`auto` traits and variadic methods, a named-constant array
  length, `out`/`inout` on a primitive, `String` or `ParcelFileDescriptor`,
  `Option<T>` over a primitive, a `descriptor` with a backslash or quote,
  trait attributes, where clauses, `async`/`unsafe`/`const`/`extern` methods,
  unrecognised method attributes, and `BinderResult<Option<&T>>`.
- **rsbinder (`macros` feature) — shapes that now render correctly:** a
  nullable out vector, an out `ParcelFileDescriptor` array (null guard), raw
  identifiers (`r#type`) in fields, methods and arguments, fixed-size out
  arrays longer than 32, qualified-path out parameters, `self::` paths, types
  from a `macro_rules!` `$t:ty` capture or in parentheses, and a raw-identifier
  parcelable's descriptor. The generated module follows the trait's
  visibility. A by-value argument must be `Copy` and a derived parcelable
  `Default`, now asserted on the user's type. `#[derive(BinderEnum)]` no
  longer requires `Copy` and accepts `#[repr(i32, align(8))]`.
- **rsbinder (`macros` feature):** `#[derive(BinderEnum)]` encoded
  discriminant expressions as `i32` (`#[repr(i64)] A = 1 << 31` went out as
  `-2147483648`); it now casts the declared variant.
- **rsbinder-aidl:** an interface that names *itself* in a signature rendered
  as `Strong<dyn Box<IFoo>>` and did not compile.
- **rsbinder-aidl:** mutually recursive parcelables produced an infinitely
  sized type (`E0072`); cycles are now found across the declaration graph
  and boxed, except through an interface, an array or `List`, or a union's
  `Tag`.
- **rsbinder-aidl:** a cycle closed by a **non-nullable** field is now a
  diagnostic (`aidl::recursive_parcelable`), since its `Default` recursed
  forever; make the field `@nullable`, as AOSP requires.
- **fuzz:** the `rpc_wire_decode` / `rpc_address_decode` / `rpc_session_handshake`
  targets compile again through `rsbinder::rpc::__fuzz_*`; new `rpc_raw_fd`
  target.
- **rsbinder:** a remote binder handle is serialized through the full 8-byte
  object union, so 4 uninitialized stack bytes no longer reach the peer.
- **rsbinder (`rpc`):** a discarded RPC reply's reference-count bumps are
  rolled back on the handler-error and oneway paths.
- **rsbinder:** `Parcel::append_from` no longer panics when the source cursor
  sits past its data end.
- **rsbinder (`rpc`):** the Unix transport retries `EINTR` on a raw read and
  rejects fds attached to an empty frame.
- **rsbinder:** `get_extended_error` and `query_interface` return an error
  instead of panicking on pure-RPC or malformed-reply paths.
- **rsbinder (`rpc`):** `StatusCode::RpcError` no longer shares AOSP
  `FROZEN_OBJECT`'s `status_t` value.
- **rsbinder (`rpc`):** `ParcelableHolder` and `Parcel::append_from` no longer
  let bytes cross between kernel, RPC and data-only parcels unchecked: a
  holder's sub-parcel inherits its parent's mode, and copies that could turn
  payload into a binder, or drop objects, are refused with `BadType` /
  `FdsNotAllowed` (user-visible effects listed under *Migrating*).
- **rsbinder (kernel binder):** a caught panic on the reply path leaked a
  kernel transaction buffer; the recovery now rewinds only to the reply's
  mark.

## [0.10.0] - 2026-07-11

### Changed

- **rsbinder:** oneway transactions now propagate transport errors (e.g.
  `DeadObject`) to the caller instead of swallowing them as `Ok(())`,
  matching AOSP's generated Rust proxies. (#159)
- Dropped the `downcast-rs` dependency (replaced by std `Arc` downcasting)
  and trimmed unused `tera` builtins from rsbinder-aidl's dependency tree.
- **rsbinder (breaking):** `AccessorSockAddr` gained the `UnixAbstract`
  variant. The enum is intentionally not `#[non_exhaustive]` (its shape is
  part of the documented contract), so downstream exhaustive `match`es
  need a new arm. (#166)
- **rpc (AOSP alignment):** additional outgoing connections opened by
  `add_outgoing_connection_android13plus` now carry the session's fd
  transport mode in the connection header (previously hardcoded to none),
  matching AOSP `RpcSession::setupOneSocketConnection`; the session id is
  also validated as exactly 32 bytes up front (`BadValue`) instead of
  being sent malformed. (#166)
- **rsbinder-aidl (breaking, AOSP alignment):** expression precedence now
  matches AOSP's C-style grammar — bitwise `|`/`^`/`&` bind looser than
  `==`/`!=` and relational operators. An unparenthesized expression mixing
  bitwise and comparison operators (e.g. `a & M == M`) now folds to the AOSP
  value; previously rsbinder grouped it as `(a & M) == M`. Add explicit
  parentheses to keep the old grouping.
- **rsbinder-aidl (breaking):** constant names are emitted verbatim
  (`const int kMagicValue` → `pub const r#kMagicValue`), matching AOSP's Rust
  backend. Previously they were upper-cased (`KMAGICVALUE`), silently renaming
  constants and colliding distinct-case names. Code referencing the old
  upper-cased names must switch to the verbatim AIDL name.
- **rsbinder-aidl (breaking):** `Bn`/`Bp` type names follow AOSP `ClassName`:
  the leading `I` is stripped only when followed by an uppercase letter.
  `interface Foo3` now generates `BnFoo3`/`BpFoo3` (previously the garbled
  `Bnoo3`/`Bpoo3`); `IFoo` still generates `BnFoo`/`BpFoo`.
- **rsbinder-aidl:** constant-expression evaluation now rejects — as AOSP
  does — what was previously accepted with a silently wrong value:
  - enum discriminants whose initializer fails to evaluate (`A = 1/0`),
    references an unknown symbol (`A = foo.Missing.X`), forms a reference
    cycle (`A = B, B = A`), or is non-integral (`A = 1.5`, `A = 'a'` — these
    used to be lossily truncated; bool-valued comparisons remain legal, as
    in AOSP);
  - circular constant references (`const int A = B; const int B = A;`);
  - arithmetic overflow in the promoted type (`2147483647 + 1`,
    `INT32_MIN % -1`), narrowing out of the declared range
    (`const byte A = 128;`), overflowing shifts (`2 << 31`, and shift
    amounts beyond the operand width no matter how large), and shifts of
    negative values (`-8 >> 1`). AOSP carve-outs preserved: hex literals
    wrap as bit patterns (`0x80000000` is INT32_MIN), `1 << 31` /
    `1L << 63` stay legal, and a negative shift count still shifts in the
    opposite direction;
  - `char` operands in binary expressions (`'a' + 1` previously misfolded to
    the string `"a1"`), non-`+` operators on strings, unary operators on
    strings (`-"x"`), and type-mismatched defaults (`const int A = "x";`,
    an array literal on a scalar, a fixed-size array default with the wrong
    element count — all previously emitted non-compiling Rust);
  - fixed-size array dimensions that fail to evaluate, are non-positive,
    non-integral, or exceed `i32::MAX` (previously demoted silently to a
    dynamic `Vec<T>` wire format);
  - constants and members named `self` / `Self` / `super` / `crate`
    (not representable as Rust raw identifiers).
- **rsbinder-aidl:** an enum discriminant referencing a sibling interface
  constant (`const int X = 5; enum E { A = X, B }`) now folds to the
  constant's value with correct auto-increment afterwards; a stale
  pre-registration cache entry used to silently duplicate one wire
  discriminant across members (A=5, B=5).
- **rsbinder-aidl:** non-nullable `IBinder` / interface /
  `ParcelFileDescriptor` members of unions (write and read) and parcelables
  (read; write already did) now enforce AOSP `UNEXPECTED_NULL` semantics:
  `None` is never sent as a null marker and an inbound null is rejected.
  Traffic that honored the non-nullable contract is byte-identical; only
  rsbinder↔rsbinder pairs that smuggled nulls (which AOSP peers already
  rejected) now see an error.

### Added

- **hub:** Android 17 (SDK 37) service-manager support. SDK 37 shares
  Android 16's service-manager wire format, so it is served by the existing
  `android_16` / `android_16_plus` features — no new feature flag.
  Validated against an API 37 emulator. (#158)
- **hub:** event-driven service lookups matching AOSP `waitForService`:
  `wait_for_service` / `wait_for_interface` (block until registered, via
  `registerForNotifications` where available) and non-blocking
  `check_interface`. (#159)
- **hub:** error-preserving lookups `try_get_service` / `try_get_interface`
  returning `Result<Option<_>>`, so "service not registered" (`Ok(None)`)
  is distinguishable from a transport/service-manager failure (`Err`). (#159)
- **hub:** `add_service` now accepts `impl Into<SIBinder>` (pass a
  `Strong<dyn IFoo>` or a reference to one directly — no `.as_binder()`),
  backed by new `From<Strong<I>>` / `TryFrom<SIBinder>` conversions; plus
  public `register_client_callback` / `try_unregister_service` for lazy
  services. (#159)
- **rsbinder:** `include_aidl!` macro replacing the
  `include!(concat!(env!("OUT_DIR"), …))` + re-export boilerplate;
  `BinderResult<T>` alias with `From<std::io::Error>` /
  `From<TryFromSliceError>` for `Status`;
  `SIBinder::link_to_death_arc` / `unlink_to_death_arc` (no more manual
  `Arc::downgrade` incantation); `ParcelFileDescriptor::try_clone` +
  `AsFd` + `From<OwnedFd>` / `From<File>`; typed `Parcel` scalar
  read/write helpers; `RpcSession::get_interface`. (#159)
- **rsbinder-aidl:** string concatenation composes through the ordinary
  expression grammar: `CONST_A + "suffix"` and parenthesized chains
  (`("a" + "b")`) now parse; previously only a literal-first chain did.
- **rsbinder-aidl:** `const T[] X = {};` (and non-empty `const` arrays) now
  generate valid slice literals (`&[…]`); empty `{}` initializers were
  silently dropped and `const` arrays emitted non-compiling `vec![…]`.
- **rsbinder-aidl:** versioned interfaces now expose
  `getInterfaceVersion()` / `getInterfaceHash()` on the async trait and the
  async proxy (cache + transact, mirroring AOSP), not just the sync side.
- **rsbinder-aidl:** generated `Bn{{Iface}}` now offers
  `new_async_binder_with_features(inner, rt, features)`, the async counterpart
  to the sync `new_binder_with_features`. An async service can now opt into
  kernel binder features (e.g. `set_requesting_sid`); previously only sync
  services could. `new_async_binder` delegates to it with default features
  (unchanged behavior).
- **rsbinder-aidl:** a `//` comment on the last line of a file no longer
  requires a trailing newline; interfaces with thousands of members parse
  without stack overflow (grammar flattened).
- **rsbinder-aidl:** `Builder` hardening — import resolution scans all
  include directories deterministically and rejects an import found under
  more than one as ambiguous (AOSP `import_resolver.cpp` "Duplicate files
  found" parity; previously a `HashSet`-ordered directory silently won);
  `set_crate_support` is no longer a process-wide one-shot latch;
  `version()`/`hash()` on a directory source, version metadata matching no
  parsed file, and `generate()` with no `.aidl` input fail loudly (new
  `AidlError::Config`) instead of silently doing nothing; `const String[]`
  constants render as `&[&str]` (the previous `&[String]` type did not
  accept string-literal initializers); crate-level rustdoc and README
  updated.
- **rpc:** abstract Unix-domain socket support (Linux/Android):
  `RpcServer::setup_unix_server_abstract`,
  `RpcSession::setup_unix_client_abstract` (R34 wire),
  `setup_unix_client_android13plus_abstract`, and the
  `AccessorSockAddr::UnixAbstract` variant for accessor addressing.
  Abstract sockets have no filesystem entry (nothing to unlink) — and no
  filesystem permissions; see the `setup_unix_server_abstract` rustdoc
  for the `set_authorizer` guidance. (#166, thanks @qwq233)
- **rpc:** `RpcUnixClientConfig` builder plus
  `RpcSession::setup_unix_client_android13plus_with_config` /
  `add_outgoing_connection_android13plus_with_config`: a single client
  entry point combining path or abstract addressing, session-id attach,
  fan-out (`outgoing_connections`), and fd transport mode. The existing
  `with_id` / `fan_out` helpers delegate to it (wire-identical). (#166)

### Fixed

- Parcel array decoding now distinguishes a null array (length `-1`) from an
  empty array (length `0`): an empty `Vec<T>` no longer deserializes as `None`,
  and a non-null `Vec<T>` rejects a null array with `UnexpectedNull` instead of
  silently yielding an empty vector. Matches AOSP `Parcel` decoding. (#162)
- Nullable decoders reject malformed sentinels instead of treating them as
  null/present: `String16` accepts only `-1` for null, and parcelable /
  `ParcelableHolder` presence accepts only `0` (null) / `1` (non-null). Other
  values now surface `UnexpectedNull`. (#162)
- Binder reply status handling aligned with AOSP: malformed status /
  remote-stack-trace reply headers now report `Unknown` (AOSP `UNKNOWN_ERROR`)
  rather than `BadValue`, `BR_TRANSACTION_PENDING_FROZEN` is treated as a
  successful oneway completion, and driver `errno` values are normalized to
  negative `status_t` form so callers never observe positive `errno`s. (#162)
- Reading a self-referential parcelable (e.g. AIDL `RecursiveList`) is now
  bounded to a nesting depth of 1000: a hostile deeply-nested payload is
  rejected with `BadValue` instead of recursing until the worker thread's
  stack overflows (a hard abort). Defense-in-depth beyond AOSP `Parcel`; the
  limit is far above any legitimate AIDL nesting, so conforming traffic is
  unaffected.
- Casting a **sync-only** local service to its async interface view
  (`into_interface::<dyn IFooAsync<_>>`, `Strong::into_async`,
  `get_interface::<dyn IFooAsync<_>>`) now fails with `BadType` at cast time
  instead of succeeding and then panicking (`unreachable!`) on the first
  method call. Matches AOSP, which rejects the same sync/async mismatch.
- **rsbinder-aidl:** a server dispatching an out-only, non-nullable
  `ParcelFileDescriptor[]` now returns `UNEXPECTED_NULL` when the service
  leaves an element unset, instead of writing a null fd marker onto the wire
  (which corrupts the reply for a conforming peer). Mirrors AOSP's server-side
  guard; scoped, as AOSP is, to `ParcelFileDescriptor` arrays only.

### Deprecated / known gaps

- **hub:** `get_service` / `get_interface` are now `#[deprecated]`: their
  implicit wait behavior is inconsistent across Android versions. Migrate to
  `wait_for_service` / `wait_for_interface` (blocking), `check_service` /
  `check_interface` (non-blocking), or the error-preserving `try_*`
  variants. The functions still work; upgrading surfaces a compile-time
  deprecation warning. (#159)
- **rsbinder-aidl:** an AIDL `/** @deprecated */` doc comment is not yet
  propagated to a Rust `#[deprecated]` attribute (AOSP does). No wire or
  correctness impact; downstream users of a deprecated AIDL member simply do
  not get a compile-time warning. The parser discards doc comments today, so
  this awaits grammar support.

## [0.9.0] - 2026-06-15

The headline of this release is a complete **RPC transport** (binder over
sockets) — a separate, opt-in stack alongside the existing kernel binder
support — together with first-class macOS support, a substantially expanded
Android compatibility surface, AIDL compiler improvements, and a large body of
correctness work from several review and audit rounds.

### Added

#### RPC transport (binder-over-socket) — new opt-in subsystem

A pure-Rust binder-over-socket stack, wire-compatible with Android's
`libbinder` RPC and kept entirely separate from the kernel binder path. It is
off by default and zero-cost when disabled, enabled via the `rpc` master
feature with per-backend opt-ins (`rpc-tcp-debug`, `rpc-vsock`, `rpc-tls`).

- `RpcServer` / `RpcSession`: multi-session serving, threading, version
  negotiation, and oneway / nested-call / timeout handling, plus
  `RpcServer::set_max_connections`.
- Transports: Unix socket, in-memory, debug TCP, vsock, and TLS (rustls) —
  including server-side TLS over unix/TCP/vsock and client mTLS.
- Wire compatibility: android-13+ versioned wire and android-16 RPC v2,
  validated against real `libbinder`.
- FD-over-RPC (`FileDescriptorTransportMode`), AOSP-faithful.
- Multi-connection session pool with per-node async ordering and AOSP
  `timesSent` / excess-`DEC_STRONG` accounting.
- RPC Accessor (`IAccessor`): both the client (consume) and registration
  sides, plus VINTF accessor integration in `rsb_hub`.
- Cross-transport calling identity and authorization: an injectable
  `PermissionAuthority`, a transport-tagged `Caller`, and a cross-transport
  service facade.
- RPC death notification and an async-over-RPC adapter.

#### Platform support

- First-class **macOS** support for the RPC stack (peer-credential authorizer,
  target-gated platform code). Kernel-binder-only APIs return
  `InvalidOperation` on macOS.

#### Android compatibility

- Calling identity: `get_calling_sid` / `get_calling_uid` / `get_calling_pid`,
  `clear_calling_identity` / `restore_calling_identity`, strict-mode policy,
  and `android.os.IPermissionController` (`check_permission`).
- `@EnforcePermission` code generation (`Single` / `AllOf` / `AnyOf`).
- Extended error reporting (`binder_extended_error` via `get_extended_error`),
  `FLAG_UPDATE_TXN`, and AppOps reply-header handling.
- Real-time priority inheritance (`FLAT_BINDER_FLAG_INHERIT_RT`, opt-in via
  `BinderFeatures`).
- Proxy-count infrastructure (global and per-uid counters with watermark
  callbacks).
- `Binder::attach_object` / `find_object` / `detach_object`,
  `LazyServiceRegistrar`, and an `IMemory` skeleton.
- Freeze-notification kernel UAPI baseline.

#### AIDL compiler (rsbinder-aidl)

- Generated `getInterfaceVersion()` / `getInterfaceHash()` meta methods
  (AIDL `--version` / `--hash`).
- `@Backing` type validation with AOSP-faithful diagnostics.
- Richer miette diagnostics (e.g. dedicated direction/primitive errors) and an
  AOSP fixture sweep.

#### Service manager (rsb_hub)

- Lazy-service poller and client-callback support.
- Accessor registration and VINTF integration.

### Changed

- Replaced the unmaintained `rustls-pemfile` with `rustls-pki-types`
  `PemObject`.
- Reduced redundant clones across the binder hot paths.
- Bumped `rsproperties` to 0.5, plus `log`, `similar`, and the dependabot
  `rust-major` / `rust-minor` dependency groups.
- Extensive documentation work: mdBook overhaul, API-doc sync, and
  cross-transport / RPC-TLS guides.

### Fixed

A large body of correctness work from multiple review and audit rounds
(PRs #147–#155). Highlights:

- `Parcel::append_from` no longer double-closes a file descriptor or
  underflows a refcount on an error path, and parcel write paths never read or
  transmit uninitialized bytes.
- `wait_for_response` no longer hangs the caller on a malformed `BR_REPLY`
  (`TF_STATUS_CODE` with an OK status).
- RPC: roll back `timesSent` and the oneway `async_number` reservation when a
  send fails; reject truncated `SCM_RIGHTS` fd batches (`MSG_CTRUNC`) instead
  of silently dropping fds.
- AIDL: reject operator-chain input that would overflow the parser stack,
  unresolvable constant references, and out-of-range shifts; `r#`-escape
  type-level names that are Rust keywords.
- `rsb_hub`: bound per-name registries and de-duplicate callback death links.
- `proxy_count`: decide per-object whether to track a proxy so toggling
  tracking no longer desyncs the count.
- Migrated `getrandom::getrandom` to `getrandom::fill` for getrandom 0.4.
- Various test fixes (WIBinder upgrade after obituary, process-wide thread-pool
  counter, RPC lifecycle flake).

### Security

- Addressed RUSTSEC-2025-0134 by replacing `rustls-pemfile` with
  `rustls-pki-types`.

[Unreleased]: https://github.com/hiking90/rsbinder/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/hiking90/rsbinder/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/hiking90/rsbinder/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/hiking90/rsbinder/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/hiking90/rsbinder/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/hiking90/rsbinder/compare/v0.8.0...v0.9.0
