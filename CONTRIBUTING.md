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
| `stage3` | the interop scripts (`example-hello/cpp/run_*.sh`, `run_stream_ac.sh --adb`, `run_stream_rpc_selinux_stage3.sh`) against the real servicemanager, libbinder and libfmq | one booted rootable device or emulator (`-s SERIAL` otherwise), `cargo-ndk`, `ANDROID_NDK_HOME`, the AOSP checkout at `$AOSP` for the libfmq/libbinder header builds |

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
cargo ndk -t aarch64-linux-android --platform 29 test --no-run -p rsbinder --features rpc,android_16
adb push target/aarch64-linux-android/debug/deps/<binary> /data/local/tmp/
adb shell 'chmod 755 /data/local/tmp/<binary> && cd /data/local/tmp && TMPDIR=/data/local/tmp ./<binary>'
```

macOS supports the RPC stack (mem / unix / TLS); kernel binder is
Linux/Android-only.

### Topic fleet test

`tests/tests/topic_fleet.rs` runs `rsbinder-topic` the way a deployment
does: 27 processes (`tests/src/bin/topic_node/`) over six topics, killed,
stopped and started again while every receive is checked. The `kernel`
tier runs it, and `integration-test.yml` on x86_64 and on the arm64 Linux
runner; by hand, with a binder device and a permissive `rsb_hub`
(`--config tests/policy/permissive.toml`):

```
cargo test -p tests --test topic_fleet -- --ignored --test-threads=1 --skip hub_restarts
cargo test -p tests --test topic_fleet -- --ignored --exact hub_restarts   # no rsb_hub running
```

| Variable | Default | Effect |
| --- | --- | --- |
| `FLEET_SCALE` | 1 | subscriber nodes ×20; 3 is 67 processes |
| `FLEET_SECS` | 60 | length of the chaos layer |
| `FLEET_SEED` | from the clock | replays a chaos schedule (the log prints the seed) |
| `FLEET_REPEAT` | 30 | rounds of the repeat phase |
| `FLEET_STEADY_SECS` | 30 | length of the steady-state run |
| `FLEET_TIME_FACTOR` | 1 | multiplies the time bounds on a loaded machine |
| `FLEET_KEEP` | unset | `1` keeps the run directory of a passing test |

A failing test keeps its run directory (`target/tmp/topic-fleet/`): the
event log and each node's stderr. A debug build's nodes take about three
cores; on a smaller machine run it with `--release`.

On a rootable emulator (userdebug image) the fleet runs against the
platform service manager, everything but `hub_restarts`:

```
cargo ndk -t x86_64-linux-android --platform 29 test --release -p tests --test topic_fleet --no-run
adb root && adb shell mkdir -p /data/local/tmp/fleet
adb push target/x86_64-linux-android/release/topic_node /data/local/tmp/fleet/
adb push target/x86_64-linux-android/release/deps/topic_fleet-<hash> /data/local/tmp/fleet/topic_fleet
adb shell 'cd /data/local/tmp/fleet && TOPIC_NODE_BIN=$PWD/topic_node TOPIC_FLEET_DIR=$PWD/run \
  ./topic_fleet --ignored --test-threads=1 --skip hub_restarts'
```

Take `<hash>` from the build's `Executable tests/topic_fleet.rs (...)` line;
`deps/` keeps older builds of the same test. Android 10 (`i686-linux-android`
for the API 29 x86 image) has no registration notices in its service
manager, so there the named subscribers are T6's alone.

## Pull request checklist

- `scripts/local_gate.sh` passes: at least `hermetic`, plus `kernel` for
  anything touching `rsbinder::*` or `rpc::`
- `cargo-semver-checks` runs automatically on PRs against `rsbinder`,
  `rsbinder-aidl` and `rsbinder-fmq` — intentional API breaks need
  acknowledgment in the PR description
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
in every build is an `#[expect]`. The library crates (rsbinder,
rsbinder-aidl, rsbinder-macros, rsbinder-fmq, rsbinder-topic,
rsbinder-tools) enforce the no-`unwrap` rule and the `reason` rule with
`#![cfg_attr(not(test), deny(clippy::unwrap_used))]` and
`#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]`
at the crate root; a new library crate adds the same two lines. Review
keeps the `#[expect]` rule everywhere and the `reason` rule in targets
those roots do not build (bins, `example-hello`); the no-`unwrap` rule
covers library code only.

