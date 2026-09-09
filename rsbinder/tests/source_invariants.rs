//! Source-level invariants enforced at test time.
//!
//! These tests don't exercise runtime behavior — they pin down
//! structural facts about the rsbinder source tree that load-bearing
//! comments depend on. A new caller of an invariant-protected function
//! flips the test red until the audit named in the invariant is
//! performed.
//!
//! Four remain: refuted prose, `remove_slot`'s callers, the files a
//! byte-order primitive may appear in plus the `ParcelPod` membership
//! list, and the method surface of `CommandStream` — each a closed set,
//! not an enumeration of ways to get it wrong. The call sites of the layer split are not
//! scanned: L2 is `src/command_stream.rs`'s `CommandStream`, whose
//! private field leaves the L1 wire codec unreachable from the command
//! stream's callers.

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
            // Match code only: a pin budgeted at exactly 0 would otherwise
            // go red for someone naming the needle in a trailing comment.
            let code = line.split("//").next().unwrap_or(line);
            if code.contains(needle) {
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

/// L1 (the parcel wire) is little-endian on every host while L2 (the
/// `BC_*`/`BR_*` command stream) and L3 (the UAPI structs) are native, so
/// pin which files may spell a byte-order primitive — native, big-endian
/// or little-endian — and which types `ParcelPod` admits to the raw-bytes
/// view the three layers share.
#[test]
fn byte_order_primitives_and_pod_membership_stay_pinned() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }

    struct Pin {
        needle: &'static str,
        /// Whether a hit outside `files` is itself a failure. True for a
        /// primitive that has no business anywhere else in the crate.
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
            // The one big-endian value in the tree: the Java reliable-PFD
            // comm-socket status, which AOSP peeks BIG_ENDIAN. Not parcel
            // wire — "correcting" it to LE would garble the status.
            needle: "_be_bytes",
            tree_wide: true,
            files: &[("file_descriptor.rs", 1)],
        },
        Pin {
            // The `parcelable_struct!` arm: L3 structs in, L3 structs out.
            needle: "transmute::<[u8",
            tree_wide: true,
            files: &[("parcelable.rs", 1)],
        },
        Pin {
            // L2 has no wire scalar. `CommandStream` exposes no L1 method,
            // but it does hand out `as_mut_ptr`, and a value re-encoded with
            // `from_le_bytes` reaches the driver byte-swapped even through
            // `write_cmd`. Both spell `_le_bytes` in one of these two files.
            needle: "_le_bytes",
            tree_wide: false,
            files: &[("thread_state.rs", 0), ("command_stream.rs", 0)],
        },
        Pin {
            // The same re-encoding without a byte array: `cmd.to_le()`,
            // `u32::from_le(..)`. `_le(` covers both, and an L1 `write_le(..)`
            // called here would be the same mistake.
            needle: "_le(",
            tree_wide: false,
            files: &[("thread_state.rs", 0), ("command_stream.rs", 0)],
        },
        Pin {
            // And the spelling that names no endianness at all.
            needle: "swap_bytes",
            tree_wide: false,
            files: &[("thread_state.rs", 0), ("command_stream.rs", 0)],
        },
        Pin {
            // L3's membership list: the types `write_aligned` / `write_array`
            // / `read_array` reinterpret as raw bytes. Adding one means
            // re-auditing padding and bit-validity (see `ParcelPod`'s
            // `# Safety`), so the macro definition plus the three explicit
            // impls are budgeted; the argument list is pinned below.
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
            // A pin budgeted at 0 goes silently green if its file moves —
            // `ends_with` stops matching and `got == want == 0`. Pin the
            // file's existence too.
            if !src_root.join(suffix).is_file() {
                failures.push(format!(
                    "`{needle}`: pinned file {suffix} no longer exists under src/"
                ));
            }
            let got = hits.iter().filter(|(p, _)| p.ends_with(suffix)).count();
            if got != *want {
                failures.push(format!(
                    "`{needle}` in {suffix}: expected {want}, found {got}"
                ));
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
            failures.push(format!(
                "`{needle}` appeared in an unlisted file: {stray:#?}"
            ));
        }
    }

    // The pin above counts the `unsafe impl` sites; membership is decided by
    // this argument list, which the macro definition hides from that count.
    let parcel_rs = fs::read_to_string(src_root.join("parcel.rs")).unwrap();
    if !parcel_rs
        .contains("impl_parcel_pod!(i8, u8, i16, u16, i32, u32, i64, u64, u128, f32, f64);")
    {
        failures.push(
            "the `impl_parcel_pod!` argument list changed — re-audit padding and \
             bit-validity before raising it (`usize`/`isize` are pointer-width, \
             so their bytes are not the same width on every host)"
                .to_string(),
        );
    }

    assert!(
        failures.is_empty(),
        "a pinned byte-order primitive or the `ParcelPod` membership list \
         moved — see this test's doc:\n{}",
        failures.join("\n")
    );
}

/// `CommandStream` (in `src/command_stream.rs`) is what keeps the L1 wire
/// codec out of the L2 command stream, and it does that by exposing no L1
/// method.
/// Nothing in the language stops a 16th `fn` here from forwarding one, from
/// handing `&mut self.0` back out, or from declaring a child module that
/// reaches the private field from its own file — so pin all three counts:
/// they move only for a deliberate edit.
#[test]
fn command_stream_exposes_no_new_forward() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }
    let path = src_root.join("command_stream.rs");
    assert!(
        path.is_file(),
        "command_stream.rs is gone — point this gate at the L2 type's new home"
    );
    for (needle, want) in [("self.0", 14), ("fn ", 15), ("mod ", 0)] {
        let got = count_call_sites(&src_root, needle)
            .iter()
            .filter(|(p, _)| p == &path)
            .count();
        assert_eq!(
            got, want,
            "`{needle}` in command_stream.rs: expected {want}, found {got}. \
             A method — or a child module, which is a descendant and so reaches \
             the private field — that gets at the inner `Parcel` re-opens the L1 \
             codec to every one of the command stream's call sites — argue for it \
             here."
        );
    }
}
