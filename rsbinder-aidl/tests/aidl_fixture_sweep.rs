// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 3: AOSP `.aidl` fixture sweep.
//!
//! Walks `tests/aidl/android/aidl/tests/**/*.aidl` (the vendored AOSP
//! fixture set) and runs each through `Builder::generate()`. Each
//! fixture must either:
//!
//! 1. Generate successfully, in which case the emitted `.rs` is fed to
//!    `syn::parse_file` — catching codegen regressions that emit
//!    syntactically invalid Rust.
//! 2. Match an entry in `EXPECTED_FAILURES`, in which case the error
//!    message must contain the listed substring — catching silent
//!    behavior drift in the expected-fail set itself.
//!
//! Any unexpected outcome (unlisted failure, allowlisted fixture that
//! suddenly passes, or codegen output that won't `syn::parse_file`)
//! fails the sweep with a single aggregated report.

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
                    (aidl_language.cpp:1612-1615). Java-only. \
                    See plans/3-3-map-non-goal.md.",
    },
    ExpectedFailure {
        relative_path: "android/aidl/tests/map/IMapTest.aidl",
        reason_substr: "unknown type 'Map'",
        rationale: "AOSP Rust/C++/NDK backends reject `Map<K,V>` \
                    (aidl_language.cpp:1612-1615). Java-only. \
                    See plans/3-3-map-non-goal.md.",
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
    let test_root = root.join("android/aidl/tests");
    assert!(
        test_root.is_dir(),
        "AOSP fixture root not found at {test_root:?}"
    );

    let out_dir = sweep_out_dir();
    std::env::set_var("OUT_DIR", &out_dir);

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
                if let Err(e) = syn::parse_file(&source) {
                    failures.push(format!(
                        "[syntactic regression] {rel}: generated Rust does \
                         not parse with syn::parse_file: {e}\n\
                         (generated file: {generated_path:?})"
                    ));
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
