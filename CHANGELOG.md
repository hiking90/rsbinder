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

## [0.12.0] - Unreleased

### Migrating from 0.11.0

- **Minimum supported Rust version is now 1.86** (was 1.85). The local-binder
  cast `Binder::<B>::try_from(SIBinder)` (behind `FromIBinder::try_from` and
  `SIBinder::into_interface`) now downcasts the `Arc` through trait upcasting
  instead of reinterpreting a raw pointer, which needs 1.86. Results are
  unchanged: a remote proxy, another `Remotable` type, or an `IBinder` wrapper
  that forwards `as_any()` to a local binder still fails with `BadValue`.
- **`Endpoint::Kernel` gained an `mmap_size` field** (see *Added*). It is not
  `#[non_exhaustive]`, so a struct literal or a `match` arm naming every field
  no longer compiles; add `mmap_size: None` for the previous behavior. Matching
  with `..` and endpoints obtained from `serve` / `Client::open` are unaffected.
- **A kernel option `serve` / `Client::open` cannot honor is now `BadValue`.**
  `binder://?threads=`, `?driver=`, `ServeOptions::threads` and
  `ClientOptions::driver` are fixed by whoever initializes `ProcessState`
  first; a later *different* value used to log a warning and now returns
  `StatusCode::BadValue` (omitting it or repeating the value still works).
  Likewise `ClientOptions::driver` / `mmap_size` must agree with the URI's
  `?driver=` / `?mmap=`.
- **An RPC session refuses a connection whose transport differs from its
  founding one** (fd passing or local peer) with `BadType` at attach. A manual
  attach (`add_{outgoing,incoming}_connection_with_config`) over such a
  transport is refused before its header goes out, so the session stays up; a
  hand-assembled session (e.g. `RpcServer::serve_connection`) can hit it too.
- **`StatusCode::from(ExceptionCode::ServiceSpecific)` is
  `ServiceSpecific(0)`, not `Ok`**, as in AOSP; `StatusCode::from(status)` for
  such a `Status` now yields `ServiceSpecific(0)`, not `FailedTransaction`.
- **On a host whose errno numbering is not Linux asm-generic (Apple
  platforms, and Linux on SPARC, MIPS, Alpha or PA-RISC) the status codes on
  the wire changed; upgrade every such peer together.** 0.12.0 sends AOSP's
  values (`utils/Errors.h` in Linux asm-generic errno numbering) on every host
  (see *Fixed*). Between a 0.11.0 and a 0.12.0 macOS peer six named codes
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
  `Unknown`** on every host (see *Fixed*), where it answered success (`0`) or
  a status the client decoded as another variant. `i32::from` and a
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
- **`FLAG_COLLECT_NOTED_APP_OPS` is `0x2`, not `0x80`** (see *Fixed*): a flag
  value sent on the wire changes, so a 0.11.0 peer testing or setting it
  disagrees with a 0.12.0 one; rebuild both sides.
- **`rsbinder-aidl` now rejects `.aidl` that AOSP's `aidl` also rejects** (see
  *Added*). An rsbinder-only contract relying on the missing validation now
  fails, with a diagnostic naming the rule.
- **`render::ConstMember` and `render::EnumMember` gained a trailing
  deprecation element**; pass `String::new()` to keep the previous output. The
  `#[non_exhaustive]` render structs' new `deprecated` field needs no change.
- **`error::SemanticError` is now `#[non_exhaustive]`** and gained
  `FixedSizeNonFixedField`, `VintfStabilityLeak` and `DirectionNotSpecified`;
  add a `_ => …` arm. `DirectionPrimitive` is replaced by `InvalidDirection`,
  which covers every refused direction and adds the argument name and the
  permitted directions; its `type_kind` is the AIDL type (`int`, `String`),
  not `"a primitive type"`.
- **A declaration nested in a `@VintfStability` type is now VINTF-stable** (see
  *Fixed*): a `ParcelableHolder` records VINTF stability for it, and
  `ParcelableHolder::set_parcelable` returns `BadValue` for a payload without
  VINTF stability. Nothing in the build warns.
- **`#[deprecated]` codegen can break a downstream `-D warnings` build**: an
  existing `/** @deprecated … */` javadoc now generates `#[deprecated]`. Drop
  the tag, or allow the lint at the call site.
- **`rsbinder-aidl`: a `@FixedSize` union's implicit `Tag` is `Tag(pub i8)`**
  (see *Fixed*): code that builds or reads it as `i32` no longer compiles,
  and a `Tag[]` is `byte[]` on the wire. A `@FixedSize` union with more than
  128 fields is rejected, because its `byte` tag cannot number the 129th;
  AOSP's `aidl` rejects it as well.
- **`rsbinder-aidl`: a `u32`/`u64` suffix on a hex literal no longer sets its
  width** (see *Fixed*), as in AOSP `ParseIntegral`: `0xFFFFFFFFu64` is the
  `int` `-1`, so a `long` constant set to it is `-1`, not `4294967295`, and
  `0x1FFFFFFFFu32` is a `long` instead of a parse error. Nothing in the build
  warns; a constant sent on the wire changes value.
- **`rsbinder-aidl`: a union whose first field is an enum without an
  initializer defaults to the enum's `Default` (backing value `0`)**, not to
  its first enumerator (see *Fixed*). For `enum E { A = 5, B = 6 }` and
  `union U { E e; … }`, `U::default()` is `U::E(E::default())`, which is
  `E(0)`, not `U::E(E::A)`; a default-constructed union sent as is carries
  `0` on the wire. An enum whose first enumerator is `0` is unaffected.
  Nothing in the build warns.
- **`rsbinder-aidl` / `rsbinder-macros`: fixed-size array arguments and returns
  have AOSP's element types** (see *Fixed*). A `@nullable T[N]` `in` argument
  or return value whose element is not a primitive or enum now wraps each
  element: `Option<&[Option<T>; N]>` and `Option<[Option<T>; N]>`, not
  `Option<&[T; N]>` and `Option<[T; N]>`. An `inout T[N]` of a binder,
  interface or `ParcelFileDescriptor` is `&mut [T; N]`, not
  `&mut [Option<T>; N]`. Service implementations and callers of such methods
  stop compiling until updated, and `#[rsbinder::interface]` refuses the old
  spellings and names the new one. The bytes on the wire do not change.
- **`rsbinder-aidl`: a union variant is its field's name with only the first
  letter uppercased** (see *Fixed*), as AOSP names it: `nullable_iface` is
  `Nullable_iface` (was `NullableIface`) and `URL` is `URL` (was `Url`). A
  single lowercase word (`ns` → `Ns`) or a lowerCamel name (`myField` →
  `MyField`) keeps the variant it had. Code naming any other variant stops
  compiling. The wire carries the tag number only, so it does not change.
- **`rsbinder-aidl`: one statement may hold at most 12 `<` that are followed
  by a name and not closed by a `>`** (see *Fixed*); 0.11.0 allowed 256. A
  comparison counts, so a constant such as `A < B || C < D || …` with 13 or
  more such comparisons is now rejected; split it across several constants
  (each `;` resets the count).
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
- **`#[rsbinder::interface]` refuses two interfaces in one module whose names
  share a stem** — `IFoo` and `Foo` both name `BnFoo`/`BpFoo` (AOSP
  `ClassName` drops a leading `I` before an uppercase letter). This failed
  with E0659 only once `BnFoo` or `BpFoo` was used; it now fails at the
  second declaration with E0428 on `__rsbinder_one_interface_per_stem_Foo`.
  Move one of the interfaces to another module or rename it.
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
  prefix** (see *Fixed*). An `.aidl` interface, parcelable, enum or union
  whose name starts with it, an interface trait so named, and a signature
  path whose first segment starts with it are now rejected; AOSP's `aidl`
  accepts such names. Rename the type, or reach it in a signature through a
  path such as `crate::…`.
- **The `tokio` feature no longer enables `tokio/full`**, only `tokio/rt` and
  `tokio/rt-multi-thread`. A crate that received `tokio/net`, `time`,
  `signal`, `fs`, `io-util` or `macros` through rsbinder must declare them in
  its own `Cargo.toml`.
