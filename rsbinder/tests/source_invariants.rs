//! Source-level invariants enforced at test time.
//!
//! These tests don't exercise runtime behavior — they pin down
//! structural facts about the rsbinder source tree that load-bearing
//! comments depend on. A new caller of an invariant-protected function
//! flips the test red until the audit named in the invariant is
//! performed.
//!
//! Five remain: refuted prose, the one slot un-push, the files a
//! byte-order primitive may appear in, the method surface of
//! `CommandStream`, and the places a raw fd
//! number becomes an fd — each a closed set, not an enumeration of ways
//! to get it wrong. The call sites of the layer split are not
//! scanned: L2 is `src/command_stream.rs`'s `CommandStream`, whose
//! private field leaves the L1 wire codec unreachable from the command
//! stream's callers.
//!
//! # Refuted prose
//!
//! `prose_does_not_restate_refuted_shutdown_claims` rejects phrases the code contradicts,
//! each of the kind a serve-loop rustdoc or the CHANGELOG tends to state: the serve loop's
//! `Ok(())` is reached by a local `RpcSession::close_session` exactly as by a peer close, and
//! what a transport `shutdown` does to bytes already received is platform- and
//! backend-dependent (macOS discards the kernel queue, Linux keeps it, `mem` keeps it like
//! Linux by design, a transport may hold a buffered leftover). The same claim tends to be restated
//! in several places and corrected in one, so the test pins every `.rs` under `src/` plus
//! `CHANGELOG.md`: a restatement trips the build rather than the next reviewer. Rephrase,
//! don't route around — if a phrase is needed to state something *true*, narrow the phrase in
//! the test.
//!
//! # The slot un-push
//!
//! A slot leaves the pool only with the whole session (`on_session_dead`, `session` module doc
//! "Session end"), with one exception (`session` module doc "Slot pool" "Leaving"): a
//! server's callback slot whose connection-init write never reached the client.
//! `SlotClaim::retire` is the one un-push: it takes the slot out of the pool while this thread
//! still claims it, so no other sender can pick it and end the session on its failure.
//!
//! The compiler holds most of this. The slot vector is the private field of `SlotPool`, in
//! `rpc/session.rs`'s child module `slot_pool`, whose API pushes, iterates and indexes and
//! removes only through `unpush_retired` and `clear_at_session_end`. Only that module builds a
//! `ConnSlot` or a `SlotPool`, so a pooled slot cannot be overwritten with a new one. A whole
//! pool can: `ConnState::slots` is assignable, so the pool of a second `ConnState` would replace
//! it. What the compiler cannot see is who calls the two removals and that constructor.
//! `slot_unpush_has_exactly_one_path` pins one call site each, matched as a whole word so a path
//! call counts: `unpush_retired` in `retire`, `clear_at_session_end` in `on_session_dead`,
//! `retire` itself in `add_callback_slot_and_init`, and `ConnState::new` in `with_shared`. It
//! also pins the module's surface — its `fn` count, one `Vec<ConnSlot>` (the field), no child
//! module, no `derive` (a `Default` would let `mem::take` empty a pool), `SlotPool`'s field
//! and `ConnSlot`'s `_pooled` private — so a method that hands the vector out or removes a slot
//! another way is a deliberate edit here. Swapping the pools of two live sessions is not
//! checked.
//!
//! A NEW un-push of a slot the peer holds as a working connection, or one that carried a
//! frame, brings back the partial session that plan 2-24 removed: the peer's books count
//! frames on it (a oneway number, a `DEC_STRONG`, a reply) that the session would then go on
//! without, or the peer sends on it and ends the session later. It MUST end the session instead
//! (`fail_session`). The scan reads `rpc/session.rs` and each out-of-file child module it
//! declares (that list is pinned too), whatever their `cfg`, so a `#[cfg(test)]` caller counts
//! as well: change the test deliberately rather than route around it.
//!
//! # Byte order
//!
//! L1 (the parcel wire) is little-endian on every host while L2 (the `BC_*`/`BR_*` command
//! stream) and L3 (the UAPI structs) are native, so `byte_order_primitives_stay_pinned`
//! pins which files may spell a byte-order primitive — native, big-endian or little-endian.
//!
//! # `CommandStream`
//!
//! `CommandStream` (in `src/command_stream.rs`) keeps the L1 wire codec out of the L2 command
//! stream by exposing no L1 method. Nothing in the language stops a 16th `fn` there from
//! forwarding one, from handing `&mut self.0` back out, or from declaring a child module that
//! reaches the private field from its own file, so `command_stream_exposes_no_new_forward`
//! pins all three counts: they move only for a deliberate edit.
//!
//! # Raw fd numbers
//!
//! A kernel parcel closes the fds it holds as `OwnedFd`s, never an fd rebuilt
//! from the object bytes (`parcel` module doc "Kernel fds"). That holds only
//! while no other code turns a number read from those bytes into an fd, or
//! closes it, which the type system cannot see: `from_raw_fd`, `borrow_raw`,
//! rustix's `io::close` and `libc::close` take any `i32`.
//! `raw_fd_numbers_become_fds_only_at_the_pinned_sites` lists every call of
//! the first two in `src/`, by file and enclosing `fn`, test code included,
//! and allows none of the closes (any `io::close` or `libc::close` path,
//! `use rustix::io::close` included). The two in `parcel.rs` are the driver-buffer
//! adoption and the `from_ipc_parts` read; the third is a test taking back the
//! number its own `into_raw_fd` released, which does not come from a parcel.
//! A new site — a second adoption, or a close of an fd named by the
//! bytes — must be argued for here.

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
            // Code only: a 0-budget pin must not trip on a needle named in a trailing comment.
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

