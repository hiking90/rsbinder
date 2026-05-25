# Stability tiers

rsbinder uses a three-tier model for public-API stability on the path to
1.0. Each tier states what kind of change can land between releases and
what callers can rely on.

| Tier | Versioning | Wire compatibility | When it can change |
|------|-----------|--------------------|--------------------|
| **Stable** | Semver-strict | AOSP-faithful, byte-tested against real `libbinder` | Only on a major bump. Breaking changes are last-resort. |
| **Provisional** | Stabilizing toward 1.0 | AOSP-faithful in the validated cases (hermetic + real-`libbinder` STAGE3) | May break in a minor bump if 1.0 review surfaces an issue. Will be re-classified as Stable or Experimental at 1.0. |
| **Experimental** | Opt-in via Cargo feature | **No guarantee** until the corresponding real-`libbinder` interop gate passes | Anytime, including the wire format. Default builds do not reach this code. |

## Stable

Surface that has shipped through multiple releases against real Android
binder and is not expected to break.

- **Kernel binder core** — `Parcel`, `IBinder`, `Interface`, `Strong<I>`,
  `Weak<I>`, `Status`, `StatusCode`, `ParcelFileDescriptor`,
  `ProcessState::{init, init_default, start_thread_pool,
  join_thread_pool}`, `Binder::new`, `BinderFeatures`.
- **Transaction code constants** — `FIRST_CALL_TRANSACTION`,
  `LAST_CALL_TRANSACTION`, `PING_TRANSACTION`,
  `INTERFACE_TRANSACTION`, `DUMP_TRANSACTION`, `FLAG_ONEWAY`,
  `FLAG_CLEAR_BUF`.
- **Service manager (Android binder)** — `hub::add_service`,
  `hub::get_service`, `hub::check_service`, `hub::get_interface`.
- **AIDL compiler entry points** — `rsbinder_aidl::Builder::{new, source,
  output, version, generate}`.
- **rsb_device** binary CLI (binderfs setup).

## Provisional

Implemented and validated, but the public-API shape is still under
review for 1.0. Expect at most a single round of renames or signature
tweaks; wire formats are already locked.

- **RPC transport single-connection** — `RpcServer`, `RpcSession`
  (single-conn role), `setup_unix_server`, `setup_unix_client*`,
  `from_preconnected_fd`, `set_root`, `get_root`, `set_android13plus`,
  `set_max_threads(1)` semantics, `set_max_connections`,
  `set_supported_fd_modes(&[FdMode::Unix])`. Validated against real
  android-13 / 14 / 15 / 16 `libbinder` (plans 2-1 … 2-11 / 2-13 / 2-14
  STAGE3).
- **FD-over-RPC** (v1+ AOSP-faithful, AC-11.3 gate passed).
- **RPC death notification** (session-disconnect, AOSP-faithful).
- **IAccessor client / server** (plans 2-13 D.8 + 2-14 D.9 STAGE3
  passed against real `libbinder`).
- **`rsb_hub`** — `addService` descriptor auto-detect, `getService2`,
  `checkService2`, `Service::Accessor` routing. Linux-native bringup.
- **macOS first-class support** (plan 2-9 Phase A+B).

## Experimental (opt-in)

Off by default, gated by a Cargo feature. Wire format and API may
change without a deprecation cycle.

| Feature | Surface | Status |
|---------|---------|--------|
| `rpc-tcp-debug` | `TcpDebugTransport` — plain TCP backend | Bring-up / interop only, never production. |
| `rpc-vsock` | `VsockTransport` — host↔VM | Linux/Android only; loopback testing requires `vsock_loopback.ko`. |
| `rpc-tls` | TLS over rustls, socket-orthogonal (tcp / unix / vsock) | Hermetic green; the `StreamOwned` decoupling (plan 2-15) has landed. Real-`libbinder` STAGE3 not yet attempted — stock emulator images ship no `libbinder_tls`. |
| `rpc-experimental-multiconn` | `RpcServer::set_max_threads(N ≥ 2)` slot-cap > 1 | Hermetic passes; real-`libbinder` interop gate AC-12.6 not yet passed (plan 2-12 §3). Default builds clamp the attach-arm cap to 1 — see `RpcServer::set_max_threads` rustdoc. |

## Non-goals

Surface that rsbinder will not support, even after 1.0. Listed here so
contributors don't burn cycles implementing something that has already
been ruled out — and so the AIDL compiler's deliberate rejection of
these types reads as intentional rather than as a gap.

- **AIDL `Map<K, V>`** — AOSP's own Rust / C++ / NDK backends reject
  `Map<K, V>` at the language layer
  (`aidl_language.cpp:1612-1615`: *"Currently, only Java backend
  supports Map."*), and have done so since 2019. The wire format is
  built on Java `Parcel`'s runtime type-tag system (`VAL_STRING`,
  `VAL_INTEGER`, `VAL_MAP`, …) which has no typed-Rust analogue. The
  rsbinder-aidl frontend therefore emits `unknown type 'Map'` for any
  `Map<…>` reference — matching AOSP. Use `parcelable` (typed struct),
  `List<Entry>`, or `ParcelableHolder` instead. Re-examine if AOSP
  removes the cross-backend block.

## What this means in practice

- A **0.x → 0.(x+1)** minor bump can change **Provisional** signatures.
  rsbinder runs `cargo-semver-checks` on every PR for both
  `rsbinder` and `rsbinder-aidl` (default features and RPC features),
  so any such change is visible on the PR before merge.
- **Experimental** features never break the default build's wire
  format. Enabling one is an explicit acknowledgement that the
  corresponding interop gate has not passed.
- After 1.0, all **Stable** items follow Semver strictly. Provisional
  items get re-classified as Stable or moved into a feature gate.
