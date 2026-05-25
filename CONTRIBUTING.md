# Contributing to rsbinder

Thanks for your interest in contributing. rsbinder is a pure-Rust Binder
IPC implementation; the public-API surface ships on docs.rs and the wire
format must stay byte-compatible with Android's `libbinder`, so a few
project-specific conventions are worth knowing up front.

## Build & test

The workspace builds with `cargo build`. The hermetic test suites need
no special environment:

```
cargo test -p rsbinder --features rpc --lib rpc::
cargo test -p rsbinder --features rpc --test rpc_server --test rpc_e2e --test rpc_fd
cargo test -p rsbinder --features rpc-tls --test rpc_tls
cargo test -p rsbinder --features rpc,android_16 --test rpc_accessor
cargo test -p rsbinder --features rpc-experimental-multiconn --test rpc_server
cargo test -p rsbinder-tools --bin rsb_device
```

The full kernel-binder integration suite is Linux-only and needs a
running `rsb_hub` + `test_service`:

```
cargo build --bin rsb_hub --bin test_service
RUST_LOG=warn nohup ./target/debug/rsb_hub > /tmp/rsb_hub.log 2>&1 & disown
sleep 2
RUST_LOG=warn nohup ./target/debug/test_service > /tmp/test_service.log 2>&1 & disown
sleep 2
cargo test -p tests
cargo test -p tests test_death_recipient -- --ignored
```

Android cross-compile via [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk):

```
cargo ndk -t aarch64-linux-android test --no-run -p rsbinder --features rpc,android_16
adb push target/aarch64-linux-android/debug/deps/<binary> /data/local/tmp/
adb shell 'chmod 755 /data/local/tmp/<binary> && cd /data/local/tmp && TMPDIR=/data/local/tmp ./<binary>'
```

macOS supports the RPC stack (mem / unix / TLS); kernel binder is
Linux/Android-only.

## Pull request checklist

- `cargo fmt --all`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo doc --workspace --all-features --no-deps` — 0 warnings
- Run the test matrix relevant to your change (full hermetic + kernel-
  binder e2e for anything touching `rsbinder::*` or `rpc::`)
- `cargo-semver-checks` runs automatically on PRs against `rsbinder`
  and `rsbinder-aidl` — intentional API breaks need acknowledgment in
  the PR description
- Wire-format-affecting changes need a STAGE3 (real-`libbinder` interop)
  result documented in the PR

## Comment & docstring policy

This project deliberately splits the comment convention by audience.

### Inline & private (`//`, `///` on private items)

- Default to no comment.
- One short line when the WHY is non-obvious — hidden invariant,
  subtle race, surprising behavior.
- Never multi-paragraph.

### Public API rustdoc (`///` on `pub`/`pub(crate)` items)

Explicitly exempt from the "one short line max" rule. rsbinder's
`pub fn` rustdoc is the contract surface that ships on docs.rs and is
the only available place to document:

- wire-format compatibility scope (AOSP version, profile, AC/V gates),
- AOSP-faithful behavior (`setMaxIncomingThreads`, `setupClient`, ...),
- feature-gated semantics (advertise vs. enforcement split, opt-in
  experimental paths),
- experimental status with the opt-in feature name.

Multi-paragraph rustdoc + plan / spec deeplinks (`plan/2-X-*.md`,
`RPC_STATUS.md`) are encouraged where they document enduring invariants.

Dated incident detail (specific dates, hex dumps, log lines) belongs in
the linked `RPC_STATUS.md` / `plan/*.md`, not in rustdoc itself —
rustdoc is read in isolation on docs.rs and dated material rots.

### References

Cite freely:

- plan IDs (`plan/2-12-multi-connection-per-session.md`),
- AOSP source paths (`frameworks/native/libs/binder/RpcServer.cpp`),
- AC / V gate IDs (`AC-12.6`, `V5`).

These are enduring artifacts.

Avoid:

- work-item IDs from a single PR session (e.g. "A-1a", "C-1") — they
  rot the moment the PR merges,
- version-specific tool notes (e.g. "rustc 1.95's clippy flagged it") —
  the version drifts; the clippy lint enforces itself,
- "previous code did X" / "this used to be Y" wording — git log /
  blame is the authoritative history.

## Stability tier

Public API stability follows the 3-tier model documented in
[`book/src/stability-tiers.md`](book/src/stability-tiers.md): **Stable**
(semver-strict), **Provisional** (signature may tweak in a minor bump,
wire format already locked), **Experimental** (opt-in Cargo feature,
wire format may change). Check the table before changing a public
signature — Stable surfaces require deprecation discussion in the PR.

## License

Apache-2.0. Contributions are accepted under the same license.