- **`BinderAsyncRuntime for TokioRuntime<Runtime>` and
  `for TokioRuntime<Arc<Runtime>>` are gone** (an owned runtime in a binder
  panics in `Runtime::drop` when the last `Strong` drops on a worker). Wrap
  `runtime.handle().clone()` in `TokioRuntime<Handle>` instead.
- **A kernel transaction dispatched inside an RPC handler now reports the
  kernel caller**: `calling_caller()`, `get_calling_uid()` / `pid()` / `sid()`
  and `CallingContext::default()` return `Caller::Kernel` and the kernel
  sender's values, not the suspended RPC peer's. Re-check handlers that pick
  an authorization rule by `match`ing on the `Caller` arm.
- **Outside a transaction `get_calling_uid()` / `get_calling_pid()` return
  this process's own uid and pid** instead of `0` (root), as AOSP's
  `IPCThreadState` does. Inside a transaction nothing changes.
- **`ProcessState::set_call_restriction` now restricts the calling thread
  too** (see *Fixed*). A thread that had already made a binder call kept its
  previous value; it now takes the new one at once, so a twoway call it makes
  afterwards logs an error (`ErrorIfNotOneway`) or panics
  (`FatalIfNotOneway`). The main looper of `ServeOptions::call_restriction`
  is such a thread: a handler running on it that makes a twoway call now hits
  the restriction.
- **RPC `link_to_death` (and `death_signal`) is refused with
  `InvalidOperation` on a session that would not notice its connection
  dropping** (a client with no incoming connections and no serve loop). Open
  incoming connections (`incoming_connections(n)`), or call the new
  `RpcSession::spawn_serve` before linking; the server side is unaffected.
- **A `oneway` call made inside an RPC handler needs a connection this end
  opened** (see *Fixed*); without one it is `WouldBlock`, so a server's oneway
  callback needs the client's incoming connections. Twoway is unchanged.
- **An RPC session ends as a whole when any of its connections fails** (AOSP
  `RpcState::handleRpcError`). A send or receive failure, a cut frame or an
  expired deadline on one connection shuts every connection down: calls in
  flight on the others return `DeadObject`, every death recipient fires, and
  every local object the peer held is released. 0.11.0 retired the failing
  connection and let a fan-out session go on. After a session ends, reconnect,
  fetch the root and register callbacks again — `rsbinder::Reconnecting` (see
  *Added*) does this for one service. A client's incoming connection
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
- **`RpcServer::setup_unix_server` removes only a stale socket**: a regular
  file is refused with `AlreadyExists`, a listening socket with `EADDRINUSE`.
  Drop removes the socket file only while it is still the one bound. On Apple
  platforms it removes no socket at all and refuses every existing one with
  `EADDRINUSE`, as AOSP `setupUnixDomainServer` does, so delete a crashed
  server's socket before restarting: XNU refuses a connect to a live listener
  whose backlog is full with the same `ECONNREFUSED` a stale socket gives, so
  probing could remove a running server's path. 0.11.0 removed whatever was at
  the path on every platform.
- **A kernel `Client::get` / `connect` reports the service manager's own
  failure** instead of `NameNotFound` when it cannot be reached, and `hub`
  logs why once.
- **An RPC `Parcel` is sent once** (see *Fixed*): build a new request per
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
- **`ServerGuard` and `Server` are `#[must_use]`**: `serve(uri)?.spawn()?;`
  stopped an RPC server at once. Bind the guard to a named variable.
- **`rsbinder-aidl` parser API: `ParcelableDecl::type_params` and
  `UnionDecl::type_params` are `Vec<TypeParam>`** (name, span, annotations)
  instead of `Vec<String>`; read `TypeParam::name`. New `Generic::type_args()`
  returns a `Foo<A, B>` reference's arguments as written.
- **Several RPC client setup calls are deprecated** in favor of
  `RpcClientConfig` (see *Deprecated*); they still work, except for the
  session ids now refused (see above), but `-D warnings` flags them.
- **`rsb_hub` checks every directory above its configuration, not only the
  configuration itself** (see *Changed*). A group- or world-writable (sticky
  or not) or foreign-owned directory anywhere on the path refuses the load:
  one kept under `/tmp` no longer loads, nor does `~/rsbinder/hub.d` when
  `~/rsbinder` was made `0775` under umask `002`. For an unprivileged
  `rsb_hub`, move it under `$XDG_RUNTIME_DIR` or `$HOME`; a root `rsb_hub`
  needs a root-owned path such as `/etc/rsbinder/hub.d`. Either way
  `chmod g-w,o-w` the directories on the way.
  `rsbinder_tools::config::check_path` is removed: the check now runs on
  held descriptors, and a path-based probe would report a different inode
  than the one read.
- **`rsbinder_tools::nss::gids_for_uid` returns `Option<BTreeSet<u32>>`**:
  `None` is a failed lookup, not an unknown uid (that is `Some` of an empty
  set). `nss::GroupCache` and `config::Enforcer` hold a boxed resolver and are
  no longer `UnwindSafe` / `RefUnwindSafe`; wrap them in `AssertUnwindSafe`
  where `catch_unwind` needs it.
- **`rsb_hub` refuses configurations 0.11.0 loaded**: it exits 1 at start,
  and a SIGHUP reload keeps the policy in force. A `[[service]]` name must be
  `interface/instance` with both halves non-empty, within `addService`'s name
  rule (at most 127 bytes of `[A-Za-z0-9._/-]`); a bare interface name, which
  0.11.0 read as `<name>/default`, is refused. A `systemd` unit must not be
  empty, an `exec` program must be an absolute path, and `connection.port`
  must be 1–65535 (`ConfigError::Toml` otherwise). An `exec` program must
  exist and pass the check the configuration itself gets (see *Security*) —
  the program and every directory on the way to it from `/` owned by root or
  `rsb_hub`'s uid and not group- or world-writable — so a program under `/tmp`
  or under a group-writable directory is refused. `--config` must name a
  regular file or a directory; in a directory, a `*.toml` entry that cannot be
  opened (a dangling symlink) fails the load, where 0.11.0 skipped it, and a
  file whose name starts with `.` (an editor lock or backup file) is no
  longer read.
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
- **`rsb_service` exits 2, not 1, when the device has no service manager**:
  0.11.0 `check` and `dump` reported every name as not registered there.
  **`rsb_device` refuses a device name over 255 bytes or naming a binderfs
  entry** (`binder-control`, `features`, `binder_logs`) before it mounts
  anything; 0.11.0 applied `--group` / `--mode` to that entry.
- **`Parcel` is no longer `RefUnwindSafe` without the `rpc` feature**: a
  kernel parcel now holds the binder proxies written into it (see *Fixed*).
  With `rpc` it already was not. Wrap a `&Parcel` in `AssertUnwindSafe`
  where `catch_unwind` needs it.
- **A parcel write over a binder or fd object it recorded is
  `PermissionDenied`** (see *Fixed*): rewinding with `set_data_position` and
  writing over an object fails where 0.11.0 overwrote it.
- **`Parcel::from_ipc_parts` has one more `# Safety` condition**: the handle
  of every `BINDER_TYPE_FD` object at an offset in `objects` must be an open
  file descriptor that stays open until the parcel drops. Reading such an
  object duplicates that fd, as 0.11.0 already did, and the parcel closes
  none of them; the signature and behavior are unchanged. Under the 0.11.0
  conditions a buffer naming a closed fd number was allowed, and a read then
  duplicated whatever fd that number named by then.
- **`Parcel::__set_for_rpc` (`rpc` + `test-util`) returns `Result<()>`** and
  refuses, with `InvalidOperation` and the mode left unchanged, a parcel that
  holds bytes or was built over a received buffer (see *Fixed*). Call it on
  a new parcel before writing to it.
- **Over kernel binder, a service's `on_dump` writes to a duplicate of the
  caller's fd**, not to the received fd itself (see *Fixed*). The duplicate
  is made before the arguments are read; a process out of file descriptors
  now answers the `DUMP_TRANSACTION` with that error instead of running
  `on_dump`. Over RPC, `on_dump` now runs as well (see *Fixed*).
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
- **`get_calling_sid()` is `None` where SELinux is not available** (see
  *Fixed*): on Linux without a mounted selinuxfs, Smack and AppArmor hosts
  included, a binder with `BinderFeatures::set_requesting_sid` no longer asks
  the driver for the caller's context. Under Smack or AppArmor 0.11.0 returned
  the label, read on to the next NUL byte.
