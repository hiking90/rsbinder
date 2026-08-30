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

/// `RpcSessionInner::remove_slot` is `pub(crate)` and safe to call from more
/// than one site now that `find_conn` / `find_conn_pinned` return
/// `Err(StatusCode::DeadObject)` (not `expect`-panic) when their reentrant
/// slot lookup misses. The three sanctioned callers are the slot's own
/// `serve_blocking_on` exit and `client_transact`'s two poison paths (a
/// transport-level send failure, and a stale-reply read failure) — both
/// retire a slot whose peer is gone / stream is desynced so it is never
/// reused, and the send-failure one is what lets a serve-less client session
/// reach death detection (`remove_slot`'s empty-pool hook, Plan 2-17 A.1b).
/// A NEW caller MUST re-audit that every slot-lookup path tolerates a missing
/// slot before being added — this bound guards against accidentally
/// reintroducing a lookup that assumes the slot is always present. The scan is
/// `cfg`-blind (it reads every `.rs` under `src/`), so a future `#[cfg(test)]`
/// caller counts too: raise the number deliberately rather than route around
/// it.
#[test]
fn remove_slot_has_exactly_four_callers() {
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
        4,
        "RpcSessionInner::remove_slot must have exactly four callers. \
         Found {} call sites: {:#?}\n\
         INVARIANT: only serve_blocking_on's exit path, \
         client_transact's send-failure / stale-reply poisons, and the \
         incoming-connection attach's spawn-failure rollback may call remove_slot — \
         find_conn / find_conn_pinned must return DeadObject (not panic) \
         on a missing slot. A new caller MUST audit every slot-lookup \
         path before being added.",
        hits.len(),
        hits,
    );
}
