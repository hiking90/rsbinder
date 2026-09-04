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

### Migrating from 0.10.0

The breaking changes are listed in *Changed* and *Removed* below. This is the
short form — and the first entry is the only one no compiler will catch.

- **`ProcessState::init(path, 0)` no longer means "the default".** The count
  now reaches the kernel as written, so `0` asks for zero binder threads. The
  signature did not change, so nothing warns: `cargo-semver-checks` cannot see
  it and neither can your build. If you meant the default, pass the newly
  public `DEFAULT_MAX_BINDER_THREADS`. `init_default()` and a `binder://` URI
  without `?threads=` are unchanged.
- **`rpc::transport::TlsStream` gained a required `shutdown()`.** Custom
  stream implementations must add `fn shutdown(&self) -> std::io::Result<()>`
  (shut the underlying stream down in both directions). There is no default
  body on purpose: silently doing nothing would leave a client's
  incoming-connection threads blocked in `recv` with nothing to wake them, so
  `RpcSession::shutdown` would hang on the join. The bundled
  `TcpStream`/`UnixStream`/`VsockStream` impls are unaffected.
- **rsbinder-aidl rejects `.aidl` it used to accept.** Five inputs that
  previously generated silently-wrong or non-compiling Rust are now build
  errors: a `@JavaOnlyStableParcelable` / `cpp_header` / `ndk_header`
  declaration with no `rust_type`, an `in out` (or `out in`) argument,
  `self`/`Self`/`super`/`crate` used as a method, argument, enum-member,
  declaration-name or package segment, an enum discriminant outside its
  `@Backing` range, and a `@RustDerive` parameter outside AOSP's schema
  (`Copy`, `Clone`, `PartialOrd`, `Ord`, `PartialEq`, `Eq`, `Hash`; `Debug`
  and `Default` are accepted and dropped, since the templates always emit
  them). All but the `in out` case (a plain syntax error) name the
  declaration.
- **rsbinder-aidl resolves names in AOSP's order** — enclosing scopes, then
  imports, then the package. An `import a.IFoo` now outranks a top-level
  `IFoo` declared in the referencing package (it used to lose to it), and a
  qualified constant names a *direct* member of its owner: `Outer.X` no
  longer reaches `Outer.Inner.X`. A build that leaned on either shadowing
  fails with an unresolved-name diagnostic rather than changing value.
- **`out` arguments of `IBinder` / `ParcelFileDescriptor` / an interface are
  now `&mut Option<T>`** in generated traits (matching AOSP). The wire is
  unchanged; implementations need the parameter type updated. `inout` is
  unaffected.
- **`@nullable` `out`/`inout` arrays of a primitive or enum are now
  `&mut Option<Vec<T>>`**, not `&mut Option<Vec<Option<T>>>` — the element
  `Option` encoded a null-marker word per element that libbinder does not
  write (AOSP `UsesOptionInNullableVector`). Non-primitive elements keep the
  `Option`, and fixed-size arrays follow the same rule. Implementations need
  the parameter type updated.
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
  helpers. `Parcel::from_ipc_parts` and `Parcel::set_for_rpc` remain the
  supported raw-buffer entry points.
- **The RPC wire codec is private** (`WireMessage`, `WireCodec`, `R34Codec`,
  `Android13PlusCodec`, `WireReply`, `WireTransaction`, `RpcState`). The
  supported RPC surface is `RpcServer` / `RpcSession` / `RpcProxy`, the
  transport traits, and the address and identity types.
- **`LazyServiceRegistrar::register_service` now registers, and the process
  exits when its last client goes away.** It used to touch nothing outside
  the struct. Code that called it *and* did its own `addService` /
  `registerClientCallback` must drop those calls; code that relied on it
  being inert needs `set_active_services_callback` or `force_persist`. Its
  signature moved too: it takes `impl Into<SIBinder>` (drop the
  `.as_binder()`) and returns `Result<(), Status>`, which keeps the service
  manager's refusal code and message. `?` into a `StatusCode` still compiles
  — `From<Status> for StatusCode` covers it — but a binding or `map_err` that
  named `StatusCode` has to be retyped.
- **`WIBinder` has no `Native` fallback for proxies.** A proxy's weak identity
  is stamped at construction; only an exhaustive `match` on the enum notices.
- **`IMemoryHeap::base` returns `Option<SharedBytes<'_>>`**, not `&[u8]`. The
  window aliases memory another process writes at any moment, which a `&[u8]`
  cannot soundly describe. The view is read-only (`load` / `copy_to` / `slice`
  / `to_vec`); writes go through `write_at` on the concrete heap types, which
  also refuses a `FLAG_READ_ONLY` mapping.

### Added

- **rsbinder (RPC):** client-side **incoming (callback) connections** —
  `RpcUnixClientConfig::incoming_connections(n)`,
  `RpcSession::add_incoming_connection_android13plus_with_config`, and
  `ClientOptions::incoming_connections` (AOSP `setMaxIncomingThreads`).
  Until now a server could reach a client's callback only from inside a
  handler answering that client; a call from any other server thread
  blocked forever. With `n ≥ 1` the client attaches `n` connections the
  server sends on, each served by a thread the session owns, so callbacks
  work from timers, workers, and oneway notifications. Such a session also
  observes the server's death as soon as the connection drops — eagerly, not
  precisely: losing the last incoming connection *is* the session's death,
  so a server that retires only that connection (its `set_reply_timeout`
  elapsing on a slow callback, say) tears the whole session down, founding
  connection included. The threads
  keep the session alive until `RpcSession::shutdown()` (which now shuts
  every connection down and joins them) or the server closes the session.
  Android-13+ profile, Unix sockets.
- **rsbinder (RPC):** `ClientOptions::handshake_timeout` and
  `RpcUnixClientConfig::handshake_timeout` — a deadline for the connection
  **handshake**, the phase `timeout` cannot reach because it is applied to a
  session that does not exist yet. Covers the founding connect and every
  fan-out / incoming attach, plus the `tls://` `connect(2)`. `None`
  (default) keeps the previous unbounded behavior; without it a peer that
  accepts the socket and then writes nothing hangs `Client::open` forever.
  The client-side counterpart of `ServeOptions::handshake_timeout`.
- **rsbinder (RPC):** `RpcServer::set_reply_timeout` — bounds how long this
  server waits for a reply to a callback it issued, by setting
  `RpcSession::set_timeout` on every session the server builds. `None`
  (default) keeps the previous block-forever behavior. Callback connections
  are exempt from the `set_idle_timeout` *read* deadline (a sticky
  `SO_RCVTIMEO` would cut this very wait short), so this is the only bound
  available on that path.
- **rsbinder (`binder`):** `Strong::try_into_async` — the fallible form of
  `into_async`. A local service published sync-only (`Bn*::new_binder`)
  cannot back the async view; the generated cast reports `BadType`, which
  `into_async` turns into a panic (now documented on it).
- **rsbinder (RPC, `vsock`):** the vsock transport implements the raw
  (unframed) byte access the android-13+ wire profile needs. Until now
  `vsock://…?profile=android13plus` parsed and then failed at the first
  handshake byte — on the Microdroid/AVF target the backend exists for, where
  the peer is real libbinder and speaks only that profile.
- **rsbinder (kernel):** a thread that talked to the binder driver now sends
  `BINDER_THREAD_EXIT` (after a final flush) when it ends — AOSP
  `IPCThreadState::threadDestructor`. Without it every short-lived thread that
  made one call left a `binder_thread` behind in the kernel until the fd
  closed, and a recycled tid inherited the dead thread's looper state.
- **rsbinder (`hub`):** an `android_15` feature — the Android 15
  service-manager protocol from `android-15.0.0_r6` on. That release inserted
  `getService2` at index 1 of `IServiceManager` and shifted every transaction
  code after it by one, without changing the SDK version, so Android 15 has
  two protocols and no way to tell them apart by version. `android_14` still
  serves the initial release; enable both (`android_14_plus` and everything
  wider do) and `hub::default` probes the running service manager once to
  pick between them. Only one enabled still works — the other numbering is
  refused, with the missing feature named.

  The module reaches services through `getService` (code 0) only, including
  `check_service`. `getService2`/`checkService` return a `Service` union
  whose payload *is* release-dependent (`r6` sends
  `{binder, accessor}`, `r20`–`r36` send `{serviceWithMetadata, accessor}`),
  and nothing on the wire says which; `getService` returns a bare
  `@nullable IBinder` throughout. Not parsing that union is what lets one
  module cover `r6` through `r36`. The union's `accessor` arm — VINTF
  `<accessor>` resolution over RPC — is consequently not supported on
  Android 15; that bridge remains Android 16 only.
- **rsbinder (`lazy_service`):** `LazyServiceRegistrar::instance` — the
  process-wide registrar, AOSP `getInstance`. A registrar has to outlive the
  services it registers: the `IClientCallback` binder survives it (the
  service manager holds a reference), but its link back to the bookkeeping is
  weak, so dropping the last handle leaves the service manager calling a
  callback that does nothing — services registered, process never shutting
  down, nothing logged. `new` is still there for a second, independent
  registrar (AOSP `createExtraTestInstance`) and now says so.
- **rsbinder (`lazy_service`):** `LazyServiceRegistrar::set_active_services_callback`
  — AOSP `setActiveServicesCallback`. Reports whether any service in the
  process has clients, and by returning `true` takes the shutdown decision
  over from the registrar. Fires only when the answer changes.
- **example-hello (`hello_callback_demo`):** a real lazy service now — it
  registers through `LazyServiceRegistrar` and exits by itself once the last
  client goes away, rather than printing `onClients` transitions for a human
  to read. `tests/scripts/run_lazy_service_ac.sh` runs it against `rsb_hub`
  and gates on the exit status, so the half of the lazy path that only exists
  on a wire — the flag reaching the registry, the client callback landing,
  the hub's 5-second poller driving the shutdown, `tryUnregisterService`
  actually removing the name — is covered automatically for the first time.
  `example-hello/cpp/run_lazy_service_stage3.sh` runs the same thing against
  Android's own `servicemanager` instead of `rsb_hub`, which removes the
  circularity of testing our port against our port.