- **Android builds must link for API 29 (Android 10, the oldest supported
  platform) or newer: pass `--platform 29` to `cargo ndk`.** rsbinder now
  calls bionic's `process_vm_readv`, which cargo-ndk's default API 21 does
  not export, so a build without it fails with `undefined symbol:
  process_vm_readv`. 0.11.0 linked at API 21.

### Added

- **`rsbinder::Reconnecting`** (`rsbinder::reconnect`): a handle to one
  service, named by a URI, that looks it up again when its process dies or
  its RPC session ends, runs an `on_connect` hook (register callbacks there)
  and hands the new proxy to callers. Calls go through `with`, which runs the
  closure once and never resends it; a call waits only for a reconnect
  attempt in progress (`ReconnectPolicy::call_wait`). `build` fails only on
  what a retry cannot fix, so a client may start before its server;
  `wait_connected` and, with `tokio`, `connected().await` wait for a
  connection. Kernel binder and every RPC transport; works with the root
  object of a libbinder `RpcServer`. See the book's "Reconnecting to a
  Service".
- **`RpcSession::is_ended`**: whether a session has ended. After a failed
  call this, not the status code, says whether to reconnect.
- **`RpcTransport::peer_closed` and `TlsStream::peer_closed`**: whether the
  peer closed the connection, from a zero-timeout poll that reads nothing
  (`POLLRDHUP` on Linux and Android; `POLLHUP` on Apple platforms, and for
  Unix-domain sockets elsewhere). Defaults to `None` (unknown); the bundled
  socket transports implement it.
- **Generic parcelables** (`rsbinder-aidl`): `parcelable Foo<T, U> { … }`
  generates `pub struct Foo<T, U>` with a `_phantom_T: PhantomData` field per
  parameter, as AOSP does; its `Default` and `Debug` put no bound on the
  parameters (AOSP derives `Debug`, which requires `T: Debug`). Parameter
  requirements (`MQDescriptor<@FixedSize T, Flavor>`) are checked at each use
  site; shapes with no Rust or AOSP form (a field typed as a parameter, a
  generic `union`, reserved names, …) are rejected.
  `impl_{serialize,deserialize}_for_parcelable!` accept `Foo<T, U>`.
- **Builtin `android.hardware.common` types**: importing
  `android.hardware.common.fmq.{MQDescriptor, GrantorDescriptor,
  SynchronizedReadWrite, UnsynchronizedWrite}` or
  `android.hardware.common.NativeHandle` needs no vendored copy; the generated
  code names `rsbinder::fmq::*` / `rsbinder::NativeHandle`.
- **`rsbinder::fmq`**: the `MQDescriptor` family, `rsbinder-fmq` re-exported,
  `TryFrom` conversions between `Descriptor` and `MQDescriptor<T, F>`, and
  `StatusCode: From<rsbinder_fmq::Error>`, on every platform; making or
  attaching a queue works on Linux and Android.
- **New crate `rsbinder-fmq`**: Android's Fast Message Queue (`libfmq`) in
  Rust, wire-compatible and independent of binder: `MessageQueue<T>::create`
  (sealed memfd), `descriptor()`, `attach` with an `AttachPolicy`,
  `write_blocking` / `read_blocking` and `EventFlag`. Synchronized flavor
  only; Linux and Android. Validated against libfmq on an Android emulator.
  For a writer that keeps to the one-writer rule,
  `begin_write_cached` / `commit_write_cached` take the write position from
  the writer's own view and reload the read counter only when the view shows
  no room (each reload checked); a peer that rewrites the write counter is no
  longer detected by that writer. `EventFlag::wake_lazy` skips the write to
  the word when the bits already stand; `wait` fences after consuming its
  bits so a lazy waker's counter store is seen (loom model in
  `tests/loom_event_flag.rs`).
  Every access to the shared memory — counters, ring and EventFlag word — is
  a Rust atomic (ring copies are relaxed `AtomicU8` / `AtomicUsize` loads and
  stores), so a second writer or reader, whether the peer, another handle in
  this process or C code on the same memory, makes the counters fail their
  check (`Error::Corrupted`) or the elements wrong, never undefined behavior.
  As with libfmq, nothing detects it: keeping to one writer and one reader is
  the callers' protocol. `attach`, and `rsbfmq_attach` in the C header below,
  refuse overlapping grantors at the same fd index or on two non-ashmem fds
  naming the same file (`st_dev`/`st_ino`, as a `dup` does); this is a
  consistency check on the descriptor, not a safety boundary.
- **C reference headers for an NDK peer**, since an NDK app cannot link
  `libfmq`: `rsbinder-fmq/c/rsbinder_fmq.h` (the FMQ) and
  `contrib/ndk/rsbinder_stream.h` (the kernel-binder stream's record layer,
  `rsbs_*`), with C++ templates for the NDK `MQDescriptor` and
  `StreamEndpoint`. On 32-bit glibc the queue header needs a 64-bit `off_t`:
  compile with `-D_FILE_OFFSET_BITS=64` (it stops with `#error` otherwise);
  64-bit glibc, musl and the NDK need no flag.
- **Streaming with back-pressure** (`rsbinder::stream`): `Sink<T>` and
  `Receiver<T>`. `Receiver::new(&peer)` yields a `StreamEndpoint<T>` passed
  in the call that opens the stream; the service calls `Sink::open(&endpoint)`.
  The `.aidl` names the item type as the endpoint's type argument
  (`in StreamEndpoint<LogLine>`), so a consumer or producer of another type
  does not compile; the argument is not on the wire, and
  `StreamEndpoint::cast` lets a Rust end use a type of its own that encodes
  alike (a handler holding `&StreamEndpoint<T>` casts
  `endpoint.try_clone()?`). `Sink::open_borrowed` opens a sink of the item
  type's borrowed form — `Sink<str>` from a `StreamEndpoint<String>`.
  Kernel binder uses an FMQ ring the consumer allocates; RPC uses the
  `oneway` `IStreamSink` / `IStreamSource` with credits. On the ring the
  consumer copies every committed record out at once and frees the space
  with one wake (so the producer runs ahead of what `recv` returned by under
  twice `ring_bytes`, and the consumer's copy buffer grows to at most the
  ring's size, plus a decode buffer kept at the largest item's size), and
  both ends look at the ring for up to 20 µs before parking on its futex
  (not on a single core, never on an async executor thread; no more than
  half the cores' worth of threads in a process look past a first short
  round at once; an end whose last 64 looks all found nothing looks in full
  on one wait in eight, until such a look finds something or a wait it
  parked for without looking ends within 20 µs, so a stream whose items come
  further apart pays the look on one item in eight); each `send` still puts
  its item in the ring. Configured through
  `SinkPolicy` (incl. `send_timeout`) and `ReceiverPolicy`; async variants
  with `tokio`. On RPC a waiting end pings a peer it has not heard from for a
  third of the session's reply deadline (`PingPolicy`, twoway
  `PING_TRANSACTION`), which catches a peer lost behind a TCP relay. A
  consumer may put an RPC stream on a ring too (`ReceiverPolicy::ring_use =
  RingUse::AlsoUnixRpc`) when the session is a Unix socket on the same host
  that passes fds; otherwise it runs on calls, and `Receiver::uses_ring` tells
  which. A session without incoming connections refuses a stream either way. Such a ring pings like the calls path
  and counts its traffic toward the server's idle timeout. A ring endpoint's
  sink that receives `onStart` or `onBatch` ends the stream with
  `EX_ILLEGAL_STATE` rather than drop the items. See the Streaming chapter of
  the book.
- **`rpc::EndReason::SessionEnded`**: a serve loop found its session already
  ended — by another connection's fault, a reply deadline or `close_session`.