## Public API design

The rules below come from the public API review of plan 17, where most
findings repeated one of these causes. Two are checked by
`rsbinder/tests/source_invariants.rs`; review holds the rest.

- **A state that only an outside change ends is reported on every ask.**
  A receive that has seen the end returns the end again on the next call,
  and a long-lived handle that can wait in such a state (refused,
  resolver gone, corrupt peer) has a way to ask for it. Reporting it once,
  or only in a log, leaves the next call waiting with no reason given.
- **A public type whose drop ends something is `#[must_use = ".."]`**,
  with a message saying what ends. A type-level attribute reaches through
  `?`. Checked: `types_whose_drop_ends_something_are_must_use`.
- **Library threads are made with `thread::Builder`.** A direct caller
  gets the spawn failure as an `Err`; a path with no caller waiting (a
  kernel-requested looper) logs it. Checked:
  `library_threads_are_made_by_builder`.
- **No adjacent positional arguments of one primitive type, and no `bool`
  with two meanings.** Use a named struct, an enum or two functions
  (`freeze_process` / `thaw_process`, not `freeze_process(pid, bool, ..)`).
- **Timeouts.** A required deadline is a `Duration`, an optional one an
  `Option<Duration>` with `None` waiting without end. `Duration::ZERO`
  means "do not wait" where one attempt is meaningful (a receive, a send, a
  wait) and is refused with `BadValue` where it is not (a reply, idle or
  handshake deadline); it never means "without end". A timed-out wait and
  an ended source return different values, as `std::sync::mpsc` does.
- **Permission and identity checks read the dispatcher's thread state**
  (`calling_caller()`), never data the caller passes in.
- **Error types follow AOSP's layers**: the Parcel and transport layer
  returns `StatusCode` (`status_t`), a service call returns `Status`
  (`BinderResult`).
- **Peer input never panics or crashes the process**: a size, offset or
  count that came from a peer is checked and refused with an `Err`.

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

Multi-paragraph rustdoc is encouraged where it documents enduring
invariants. Cite a design plan by its ID (`plan 2-12`) rather than
linking it: the `plans/` directory holds the maintainer's local design
notes and is not part of the repository, so a link to one resolves
neither on docs.rs nor on GitHub.

Dated incident detail (specific dates, hex dumps, log lines) belongs in
the commit message or the PR description, not in rustdoc itself —
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
wire format already locked), **Experimental** (an opt-in Cargo feature or
a crate of its own, `rsbinder-topic`; wire format may change). Check the
table before changing a public signature — Stable surfaces require
deprecation discussion in the PR.

## Releasing

Every crate of the workspace is published at the workspace version, in
dependency order, since each must already be on crates.io before the next
one can resolve it:

1. `rsbinder-fmq` — depends on no other crate here.
2. `rsbinder-aidl`
3. `rsbinder-macros` — depends on `rsbinder-aidl`.
4. `rsbinder` — requires `rsbinder-fmq`, and names `rsbinder-macros` behind
   the `macros` feature with a version requirement.
5. `rsbinder-topic` — depends on `rsbinder` and builds its AIDL with
   `rsbinder-aidl`.
6. `rsbinder-tools`

`source envsetup.sh` and then `publish_dry_run` / `publish` run
`cargo publish` in this order (`publish_dirs` there); a crate added to the
workspace is added to that list in its place. `tests`, `example-hello`,
`fuzz` and `bench/ipc-compare` are not published.

## License

Apache-2.0. Contributions are accepted under the same license.