/// Rejects prose in `src/` and `CHANGELOG.md` the code contradicts; see module doc "Refuted prose".
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
    // Line 0 = found only after joining lines (a phrase wrapped across rustdoc lines).
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

/// Pins the slot pool's removal call sites and surface; see module doc "The slot un-push".
#[test]
fn slot_unpush_has_exactly_one_path() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // A binary pushed to a device cannot see the baked-in sources; host builds always can.
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }
    // Only `session.rs` and its out-of-file child modules see the private pool and `SlotClaim`.
    let rpc_dir = src_root.join("rpc");
    let session_src = fs::read_to_string(rpc_dir.join("session.rs")).expect("rpc/session.rs");
    let lines: Vec<&str> = session_src.lines().map(code_only).collect();
    let mut failures = Vec::new();
    let children = out_of_file_children(&lines);
    if children != ["ref_accounting_tests.rs"] {
        failures.push(format!("`session`'s out-of-file children: {children:?}"));
    }
    let child_srcs: Vec<(String, String)> = children
        .into_iter()
        .map(|rel| {
            let src = fs::read_to_string(rpc_dir.join(&rel)).unwrap_or_default();
            (rel, src)
        })
        .collect();
    for (word, home) in [
        ("unpush_retired", "fn retire("),
        ("clear_at_session_end", "fn on_session_dead("),
        ("retire", "fn add_callback_slot_and_init("),
        ("ConnState::new", "fn with_shared("),
    ] {
        for (rel, src) in &child_srcs {
            let child: Vec<&str> = src.lines().map(code_only).collect();
            let sites = word_sites(&child, word);
            if !sites.is_empty() {
                failures.push(format!("`{word}` in child `{rel}` at lines {sites:?}"));
            }
        }
        match word_sites(&lines, word).as_slice() {
            [line] => {
                let enclosing = enclosing_fn(&lines, *line);
                if !enclosing.is_some_and(|l| l.contains(home)) {
                    failures.push(format!("`{word}` at line {line} is in {enclosing:?}"));
                }
            }
            sites => failures.push(format!("`{word}` has call sites at lines {sites:?}")),
        }
    }
    match lines.iter().position(|l| l.starts_with("mod slot_pool {")) {
        None => failures.push("`mod slot_pool {` is gone".to_string()),
        Some(start) => {
            let len = lines[start..].iter().position(|l| *l == "}");
            let body = &lines[start..=start + len.expect("`slot_pool`'s closing brace")];
            for (needle, want) in [
                ("fn ", 10),
                ("Vec<ConnSlot>", 1),
                ("mod ", 1),
                ("#[derive", 0),
                ("pub(super) struct SlotPool(Vec<ConnSlot>);", 1),
            ] {
                let got = body.iter().filter(|l| l.contains(needle)).count();
                if got != want {
                    failures.push(format!("`{needle}` in `slot_pool`: {got}, not {want}"));
                }
            }
            // The private field and the one literal that fills it: a `pub` on it drops one.
            let pooled = body
                .iter()
                .filter(|l| l.trim_start().starts_with("_pooled: (),"));
            if pooled.count() != 2 {
                failures.push("`_pooled` is no longer private to `slot_pool`".to_string());
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the slot pool's removals or surface moved: {failures:#?}\n\
         INVARIANT: the one slot un-push is SlotClaim::retire, called only \
         for a callback slot whose connection-init write failed; every \
         other slot leaves the pool only with the session (fail_session / \
         close). See this test's doc."
    );
}

/// Files of the `mod x;` items in `rpc/session.rs`'s `lines`, relative to `rpc/`.
fn out_of_file_children(lines: &[&str]) -> Vec<String> {
    let mut children = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(item) = line.trim().strip_suffix(';') else {
            continue;
        };
        let public = item
            .split_once(" mod ")
            .filter(|(vis, _)| vis.starts_with("pub"));
        let Some(name) = item.strip_prefix("mod ").or(public.map(|(_, n)| n)) else {
            continue;
        };
        let path = lines[..i]
            .iter()
            .rev()
            .map(|l| l.trim())
            .take_while(|l| l.starts_with("#["))
            .find_map(|l| l.strip_prefix("#[path = \"")?.strip_suffix("\"]"));
        children.push(path.map_or_else(|| format!("session/{name}.rs"), str::to_string));
    }
    children
}

