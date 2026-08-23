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

### Added

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
- **rsbinder-tools (`rsb_device`):** `--group` and `--mode` for the binder
  device node. Binder has no in-kernel access control of its own, so the node
  is the only gate on who may speak binder at all.

### Changed

- **rsbinder — breaking (internal representation):** a proxy's weak identity
  is now stamped into the `ProxyHandle` at construction instead of being looked
  up in the proxy cache on every `SIBinder::downgrade`. `WIBinder` no longer
  has a `Native` fallback for proxies, `downgrade` no longer takes the cache
  lock, and it no longer requires an initialized `ProcessState`. Two weak
  references naming the same `(handle, generation)` compare equal even when
  they come from different `Arc<ProxyHandle>` allocations — which was already
  the documented intent for case-(b) resurrection, and is now also true after
  the obituary.
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

### Fixed

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

### Changed

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

### Fixed

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