- **`RpcTransport::set_liveness` and `TlsStream::set_liveness`**: the kernel's
  peer-liveness check (keepalive, `TCP_USER_TIMEOUT`) sized to the session
  timeout. The default is a no-op; `tcp_debug` and TLS over `TcpStream`
  implement it, and a custom transport overrides it to take part.
- **Work source API**: `set_calling_work_source_uid`,
  `get_calling_work_source_uid`, `clear_calling_work_source`,
  `restore_calling_work_source`, `clear_propagate_work_source` and
  `should_propagate_work_source`. The work source is per-thread, as in AOSP,
  and outgoing kernel calls now carry it (RPC has no field for it).
- **Transaction names** (AOSP `aidl --trace`): `Builder::trace(true)` emits a
  method-name table answered through the new defaulted
  `Remotable::transaction_name(code)`; `declare_binder_interface!` takes an
  optional `function_names: [..]`.
- **Transaction observers** (`rsbinder::observe`): `set_observer` installs a
  process-wide `TransactionObserver` called around every incoming
  transaction (kernel and RPC) with a `TxnContext`. Provided:
  `observe::LogObserver` and `observe::StatsObserver` (`snapshot()`).
  `Transactable` gained a defaulted `transaction_name`.
- **`tracing` feature** (off by default): `aidl` spans named
  `AIDL::rust::<descriptor>::<method>::server|client`, as AOSP's ATrace;
  `observe::TracingObserver` and `Builder::trace(true)` proxies open them.
- **`rpc::RpcClientConfig`, one android-13+ client config for every
  transport** (`unix`, `unix_abstract`, `vsock`, `tls`, `tcp_debug`, `new`).
  `ClientOptions::incoming_connections` / `outgoing_connections` now work on
  `vsock://` and `tls://` too. New
  `RpcSession::add_{outgoing,incoming}_connection_with_config` work over any
  transport and refuse session-wide settings (`timeout`) with `BadValue`.
  `FileDescriptorTransportMode::Unix` on a transport that cannot pass fds is
  now `BadValue` at setup.
- **`ParcelFileDescriptor::pipe()`**, plus `Read` and `Write` for
  `ParcelFileDescriptor` and `&ParcelFileDescriptor`. Both ends are
  `O_CLOEXEC`. A pipe holds about 64 KB, so a writer needs a reader draining it.
- **`Parcel::write_blob` / `read_blob`**: AOSP's large-payload convention,
  inline up to `BLOB_INPLACE_LIMIT` (16 KB), otherwise in a memfd; compatible
  with Java's `Parcel.writeBlob`. Inline whenever fds cannot travel (see the
  now public `Parcel::allow_fds()`) or the blob cannot be sealed (Linux < 5.1).
- **Cooperative cancellation**: `cancel::CancellationSignal`,
  `CancellationToken` and `cancel_remote`, over AOSP's
  `android.os.ICancellationSignal`, interoperable with framework peers.
  Delivery is best-effort and unordered (see the module docs).
- **Service-specific errors can be a type instead of an `i32`**:
  `ServiceSpecificError`, `Status::service_specific`,
  `Status::service_error::<T>()`, `#[derive(ServiceSpecificError)]` and
  `impl_service_specific_error!` (for `.aidl` enums). The wire is unchanged.
- **`Status::message()`**: the message a peer attached.
- **The receive mapping is now configurable**:
  `ProcessState::init_with_mmap_size`, `binder://?mmap=<bytes>`,
  `ClientOptions::mmap_size`, `MAX_BINDER_MMAP_SIZE` (4 MB),
  `ProcessState::default_mmap_size()` / `mmap_size()`. Raising it lets a
  service accept larger transactions. Out of range, or different from the
  size in force (incl. `ServeOptions::mmap_size`), is `BadValue`.
- **`TransportCaps`**: `FD_PASSING`, `TRUSTED_UID`, `CALLBACKS`, `SAME_HOST`,
  `KERNEL_KNOBS`, read from `Client::caps()`, `RpcSession::caps()`,
  `Endpoint::static_caps()` or `calling_caps()`; `caps.require(..)` fails
  early with `InvalidOperation`. A summary; underlying checks still apply.
- **`wait_for_interface_async` and `check_interface_async`** (`tokio`):
  dropping the future ends the wait.
- **`death_signal` / `DeathSignal`** (`tokio`): await a binder's death, over
  kernel binder and RPC; dropping the signal unlinks. The `tokio` feature now
  also enables `tokio/sync`.
- **`rsbinder-aidl`: AIDL validation matching AOSP.** Now rejected with a
  diagnostic: a non-fixed-size field in a `@FixedSize` type; a non-VINTF
  reference from a `@VintfStability` type (a `@RustOnlyStableParcelable` is
  exempt); misplaced `ParcelableHolder` or `void`; an empty `union`; a
  `@FixedSize` union with more than 128 fields; duplicate names of any kind,
  including a type defined in two input files (a nested type or a union's
  implicit `Tag` counts, so nested `a.b.c` beside a top-level `a.b.c` is
  refused) and union fields whose names
  differ only in the first letter's case; a type argument on a non-generic
  type; a `const` not of a primitive, `String` or array of those; a
  non-decimal transaction code (`1_0`, `10L`); an argument direction the type
  does not permit, or none where it permits more than `in` (see *Migrating*).
  Any argument name is accepted.
- **`@deprecated` support.** `rsbinder-aidl` emits a `/** @deprecated note */`
  javadoc as `#[deprecated = "note"]` (AOSP `FindDeprecated` rules), and
  `rsbinder-macros` carries `#[deprecated]` through as AIDL `@deprecated`
  (`since` / `note = …` are refused). `render::deprecated_attr` is public.
- **`rsbinder-macros`: the signature checks cover every out/inout and `in`
  array shape `.aidl` renders.**
- **rsbinder-tools: `config::is_valid_service_name`**, the `addService` name
  rule that `[[service]]` names are checked against. `config::Config` and
  `config::FileContents`, which `load` and `parse_file` return, are now
  re-exported.

### Changed

- **A kernel proxy's cache entry and its `BC_INCREFS` reference are released
  when the last `SIBinder` and the last `WIBinder` for the handle are gone**,
  as AOSP releases them in `~BpBinder` (`expungeHandle` + `decWeakHandle`).
  In 0.11.0 they stayed until the remote died, so a process that looked up
  many short-lived remote objects kept a cache entry and a kernel weak
  reference for each. A live `WIBinder` keeps the entry, so re-receiving the
  handle reuses its generation and descriptor and the new proxy compares
  equal to that `WIBinder`; the proxy is still a new allocation, so that
  `WIBinder`'s `upgrade()` keeps returning `Err(DeadObject)` (AOSP revives
  the same `BpBinder`, so its `wp::promote()` succeeds). Once nothing holds
  the entry, the next lookup builds a proxy under a new generation (one
  `INTERFACE_TRANSACTION`, as a new `BpBinder` does). The last strong drop clears a registered death
  notification before `BC_RELEASE`, as AOSP `onLastStrongRef` does.
- **`to_bytes` / `from_bytes` no longer require the `rpc` feature.** Binders
  (`BadType`) and file descriptors (`FdsNotAllowed`) are still refused on
  write and read, and `Parcel::allow_fds` is `false` on such a parcel. No wire
  or signature change.
- **rsbinder-tools (`rsb_hub`):** the configuration trust check, which in
  0.11.0 looked at the configuration path and its files only, walks from `/`
  one `openat(O_NOFOLLOW)` per component, checks every directory on the way
  and follows symlinks itself (the directory holding one is checked too), and
  reads or lists the final inode through the descriptor it checked, so a
  rename or symlink swap between check and read changes nothing. A group- or
  world-writable (sticky or not) or foreign-owned directory on the way is
  refused, so `/tmp` does not qualify.