/// `line` without its comment, or empty for a comment line.
fn code_only(line: &str) -> &str {
    match line.trim_start().starts_with("//") {
        true => "",
        false => line.split("//").next().unwrap_or(line),
    }
}

/// 1-based lines holding `word` as a whole identifier, once per use; its `fn` definition is not.
fn word_sites(lines: &[&str], word: &str) -> Vec<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut sites = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for (at, _) in line.match_indices(word) {
            let before = &line[..at];
            let after = &line[at + word.len()..];
            let whole = !before.ends_with(ident) && !after.starts_with(ident);
            if whole && !before.ends_with("fn ") {
                sites.push(i + 1);
            }
        }
    }
    sites
}

/// The nearest `fn` line at or above 1-based `line`.
fn enclosing_fn<'a>(lines: &[&'a str], line: usize) -> Option<&'a str> {
    lines[..line]
        .iter()
        .rev()
        .map(|l| l.trim_start())
        .find(|l| l.starts_with("fn ") || (l.starts_with("pub") && l.contains(" fn ")))
}

/// Pins where byte-order primitives appear; see module doc "Byte order".
#[test]
fn byte_order_primitives_stay_pinned() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }

    struct Pin {
        needle: &'static str,
        /// True when a hit outside `files` fails too: the primitive belongs nowhere else.
        tree_wide: bool,
        files: &'static [(&'static str, usize)],
    }

    let pins = [
        Pin {
            needle: "_ne_bytes",
            tree_wide: true,
            files: &[
                // `NativeScalar` (2) + `Debug` dump (1) + tests: what stays native (3), L3 fd (2).
                ("parcel.rs", 8),
                // `FlatBinderObject` L3 codec, union accessors (10) + layout/byte-order tests (6).
                ("binder_object.rs", 16),
                // `TransactionData`'s L3 codec and `target` accessors (15) + layout tests (8).
                ("transaction_data.rs", 23),
                // fd/memfd bookkeeping, never parcel wire.
                ("shared_memory/mod.rs", 4),
            ],
        },
        Pin {
            // Java reliable-PFD comm-socket status, which AOSP peeks BIG_ENDIAN; not parcel wire.
            needle: "_be_bytes",
            tree_wide: true,
            files: &[("file_descriptor.rs", 1)],
        },
        Pin {
            // None anywhere: the L3 structs have field-by-field codecs, not a raw-bytes view.
            needle: "transmute::<[u8",
            tree_wide: true,
            files: &[("parcelable.rs", 0)],
        },
        Pin {
            // L2 is native: an LE re-encode reaches the driver byte-swapped even via `write_cmd`.
            needle: "_le_bytes",
            tree_wide: false,
            files: &[("thread_state.rs", 0), ("command_stream.rs", 0)],
        },
        Pin {
            // The same re-encoding without a byte array: `to_le()`, `from_le(..)`, `write_le(..)`.
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
    ];

    let mut failures = Vec::new();
    for pin in &pins {
        let needle = pin.needle;
        let hits = count_call_sites(&src_root, needle);
        for (suffix, want) in pin.files {
            // A 0-budget pin would pass silently if its file moved; pin existence too.
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

    assert!(
        failures.is_empty(),
        "a pinned byte-order primitive moved — see this test's doc:\n{}",
        failures.join("\n")
    );
}

/// Pins every `from_raw_fd` / `borrow_raw` call in `src/`, allows no raw close; module doc.
#[test]
fn raw_fd_numbers_become_fds_only_at_the_pinned_sites() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!("skipping: sources not reachable at {}", src_root.display());
        return;
    }
    // (file under `src/`, enclosing `fn` line prefix), one entry per call.
    const PINNED: &[(&str, &str)] = &[
        // The one adoption: the fds the binder driver installed for a received buffer.
        ("parcel.rs", "pub(crate) unsafe fn from_driver_buffer("),
        // A `from_ipc_parts` caller keeps each FD object's fd open (its `# Safety`).
        ("parcel.rs", "fn kernel_fd_at("),
        // A test taking back the fd `into_raw_fd` released.
        ("file_descriptor.rs", "fn test_parcel_file_descriptor("),
    ];
    let mut found: Vec<(String, String)> = Vec::new();
    visit(&src_root, &mut |path, content| {
        let lines: Vec<&str> = content.lines().map(code_only).collect();
        let rel = path.strip_prefix(&src_root).unwrap().to_string_lossy();
        let rel = rel.replace('\\', "/");
        for (i, line) in lines.iter().enumerate() {
            // A close is never pinned, so each one is reported as unpinned.
            let calls = ["from_raw_fd(", "borrow_raw(", "io::close", "libc::close"]
                .iter()
                .map(|needle| line.matches(needle).count())
                .sum::<usize>();
            let enclosing = enclosing_fn(&lines, i + 1).unwrap_or("<no fn>");
            for _ in 0..calls {
                found.push((rel.clone(), enclosing.to_string()));
            }
        }
    });
    let mut missing = Vec::new();
    for (file, prefix) in PINNED {
        match found
            .iter()
            .position(|(p, f)| p == file && f.starts_with(prefix))
        {
            Some(at) => drop(found.swap_remove(at)),
            None => missing.push(format!("{file}: {prefix}")),
        }
    }
    assert!(
        found.is_empty() && missing.is_empty(),
        "a raw fd number becomes an fd somewhere new, or a pinned site moved — see this \
         test's doc.\nunpinned: {found:#?}\nmissing: {missing:#?}"
    );
}

/// Pins `CommandStream`'s `fn`, `self.0` and `mod` counts; see module doc "`CommandStream`".
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
