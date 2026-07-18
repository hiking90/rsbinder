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
/// slot lookup misses. The two sanctioned callers are the slot's own
/// `serve_blocking_on` exit and `client_transact`'s stale-reply poison (retire
/// a slot whose reply read failed so the desynced stream is never reused).
/// A NEW caller MUST re-audit that every slot-lookup path tolerates a missing
/// slot before being added — this bound guards against accidentally
/// reintroducing a lookup that assumes the slot is always present.
#[test]
fn remove_slot_has_at_most_two_callers() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hits = count_call_sites(&src_root, ".remove_slot(");
    // Pin the location too: a count-only bound would still pass if a
    // sanctioned caller were deleted and an unaudited one added elsewhere.
    assert!(
        hits.iter().all(|(p, _)| p.ends_with("rpc/session.rs")),
        "remove_slot callers must live in rpc/session.rs: {hits:#?}"
    );
    assert!(
        hits.len() <= 2,
        "RpcSessionInner::remove_slot must have at most two callers. \
         Found {} call sites: {:#?}\n\
         INVARIANT: only serve_blocking_on's exit path and \
         client_transact's stale-reply poison may call remove_slot — \
         find_conn / find_conn_pinned must return DeadObject (not panic) \
         on a missing slot. A new caller MUST audit every slot-lookup \
         path before being added.",
        hits.len(),
        hits,
    );
}