- **rsbinder-tools (`rsb_hub`):** a caller denied by a group rule has its
  groups re-read, at most once every 15 s per uid
  (`GroupCache::MIN_REVALIDATE`), and is judged again. glibc returns a subset
  of the groups as a success when one backend (e.g. sssd) is down, and that
  subset used to be held until SIGHUP; a failed lookup was retried on every
  transaction and is now held as an empty set under the same limit. A re-read
  is merged into the cached set and never shrinks it, so a removed group
  still takes effect only at SIGHUP. `nss::gids_for_uid` returns `Option` and
  documents that its `Some` may be a subset. New API:
  `GroupCache::{with_resolver, revalidate, MIN_REVALIDATE}`, `nss::Resolver`,
  `Policy::group_could_grant`. An `_r` lookup's errno is now always a failure
  on glibc/BSD (`ENOENT`, `ESRCH`, `EBADF`, `EPERM` used to read as
  not-found, so a missing `/etc/passwd` looked like an unknown user).
- **Fewer copies and allocations per transaction.** A `String16` is written
  and read without an intermediate `Vec<u16>`, and the interface token is
  compared in place (kernel `check_interface` and the RPC token). A oneway
  transaction no longer allocates a reply buffer. An RPC send encodes the
  frame from the parcel's bytes instead of copying them into it first.
  `to_bytes` hands back the parcel's own buffer when that is at most twice
  the value's size. A stream batch decodes from the bytes it received without
  copying them. Over a kernel-binder ring, a stream producer encodes each item
  into a buffer it keeps from item to item, and the consumer decodes every
  item through one parcel it keeps, where each side used to allocate per item.
  An item `send_async` hands to the pool on a full ring takes that buffer
  with it, so the producer allocates a new one for the next item.
  An android-13+ RPC message's 16-byte header is read into a
  stack buffer instead of a heap allocation of its own. No wire or signature
  change.
- **RPC (r34 framing over Unix sockets, and `tcp_debug`): frames move with
  fewer copies.** A frame goes out as its length and its body in one
  `sendmsg`, without first joining them into a new buffer. A connection in fd
  mode reads each frame's header and then its body straight into the frame,
  where it used to read through an 8 KiB scratch buffer zeroed before every
  read and copy the frame out of it. No wire change.
- **`@EnforcePermission` over kernel binder costs one IPC per check, not
  two.** `check_permission` keeps the `IPermissionController` that answered
  last, as AOSP libbinder keeps `gPermissionController`, instead of looking
  `"permission"` up in the service manager on every guarded call. A kept
  controller that fails is dropped and the check goes once to a fresh
  lookup, so a `system_server` restart does not deny the next check.
- **rsbinder-tools (`rsb_hub`):** a policy check no longer copies the
  caller's group set, and the `listServices`, `dump`, `getDeclaredInstances`
  and `getServiceDebugInfo` filters read the caller once per request.
- **rsbinder-tools (`rsb_hub`):** on-demand start is debounced per
  activation as well as per name: declarations sharing one `exec` argv or
  `systemd` unit start it once while that start is outstanding (init runs one
  service for several interfaces). An `exec` start is outstanding for the
  process's lifetime, a `systemd` one until `systemctl --no-block start`
  returns. In 0.11.0 each name started its own.

The behavior changes an existing program can observe are listed under
*Migrating from 0.11.0* above.

### Deprecated

- **`rpc::RpcUnixClientConfig`** → `RpcClientConfig::unix` / `unix_abstract`.
- **`RpcSession::setup_unix_client_android13plus_with_config`** →
  `setup_client_android13plus_with_config` with a `RpcClientConfig`.
- **`RpcSession::setup_unix_client_android13plus_with_id`** →
  `setup_client_android13plus_with_config(RpcClientConfig::unix(path, v))`
  for a new session, then `add_outgoing_connection_with_config` with
  `.session_id(&session.get_session_id()?)` on that session to add a
  connection; a non-empty id is refused (see *Migrating*). **`_fan_out`** →
  the same setup call with `.outgoing_connections(n)`.
- **`RpcSession::connect_android13plus_fd_with_id`** →
  `connect_android13plus_fd`; a non-empty id is refused (see *Migrating*).
- **`ClientOptions::session_id`** → open a new session, then
  `RpcSession::add_outgoing_connection_with_config` on it; a non-empty id is
  refused (see *Migrating*).
- **`RpcSession::add_outgoing_connection_android13plus`** and both
  **`add_{outgoing,incoming}_connection_android13plus_with_config`** →
  `add_{outgoing,incoming}_connection_with_config`; an id other than the
  session's own is refused (see *Migrating*).
- **`RpcClientConfig::handshake_timeout`** and
  **`ClientOptions::handshake_timeout`** → `timeout`, which now bounds each
  connect and handshake step. A value still set takes precedence for the
  handshake.
- **`rpc::EndReason::Unreadable`** and **`EndReason::Retired`** are no longer
  produced: a connection fault ends the whole session, and a serve loop reports
  `SessionEnded`.

The deprecated items still work and delegate to the new ones, except that the
setup calls taking a session id refuse a non-empty one and the attach calls
refuse one other than the session's own; they are removed in the release
after 0.12.0. The single-connection one-liners
(`setup_unix_client_android13plus`, `_abstract`, `_fd`,
`setup_tcp_client_tls_android13plus`) are not deprecated.

### Fixed

- **`rsbinder-aidl`: an AIDL item named `Ok`, `Err`, `Some`, `None` or
  `Default` no longer breaks the generated code.** Such a constant or nested
  type is an item of the generated module and shadows the std prelude there,
  so a union's `read_from_parcel`, an `@EnforcePermission` check, a field
  default or an `impl Default` failed to compile (E0618, E0404, E0425). The
  generated code now names these by path (`::core::result::Result::Ok`,
  `::core::default::Default`, …). A nested type named `Option`, `Vec`, `Box`
  or `String` still shadows the field types that use those names.
- **`rsbinder-aidl`: a bidirectional control character (U+202A–U+202E,
  U+2066–U+2069) no longer reaches a generated string literal raw**, where
  rustc's deny-by-default `text_direction_codepoint_in_literal` failed the
  user's crate with no AIDL diagnostic. `Builder::hash` and `@deprecated`
  notes escape it; an `@Descriptor` value holding one is refused with a
  diagnostic.
- **RPC TLS: a send that rustls stopped accepting after part of the frame
  went out now ends the session.** It returned `Protocol`, which means
  nothing was sent and keeps the session, so the peer kept a partial frame.
- **RPC on macOS: a call or send whose peer closed while it waited (with
  `set_timeout` set) returned `BadValue` instead of `DeadObject`.** XNU
  refuses `SO_RCVTIMEO` on a socket shut in both directions; the deadline is
  now skipped on a closed peer and the call ends with `DeadObject`.
- **RPC: a transaction made inside a reply wait on the same connection**,
  such as one from a `Drop` that a `DEC_STRONG` released, no longer clears
  the outer call's `set_timeout` reply deadline. Each deadline guard now
  restores the deadline it replaced.
- **RPC `unix` transport: `send_raw_draining` and `send_raw_with_fds` no
  longer count the 16-byte `RpcWireHeader` against `MAX_FRAME_LEN`**, so a
  `MAX_FRAME_LEN` body is accepted as on the other transports.
- **`rsbinder-aidl`: a union's implicit `Tag` has its enumerators**, one per
  field in order (AOSP `UnionTagGenerater`), so `U.Tag.b` resolves as a field
  default and in a constant expression (`const int K = U.Tag.b;` is `1`); it
  was "unresolved".
- **`FLAG_COLLECT_NOTED_APP_OPS` is `0x2`**, the value of AOSP
  `IBinder.java`, which Java `Binder.execTransactInternal` tests. It was
  `0x80`, which a Java service ignored and which the android17-6.18 kernel
  UAPI assigns to `TF_DEFER_COMPLETE`.
- **A parcel refuses a write that overlaps an object it recorded** with
  `PermissionDenied`, as AOSP `Parcel::validateReadData` does (for an RPC
  parcel, android-17 `validateRpcReadData`). A `set_data_position` followed by
  `write` (or a second object at the same offset) could rewrite a kernel
  `ParcelFileDescriptor`'s fd number, and dropping the parcel then closed an
  fd it did not own, or one fd twice. In an RPC parcel it could rewrite a
  binder's address or an fd's table index, so the peer received another
  object than the one this end had counted as sent.