- **rsbinder-tools (`rsb_hub`):** an honoured `tryUnregisterService` now logs
  `Unregistering <name>`, as AOSP `ServiceManager.cpp` does. Without it a
  lazy shutdown and a crash are indistinguishable in the log — the name
  disappears either way, since the death notification would clear it too.
- **rsbinder-tools (`rsb_service`):** a new CLI for asking the running service
  manager what it knows — the Linux counterpart of Android's `service` and
  `dumpsys -l`. `list`, `info`, `check`, `declared`, `instances`,
  `connection`, and `dump <name> [args...]` (which sends `DUMP_TRANSACTION`
  to any service, not just the hub). Exit status is the answer: `0` yes, `1`
  no, `2` the question could not be answered. It is an ordinary binder client,
  so `rsb_hub`'s policy applies to it like anything else, and a denial is
  reported as a denial rather than as an empty result.
- **rsbinder-tools (`rsb_hub`):** `dump` support. `rsb_service dump manager`
  (or a `DUMP_TRANSACTION` to handle 0) prints the registry as the hub sees
  it: every registration with its pid, uid, dump-priority flags and callback
  counts, the death-subscription count, the access-control mode, and the
  names something is *waiting* on that nothing has registered. Gated on
  `list`, with each name filtered by `find`; withheld names are counted, not
  listed. AOSP's `servicemanager` does not implement `dump` at all — on
  Android these questions are answered by `dumpsys`, `service list` and the
  init/VINTF files, none of which exist on Linux.
- **rsbinder-tools (`rsb_hub`):** service-supervisor integration. `SIGTERM`
  and `SIGINT` now stop the hub cleanly (exit 0 with a log line, instead of
  dying by signal), and under systemd `Type=notify` it reports `READY=1` only
  once it holds handle 0 — so units ordered `After=` it cannot race the
  registry — plus `STOPPING=1` on a deliberate stop and a `STATUS=` line for
  `systemctl status`. No `libsystemd` dependency: `$NOTIFY_SOCKET` takes one
  datagram. Starting a second hub on a device that already has one now names
  the cause instead of surfacing a raw ioctl errno.
- **rsbinder (hub):** error-preserving `try_*` counterparts for the client
  calls that used to swallow the service manager's `Status` —
  `try_list_services`, `try_is_declared`, `try_get_declared_instances`,
  `try_get_connection_info`, `try_get_service_debug_info` (each on
  `ServiceManager` and as a free function). The swallowing wrappers report a
  policy denial as an empty list / `false` / `None`, indistinguishable from
  the negative answer; anything that must explain *why* needs the status.

- **rsbinder-tools (`rsb_hub`):** on-demand service start. A `[[service]]`
  entry with `start = { systemd = "unit" }` or `start = { exec = [...] }` is
  brought up when a `getService` misses it — AOSP's `tryStartService`, with
  the declaration standing in for the `ctl.interface_start` property. Until
  now the lazy-service machinery was all present (the client-callback poller,
  `onClients`, `tryUnregisterService`) except the trigger, so
  `wait_for_service` on a service that had not started yet waited forever.
  `checkService` still never starts anything. At most one attempt per name is
  outstanding at a time, so polling cannot spawn a copy per attempt.
- **rsbinder-tools (`rsb_hub`):** the configuration is now checked before it
  is read, and `rsb_hub` refuses to start if it or any file in it is writable
  by anyone but its owner, or owned by someone other than root or `rsb_hub`.
  A `start` entry runs with `rsb_hub`'s privileges and is triggered by any
  client allowed to look the name up, so a file anyone can edit is a file
  anyone can use to run code. Same discipline sudo, ssh and cron apply.
- **rsbinder (hub):** `hub::get_declared_instances` and
  `hub::get_connection_info` — the client half of the two calls above.
  `isDeclared` had a wrapper; these two were reachable only through the raw
  AIDL proxy. Available from the Android 12 and 13 protocols respectively,
  and on Linux.
