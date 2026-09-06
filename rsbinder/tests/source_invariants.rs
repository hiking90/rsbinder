//! Source-level invariants enforced at test time.
//!
//! These tests don't exercise runtime behavior — they pin down
//! structural facts about the rsbinder source tree that load-bearing
//! comments depend on. A new caller of an invariant-protected function
//! flips the test red until the audit named in the invariant is
//! performed.

use std::fs;
use std::path::{Path, PathBuf};

fn count_call_sites(src_root: &Path, needle: &str) -> Vec<(PathBuf, usize)> {
    let mut hits = Vec::new();
    visit(src_root, &mut |path, content| {
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }
            if line.contains(needle) {
                hits.push((path.to_path_buf(), i + 1));
            }
        }
    });
    hits
}

fn visit<F: FnMut(&Path, &str)>(dir: &Path, f: &mut F) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, f);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(content) = fs::read_to_string(&path) {
                f(&path, &content);
            }
        }
    }
}

/// Prose the code contradicts. Each phrase below was once written in a serve-loop
/// rustdoc or the CHANGELOG and later shown false: the serve loop's `Ok(())` is
/// reached by a local `RpcSession::close_session` exactly as by a peer close, and
/// what a transport `shutdown` does to bytes already received is platform- and
/// backend-dependent (macOS discards the kernel queue, Linux keeps it, `mem`
/// discards logically, a transport may hold a buffered leftover). The same
/// claim tends to be restated in several places and corrected in one, so this
/// pins every `.rs` under `src/` plus `CHANGELOG.md`: a restatement trips the
/// build rather than the next reviewer. Rephrase, don't route around — if a
/// phrase is needed to state something *true*, narrow the phrase here.
#[test]
fn prose_does_not_restate_refuted_shutdown_claims() {
    const FORBIDDEN: &[&str] = &[
        // Peer-agency shorthands for an end of stream that either side reaches.
        "until the peer closes",
        "until peer closes",
        "peer closed (clean)",
        "peer that closed cleanly",
        // Absolute claims about what `shutdown` does to received bytes.
        "drops neither",
        "discards neither",
    ];
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_root = manifest.join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }
    // Line 0 marks a phrase that only appears once comment lines are joined —
    // a wrapped rustdoc sentence evades a per-line scan otherwise.
    let mut hits: Vec<(PathBuf, usize, &str)> = Vec::new();
    let mut scan = |path: &Path, content: &str| {
        for (i, line) in content.lines().enumerate() {
            for phrase in FORBIDDEN {
                if line.contains(phrase) {
                    hits.push((path.to_path_buf(), i + 1, phrase));
                }
            }
        }
        let joined = content
            .split_whitespace()
            .filter(|w| !matches!(*w, "///" | "//!" | "//"))
            .collect::<Vec<_>>()
            .join(" ");
        for phrase in FORBIDDEN {
            let per_line = hits
                .iter()
                .any(|(p, l, ph)| p == path && *l > 0 && ph == phrase);
            if !per_line && joined.contains(phrase) {
                hits.push((path.to_path_buf(), 0, phrase));
            }
        }
    };
    visit(&src_root, &mut scan);
    let changelog = manifest.join("../CHANGELOG.md");
    if let Ok(content) = fs::read_to_string(&changelog) {
        scan(&changelog, &content);
    }
    assert!(
        hits.is_empty(),
        "prose restates a refuted shutdown claim — see this test's doc: {hits:#?}"
    );
}

/// `RpcSessionInner::remove_slot` is private to `rpc/session.rs` and safe to call from more
/// than one site now that `find_conn` / `find_conn_pinned` return
/// `Err(StatusCode::DeadObject)` (not `expect`-panic) when their reentrant
/// slot lookup misses. The six sanctioned callers are the slot's own
/// `serve_blocking_on` exit; `retire_after_failed_send`, the one rule every
/// outbound frame's transport-level send failure funnels through;
/// `client_transact`'s two reply-wait slot-retiring paths (a reply wait that
/// failed to arm, read, decode or nested-dispatch, and the reply wait's
/// refusal of a slot a nested call marked unreadable) — all retire a slot whose
/// peer is gone / stream is desynced so it is never
/// reused, and the send-failure one is what lets a serve-less client session
/// reach death detection (`remove_slot`'s empty-pool hook, Plan 2-17 A.1b);
/// and the two attach rollbacks, which un-push a slot the peer will never be
/// able to use — an incoming connection whose serve thread failed to spawn,
/// and a callback slot whose connection-init write never reached the client.
/// A NEW caller MUST re-audit that every slot-lookup path tolerates a missing
/// slot before being added — this bound guards against accidentally
/// reintroducing a lookup that assumes the slot is always present. The scan is
/// `cfg`-blind (it reads every `.rs` under `src/`), so a future `#[cfg(test)]`
/// caller counts too: raise the number deliberately rather than route around
/// it.
#[test]
fn remove_slot_has_exactly_six_callers() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // `CARGO_MANIFEST_DIR` is baked in at compile time, so a binary copied
    // elsewhere (a device push, for one) cannot see the sources. Skip loudly
    // rather than fail: on a host build the directory always exists, so this
    // never silently drops the guard where it is meant to run.
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }
    let hits = count_call_sites(&src_root, ".remove_slot(");
    // Pin the location too: a count-only bound would still pass if a
    // sanctioned caller were deleted and an unaudited one added elsewhere.
    assert!(
        hits.iter().all(|(p, _)| p.ends_with("rpc/session.rs")),
        "remove_slot callers must live in rpc/session.rs: {hits:#?}"
    );
    assert_eq!(
        hits.len(),
        6,
        "RpcSessionInner::remove_slot must have exactly six callers. \
         Found {} call sites: {:#?}\n\
         INVARIANT: only serve_blocking_on's exit path, \
         retire_after_failed_send, client_transact's failed-reply-wait / \
         unreadable-slot retirements, the \
         incoming-connection attach's spawn-failure rollback, and the \
         callback attach's init-write-failure rollback may call remove_slot — \
         find_conn / find_conn_pinned must return DeadObject (not panic) \
         on a missing slot. A new caller MUST audit every slot-lookup \
         path before being added.",
        hits.len(),
        hits,
    );
}

