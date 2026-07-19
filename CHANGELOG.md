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

### Changed

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

### Fixed

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