- **A kernel parcel closes the file descriptors it holds, not the ones its
  bytes name.** A parcel now keeps each fd it will close as an owned value:
  the duplicate it took when the fd was written (a `ParcelFileDescriptor`, a
  blob, an `IMemoryHeap` reply), the fd `ProxyHandle::dump` takes over from
  its caller, the duplicate `append_from` took (a `ParcelableHolder`
  payload), or the fd the binder driver installed for a received
  transaction or reply. It closes exactly those when it drops, before the
  buffer goes back to the driver, as AOSP `Parcel::freeDataNoInit` does.
  0.11.0 rebuilt each fd from the object bytes when the parcel dropped, so a
  write that changed those bytes could close an fd the parcel did not own, or
  one fd twice, and a received parcel switched to RPC mode with
  `__set_for_rpc` left its fds open; that switch, like any on a parcel
  holding bytes, is now refused (see *Migrating*).
  Over kernel binder, `on_dump` writes to a duplicate of the received fd,
  closed when `on_dump` returns. A parcel built by the public
  `from_ipc_parts` closes none of the fds it names, as in 0.11.0.
- **Kernel binder on a big-endian host: a written fd or handle object names
  it.** The object stored the value through the union's 8-byte `binder`
  member, which puts it where the 4-byte `handle` lies only on a
  little-endian host; on a big-endian one the driver read handle or fd `0`.
  The union is now zeroed and the value written to `handle`, as AOSP
  `obj.binder = 0; obj.handle = ...;` does. Little-endian bytes are
  unchanged.
  `append_from` reads each object it copies from the source, not from the
  copy: with a `from_ipc_parts` source whose objects overlap, 0.11.0 dup'd
  the fd number its own rewrite of the previous object had left in the copy.
- **Kernel binder: `get_calling_sid()` no longer reads a freed transaction
  buffer.** The SELinux context is copied when the transaction arrives; a
  handler that took and dropped its incoming `Parcel` made `get_calling_sid()`,
  `calling_caller()` and `CallingContext::default()` read freed memory. The
  sid now stays set, like the uid and pid, until the reply is sent. The
  kernel delivers neither the context's length nor where its buffer ends,
  and only SELinux counts a NUL in the bytes it copies, so a sid is now kept
  only where SELinux is available (Android, or Linux with a mounted
  selinuxfs). Under Smack or AppArmor `get_calling_sid()` is `None`; 0.11.0
  returned the label run on into the bytes after it, up to the next NUL. The
  copy is made with `process_vm_readv(2)` on the process's own pid: a load
  from a receive-mapping page the driver had not installed raised `SIGBUS`,
  while the syscall reports such a page as `EFAULT`, and the sid is then
  `None`. Each transaction to a binder that requested the context costs a
  `getpid(2)` and one `process_vm_readv` per page the context touches, and
  `process_vm_readv` must pass the process's seccomp filter: a filter that
  returns an errno for it makes the sid `None`, one that kills or traps on it
  ends the thread or the process on the first such transaction.
- **`BinderFeatures::set_requesting_sid` requests the caller's security
  context only where SELinux is available** (Android, or Linux with
  `/sys/fs/selinux/enforce`), the check that already decides whether
  `get_calling_sid()` keeps a context. Elsewhere the binder is published
  without `FLAT_BINDER_FLAG_TXN_SECURITY_CTX` and the sid is `None`. With no
  LSM that can produce a context, the driver fails every transaction to a
  binder that requests one with `BR_FAILED_REPLY`, so in 0.11.0 every call to
  such a service on such a host failed with `FailedTransaction`,
  including the service manager's own calls made while registering it.
- **A `DUMP_TRANSACTION` that arrives over RPC runs `on_dump`.** The fd is
  read as AOSP `BBinder::onTransact` reads it (`readFileDescriptor`), from the
  session's fd table on a `Unix` fd-mode session; 0.11.0 read only a kernel
  fd object and answered `BadType` before `on_dump` ran. A session without an
  fd mode answers `FdsNotAllowed`.
- **`MappedHeap::from_fd` (and so `BpMemoryHeap::map`) refuses an ashmem
  range past the region** with `BadValue`; it mapped the range, and the first
  access raised `SIGBUS`.
- **A thread with `CallRestriction::FatalIfNotOneway` no longer panics when it
  reads a binder proxy new to the process.** The interface lookup rsbinder
  makes on first sight is its own twoway call, and now runs without the
  restriction, as AOSP runs its ping in `getStrongProxyForHandle`.
- **`ProcessState::set_call_restriction` now applies to the calling thread.**
  It set only the process value, which a thread copies on its first binder
  call, so a thread that had already used binder kept the old restriction:
  `serve(..).add(..)` made that call, and the main looper of
  `ServeOptions::call_restriction` ran unrestricted. Other threads that
  already used binder still keep their value; set it before the pool starts.
- **Reading a `ParcelableHolder` from a kernel parcel whose objects were
  written out of offset order** no longer underflows an offset (a panic in a
  debug build, a failed read in release); an object outside the payload is
  skipped.
- **On a host whose errno numbering is not Linux asm-generic a status goes on
  the wire in the numbering AOSP peers use**, `utils/Errors.h` over Linux
  asm-generic errno, so such a peer and a Linux or Android peer read the same
  code. The rule follows the host's errno values, checked at compile time,
  not its OS: it covers Apple platforms and Linux on SPARC, MIPS, Alpha and
  PA-RISC. On Apple platforms the six named codes defined by an errno
  Darwin numbers differently (`InvalidOperation`, `UnknownTransaction`,
  `BadIndex`, `NotEnoughData`, `WouldBlock`, `TimedOut`) went out as `-78`,
  `-94`, `-84`, `-96`, `-35` and `-60`, which a Linux or Android peer decoded
  as `Errno`; they are now `-38`, `-74`, `-75`, `-61`, `-11` and `-110` on
  every host. On such a host a `StatusCode::Errno` (a host errno with no named
  variant) is now sent as `UNKNOWN_ERROR`: it went out in the host's
  numbering, so a Linux or Android peer read Darwin `ECONNREFUSED` (61) as
  `NotEnoughData`, and `ENOTSOCK` and `EDEADLK` as `InvalidOperation` and
  `WouldBlock`. A received negative status with no named variant decodes as
  `Unknown` there, as AOSP's NDK (`PruneStatusT`) and Rust backends decode it
  on every host. A host with asm-generic numbering (Linux and Android on every
  other architecture) keeps passing such a status through as `Errno`, as AOSP
  C++ libbinder does.
- **A transaction status never encodes a payload as `0`.** `i32::from` sends
  `StatusCode::Errno(x)` with `x >= 0` and `StatusCode::ServiceSpecific(v)`
  with `v <= 0` as `UNKNOWN_ERROR` on every host. `Errno(0)` and
  `ServiceSpecific(0)` (what
  `StatusCode::from(ExceptionCode::ServiceSpecific)` returns) went out as `0`,
  which the peer read as success; a positive `Errno` arrived as
  `ServiceSpecific`, and a negative `ServiceSpecific` as a named code or
  `Errno`. The error code inside a `Status` with
  `ExceptionCode::ServiceSpecific` is AOSP's `int32` and is still written
  unchanged, whatever its sign.
- **`StatusCode::from(std::io::Error)` no longer panics on errno `0` or a value
  outside the OS's errno range**: where rustix uses its linux_raw backend
  (Linux on x86, x86_64, arm, aarch64 and riscv64), `Errno::from_raw_os_error`
  asserts `1..4096`, which an `io::Error` built from errno `0` failed. Such an
  error is now `Unknown`, as is a zero or negative `rustix::io::Errno`. A
  libc-backed rustix (Android, Apple, Linux on other architectures) never
  panicked here except on `i32::MIN` in a debug build, which is now `Unknown`
  too; it keeps an errno above `4095` as `Errno`.
- **`rsb_hub`: a `dump` that fails writing to the caller's fd answers `OK`**
  and logs the failure, as AOSP's `BBinder::dump` (which servicemanager does
  not override) answers. It answered with the positive errno, which the
  client read as `ServiceSpecific(32)`.
- **`rsb_hub`: a `systemd` unit is passed to `systemctl` after `--`**, so a
  unit name starting with `-` is started as a unit, not read as an option.
