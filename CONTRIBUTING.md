# Contributing to rsbinder

Thanks for your interest in contributing. rsbinder is a pure-Rust Binder
IPC implementation; the public-API surface ships on docs.rs and the wire
format must stay byte-compatible with Android's `libbinder`, so a few
project-specific conventions are worth knowing up front.

## Build & test

The workspace builds with `cargo build`. Before opening a PR, run the
local gate; it runs what CI runs and what CI cannot:

```
scripts/local_gate.sh                 # all three tiers
scripts/local_gate.sh hermetic        # one tier
scripts/local_gate.sh stage3 --only fmq_interop,stream_interop
```

| Tier | Runs | Needs |
| --- | --- | --- |
| `hermetic` | every step of `.github/workflows/build.yml`: fmt, clippy, rustdoc, MSRV, all hermetic test targets, big-endian (s390x), public API goldens, Android builds | nothing beyond Rust; `cross` + docker, the pinned nightly + `cargo-public-api`, `cargo-semver-checks`, `cargo-ndk` enable their steps |
| `kernel` | the Linux job of `integration-test.yml` (unit tests with a live binder, the `tests` suite sync/async, the `#[ignore]`d kernel tests one by one), `tests/scripts/run_*_ac.sh`, `run_d8b_register.sh`, vsock loopback, the libfmq host peer | a writable `/dev/binderfs/binder` (`sudo target/debug/rsb_device binder`), no `rsb_hub` already running; `vsock_loopback` and the AOSP checkout enable their steps |
| `stage3` | the interop scripts (`example-hello/cpp/run_*.sh`, `run_stream_ac.sh --adb`) against the real servicemanager, libbinder and libfmq | one booted rootable device or emulator (`-s SERIAL` otherwise), `cargo-ndk`, `ANDROID_NDK_HOME`, the AOSP checkout at `$AOSP` for the libfmq/libbinder header builds |

Whatever a tier cannot run on the machine is listed as `SKIP` with what
it needs; the exit status is non-zero when a step fails, and each step's
output is under `target/local-gate/<timestamp>/`. `stage3` runs only when
the diff against `--base` (default `origin/master`) touches a crate a
device run exercises; `--stage3-all` forces it. Each script is selected by
the device's SDK level, so an Android 16 emulator runs the most of them;
`run_a15_qpr_stage3.sh`, `run_phasec_vintf.sh` and
`run_rt_inherit_interop.sh` are run by hand (see their headers).

Android cross-compile via [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk):

```
cargo ndk -t aarch64-linux-android -p 29 test --no-run -p rsbinder --features rpc,android_16
adb push target/aarch64-linux-android/debug/deps/<binary> /data/local/tmp/
adb shell 'chmod 755 /data/local/tmp/<binary> && cd /data/local/tmp && TMPDIR=/data/local/tmp ./<binary>'
```

macOS supports the RPC stack (mem / unix / TLS); kernel binder is
Linux/Android-only.

## Pull request checklist

- `scripts/local_gate.sh` passes: at least `hermetic`, plus `kernel` for
  anything touching `rsbinder::*` or `rpc::`
- `cargo-semver-checks` runs automatically on PRs against `rsbinder`
  and `rsbinder-aidl` — intentional API breaks need acknowledgment in
  the PR description
- Wire-format-affecting changes need a STAGE3 (real-`libbinder` interop)
  result documented in the PR

## Error handling

Library code (each lib crate outside `#[cfg(test)]`) does not call
`unwrap()`; tests may. Where a value is taken out of an `Option` or
`Result`:

- A failure that can come from outside (the kernel, an RPC peer, AIDL
  source, a file, a caller's argument) is returned as an `Err`.
- If reshaping the code makes the `Option`/`Result` disappear at no cost
  (`first_chunk` on a checked slice, a match pattern instead of a guard
  plus `expect`), reshape it.
- `expect()` is for what cannot be recovered: lock poison, an internal
  invariant the types cannot express, a documented caller contract. The
  message names the broken premise (`"conn_state poisoned"`).

Every `allow` outside tests carries `reason = ".."`; one the code needs
in every build is an `#[expect]`. Both rules are enforced by
`#![cfg_attr(not(test), deny(clippy::unwrap_used))]` and
`#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]`
at each lib crate root; a new lib crate adds the same two lines.

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