/// The parcel has three byte layers and only one of them is the wire.
///
/// L1 (data-parcel scalars, arrays, String16) is defined little-endian on
/// every host. L2 (the `BC_*`/`BR_*` ioctl stream) and L3 (the UAPI
/// structs the driver parses) are host-native. All three share the same
/// `Parcel`, so nothing but the primitive a call reaches for says which
/// layer it is in — and getting it wrong is invisible on a little-endian
/// host, where the two codecs emit identical bytes.
///
/// So pin where the native primitives live. A new `_ne_bytes` in
/// `parcelable.rs` means a wire value stopped being portable; a new one
/// in `parcel.rs` outside `NativeScalar` means the same. Raise a count
/// only after deciding which layer the new code belongs to.
#[test]
fn native_byte_codecs_stay_in_their_islands() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }

    struct Pin {
        needle: &'static str,
        /// Whether a hit outside `files` is itself a failure. True for a
        /// primitive that has no business anywhere else in the crate;
        /// false for `as *const u8`, which is ordinary Rust (pointer
        /// identity, FFI) away from the parcel.
        tree_wide: bool,
        files: &'static [(&'static str, usize)],
    }

    let pins = [
        Pin {
            needle: "_ne_bytes",
            tree_wide: true,
            files: &[
                // `NativeScalar`'s two methods, plus three tests that
                // assert what stays native: the command stream, the null
                // binder's object header, and the forged object header a
                // data decode must refuse.
                ("parcel.rs", 5),
                // fd/memfd bookkeeping, never parcel wire.
                ("shared_memory/mod.rs", 4),
            ],
        },
        Pin {
            // The `parcelable_struct!` arm: L3 structs in, L3 structs out.
            needle: "transmute::<[u8",
            tree_wide: true,
            files: &[("parcelable.rs", 1)],
        },
        Pin {
            // Raw byte views of parcel data. `parcel.rs`: `write_aligned`
            // and the object table. `parcelable.rs`: the String16
            // `Vec<u16>` view, which stays behind a `target_endian` gate
            // rather than disappearing.
            needle: "as *const u8",
            tree_wide: false,
            files: &[("parcel.rs", 2), ("parcelable.rs", 1)],
        },
        Pin {
            // L3's membership list — the types whose bytes cross verbatim.
            needle: "unsafe impl ParcelPod for",
            tree_wide: true,
            files: &[("parcel.rs", 4)],
        },
    ];

    let mut failures = Vec::new();
    for pin in &pins {
        let needle = pin.needle;
        let hits = count_call_sites(&src_root, needle);
        for (suffix, want) in pin.files {
            let got = hits.iter().filter(|(p, _)| p.ends_with(suffix)).count();
            if got != *want {
                failures.push(format!("`{needle}` in {suffix}: expected {want}, found {got}"));
            }
        }
        if !pin.tree_wide {
            continue;
        }
        let stray: Vec<_> = hits
            .iter()
            .filter(|(p, _)| !pin.files.iter().any(|(suffix, _)| p.ends_with(suffix)))
            .collect();
        if !stray.is_empty() {
            failures.push(format!("`{needle}` appeared in an unlisted file: {stray:#?}"));
        }
    }

    assert!(
        failures.is_empty(),
        "the three parcel byte layers moved — see this test's doc:\n{}",
        failures.join("\n")
    );
}

/// `thread_state.rs` writes the kernel command stream, which is an ioctl
/// buffer the driver parses with native loads — not wire. It must reach
/// the parcel through `write_native` / `read_native` only.
///
/// This is the gate for the one mistake the naming cannot prevent: a new
/// `BC_*` command written as `out_parcel.write::<u32>(&cmd)` compiles,
/// passes every little-endian test, and sends byte-swapped commands to a
/// big-endian driver. Matching on the method rather than the receiver
/// catches the wrapped call chains too, where the receiver is a line up.
#[test]
fn the_kernel_command_stream_never_reaches_the_wire_codec() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/thread_state.rs");
    let Ok(content) = fs::read_to_string(&path) else {
        eprintln!("skipping: sources not reachable at {}", path.display());
        return;
    };

    let mut offenders = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if !line.contains(".write::<") && !line.contains(".read::<") {
            continue;
        }
        // L3: the UAPI transaction structs are read whole and native, by
        // the `parcelable_struct!` path the driver's layout requires.
        if line.contains("binder_transaction_data") {
            continue;
        }
        // A *reply* is a data parcel, not the command stream, so the two
        // test writes below are wire — correctly.
        if line.contains("reply.write::<") {
            continue;
        }
        offenders.push(format!("thread_state.rs:{}: {}", i + 1, line.trim()));
    }

    assert!(
        offenders.is_empty(),
        "the kernel command stream must use write_native / read_native — \
         see this test's doc:\n{}",
        offenders.join("\n")
    );
}