- **`get_extended_error()` returns `InvalidOperation` on a driver without
  `BINDER_GET_EXTENDED_ERROR`** as documented: such a driver answers
  `EINVAL`, which surfaced as `BadValue`.
- **RPC: a send to a peer that has closed is an error, not `SIGPIPE`, on the
  bundled Unix-socket, TLS and `tcp_debug` paths.** On Linux and Android
  every send over a Unix-domain socket (r34 frames, android-13+ messages,
  and the `sendmsg` calls that carry fds) and TLS over a `UnixStream` went
  out through `write(2)` or a flagless `sendmsg`; they now pass
  `MSG_NOSIGNAL`, as std's own TCP `send` does. Such a send killed a process
  whose runtime does not ignore `SIGPIPE` (a C or JNI host). On Apple
  platforms, which have no `MSG_NOSIGNAL` flag in rustix, `SO_NOSIGPIPE` is
  now set on every socket `UnixTransport::from_stream` wraps (a `pair()`
  socketpair and an fd adopted by `from_owned_fd` included), on each stream
  `RpcServer` accepts for TLS, and in `TcpDebugTransport::from_stream`; std
  sets it only on the sockets its own `connect` creates. A `TlsStream` over a
  stream from another source (your own `accept`, `UnixStream::pair`, a raw
  fd) needs the caller to set it.
- **RPC (r34): an android-12 libbinder peer's binder is no longer refused one
  time in 256.** AOSP r34 `RpcAddress::unique` fills all 32 bytes at random;
  a received binder whose byte 8 equaled this end's role tag was read as an
  address this end had minted and refused with `BadValue`. Only an address
  of the shape this end mints (role tag, zero tail) is refused now.
- **RPC: a session timeout too large for an `Instant` (e.g. `Duration::MAX`)
  no longer panics** when a call waits for a free connection; that wait then
  has no deadline, as with no timeout. The panic poisoned the session's
  connection-state lock, so later calls on that session panicked too.
