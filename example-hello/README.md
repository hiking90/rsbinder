# Examples for **crate rsbinder**

Every example is a `[[bin]]`: run one with
`cargo run -p example-hello --bin <name>`, adding `--features rpc` where noted.
The kernel-binder examples need `/dev/binderfs/binder` and a running service
manager (`rsb_hub`); see the book's *Installation* chapter.

## Start here

* [`build.rs`](build.rs) — generating Rust from an `.aidl` file
* [`src/lib.rs`](src/lib.rs) — including the generated code
* [`src/bin/hello_service.rs`](src/bin/hello_service.rs) — a Binder service
* [`src/bin/hello_client.rs`](src/bin/hello_client.rs) — a Binder client

## One codebase, either transport

Needs `--features rpc`.

* [`unified_service.rs`](src/bin/unified_service.rs) /
  [`unified_client.rs`](src/bin/unified_client.rs) — `rsbinder::serve(uri)`
  and `connect(uri)`. The same code over kernel binder or RPC; pass `kernel`,
  `rpc`, or an endpoint URI.
* [`rpc_hello_service.rs`](src/bin/rpc_hello_service.rs) /
  [`rpc_hello_client.rs`](src/bin/rpc_hello_client.rs) — the RPC transport on
  its own: no kernel binder, no root, no service manager.
* [`gateway_service.rs`](src/bin/gateway_service.rs) — connect on one
  transport, re-publish on another. The whole bridge is
  `BnHello::new_binder(upstream)`; see the book's *Cross-Transport Services*
  chapter for what stops at a gateway.
* [`authz_service.rs`](src/bin/authz_service.rs) /
  [`authz_client.rs`](src/bin/authz_client.rs) — handler-side authorization
  that matches on the transport-tagged caller and denies with `EX_SECURITY`.

## Storing values

* [`serde_demo.rs`](src/bin/serde_demo.rs) — AIDL parcelables written to a
  file with `rsbinder::to_bytes` instead of sent in a transaction, from the
  `.aidl` definitions in [`aidl/settings/`](aidl/settings/). Shows the round
  trip, both directions of version skew across an appended field, the strict
  read, and the refusal of a file descriptor at write time. Talks to nothing —
  no binder device, no socket. See the book's *Storing Values* chapter.

## Shared memory

* [`shm_service.rs`](src/bin/shm_service.rs) /
  [`shm_client.rs`](src/bin/shm_client.rs) — one `SharedMemory` region over a
  `ParcelFileDescriptor`, plus `MemoryDealer` frames as `IMemory` binders,
  over kernel binder or a Unix-socket RPC session. See the book's *Shared
  Memory* chapter.

## Interop harnesses

The remaining binaries (`rpc_accessor_interop_client`,
`rpc_accessor_register_interop_server`, `rpc_multiconn_interop_server`,
`rpc_incoming_interop_client`, `rpc_fd_interop_server`, `a15_qpr_interop`) are
not tutorials. They are test harnesses that talk to real Android `libbinder`
on a device or emulator, driven by the scripts under [`cpp/`](cpp/), and they
live here because they need the same generated stubs as the examples above.