- **rsbinder-tools (`rsb_hub`):** service declarations. A `[[service]]` entry
  in the configuration names a concrete instance
  (`pack.age.IFoo/instance`, split as AOSP's `NameUtil.h` splits it) and
  answers `isDeclared`, `getDeclaredInstances` and `getConnectionInfo` — the
  three calls that were stubs because they read VINTF manifests on Android
  and there is no VINTF on a plain Linux host. A declaration is the
  equivalent statement: these instances are expected to exist, so a client
  can tell "not installed" from "not started yet". Declared instances are
  filtered by `find`, as AOSP filters `getUpdatableNames`. A host that
  declares nothing behaves exactly as before.
- **rsbinder:** `WIBinder` now implements `PartialEq<SIBinder>` (and the
  reverse), so a `DeathRecipient` can ask the question it actually has —
  "is this the binder that died?" — as `*who == stored`. Writing it by hand
  meant downgrading the stored strong reference and comparing two weaks, which
  was the shape of the bug fixed below.
- **rsbinder (`macros` feature):** `#[rsbinder::interface]`,
  `#[derive(Parcelable)]` and `#[derive(BinderEnum)]` — declare a binder
  interface as a Rust trait and its data types as ordinary structs and enums,
  with no `.aidl` file and no `build.rs`. `&mut T` is an out parameter,
  `#[inout]` makes it round-trip, `Option<T>` is nullable, `#[oneway]` drops
  the reply, and transaction codes follow declaration order. The macros fill
  the same render structs `rsbinder-aidl` fills and run the same templates, so
  a declaration here and the equivalent `.aidl` produce **identical** generated
  code — golden-tested in `rsbinder-macros`, interface and parcelable alike.
  `#[derive(Parcelable)]` emits only the codec, leaving `Default`/`Debug`/… to
  the usual derives. `#[derive(BinderEnum)]` needs `#[repr(i8|i32|i64)]` (the
  wire format) and explicit variant values, and is **closed**: an undeclared
  value is `BadValue`, where an `.aidl` enum carries it through — use `.aidl`
  or `declare_binder_enum!` across version skew. `.aidl` stays canonical for
  unions, constants, `@VintfStability`, `@EnforcePermission`,
  `ParcelableHolder`, generics and nested types. Off by default; the macros
  pull the AIDL compiler in as a proc-macro dependency.
- **rsbinder-aidl:** new public `render` module — `FnMembers`,
  `InterfaceRender` / `ParcelableRender` / `EnumRender`, and
  `render_interface` / `render_parcelable` / `render_enum`. This is the seam
  `rsbinder-macros` plugs into: the templates and their inputs are now a
  documented surface rather than private detail, so a second front-end cannot
  drift from the AIDL one. Generated output is byte-for-byte unchanged. The
  input structs are `#[non_exhaustive]` with `new()` constructors, so a
  template gaining a field stays a minor release, and `ParcelableMember` is a
  named struct rather than a five-element tuple whose two adjacent `bool`s
  could be swapped without a compile error.
- **rsbinder-aidl:** `Builder::dest_dir` — set the output directory directly
  instead of through the process-wide `OUT_DIR` environment variable, which a
  caller outside a `build.rs` cannot set without a data race.

- **rsbinder (entry API):** `rsbinder::serve(uri)` / `rsbinder::connect::<dyn I>(uri)`
  / `rsbinder::Client` — one bootstrap for every transport, selected by a
  URI (`binder://`, `unix://`, `unix-abstract://`, `vsock://`, `tls://`,
  `#service`, `?profile=android13plus`). `serve("binder://")?.add(name,
  BnFoo::new_binder(impl))?.run()?` is the whole kernel service setup
  (`ProcessState` init + thread pool + `hub::add_service` + join); the same
  three lines with `unix:///path` are an RPC server. Options that do not fit
  a URI go through `ServeOptions` / `ClientOptions`; an option for the wrong
  transport is `BadValue` at `run`/`open` time, logged. `connect_async` under
  the `tokio` feature. `CallRestriction` is now re-exported at the crate root.
  `ServerGuard::server()` and `Client::session()` are the escape hatches to
  the underlying `RpcServer` / `RpcSession` (bound address after a `:0`
  port, session counters, RPC-only powers the entry layer does not wrap).
  `Client::open_with`'s closure also receives the parsed `Endpoint`, and
  `Endpoint::supports_fd_passing()` names the one predicate behind fd-mode
  validation, so transport-conditional options never need URI string matching.
  `CallRestriction` is documented, `#[non_exhaustive]`, and `PartialEq`.
- **rsbinder (rpc):** `RpcSession::shutdown()` — declare a session dead now
  (obituaries + release of the peer's local objects, AOSP `RpcState::clear`).
  This is the only way to break the `session → local object → stored proxy →
  session` cycle for a session that has no serve loop and will never transact
  again; unlike AOSP `shutdownAndWait` it does not join workers.
- **rsbinder (shared memory):** `shared_memory` is now a working
  implementation instead of a trait skeleton. `MemoryHeapBase` allocates an
  anonymous shared region (`memfd_create` + `F_ADD_SEALS` on Linux/Android,
  matching AOSP `MemoryHeapBase`'s `FORCE_MEMFD` path; `shm_open` on macOS)
  and `MappedHeap` maps a received fd with the wire geometry
  (`from_fd_strict` additionally verifies the sender's seals). New
  wire-faithful stubs `BnMemoryHeap`/`BpMemoryHeap` and `BnMemory`/`BpMemory`
  (`android.utils.IMemoryHeap` / `android.utils.IMemory`, handwritten AOSP
  `IMemory.cpp` layout, no AIDL) let a heap travel over the kernel binder or
  a Unix-socket RPC session with `FileDescriptorTransportMode::Unix`.
  `SharedMemory` mirrors `android.os.SharedMemory` / NDK `ASharedMemory`:
  one fd on the wire, size recovered from the fd (`fstat`, or
  `ASHMEM_GET_SIZE` for legacy ashmem on Android), `Serialize`/`Deserialize`
  byte-identical to `ParcelFileDescriptor`. Verified against the real
  libbinder `IMemory`/`MemoryHeapBase` on an Android 16 emulator in both
  directions (`example-hello/cpp/run_imemory_interop.sh`). `rustix` gains the
  `fs` feature (and `shm` on macOS). Plan: `plans/4-7a-shared-memory-impl.md`.
  On macOS the same protections memfd seals give are kernel-backed too: a
  POSIX shm object's size is fixed after creation, and a read-only heap
  exports an `O_RDONLY` fd the kernel refuses to map writable.
  `MemoryHeapBase::seals()` reports these as the same `SEAL_*` bits and
  `MappedHeap::from_fd_strict` works there; new
  `MemoryHeapBase::seal_future_write` / `MappedHeap::seal_future_write`.
  Plan: `plans/4-7b-macos-shared-memory.md`.
- **rsbinder (shared memory):** `MemoryDealer` — AOSP's chunk allocator over
  one shared heap (`SimpleBestFitAllocator`: 32-byte granules, best-fit,
  coalescing, page-aligned blocks). Each `Allocation` is an
  `android.utils.IMemory` window returned to the dealer on drop. On the
  receiving side `HeapCache` shares one `BpMemoryHeap` per heap binder, so a
  peer maps a dealer's heap once (`BpMemory::new_with_cache`).
- **example-hello / book:** `shm_service` / `shm_client` (kernel binder or
  Unix-socket RPC with `--features rpc`) and a *Shared Memory* chapter
  covering both the `SharedMemory` region and the `MemoryDealer` frame
  patterns.
- **rsbinder (internal):** `ParcelFileDescriptor` serialization is now layered
  on crate-private `write_raw_fd` / `read_raw_fd` (AOSP
  `Parcel::writeFileDescriptor` / `readFileDescriptor` — the bare fd object
  without the AIDL not-null / comm markers), which the handwritten
  `IMemoryHeap` wire uses directly. Bytes on every transport are unchanged.
- **rsbinder-tools (`rsb_hub`):** per-name access control. `rsb_hub` now gates
  every AIDL entry point on an `add` / `find` / `list` policy keyed on the
  caller's uid and its NSS-resolved groups — the same policy model AOSP's
  `servicemanager` applies through SELinux, but keyed on credentials every
  platform reports rather than on an LSM that most Linux distributions do not
  enable and macOS does not have. Policy is TOML, `--config <PATH>` takes a
  file or a directory of `*.toml` loaded in file-name order, and the first rule
  whose `name` pattern matches decides. `SIGHUP` reloads in place; a reload
  that fails to parse or resolve keeps the policy already in force. A denied
  *lookup* reports "not registered" rather than an error, matching AOSP's
  `tryGetBinder`; every other denial is `EX_SECURITY`. `listServices` and
  `getServiceDebugInfo` apply the global `list` gate *and* filter their results
  per-name by `find`, which is stricter than AOSP's all-or-nothing `canList`.
  Deliberately **not** keyed on pid: pid reuse makes pid-derived attributes
  unsound for authorization, which is why AOSP names its own field `debugPid`.
  See `plans/6-1-hub-access-control.md` and the
  [Service Manager chapter](https://hiking90.github.io/rsbinder/service-manager.html#access-control).
- **rsbinder-tools (`rsb_device`):** `--group` and `--mode` for the binder
  device node. Binder has no in-kernel access control of its own, so the node
  is the only gate on who may speak binder at all.

### Changed

- **rsbinder (RPC):** a non-nested call on a session with no connection to
  send on — a server calling a client callback outside any handler, the
  client having opened no incoming connection — now fails immediately with
  `FailedTransaction` (AOSP `WOULD_BLOCK`) and a log line naming the
  remedy, instead of waiting forever (or until `set_timeout`). Connection
  slots are now tagged with the direction they are used in (AOSP
  `mOutgoing`/`mIncoming`), so a serve-driven connection is never claimed
  by another thread's transaction between two of its messages. A
  same-thread nested call re-enters a serve-driven connection only while a
  *twoway* handler is running on it (AOSP `RpcConnection::allowNested`):
  from inside a **oneway** handler the peer is no longer reading that
  socket, so the nested call now goes out on a connection the peer does
  read — or fails with `FailedTransaction` if there is none — instead of
  blocking forever on a reply that could never arrive.
  **Breaking for `RpcSession::new(t, AddressSpace::Acceptor)`:** the
  founding connection of an acceptor session is serve-driven, so such a
  session can no longer open a transaction of its own — `get_root`,
  `RpcProxy::transact` and `ping_binder` from outside a dispatch return
  `FailedTransaction` where they used to round-trip on that connection.
  Callbacks from inside a twoway handler are unaffected. To call out of
  an acceptor otherwise the peer must open incoming connections, which
  needs `?profile=android13plus`; the r34 profile has no such mechanism.
- **rsbinder (RPC):** the server's callback-slot budget (`2 × set_max_threads`
  per session) now counts callback connections only; a client's outgoing
  fan-out no longer eats into it (before, a default server admitted a single
  callback connection). The server also no longer arms the `set_idle_timeout`
  *read* deadline on callback slots (it would have cut short a server's own
  reply wait there) — the write deadline stays, as the only bound on a
  callback send to a peer that has stopped reading — and confirms a callback
  attach only after admitting it, so a refused attach is an error on the
  client instead of a silently dead connection. With that read deadline gone,
  `RpcServer::set_reply_timeout` is what bounds a callback's *reply* wait:
  without it a client that accepts a callback and never answers pins the
  sending worker — and everything queued behind it on that session — forever.
- **rsbinder (RPC):** a `DEC_STRONG` no longer takes a **serve-driven**
  connection slot from the scan; only a slot the calling thread already
  drives (the reentrant pin) may carry one. This matches AOSP
  `ExclusiveConnection::find`, which looks `mIncoming` up with
  `available = nullptr`. The peer reads such a connection only inside its
  own reply wait, so writing there is not a delayed frame but a send that
  blocks once the socket buffer fills — while the sender holds the slot's
  `exclusive_tid`, locking that slot's serve loop out of its own
  connection. Consequence: on a session with no outgoing connection the
  reference is released at session end instead of promptly, which is the
  documented best-effort contract for refcounts. A client that wants
  prompt release opens an incoming connection
  (`RpcUnixClientConfig::incoming_connections`).
- **rsbinder (RPC):** on session death every connection slot's transport is
  shut down and the pool is emptied, releasing callback-slot descriptors at
  once rather than when the session object is finally dropped. The shutdown
  now runs *first*, before the obituaries and the local-object drop, so a
  `binder_died` handler (or a `Drop`) that calls `RpcSession::shutdown` can
  join the incoming threads instead of deadlocking on them.
- **rsbinder (RPC):** attaching a connection to a session that died during the
  attach handshake now fails with `DeadObject` instead of pushing a slot no
  serve loop will ever retire. The gate moved inside the pool lock, where it
  is serialized against the death sequence emptying that pool.
- **rsbinder (`shared_memory`) — breaking:** `IMemoryHeap::base` now returns
  `Option<SharedBytes<'_>>`, a read-only view. A `&[u8]` over a `MAP_SHARED`
  region promises the compiler an immutability the peer process does not
  keep, so it was undefined behaviour to hold; the view copies through
  atomics instead, and offers no store — a `FLAG_READ_ONLY` receiver maps
  `PROT_READ`, so a writable view would let safe code fault. `read_at` /
  `write_at` copy through the same atomics (word-sized wherever the window
  is word-aligned), which also makes `MemoryHeapBase`'s `Sync` sound — two
  threads writing the same window through one `Arc` used to be a data race
  reachable from safe code. `MemoryDealer::allocate` now hands out a window
  of the *requested* size rather than the granule-rounded one (AOSP
  `MemoryDealer::allocate` does the same); the rounding exposed up to 31
  bytes of whatever a freed neighbour had left in the granule.
- **rsbinder (`shared_memory`):** `MappedHeap::from_fd` refuses a zero-length
  fd unless it really is an ashmem device (previously any `st_size == 0` was
  "trusted as ashmem"). A memfd the sender never `ftruncate`d mapped fine
  and `SIGBUS`ed this process on first access — a remote crash. The Android
  `ASHMEM_GET_SIZE` ioctl in `SharedMemory` is gated the same way, as
  libcutils does, instead of being issued on whatever fd arrived.
  `MemoryHeapBase::seal_future_write` reports `InvalidOperation` on
  Linux/Android for a heap created without `FLAG_MEMFD_ALLOW_SEALING`
  (`F_SEAL_SEAL` makes it impossible; the docs now say so — only the macOS
  caveat was documented).
- **rsbinder (`proxy`):** `ProxyHandle` equality is `(handle, generation)`,
  as its own `generation` field documents, and it is now `Eq`. Comparing by
  handle alone called two proxies equal when the kernel had recycled the
  number for a different node.
- **rsbinder (`Parcel`):** `set_data_position` ignores (and logs) a position
  above `i32::MAX`, as AOSP's `LOG_ALWAYS_FATAL` guard does; and a forward
  seek with nothing written behind it no longer inflates the byte count
  handed to the kernel — see *Fixed*.
- **rsbinder (entry):** `ClientOptions::tls` / `tls_server_name` on anything
  but a `tls://` endpoint, and `ServeOptions::tls` likewise, are `BadValue`.
  Both were silently ignored — a caller asking for TLS got a plaintext socket
  and no error. `ServeOptions::tls` had also accepted `unix://` and
  `vsock://`, which no client this facade builds can reach; TLS over those
  sockets stays available one layer down (`RpcServer::setup_unix_server_tls`
  / `setup_vsock_server_tls`). The option types already promised "never
  ignored". A `ClientOptions::session_id` on the multi-connection path was
  dropped the same way and opened a fresh session; it is forwarded now (the
  session layer refuses the combination).
- **rsbinder (RPC):** a `GET_FD_MODE` on a session already in `Unix` fd mode
  answers `1` and changes nothing. It used to re-run the negotiation — which
  on the client role can only ever compute `None` — so any peer packet
  downgraded an established session, dropping in-flight fds and, on the r34
  profile, desynchronising the fd-aware receive buffer. An unsolicited
  `REPLY` now ends the session (AOSP `processCommand` does the same) instead
  of being logged and skipped at wire speed. `find_conn` waits are bounded by
  `RpcSession::set_timeout` (`TimedOut`) instead of parking forever when
  every slot is driven by another thread. `RpcSession::shutdown` logs when it
  finds nothing to tear down.
- **rsbinder (`lazy_service`):** `force_persist` may **not** be called from
  the active-services callback — the docs listed it as allowed, but it
  re-takes the lock the callback runs under (AOSP's `forcePersist` does too,
  and AOSP's header never allowed it either). `try_unregister` / `re_register`
  remain the callback's tools.
- **rsbinder-aidl — breaking:** declarations with no Rust representation are
  now build errors instead of silently generating an empty type. A
  `@JavaOnlyStableParcelable` declaration, or an unstructured parcelable
  declared only with `cpp_header` / `ndk_header`, previously produced a
  field-less `pub struct Foo {}` together with a live `Parcelable` impl that
  wrote an empty payload — wire-incompatible with any peer that has the real
  type, and announced only through an `eprintln!` that cargo hides without
  `-vv`. Such a declaration is now an `AidlError::Semantic` naming the
  declaration. A `rust_type "..."` alias is still generated as before, and
  takes precedence over the other markers.
- **rsbinder-aidl — breaking:** an `out` argument whose type has no `Default`
  (`IBinder`, `ParcelFileDescriptor`, an interface `Strong<_>`) is now
  declared as `&mut Option<T>`, matching AOSP
  `aidl_to_rust.cpp::RustNameOf` for `OUT_ARGUMENT`. The previous `&mut T`
  could not compile: the server's local is initialised with
  `Default::default()`. `inout` arguments read their value from the parcel
  and stay unwrapped. Implementations of affected traits need the parameter
  type updated; the wire format is unchanged.
- **rsbinder-aidl — breaking:** `in out` / `out in` argument declarations are
  rejected. The grammar accepted a run of direction keywords and kept only
  the last, so a mistyped `inout` silently generated one-way semantics. AOSP
  `aidl_language_y.yy` allows a single direction; rsbinder now matches.
- **rsbinder-aidl — breaking:** `AidlError::Template` gained a `source` field
  carrying the underlying `tera::Error`, whose cause chain holds the detail its
  `Display` omits. `AidlError` is not `#[non_exhaustive]`, so a `match` arm
  written as `AidlError::Template { message }` needs `..` added.
- **rsbinder-aidl — breaking:** `self`, `Self`, `super` and `crate` are
  rejected as method names, argument names, enum member names, declaration
  names and package segments (each segment of a dotted name is checked), not
  only as parcelable/union member names. None of the four is a legal Rust raw
  identifier, so every emission site produced code that could not compile.
- **rsbinder-aidl — breaking:** a `@nullable` `out` or `inout` array of a
  primitive or enum element is declared as `&mut Option<Vec<T>>`. The previous
  `Vec<Option<T>>` element type serialised a null-marker word before every
  element, which libbinder neither writes nor reads for primitives (AOSP
  `aidl_to_rust.cpp::UsesOptionInNullableVector`); `in` arguments already
  followed that rule. Elements that are parcelables, strings, binders or
  interfaces keep the per-element `Option`.
- **rsbinder-aidl — breaking:** `@RustDerive` accepts only the seven traits
  AOSP's schema lists (`Copy`, `Clone`, `PartialOrd`, `Ord`, `PartialEq`,
  `Eq`, `Hash`); anything else is dropped rather than interpolated into a
  `#[derive(...)]`. `Debug` and `Default` were emitted a second time next to
  the impl the templates always generate (E0119), and an unknown name became
  E0405 in the generated crate.
- **rsbinder-aidl:** the generic-nesting guard is now separate from the
  bracket guard and much tighter (12 vs 256). Parse cost is exponential in
  generic depth (~3.7x per level), so the old shared limit let a build hang
  rather than report a diagnostic. Only a `<` that a `>` closes counts as
  generic nesting, so a statement full of `<` comparisons is not mistaken for
  one (an unclosed run is still bounded, at the looser bracket limit). The
  diagnostic names which limit it hit — bracket nesting, generic type
  nesting, or operators in one expression.
- **rsbinder-aidl:** `Builder::hash("")` now panics like `Builder::version(0)`
  already did. An empty hash is falsy to Tera and silently suppressed
  `getInterfaceHash()`.

- **rsbinder (`lazy_service`) — breaking:** `LazyServiceRegistrar` now does
  what its name says. `register_service` makes the service-manager calls
  itself — `addService` with AOSP's `FLAG_IS_LAZY_SERVICE`, then
  `registerClientCallback` with an `IClientCallback` the module owns — routes
  the incoming `onClients` into its own state machine, and once nothing is
  using any of its services unregisters them and exits the process with
  status 0. That is AOSP `tryShutdownLocked`, and the reason the pattern
  exists: the service manager starts the process again on the next lookup.
  It used to be an in-process state machine that made no calls at all, so
  `register_service` registered nothing and a caller owed every one of those
  steps by hand — none of which the type could tell them about.

  `force_persist` suspends the shutdown as before (and releasing it re-checks
  immediately, as AOSP does); the new `set_active_services_callback` takes the
  decision over entirely. `re_register` no longer resets `has_clients` to
  `true`: AOSP `reRegisterLocked` leaves it alone, and inventing clients hides
  the state the service manager just reported. `LazyServiceRegistrar` is
  `Clone`, sharing one set of registrations the way AOSP's handle shares its
  `ClientCounterCallback`. Re-registering a name replaces the tracking entry
  when the binder differs — AOSP keeps the first, which then fails to match
  its own `onClients` — but never registers a second client callback: the
  service manager stores those by name and de-duplicates nothing. The entry
  is published before the `addService` / `registerClientCallback` round trips
  rather than after: the service manager dispatches `onClients` from inside
  those calls, and since `IClientCallback` is `oneway` that notification runs
  on a binder pool thread while the round trip is still open — an entry that
  is not there yet would drop it, with no repeat coming. A newly registered
  service starts with *no* clients, as AOSP's `Service::clients` does;
  assuming one would be an assumption nothing takes back, because the service
  manager reports only changes and never announces a name nobody looked up.

  `register_service` takes `impl Into<SIBinder>` (so a `Strong<dyn IFoo>`
  goes in directly, as with `hub::add_service`) and returns
  `Result<(), Status>` rather than `Result<(), StatusCode>`, which kept the
  service manager's refusal code and message. **Android 10 is refused before
  `addService` is called**: that service manager has neither
  `registerClientCallback` nor `tryUnregisterService`, so a service published
  there could be neither tracked nor taken back down.

  One deliberate departure from AOSP: the *state* lock is not held across the
  service-manager calls, so an `onClients` arriving mid-round-trip is recorded
  when it arrives instead of queueing behind the call it answers. What AOSP's
  single `mMutex` also buys — no two service-manager sequences interleaving —
  is kept by a second lock that spans a whole `register_service` or shutdown
  decision. A shutdown that waited on a `register_service` runs the moment it
  returns, so registering several services in a row can end mid-loop with the
  process exiting; hold it with `force_persist(true)` if every service has to
  be up first.
- **rsbinder — breaking:** `ProcessState::init`'s `max_threads` is now passed
  to `BINDER_SET_MAX_THREADS` **as written**, matching AOSP's
  `setThreadPoolMaxThreadCount`. Previously `0` was a sentinel meaning "use
  the default" and any value `>= 15` was silently clamped down to 15, so
  neither "literally zero" nor a larger pool was expressible. Zero is what a
  single-threaded service manager asks for (AOSP's `servicemanager` does),
  and 15 is a *default*, not a maximum — a service that asked for 32 got 15
  and stalled at it under load, with nothing in the log to say so.

  The default now lives where the default is chosen: `init_default()` passes
  the newly public `DEFAULT_MAX_BINDER_THREADS` explicitly, and a `binder://`
  URI without `?threads=` does the same, so both are byte-for-byte unchanged.
  `?threads=0` asks for zero and gets it. Callers that wrote
  `ProcessState::init(path, 0)` meaning "the default" must now write
  `DEFAULT_MAX_BINDER_THREADS`; the difference is only observable in a
  process that also calls `start_thread_pool()`, which is exactly the case
  that has always logged a warning about it — and that warning was
  unreachable until now, because the clamp guaranteed the stored value could
  never be 0.

  The resolved ceiling is logged at `info` on init, calling out the two ends
  of the range.

- **rsbinder — breaking (internal representation):** a proxy's weak identity
  is now stamped into the `ProxyHandle` at construction instead of being looked
  up in the proxy cache on every `SIBinder::downgrade`. `WIBinder` no longer
  has a `Native` fallback for proxies, `downgrade` no longer takes the cache
  lock, and it no longer requires an initialized `ProcessState`. Two weak
  references naming the same `(handle, generation)` compare equal even when
  they come from different `Arc<ProxyHandle>` allocations — which was already
  the documented intent for case-(b) resurrection, and is now also true after
  the obituary.
- **rsbinder-tools (`rsb_hub`) — breaking:** `rsb_hub` refuses to start unless
  it can load an access-control policy, and denies every request the policy
  does not allow. Previously any local process could register, overwrite, look
  up, and enumerate any service. Existing deployments must supply a policy
  (`--config <PATH>`, default `/etc/rsbinder/hub.d`) or opt out explicitly with
  `--insecure-allow-all`, which is named that way on purpose and cannot be
  reached by accident. There is no permissive fallback for a policy that fails
  to load: a typo in a config file must never silently remove access control
  from a running system.
- **rsbinder-tools (`rsb_device`) — breaking:** the binder device node is now
  created `0600` (root only) instead of `0666` (world read/write). Grant access
  deliberately: `sudo rsb_device binder --group binder --mode 0660`, with your
  user in that group — the same model as `/dev/kvm` being `0660 root:kvm`.
- **rsbinder-tools (`rsb_hub`):** `listServices` and `getServiceDebugInfo`
  return names in sorted order. The registry is a `BTreeMap` now, matching
  AOSP's `std::map`; previously the order shuffled between calls.
- **rsbinder-aidl / rsbinder (async):** generated `IFooAsyncService` impls now
  carry `#[rsbinder::__async_trait]` instead of `#[::async_trait::async_trait]`,
  and `rsbinder` re-exports the attribute. A crate that only consumes generated
  code no longer needs an `async-trait` line of its own — which the
  `#[rsbinder::interface]` path had no way to tell users about, since it has no
  `build.rs` step to document. Existing manifests keep working; the dependency
  is simply redundant now. This changes the generated source text (not the
  wire) for async builds.
- **rsbinder (rpc):** an RPC proxy now holds its `RpcSession` **strongly**
  (AOSP `BpBinder` ↔ `sp<RpcSession>`). Dropping the `RpcSession` handle no
  longer invalidates proxies obtained from it — the proxy alone keeps the
  connection open, and the connection closes when the last proxy (and
  handle) is gone. Previously every call on such a proxy returned
  `DeadObject`. Code that relied on "drop the session to disconnect" while
  still holding a proxy must drop the proxy too. To keep the strong ref
  acyclic, session death now also releases every local object the peer
  held (AOSP `RpcState::clear`), on both the served path and — new — a
  client-only session whose last connection is lost (detected on the next
  failed transaction, which also fires `DeathRecipient`s). Because a failing
  transaction may now declare session death before the slot's own serve
  worker exits, `serve_blocking` no longer trips a debug assertion on that
  ordering.
- **rsbinder (entry API):** `ServeOptions::fd_modes` / `ClientOptions::fd_mode`
  now reject Unix fd passing on a transport that cannot carry `SCM_RIGHTS`
  (vsock, TLS, kernel binder) instead of agreeing a mode that fails later on
  the wire.
- **rsbinder (AOSP alignment):** `FLAG_PRIVATE_VENDOR` is now `0x10000000`
  (AOSP `IBinder.h`) instead of `0`. Code passing this flag to `transact`
  now sets bit 28 on the wire. `FLAG_PRIVATE_LOCAL` is unchanged (`0`).
- **rsbinder (`rpc` feature, breaking):** `WireMessage::DecStrong` now carries
  the decrement amount (`DecStrong(RpcAddress, u32)`), and
  `RpcState::dec_strong_local` takes an `amount` argument. A batched
  `RpcDecStrong` from a libbinder peer (AOSP `sendDecStrongToTarget` sends
  `timesRecd - target`) is now honored instead of applying a single decrement,
  which under-counted and leaked local nodes.
- **rsbinder (`rpc` feature, breaking):** the RPC wire-codec types
  (`WireMessage`, `WireCodec`, `R34Codec`, `Android13PlusCodec`, `WireReply`,
  `WireTransaction`) and `RpcState` are no longer part of the public API. They
  were internal implementation detail — the codec is selected internally with no
  user injection point, and `RpcState` is per-session bookkeeping. The public
  RPC surface stays `RpcServer`, `RpcSession`, `RpcProxy`, the transport traits
  (`RpcTransport`/`PeerIdentity`/`CertId`), and the address/identity types. This
  lets the wire protocol evolve without further semver-breaking releases.
- **rsbinder (breaking):** additional helpers that were `pub` but never part of
  the intended API are now `pub(crate)`: the `Parcel` RPC-ops plumbing
  (`RpcParcelOps`, `Parcel::attach_rpc_ops`, `FnFreeBuffer`), `rpc::RpcSessionInner`,
  `RpcProxy::stamp_descriptor`, and the `thread_state` functions `check_interface`,
  `is_handling_transaction`, and `get_calling_uid_or_self`. The public
  `rsbinder::is_handling_transaction` (re-exported from `native`) is unchanged.
- **rsbinder (breaking):** the low-level `Parcel` buffer accessors `as_ptr`,
  `as_mut_ptr`, `capacity`, `set_data_size`, `close_file_descriptors`, `is_empty`,
  and `from_vec` are now `pub(crate)` (internal kernel-buffer plumbing).
  `Parcel::from_ipc_parts` (the documented `unsafe` raw-buffer primitive) and
  `Parcel::set_for_rpc` remain public.

### Removed

- **rsbinder (`service` module):** the `rsbinder::service` facade (`Registry` /
  `Broker`, `service::kernel::{Host, Broker}`, `service::rpc::{Host, Broker}`)
  is gone, replaced by the entry API above (`serve` / `connect` / `Client`).
  Migration: `kernel::Host::new()? + add_service + serve()` →
  `serve("binder://")?.add(..)?.run()?`; `rpc::Host::unix(p)` →
  `serve("unix://<p>")`; `kernel::Broker::new()?.get_interface(n)` →
  `connect("binder://<n>")`; `rpc::Broker::unix(p)?.get_interface(n)` →
  `connect("unix://<p>#<n>")`; `Host::builder()` options → `ServeOptions`
  via `Server::with`. The examples, the book chapter *Cross-Transport
  Services*, and the crate quick-start now use the entry API.

### Fixed

- **rsbinder (RPC) — a refused attach was reported to the client as success.**
  The outgoing direction of an android-13+ attach carries no acknowledgement
  (AOSP writes `RpcNewSessionResponse` only for a new session, and an outgoing
  connection's `"cci"` flows client→server), so a peer that refused the
  connection — `session_id` unknown or stale, its `set_max_threads`
  outgoing-slot cap already spent, shutting down — could only close the socket.
  `RpcSession::add_outgoing_connection_android13plus` returned `Ok(slot)`
  anyway and left the dead connection in the pool, where it broke one *later,
  unrelated* call — whichever one the connection picker happened to route onto
  that slot — and was then reclaimed, so a single-threaded client saw exactly
  one inexplicable failure per run and a multi-threaded one saw them
  intermittently. `setup_unix_client_android13plus_with_id` (and
  `ClientOptions::session_id` over it) likewise handed back a session whose
  only connection the peer had already closed. Both now confirm admission with
  one `GET_SESSION_ID` round trip on the fresh connection *before* using it —
  the reply must carry the id that was echoed — and report the refusal at the
  attach call. Costs one extra round trip per attached connection; a new
  session (empty id) is untouched, and so is the incoming (callback)
  direction, whose post-admission `"cci"` already reported refusals. A
  client's `RpcSession::session_id()` is a client-local value that is never
  the peer's id (only `get_session_id()` fetches that), which is the mistake
  this used to accept silently; its rustdoc now says so.
- **rsbinder (RPC) — a write-side disconnect on an android-13+ session
  reported `Unknown` instead of `DeadObject`.** `RawTransportIo` bridges the
  transport to `std::io`, and its write side stringified the `RpcError`
  into `io::Error::other`, so a `PeerClosed` raised while *sending* came
  back from the other side of that boundary as `Io(Other)` — a peer that had
  gone away was reported as an unclassified error, on the send path of every
  call on a non-fd session as well as the handshake. `From<RpcError> for io::Error` is now
  kind-preserving for the two variants that have an `io::ErrorKind` meaning
  the same thing (`PeerClosed` → `BrokenPipe`, `Timeout` → `TimedOut`), so a
  peer close comes back from the bridge as `PeerClosed` and a timeout comes
  back in the shape the framing reader's deadline arm keys on. The TLS
  transport's R34 framing adapter (`transport::tls`'s `RawIo`) carried the
  same stringifying projection on its write side and is fixed with it, so an
  `rpc-tls` R34 session now reports a dead peer as `DeadObject` too.
- **rsbinder (RPC) — a wire-profile mismatch never mentioned the profile.**
  An android-13+ server clamps whatever version the connection header
  decodes to (`min(client_version, server_max_version)`, as AOSP does), so an
  r34 (default) client's leading bytes got past the header: the server
  accepted the connection, answered, and failed at the `"cci"` check with a
  reject that named nothing an operator could act on; the server's handshake
  log also flattened every cause into the opaque `RpcError` status name. In
  the other direction an r34 server hung up mid-handshake and the client got
  a bare dead-peer status with no log at all. Both rejects now name the r34
  profile as the likely cause, in the same shape as the existing
  `Client::open` diagnostics. Which side of that exchange notices the
  disconnect first is a host- and timing-dependent race (EOF on the client's
  response read, or `EPIPE`/`ECONNRESET` on its `"cci"` write), so the client
  hint covers both — and a stall or a mid-frame cut names the deadline or the
  partial response instead of guessing. An attach whose `max_version` is
  below the session's negotiated version (which can never work — an attach
  does not negotiate) now logs that `wire_protocol_version()` is the value
  to pass instead of failing with a bare `BadType`.
- **rsbinder (kernel) — a failed reply flush left a `BC_REPLY` pointing at
  freed memory.** `write_transaction_data` stores raw pointers to the reply
  parcel (or the status word on the stack) in the thread's command buffer.
  When `wait_for_response` failed before the driver consumed that command —
  a queued `BR_DEAD_REPLY`/`BR_FAILED_REPLY`, or an ioctl error — the
  function returned through `?`, the reply was dropped, and the next flush
  (`join_thread_pool`'s exit, or any later call on that thread) handed the
  kernel the dangling pointer to copy into the peer: a cross-process
  use-after-free. The panic path already rewound the buffer; the error path
  did not. The reply path now retries the flush while the pointers are
  still valid and rewinds the unconsumed tail otherwise; outbound
  `transact` only rewinds — a retried `BC_TRANSACTION` would leave a
  two-way call in flight whose reply the next call on that thread would
  take as its own. An `IBinder::dec_strong` failure on the same path no longer
  skips the reply (the two-way caller hung forever) or the transaction-state
  restore (the next call inherited the previous caller's uid).
- **rsbinder (`Parcel`) — a safe forward seek could make the kernel read past
  the buffer.** `data_size()` is `max(len, pos)` (AOSP `dataSize`), and the
  outbound path sent exactly that with `as_ptr()`, so
  `set_data_position(1024)` on a 48-byte parcel followed by a transact made
  the driver `copy_from_user` a kilobyte from a 256-byte allocation and
  deliver the surplus heap to the peer — from `pub`, non-`unsafe` calls. The
  kernel now receives the initialized length. The `Vec::set_len` contract in
  `ParcelData` was also misstated (and one test violated it by claiming
  never-written capacity); growing is now a separate `unsafe` entry used
  only for the driver-filled read buffer, and the byte-copy generics
  (`write_aligned` / `write_array` / `read_array`) are sealed behind a
  `ParcelPod` marker instead of trusting whichever type instantiates them.
- **rsbinder (kernel) — `become_context_manager` never asked for the
  caller's security context.** AOSP registers with
  `FLAT_BINDER_FLAG_TXN_SECURITY_CTX`, the one bit the kernel reads to decide
  whether the context manager gets `BR_TRANSACTION_SEC_CTX`; rsbinder passed
  only `ACCEPTS_FDS`, so every transaction into `rsb_hub` arrived without a
  security context and `get_calling_sid()` was always `None` there. The flag
  is now set on Android and on Linux with SELinux mounted — and deliberately
  *not* elsewhere, because a kernel that cannot produce a context fails every
  transaction to such a node with `BR_FAILED_REPLY` (verified on a
  non-SELinux box). `ACCEPTS_FDS` stays set, unlike AOSP, since `rsb_hub`
  answers `DUMP_TRANSACTION`, which carries an fd.
- **rsbinder (`parcelable`) — the array pre-allocation cap bounded the count,
  not the bytes.** `Vec::with_capacity(min(len, data_avail()))` still
  multiplies by `size_of::<T>()`: a 64 MiB RPC frame declaring
  `len = 64_000_000` for `Vec<String>` requested ~1.5 GiB up front, and an
  allocation failure aborts. Every element on that path costs at least 4 wire
  bytes, so the cap is now `data_avail() / 4`.
- **rsbinder (RPC) — a `DEC_STRONG` could deadlock the session.**
  `RpcState::dec_strong_local` dropped the freed node's strong ref while the
  state lock was held; if that was the last reference to a user service
  holding a proxy of the same session, `RpcProxy::drop` re-took the lock on
  the same thread. The removed reference is now handed back and dropped after
  the guard, as `clear_local` already did. The same drop from inside a
  dispatch used to send its `DEC_STRONG` inline, *ahead of the reply* being
  built — so a binder passed as an argument and returned in the same call
  (`repeatBinder`) came back to its owner after the owner had already freed
  the node, and the owner minted a bogus proxy to itself. Such a `DEC` is
  deferred until the slot is free, which is the ordering AOSP has.
- **rsbinder (RPC) — a peer could plant a proxy entry in our own address
  subspace.** `RpcState::remote_proxy` minted a remote proxy for any unknown
  address; AOSP `onBinderEntering` refuses one the receiving side would have
  created itself. A forged address could then alias a local node and the next
  legitimately minted one. Refused as `BadValue` now.
- **rsbinder (RPC, `tokio`) — a nested callback from an RPC handler
  deadlocked in a pure-RPC process.** `BinderAsyncPool::spawn` gated the
  run-on-this-thread arm on `ProcessState::is_initialized()`, a guard that
  `is_handling_transaction()` had since taken over itself. Without kernel
  binder the call went to the blocking pool, where the session's thread-local
  re-entrancy pin does not follow it, and it parked on the slot the
  dispatching thread was waiting on.
- **rsbinder (RPC, macOS) — a non-`AF_UNIX` fd resolved to a root peer.**
  `getpeereid` on a TCP socket *succeeds* on macOS with `euid = 0`
  (measured), which the `rc != 0` ladder could not catch, so
  `UnixTransport::from_owned_fd` on a foreign-family fd minted
  `PeerIdentity::Local { uid: 0 }`. The socket family is checked first. On
  Linux/Android `SO_PEERCRED` is read through libc rather than rustix's
  `Pid(NonZeroI32)`, which the kernel's `pid == 0` (peer outside our PID
  namespace) would have made an invalid value.
- **rsbinder (RPC server) — the r34 (default) profile never lifted the
  handshake *write* deadline.** Only the read side was cleared after the
  first frame, so `SO_SNDTIMEO` stayed armed for the session's whole life and
  a large reply to a slow reader failed with `WouldBlock` — contrary to
  `set_handshake_timeout`'s contract. The incoming-slot cap on attach was a
  check-then-act (concurrent attaches could overshoot it) and counted the
  peer's callback connections against `setMaxIncomingThreads`; it is now one
  critical section over serve-driven slots only, as AOSP counts.
- **rsbinder (`proxy_count`) — a panicking `ProxyCountCallback` escaped a
  `Drop`.** The remaining queued events were lost with it and, during an
  unwind, the process aborted. Callbacks are isolated with `catch_unwind`
  like death recipients already were.
- **rsbinder (`lazy_service`) — a failed re-registration could hide a service
  forever.** `restore` copied back the optimistic `registered: true` the call
  itself had just written, so after a public `try_unregister` followed by a
  refused `register_service` the entry said "registered" while the service
  manager had nothing, and `re_register` skipped it for good.
- **rsbinder (`lazy_service`) — a refused `registerClientCallback` came back
  as `EX_TRANSACTION_FAILED`.** The service manager's own exception
  (`EX_SECURITY` for a policy refusal, `EX_ILLEGAL_STATE` for a binder it
  does not know) was folded into a `StatusCode` on the way through `hub`
  and re-wrapped, so `register_service` could never report what its
  `# Errors` section promises. The registrar now keeps the service
  manager's `Status` for both round trips.
- **rsbinder (`hub`, Android 15) — the numbering probe ran on every
  `default()`.** A refused numbering (only one of `android_14` /
  `android_15` compiled in) re-sent the probe transaction and re-logged the
  five-line refusal on every lookup; the answer is now cached for the
  process and logged once. A build with neither feature meeting an
  Android 15 device also failed silently — it now names both features.
- **rsbinder (RPC) — a proxy from another session was sent as a local
  address.** `write_binder` reused an `RpcProxy`'s address without checking
  which session minted it, so a proxy obtained over session A and written
  into session B's parcel named one of A's peer's nodes on B's wire —
  which B's peer resolved against its *own* nodes (the android-13+ address
  is a small counter). AOSP `onBinderLeaving` refuses this with
  `INVALID_OPERATION`; so does rsbinder now.
- **rsbinder (`hub`, Android 15 r6+) — `check_service` starts lazy
  services.** It is carried by `getService` (the only lookup that returns a
  bare `IBinder` across the release range), and AOSP's servicemanager runs
  `tryStartService` for an unregistered name on `getService` but not on
  `checkService`. Documented on the function and in the lookup table; the
  behaviour cannot change without parsing the release-dependent `Service`
  union.
- **rsbinder (`file_descriptor`) — the fd null marker accepted any non-zero
  value** (AOSP rejects anything but `1` as `UNEXPECTED_NULL`), and a
  reliable-pipe `ParcelFileDescriptor` from Java never received the
  `DETACHED` notice on its comm channel, so the sender's `checkError()`
  reported a clean close. The notice is best-effort, as in AOSP: a sender
  that already closed its end (a oneway send followed by `close()`) no
  longer fails the whole fd read with `BadType`, and the write cannot raise
  `SIGPIPE`.
- **rsbinder (RPC) — hardening and housekeeping:** `wire_android13` reads a
  message body straight into its final buffer (a 16-byte header could commit
  twice `bodySize` before any body byte arrived) and rejects unknown
  `RpcWireAddress` option bits instead of normalizing them; the in-memory
  test transport enforces `MAX_FRAME_LEN` like every real backend; a failed
  `SCM_RIGHTS` attach is an error rather than a `debug_assert`; a reaper-thread
  spawn failure is logged (it silently lost every deferred `DEC_STRONG`);
  `unix-abstract` and `binder://name` URIs decode percent-escapes like their
  siblings and `%+9` is no longer an escape.
- **rsbinder — comments and tests that said the wrong thing:** the
  `AccessorRoot` drop-order note (the proxy holds the session strongly), the
  `ParcelableHolder` `!Send` claim (it is `Send + Sync`), the `obituary_sent`
  ordering rationale, the `RefCounter` double-dec assertion (it could never
  fire — the sentinel reset made `c == INITIAL_STRONG_VALUE`), the Android 15
  probe pin (it never referenced the probe constant), and several tests that
  could not fail or asserted on the wrong gate (`test_strong`, `test_errors`,
  `test_try_from`, the `Status` round trip that could not see `message`, the
  RPC parcel fuzz target's binder claim, the abstract-socket
  `stdout` fd test that took ownership of fd 1).
- **rsbinder-aidl — an unqualified constant reference could resolve to an
  unrelated declaration's constant.** Interface constants were registered in a
  flat symbol table under their bare name, and that table was consulted before
  the namespace-aware lookup. With `interface IX { const int A = 999; }`
  anywhere in the build, a sibling `parcelable P { const int A = 1; const int
  B = A + 1; }` generated `B = 1000`. Constants are now keyed by their
  declaring namespace and resolved outward through enclosing scopes only —
  and an imported interface's constant (`import a.IFoo; … IFoo.BAR`) still
  resolves, through the import's own candidate.
- **rsbinder-aidl — a constant's expression was folded in the referencing
  declaration's scope.** `IFoo.BAR` defined as `BASE + 1` and read from
  `IBaz` resolved `BASE` against `IBaz`, so a same-named constant there
  changed the value (or an absent one failed the build); the fully-qualified
  spelling had the same problem. A constant is now folded where it is
  declared, once per constant, so a diamond of cross-package references and
  a genuine reference cycle (still reported as such) both terminate.
- **rsbinder-aidl — `@JavaOnlyImmutable` parcelables and unions lost all their
  members.** The annotation check was a `starts_with("@JavaOnly")` prefix
  match, so it also caught `@JavaOnlyImmutable` — a *structured* parcelable
  that AOSP's Rust backend generates normally. Affected parcelables rendered
  as `pub struct Foo {}`, and affected unions produced no type at all.
- **rsbinder-aidl — enum references folded incorrectly in comparisons and
  float expressions.** `ValueType::Reference` outranked every arithmetic type
  in the promotion order, so `Status.OK == 0` folded to `false` while
  `0 == Status.OK` folded to `true`, `Status.OK != Status.OK` folded to
  `true`, and a float operand was truncated to the reference's integer width.
  A reference now promotes to its integral value, per AOSP
  `AidlConstantReference`. `to_bool`/`to_f64` accept references too, so
  `E.FOO || false` no longer fails to build, and unary `-E.FOO` / `~E.FOO`
  fold like `!E.FOO` instead of failing on the unfolded reference.
- **rsbinder-aidl — an enum reference widened the expression around it.** A
  reference folded as `long` whatever its `@Backing` type, so with
  `@Backing(type="int") enum F { BIT = 1 }`, `F.BIT << 31` was rejected as
  out of range for `int` while the identical `1 << 31` folded to
  `-2147483648`. A reference now folds at its backing width (`byte` promoting
  to `int`, as in C++).
- **rsbinder-aidl — a non-nullable `out` argument could send a null.** The
  server's local for an `out` `IBinder` / `ParcelFileDescriptor` / interface
  is `Option<T>` initialised to `None`; a service that left it unset had that
  `None` written straight into the reply. The server now fails with
  `UNEXPECTED_NULL` instead, as AOSP `generate_rust.cpp` does. The matching
  guard for out `ParcelFileDescriptor` arrays now covers fixed-size arrays
  too, which AOSP also guards.
- **rsbinder-aidl — a `@nullable` fixed-size array wrapped its elements
  unconditionally.** `@nullable out int[3]` became `Option<[Option<i32>; 3]>`,
  prefixing every element with a null marker libbinder does not write, and
  `@nullable out MyEnum[3]` did not compile at all (`declare_binder_enum!`
  implements no `SerializeOption`). Fixed-size arrays now use the same
  primitive/enum rule as vectors.
- **rsbinder-aidl — the trait signature and the server local disagreed for
  `inout` arrays.** For example `inout ParcelFileDescriptor[]` declared the
  trait parameter as `&mut Vec<T>` while the server declared its local as
  `Vec<Option<T>>` and passed `&mut` it straight in (E0308). Both are now
  `Vec<T>`: an `inout` vector is read from the parcel fully populated, so its
  elements need no `Default`, and AOSP `aidl_to_rust.cpp::RustNameOf` keeps
  `element_mode = VALUE` for `INOUT_ARGUMENT` for the same reason.
- **rsbinder-aidl — a fixed-size array constant did not compile.**
  `const int[3] X = {1,2,3};` emitted `pub const r#X: &[i32; 3] = [1,2,3,];`
  — a reference type initialised with an array literal. Fixed-size constants
  are now value types (`[&str; N]` for `String` elements, whose initializer is
  string literals); the variable-length form still emits a slice. A
  `@nullable` `String` array constant now declares the element `Option` its
  initializer actually emits — `const @nullable String[] X` was typed
  `&[&str]` while initialised `Some(&[Some("a")])`.
- **rsbinder-aidl — a directory source containing a symlink cycle never
  finished.** `Path::is_dir()` follows symlinks and the visited set was keyed
  by the path string, so `aidl/loop -> .` produced endlessly deeper paths.
  The set is now keyed by the canonical path, and so is the include-directory
  set: an absolute `include_dir` plus a relative `source` under the same
  directory listed it twice, so every import beneath it was reported as an
  `AmbiguousImport` between two spellings of one file. A source sitting
  directly under its own package path (`source("android/os/IFoo.aidl")` with
  `package android.os;`) derives an empty include path, which names the
  working directory rather than a directory of its own — it no longer
  collides with `include_dir(".")`, and no longer leaves an empty
  `cargo:rerun-if-changed=`.
- **rsbinder-aidl — the output directory was never created.** `generate()`
  failed with a bare "No such file or directory (os error 2)" when `dest_dir`
  did not exist (including the documented `aidl_gen/` fallback outside cargo)
  or when `output` named a subdirectory. It is created on demand, and I/O
  failures now name the path.
- **rsbinder-aidl — a second `Builder` could inherit the first one's
  declarations.** Parsing happens in `generate()` but the parser was reset in
  `Builder::new()`, so two builders constructed before either generated shared
  state: the second resolved types the first had declared without importing
  them. The reset moved to the parse phase.
- **rsbinder-aidl — legitimate AIDL was rejected as "nesting too deep".** The
  pre-parse guard counted `>>` only as a shift, so generic depth accumulated
  across a statement and a signature with 129 `List<List<T>>` arguments was
  refused at a real nesting depth of 2.
- **rsbinder-aidl — enum discriminants outside their `@Backing` range were
  emitted anyway.** `@Backing(type="byte") enum E { A = 200 }` produced a
  literal that is a deny-by-default rustc error in the generated crate,
  pointing at `OUT_DIR` rather than the `.aidl`. The range is now checked at
  AIDL-compile time, as AOSP does.
- **rsbinder-aidl — two union fields could collapse into one Rust variant.**
  `union U { int my_field; int myField; }` emitted `r#MyField` twice (E0428);
  the collision is now a diagnostic naming both fields.
- **rsbinder-aidl — `@RustDerive(Debug=true)` emitted `#[derive(Debug)]`
  twice** (E0119); the templates already derive it unconditionally. A
  parameter outside the schema (`@RustDerive(Cloen=true)`) is now a
  diagnostic naming it; it used to be dropped silently and surface as a
  missing trait in the user's crate.
- **rsbinder-aidl — a nested fixed-size `out ParcelFileDescriptor` array did
  not compile.** The `UNEXPECTED_NULL` guard iterated one level of
  `[[Option<_>; 3]; 2]`; it now flattens one level per extra dimension.
- **rsbinder-aidl — a `rust_type` declaration named after a Rust keyword was
  not escaped**, unlike every other declaration path.
- **rsbinder-aidl — a negative out-of-range shift amount was reported by its
  magnitude.** `1 << -100` said "shift amount 100", which does not match the
  source.
- **rsbinder-aidl — a package segment that is a Rust keyword was emitted
  unescaped.** `package com.example.impl;` produced `pub mod impl {`, while
  references to it were escaped as `super::r#impl::...`. The escape list also
  covers `gen`, reserved in Rust 2024 — generated code is compiled in the
  consumer's edition.
- **rsbinder-aidl — the `#[allow(clippy::all)]` / `#[allow(unused_imports)]`
  header applied only to the first top-level module** in the generated file,
  and to nothing at all in a document without a `package`. Both lints are now
  part of every generated module's inner `#![allow(...)]`, alongside the
  naming allowances; the header stays on each top-level package module, whose
  own lints (e.g. `module_inception` against the `include!` site) an inner
  attribute one level down cannot cover.

- **rsbinder (`hub`) — Android 15 QPR builds are now refused instead of
  silently losing registrations.** `android-15.0.0_r6` inserted `getService2`
  at index 1 of `IServiceManager.aidl`, shifting every transaction code after
  it by one (`addService` 2 → 3, `registerClientCallback` 11 → 12,
  `tryUnregisterService` 12 → 13) while leaving the SDK at 35 — the interface
  is not a frozen `aidl_interface`, and AOSP is unaffected because
  `servicemanager` and `libbinder` ship in one image. rsbinder picks its
  protocol from the SDK version, so on such a build it addressed the wrong
  method for everything past `getService`: measured against the
  `BP11.241210.004` servicemanager, `addService` lands on `checkService`,
  which reads the name and leaves the rest of the parcel, and AOSP's
  generated `onTransact` rejects the leftovers with `BAD_PARCELABLE`. Nothing
  is registered, and the error says nothing about why.
  `hub::default` now measures which numbering the device speaks — one
  argument-free transaction to a code that exists in exactly one of the two —
  and dispatches accordingly. The shifted numbering is served by the new
  `android_15` feature (see *Added*); a build without it refuses the device
  rather than addressing the wrong method, and says which feature is missing.
  Every other version is unaffected. Only kernel-binder use through `hub` is
  in scope — the RPC transport does not go through the service manager.
- **rsbinder (hub):** `ServiceManager::get_connection_info` did not compile
  on Android 13/14 — each version generates its own `ConnectionInfo` and the
  dispatch arms returned the version's type where the unified one was
  expected. Latent because no build enabled those features together.
- **rsbinder (`ProxyHandle::dump`):** a `write_object` that failed after the
  descriptor had been detached from its RAII wrapper leaked the descriptor.
  Every other path is covered by the parcel's own ownership of it.
- **rsbinder:** a `DeathRecipient` could not identify the binder that died.
  `SIBinder::downgrade` read a proxy's generation from the proxy cache and fell
  back to the `Native` variant when the entry was missing — and the obituary
  retires that entry *before* dispatching `binder_died`. Every weak taken
  inside a death recipient therefore compared unequal to the `who` it was
  handed and to every weak taken while the binder was alive, so a recipient
  watching more than one binder matched none of them. This is what made
  `rsb_hub` keep dead services in its registry (see below); any user code
  matching `who` against stored binders hit the same wall.
- **rsbinder-tools (`rsb_hub`):** a dead service was never removed from the
  registry. The death handler matched the obituary against a *freshly*
  downgraded `WIBinder`, but a proxy `WIBinder` compares on
  `(handle, proxy-cache generation)` and the obituary retires that cache entry
  before invoking callbacks — so every death matched nothing and retired zero
  entries. A crashed service kept its name reserved, kept being handed to
  clients as a dead binder, and leaked its callbacks and death subscription
  until `rsb_hub` restarted. The underlying `SIBinder::downgrade` defect is
  fixed above; `rsb_hub` now matches `who` against its stored binders directly.
- **rsbinder-tools (`rsb_hub`):** death subscriptions are reference-counted per
  binder, so link and unlink can no longer drift apart. They drifted both ways:
  `unregisterForNotifications` removed a callback without unlinking, so every
  register/unregister cycle left another subscription behind — unbounded, and
  reachable by any local caller — and one binder registered under K names took
  K subscriptions, so its death ran K full cleanup sweeps instead of one.
- **rsbinder-tools (`rsb_hub`):** `getService2`/`checkService2` now report
  `isLazyService` from the registered `FLAG_IS_LAZY_SERVICE` instead of always
  `false`. AOSP's client uses it to skip caching a lazy service, which can
  withdraw via `tryUnregisterService` *without dying* — so no death
  notification would have invalidated the client's cached binder.
- **rsbinder-tools (`rsb_hub`):** exception-code parity with AOSP on three
  replies that are visible to a C++ `LazyServiceRegistrar`: "only a server can
  register client callbacks" and "only a server can unregister itself" are now
  `EX_UNSUPPORTED_OPERATION` (were `EX_SECURITY`), and `tryUnregisterService`
  on an unregistered or mismatched name is `EX_ILLEGAL_STATE` (was
  `EX_ILLEGAL_ARGUMENT`).
- **rsbinder-tools (`rsb_hub`):** a failed self-registration no longer stops
  the process — it is logged, as AOSP does. Clients reach the hub through
  handle 0 regardless. `addService` also warns when `dumpPriority` sets no
  `DUMP_FLAG_PRIORITY_*` bit, which silently hides the service from every
  `listServices` filter.
- **rsbinder (`macros` feature):** signature shapes the macros accepted but
  should not have, all found by review and each now refused with a compile-fail
  case in `rsbinder-macros/tests/ui`: a `#[oneway]` method with an out/inout
  parameter (which `.aidl` rejects, and whose value could never come back); a
  `ParcelableHolder` field, whose derived codec could never decode a peer's
  `@VintfStability` holder; borrowed types nested inside another type; an
  `unsafe` or `auto` trait and a variadic method, whose modifiers the
  re-render dropped without a word; and a fixed-array length written as a named
  constant, which the macro cannot evaluate and so silently mis-initialised.
- **rsbinder (`macros` feature):** shapes that no `.aidl` can express are now
  refused instead of building an interface with no migration path — `out`/
  `inout` on a primitive, on `String` or on `ParcelFileDescriptor`, and
  `Option<T>` over a primitive. AOSP's `GetArgumentAspect` gives every
  non-array builtin `in` as its only direction and rejects `@nullable` on a
  scalar; `rsbinder-aidl` matched it, the macros did not.
- **rsbinder (`macros` feature):** shapes the macros accepted and now render
  correctly, unit-tested in `rsbinder-macros`: a nullable out vector, which
  generated code that did not type-check; an out `ParcelFileDescriptor` array,
  which silently omitted the null guard `.aidl` emits and would have put a null
  fd on the wire; a raw-identifier field such as `r#type`, which rendered as
  `r#r#type`; and a fixed-size out array longer than 32. A by-value argument
  must be `Copy` (the proxy passes it twice) and a derived parcelable must
  implement `Default` — both are now stated in the rustdoc and asserted against
  the user's own type instead of failing inside generated code.
  `#[derive(BinderEnum)]` no longer requires `Copy` and accepts
  `#[repr(i32, align(8))]`; an unrecognised method attribute is refused rather
  than dropped; and the generated items follow the trait's own visibility.
- **rsbinder (`macros` feature):** `#[derive(BinderEnum)]` re-emitted each
  variant's discriminant *expression* and cast that, so the literal inside was
  typed on its own and fell back to `i32`: `#[repr(i64)] A = 1 << 31` went on
  the wire as `-2147483648` instead of `2147483648`, and `1 << 40` failed to
  compile with a message about `i32`. Both ends of the macro used the same
  wrong value, so only a `.aidl` or AOSP peer saw the mismatch. The codec now
  casts the declared variant.
- **rsbinder (`macros` feature):** a `descriptor = "…"` containing a backslash
  or a quote was spliced verbatim into the generated source, either changing
  the wire descriptor or breaking the string literal — the latter surfacing as
  a parse failure printing the whole generated module. It is now refused on the
  attribute itself. The `.aidl` path is spared this by its grammar.
- **rsbinder (`macros` feature):** a type reaching the macros through a
  `macro_rules!` `$t:ty` capture, or wrapped in parentheses, was refused as
  "unsupported" even when it was something as ordinary as `i32` — `syn` hands
  such a capture over inside invisible delimiters, which the type walkers now
  see through. `#[derive(Parcelable)]` on a raw-identifier type also sent
  `"r#type"` as its descriptor, where the `.aidl` path sends `"type"`; the
  descriptor is wire-visible through `ParcelableHolder`, which records and
  compares it.

- **rsbinder-aidl:** an interface that names *itself* in a signature
  (`interface IFoo { void register(in IFoo cb); }`) rendered as
  `Strong<dyn Box<IFoo>>` and did not compile — the guard that boxes a
  self-referencing *parcelable* field, which genuinely needs the indirection,
  was being applied to interfaces, where `Strong<dyn …>` is already a handle.
  Self-referencing callbacks now generate correctly; recursive parcelables are
  unchanged, and so is every other generated file.
- **rsbinder-aidl:** two parcelables that reference *each other* produced an
  infinitely sized Rust type (`E0072`). The box guard compared names, so it saw
  only cycles of length one; it now walks the declaration graph. A cycle that
  runs through an interface stays unboxed — `Strong<dyn …>` is a handle, which
  is what makes the AOSP `CircularParcelable` / `ITestService` pair finite. So
  is a cycle that runs through an array or a `List`: a `Vec` element is a
  fixed-size handle whatever it holds, and `Box<T>` implements neither
  `SerializeArray` nor `DeserializeArray`, so boxing one emitted a
  `Vec<Box<T>>` that did not compile. `<Union>.Tag` is likewise never boxed —
  it resolves to its parent union's namespace, which made it look like the
  union itself, but the generated `Tag` is a scalar newtype holding no union.
- **rsbinder-aidl:** a cycle closed by a **non-nullable** field is now a
  diagnostic (`aidl::recursive_parcelable`) rather than a box. `Box<T>` alone
  breaks the size, not the recursion: the generated `impl Default` — which is
  the deserialization entry point, not decoration — called itself until the
  stack ran out, so a peer's parcel aborted the process. `@nullable` renders
  the field as `Option<Box<T>>`, which terminates; AOSP likewise requires the
  cycle-closing field to be nullable.
- **rsbinder (`macros` feature):** a second batch of signature shapes, each now
  covered by a test. A raw identifier — `fn r#type`, or an argument named
  `r#match` — rendered as `fn r#r#type` / `_arg_r#type` and did not lex, the
  same double-escape already fixed for parcelable fields. An out parameter
  spelled through a qualified path (`&mut std::vec::Vec<i32>`) silently lost
  the length word `.aidl` writes, and a qualified `ParcelFileDescriptor`
  element lost the null guard that keeps a null fd off the wire — those
  decisions now read the type, not its rendered text. `BinderResult<Option<&T>>`
  was rewritten to `Option<String>` with no diagnostic on the declaration. An
  attribute on the *trait* (a `#[cfg]`, a trait-level `#[oneway]`), a where
  clause, and `async`/`unsafe`/`const`/`extern` on a method were all dropped by
  the re-render rather than refused. `self::` in a signature resolved inside the
  generated module instead of the caller's. The generated module followed
  neither the trait's visibility — a private trait was reachable through it —
  nor the scope its own `Copy` assertion used, so a path in a by-value argument
  was resolved at two different depths.
- **fuzz:** the `rpc_wire_decode` / `rpc_address_decode` / `rpc_session_handshake`
  targets compile again — their entrypoints are re-exported as
  `rsbinder::rpc::__fuzz_*` after the wire-codec module became crate-private.
  New `rpc_raw_fd` target covers the bare (`IMemoryHeap`-style) fd decode.

- **rsbinder:** a remote binder handle is serialized through the full 8-byte
  object union, so no uninitialized stack bytes are copied onto the wire; the
  proxy path previously wrote only the 32-bit handle and leaked 4 bytes to the
  peer.
- **rsbinder (`rpc`):** a discarded RPC reply's reference-count bumps are rolled
  back on the handler-error and oneway dispatch paths, preventing local-node
  leaks.
- **rsbinder:** `Parcel::append_from` bounds the source range against the
  backing buffer length instead of the logical size, avoiding a panic when the
  source cursor sits past its data end.
- **rsbinder (`rpc`):** the Unix transport retries `EINTR` on a raw read and
  rejects file descriptors attached to an empty frame instead of dropping them.
- **rsbinder:** `get_extended_error` and `query_interface` return a recoverable
  error instead of panicking in a pure-RPC or malformed-reply path.
- **rsbinder (`rpc`):** `StatusCode::RpcError` no longer shares AOSP
  `FROZEN_OBJECT`'s `status_t` value, so an incoming frozen-object status is not
  mis-decoded.

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

[Unreleased]: https://github.com/hiking90/rsbinder/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/hiking90/rsbinder/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/hiking90/rsbinder/compare/v0.8.0...v0.9.0