- **RPC: a TLS or `tcp_debug` connect that hits its own deadline returns
  `TimedOut`**, not `Unknown` (std's error for it carries no errno).
- **RPC: dropping a proxy right after a oneway on it no longer aborts a
  libbinder peer.** The proxy's `DEC_STRONG` could go out on another
  connection than the oneway and be handled first; the peer then freed its
  node and aborted on the oneway (`Local binder must have been sent`), and an
  rsbinder peer dropped it as addressed to an unknown node. rsbinder now counts
  what the peer pays back with a `DEC_STRONG`, as AOSP `sentRef` does: each
  transaction's target on the android-13+ wire, and at v2 each proxy written
  into a parcel. A dropped proxy's `DEC_STRONG` waits until those are paid, so
  releasing the peer's object can wait for this end's next twoway on that
  connection, or for the session's end.
- **RPC: an android-13+ libbinder client no longer keeps an rsbinder object
  alive until the session ends.** It counts each transaction's target and each
  binder it sends back home as a send owed a `DEC_STRONG`, which rsbinder never
  returned; it now does, as AOSP `flushExcessBinderRefs` does. On the v2 wire a
  parcel dropped unread also pays back the binders it carried.
- **RPC: reading a binder twice no longer returns a reference the peer never
  gave.** A `ParcelableHolder` retried after a failed `get_parcelable`, or a
  request read back before it was sent, cost the peer an extra `DEC_STRONG`:
  libbinder 16_r4+ ends the session, other peers free the node early
  (`DeadObject`). A proxy written into a received parcel is `InvalidOperation`.
- **RPC: a client that only sends oneways no longer deadlocks with its
  server.** A server writes the `DEC_STRONG`s raised while serving a oneway on
  that connection when no other is free (libbinder at once, as AOSP
  `ExclusiveConnection::find` does). rsbinder read that connection only while
  waiting for a reply, so enough of them blocked both ends. A transaction's
  send now reads `DEC_STRONG`s off its connection while it waits for room, as
  AOSP's `drainCommands` does; any other command read there ends the session
  (`BadType` for a request), judged on the android-13+ wire from its header
  alone, and those reads are bounded by the session's send deadline
  (`set_timeout`, or a server's idle timeout). A send that fails ends the
  session before anything else is written on its connection. An rsbinder
  server also holds such `DEC_STRONG`s, up to 10 000 addresses per
  connection, until its next reply there, or sends them on a connection the
  client serves; past that it writes them as libbinder does, which a client
  that never drains (libbinder 16_r4+ with incoming threads, android-12
  libbinder, rsbinder before this release) can still block on. A custom
  `RpcTransport` drains by implementing the new `send_raw_draining` and
  `send_frame_draining` (the defaults send without reading), and a custom
  `TlsStream` by returning its socket from the new `socket`.
- **An RPC parcel dropped without being sent releases its local binders'
  `timesSent` reservations** (AOSP `mSendState`); a request refused before the
  send (`WouldBlock`, `DeadObject`) keeps them, so the documented retry with
  the same parcel is sound. Sending one parcel twice is `InvalidOperation`
  (see *Migrating from 0.11.0*).
  Writing a local binder into a parcel of an ended session is `DeadObject`
  (was accepted, leaking the node).
- **RPC: a `ParcelableHolder` read from a transaction decodes the binders and
  v1+ fds in its payload.** Its sub-parcel dropped the object positions and
  the received fds, so on v2 a binder read as `BadValue` and on v1/v2 an fd
  failed to read. On v2 an undecoded holder written back into the session now
  takes its own reference on each binder and duplicates each fd (see
  *Migrating from 0.11.0* for the other wires).
- **A `ParcelableHolder` whose read fails after the stability check is left
  empty**, as AOSP `ParcelableHolder::readFromParcel` leaves it; it kept its
  previous value.
- **RPC: a handler reply refused at the send** now reaches the caller as a
  status and the connection keeps serving, as in AOSP
  `RpcState::processTransactInternal`: `BadValue` for more than 64 fds
  (rsbinder's per-frame cap; AOSP allows 253) and `FailedTransaction` for a
  reply over `MAX_FRAME_LEN`. Both ended the connection.
- **Kernel binder: a handler can return a binder proxy it received in the same
  transaction.** A kernel `Parcel` now holds a strong reference on every proxy
  written into it until it drops (AOSP `acquire_object`); the reply failed with
  `FailedTransaction` when the handler's own references to the proxy were
  released before `BC_REPLY`.
- **RPC: a `oneway` call made inside a handler no longer rides the connection
  the request came in on**, where the server's reply to the outer call could
  be misread by nested calls (seen against libbinder as `UNEXPECTED_NULL` or
  an abort). It now takes a connection this end opened, as AOSP does, and is
  `WouldBlock` when there is none (see *Migrating from 0.11.0*).
- **`connect_async("binder://…")` can be cancelled**: dropping the future now
  ends the wait instead of leaving a blocking-pool thread waiting for the
  service (which also stalled `Runtime::drop`).
- **`list_services` on Android 10 no longer stops at a name it cannot read**
  (a 127-character name overflows the legacy service manager's reply buffer).
  Such an entry is kept as an empty string, and the listing ends only on a
  failed transaction, as in AOSP.
- **A synchronous handle to a local async service no longer panics when called
  from async code** ("Cannot start a runtime from within a runtime"):
  `TokioRuntime` now uses `block_in_place`. Two positions still panic and one
  still stalls (see `TokioRuntime`'s docs); on a hot path convert once with
  `into_async::<P>()`.
- **Codegen `async` can be turned off again by a downstream crate.** A pinned
  `features = ["async"]` on rsbinder's own `rsbinder-aidl` build-dependency
  leaked through feature unification, so `default-features = false` still
  generated async code that failed to compile. Codegen now follows the
  runtime's `async` feature; `Builder::set_async_support(true)` still forces it.
- **`rsbinder-aidl`: a nested declaration inside a `@VintfStability` type now
  inherits that stability**, as in AOSP; it previously reported default
  stability, visible on the wire through `ParcelableHolder`.
- **`rsbinder-aidl`: a `@VintfStability` interface published through the sync
  path registered with the default `System` stability**, so peers requiring
  VINTF refused it. The generated `declare_binder_interface!` now emits
  `stability:`.
- **`rsbinder-aidl`: a type nested three or more levels deep could not name a
  type from a grandparent scope.** Name lookup now walks to the package
  boundary, as AOSP does.
- **`rsbinder-aidl`: `@Descriptor("")` no longer produces an empty interface
  descriptor**; as in AOSP `AidlInterface::GetDescriptor`, the canonical name
  is used, so the interface token matches AOSP-generated peers.
- **`rsbinder-aidl`: the implicit `Tag` of a `@FixedSize` union is backed by
  `i8`**, as AOSP `UnionTagGenerater` declares it (`@Backing(type="byte")`).
  It was `i32`, so a `U.Tag[]` went out as `int[]` where AOSP peers read and
  write `byte[]`. The field type changes from `Tag(pub i32)` to `Tag(pub i8)`;
  non-`@FixedSize` unions keep `i32`.
- **`rsbinder-aidl`: a hex literal ignores a `u32`/`u64` suffix**, as AOSP
  `ParseIntegral` does: it is an `int` when it fits 32 bits, else a `long`.
  `0xFFFFFFFFu64` was the `long` `4294967295` and is now `-1`; a decimal
  literal's `u32`/`u64` suffix still sets its width.
- **`rsbinder-aidl`: a union's `Default` for an enum-typed first field
  without an initializer is the enum's `Default` (`0`)**, as AOSP
  `GenerateParcelDefault(AidlUnionDecl)` generates it; it was the first
  enumerator (see *Migrating from 0.11.0*). A union whose first field is
  another union's `Tag` (`union B { A.Tag t; … }`) now generates; the
  generator panicked on it.
- **`rsbinder-aidl`: a name that collides with an item the Rust generator
  emits is a diagnostic instead of a rustc error in `OUT_DIR`.** Refused: in
  a union, a constant or nested type named `Tag`, a union named `Tag`, and a
  field named `get` or `enum_values` (the implicit `Tag`'s methods); an
  enumerator named `get` or `enum_values`; in an interface `IFoo`, a
  constant named `DEFAULT_IMPL`, `on_transact`, `BnFoo`, `VERSION` (when
  versioned) or `HASH` (when a hash is set), and a nested type named
  `transactions`, `BnFoo`, `BpFoo`, `IFooDefault`, `IFooDefaultRef` or, with
  async codegen, `BnFooAdapter`, `IFooAsync`, `IFooAsyncService`. AOSP's
  `aidl` also refuses a union named `Tag` and a nested `Tag` in a union; the
  other names come from the Rust backend.
- **`rsbinder-aidl`: fixed-size array arguments match AOSP's Rust backend**
  (`aidl_to_rust.cpp` `RustNameOf`). A server for `in @nullable String[3]`,
  or a client reading a `@nullable String[3]` return, failed the whole call
  with `UNEXPECTED_NULL` when a Java or C++ peer sent a null element; such an
  element is now `Option<_>`. An `inout` fixed-size array of a binder,
  interface or fd no longer has `Option` elements, so a service cannot leave
  a `None` that went into the reply as a null the AOSP client rejects, and a
  client cannot send one.
- **`rsbinder-aidl`: union variant names follow AOSP `GetCapitalizedName`**,
  which uppercases the first letter only. The UpperCamel conversion made code
  written against AOSP's generated Rust (`MyUnion::Nullable_iface`) fail to
  compile, and turned AOSP-valid fields such as `SELF`, `self_`, `_1`, `__`,
  or both `my_field` and `myField`, into variants that are not Rust
  identifiers or that collide. The generated union module allows
  `non_camel_case_types` for the variants this produces.
- **`rsbinder-aidl`: an unclosed `<` counts against the generic nesting limit
  (12)**, the same as a closed one, since it costs the parser the same
  exponential re-parse; it was bounded by the bracket limit (256). The
  diagnostic now says that a comparison before a name counts, instead of
  reporting only "generic types are nested too deeply".
- **`rsbinder-macros`: a method named `descriptor`, `getDefaultImpl`,
  `setDefaultImpl`, `dump` or `as_binder` is a diagnostic** on the method
  name, as on the `.aidl` path, instead of a rustc error inside the
  expansion (an item of the generated trait or of `rsbinder::Interface` has
  that name).
- **`rsbinder-macros`: `#[rsbinder::interface]` and `#[derive(Parcelable)]`
  compile in a module that defines or glob-imports a name the prelude also
  has, and a signature may name a user type called `Ok`, `Err`, `Some`,
  `None`, `Default`, `Wrapper`, `T`, `R` or `P`.** The interface module
  reaches its parent through `use super::*;`, and the derive expands in the
  user's module, so both now name every prelude item by absolute path
  (`::core::result::Result::Ok`, `::core::option::Option`,
  `::std::string::String`, `::core::marker::Send`, `::std::sync::Arc`, …)
  instead of by bare names the parent's items could replace. Under the
  `async` feature the generated type parameters and helper struct, which
  share the signatures' scope, were `P`, `T`, `R` and `Wrapper`; they are now
  `__RsbPool`, `__RsbService`, `__RsbRuntime` and `__RsbAsyncWrapper`, and
  the `__Rsb` prefix is reserved: the macro refuses a trait name or a bare
  signature path starting with it, and `rsbinder-aidl` refuses an
  interface, parcelable, enum or union named with it (AOSP `aidl` accepts
  such names). `rsbinder-aidl` output spells the same paths and names. Three
  names still resolve in the module: `Box` under the `async` feature
  (`async-trait`'s expansion names it bare), the owned `String` / `Vec<T>`
  the stub declares for a `&str` / `&[T]` argument, and the `rsbinder` of
  every generated `rsbinder::` path, which a parent `mod rsbinder` or
  `use … as rsbinder` takes over.

### Security

- **Shared memory: `BpMemory` and `BpMemoryHeap` send their calls only with
  the `IMemory` / `IMemoryHeap` interface token**, as AOSP `IMemory.cpp`
  writes it. A reply naming a binder of another interface as the heap made the
  client send that binder transaction code 1 under the client's identity, with
  the binder's own descriptor as the token, and an RPC proxy with no known
  descriptor stayed stamped as `IMemoryHeap`. A binder known to be another
  interface is now `BadType`.

- **`rsbinder-aidl`: a permission annotation written after `oneway`
  (`oneway @EnforcePermission("X") void m();`) is enforced.** It was parsed as
  a return-type annotation and ignored, so the generated service ran the
  method for every caller. AOSP applies annotations on either side of `oneway`
  to the method; rsbinder now does the same, and also applies the
  interface-and-method conflict check and the repeated-annotation check to
  that position. Rebuild services generated from AIDL of this shape.

- **rsbinder-tools (`rsb_hub`): an `exec` program is checked like the
  configuration that names it.** It runs as `rsb_hub` (normally root) on a
  `getService` miss by any client allowed to `find` the name, but only its
  absoluteness was checked, so whoever could replace the file or a directory
  above it could run code as root. The program and every directory on the way
  to it from `/` must now be owned by root or `rsb_hub`'s uid and be neither
  group- nor world-writable, or the load fails (`ConfigError::UntrustedExec`;
  a missing program is `ConfigError::ExecIo`). On Linux and Android the
  program needs execute permission only (it is opened with `O_PATH`);
  elsewhere it needs read permission as well. The check runs at load and
  again right before each start; a start whose program fails it then is
  refused and logged, so a program a failed SIGHUP reload judged untrusted
  does not run under the policy that reload left in force. The start
  executes the path after its check, so a replacement between the two is not
  caught — after the check passes, only root or `rsb_hub`'s uid can make one.

- Raised the `rustls` floor to 0.23.45 for RUSTSEC-2026-0285 (TLS 1.3 handshake
  messages accepted across encryption level boundaries). Affects the `rpc-tls`
  feature only. The floor is in the manifest, so it constrains downstream
  lockfiles too.

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

[Unreleased]: https://github.com/hiking90/rsbinder/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/hiking90/rsbinder/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/hiking90/rsbinder/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/hiking90/rsbinder/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/hiking90/rsbinder/compare/v0.8.0...v0.9.0
