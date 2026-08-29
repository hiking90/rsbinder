// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AOSP `.aidl` fixture sweep.
//!
//! Walks every vendored `.aidl` under `tests/aidl/` and runs each through
//! `Builder::generate()`. Each fixture must either:
//!
//! 1. Generate successfully, in which case the emitted `.rs` is fed to
//!    `syn::parse_file` and checked for an item named after the fixture —
//!    catching codegen regressions that emit syntactically invalid Rust or
//!    silently drop the declaration.
//! 2. Match an entry in `EXPECTED_FAILURES`, in which case the error
//!    message must contain the listed substring — catching silent
//!    behavior drift in the expected-fail set itself.
//!
//! Any unexpected outcome (unlisted failure, allowlisted fixture that
//! suddenly passes, or codegen output that fails those checks) fails the
//! sweep with a single aggregated report.
//!
//! Scope limit worth knowing: `syn::parse_file` proves the output is valid
//! Rust *syntax*, not that it type-checks. `tests/build.rs` compiles a
//! subset of these fixtures for real (including the whole `tests/aidl_v1`
//! tree, which is why it is not swept here); anything outside that subset
//! is covered only to the depth described above.

use rsbinder_aidl::Builder;
use std::path::{Path, PathBuf};

/// AOSP fixtures the rsbinder-aidl generator deliberately refuses.
/// Adding an entry here requires a written rationale — the sweep
/// reports unexpected drift either way (a listed entry that passes is
/// just as serious as an unlisted entry that fails).
struct ExpectedFailure {
    /// Path relative to `tests/aidl/`.
    relative_path: &'static str,
    /// Substring that must appear in the generator error message.
    reason_substr: &'static str,
    /// Why this fixture is expected to fail (keep terse — link the
    /// authoritative source).
    #[allow(dead_code)]
    rationale: &'static str,
}

const EXPECTED_FAILURES: &[ExpectedFailure] = &[
    ExpectedFailure {
        relative_path: "android/aidl/tests/map/Foo.aidl",
        reason_substr: "unknown type 'Map'",
        rationale: "AOSP Rust/C++/NDK backends reject `Map<K,V>` \
                    (aidl_language.cpp:1612-1615). Java-only.",
    },
    ExpectedFailure {
        relative_path: "android/aidl/tests/map/IMapTest.aidl",
        reason_substr: "unknown type 'Map'",
        rationale: "AOSP Rust/C++/NDK backends reject `Map<K,V>` \
                    (aidl_language.cpp:1612-1615). Java-only.",
    },
    ExpectedFailure {
        relative_path: "android/aidl/tests/immutable/Foo.aidl",
        reason_substr: "unknown type 'Map'",
        rationale: "Declares `Map<String, Bar> d`. AOSP Rust/C++/NDK backends \
                    reject `Map<K,V>` (aidl_language.cpp:1612-1615). Java-only.",
    },
    ExpectedFailure {
        relative_path: "android/aidl/tests/immutable/IBaz.aidl",
        reason_substr: "unknown type 'Map'",
        rationale: "Imports `immutable/Foo.aidl`, which declares a Java-only \
                    `Map<K,V>` field; the failure is inherited.",
    },
    ExpectedFailure {
        relative_path: "android/aidl/tests/permission/platform/IProtected.aidl",
        reason_substr: "import 'android.content.AttributionSource' not found",
        rationale: "Imports an Android-framework type that is not vendored \
                    in this fixture tree. AOSP builds resolve it via the \
                    framework AIDL search path.",
    },
];

fn walk_aidl(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            walk_aidl(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("aidl") {
            out.push(p);
        }
    }
}

fn aidl_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tests/aidl")
}

fn sweep_out_dir() -> PathBuf {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("aidl_fixture_sweep");
    std::fs::create_dir_all(&out).unwrap();
    out
}

#[test]
fn aidl_fixture_sweep() {
    let root = aidl_root();
    // Sweep the whole vendored tree: narrowing to a subdirectory silently
    // drops fixtures (`android/aidl/loggable/*` was covered nowhere).
    let test_root = root.clone();
    assert!(
        test_root.is_dir(),
        "AOSP fixture root not found at {test_root:?}"
    );

    let out_dir = sweep_out_dir();

    let expected_map: std::collections::HashMap<&str, &ExpectedFailure> = EXPECTED_FAILURES
        .iter()
        .map(|e| (e.relative_path, e))
        .collect();

    let mut files = Vec::new();
    walk_aidl(&test_root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "fixture walk found 0 .aidl files under {test_root:?}"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut allowlist_hit: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let safe = rel.replace('/', "__");
        let output_name = PathBuf::from(format!("sweep_{safe}.rs"));

        let result = Builder::new()
            .source(path.clone())
            .include_dir(&root)
            // `dest_dir` rather than mutating the process-wide `OUT_DIR`,
            // which is not thread-safe under the test harness.
            .dest_dir(&out_dir)
            .output(output_name.clone())
            .generate();

        match (expected_map.get(rel.as_str()), result) {
            (Some(exp), Err(e)) => {
                allowlist_hit.insert(exp.relative_path);
                let msg = format!("{e:#}");
                if !msg.contains(exp.reason_substr) {
                    failures.push(format!(
                        "[allowlist drift] {rel}: expected error containing \
                         {:?}, got {:?}",
                        exp.reason_substr, msg
                    ));
                }
            }
            (Some(exp), Ok(_)) => {
                allowlist_hit.insert(exp.relative_path);
                failures.push(format!(
                    "[allowlist stale] {rel}: listed in EXPECTED_FAILURES \
                     (reason: {:?}) but generation now passes — either the \
                     generator gained support or the upstream fixture \
                     changed; remove or update the allowlist entry",
                    exp.reason_substr
                ));
            }
            (None, Err(e)) => {
                failures.push(format!(
                    "[unexpected failure] {rel}: {e:#}\n\
                     Either fix the regression, or add an EXPECTED_FAILURES \
                     entry with a written rationale and AOSP source link.",
                ));
            }
            (None, Ok(())) => {
                let generated_path = out_dir.join(&output_name);
                let source = match std::fs::read_to_string(&generated_path) {
                    Ok(s) => s,
                    Err(e) => {
                        failures.push(format!(
                            "[output missing] {rel}: cannot read generated \
                             file {generated_path:?}: {e}"
                        ));
                        continue;
                    }
                };
                match syn::parse_file(&source) {
                    Err(e) => failures.push(format!(
                        "[syntactic regression] {rel}: generated Rust does \
                         not parse with syn::parse_file: {e}\n\
                         (generated file: {generated_path:?})"
                    )),
                    Ok(_) => {
                        // An empty-but-valid file parses fine, so anchor on the
                        // declaration actually being emitted.
                        let stem = path.file_stem().unwrap().to_string_lossy();
                        if !source.contains(&format!("pub mod {stem}"))
                            && !source.contains(&format!("pub mod r#{stem}"))
                        {
                            failures.push(format!(
                                "[missing declaration] {rel}: generated Rust \
                                 contains no `pub mod {stem}` — the fixture's \
                                 declaration was dropped\n\
                                 (generated file: {generated_path:?})"
                            ));
                        }
                    }
                }
            }
        }
    }

    // Any allowlist entry the walk never encountered must be stale —
    // the fixture was renamed or removed upstream.
    for exp in EXPECTED_FAILURES {
        if !allowlist_hit.contains(exp.relative_path) {
            failures.push(format!(
                "[allowlist orphan] {}: listed in EXPECTED_FAILURES but \
                 no matching fixture under {test_root:?}",
                exp.relative_path
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "AIDL fixture sweep found {} regression(s):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
