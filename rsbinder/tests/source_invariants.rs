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

/// `RpcSessionInner::remove_slot` is `pub(crate)`; callers other than
/// the slot's own `serve_blocking_on` exit break the "structurally
/// unreachable" expect at `find_conn_pinned` ([session.rs:780]) and turn
/// a documented design invariant into a reachable panic on the hot
/// transact path.
#[test]
fn remove_slot_has_exactly_one_caller() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hits = count_call_sites(&src_root, ".remove_slot(");
    assert_eq!(
        hits.len(),
        1,
        "RpcSessionInner::remove_slot must have exactly one caller. \
         Found {} call sites: {:#?}\n\
         INVARIANT: only RpcSession::serve_blocking_on's own exit path \
         may call remove_slot — find_conn / find_conn_pinned rely on it \
         to keep their slot-lookup `expect` panics structurally \
         unreachable. A new caller MUST audit both call sites before \
         being added.",
        hits.len(),
        hits,
    );
}
