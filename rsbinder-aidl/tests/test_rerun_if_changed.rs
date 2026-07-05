// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `Builder::generate()` emits `cargo:rerun-if-changed=<path>`
//! for every `.aidl` file and directory that contributes to the
//! generated output, so cargo reruns the build script when the user
//! edits a source or imported `.aidl`.
//!
//! These tests exercise the [`Builder::collect_aidl_dependencies`]
//! collector — the same path-set [`Builder::generate`] emits via stdout
//! `cargo:rerun-if-changed=` lines — without depending on stdout
//! capture.

use rsbinder_aidl::Builder;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_aidl(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn contains_path(deps: &[PathBuf], needle: &Path) -> bool {
    deps.iter().any(|d| d == needle)
}

#[test]
fn source_file_is_recorded() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let main_aidl = root.join("com/example/IMain.aidl");
    write_aidl(
        &main_aidl,
        r#"
package com.example;
interface IMain {
    void foo();
}
"#,
    );

    let deps = Builder::new()
        .source(&main_aidl)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    assert!(
        contains_path(&deps, &main_aidl),
        "source file not recorded as dependency: {deps:?}"
    );
}

#[test]
fn resolved_import_is_recorded() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let main_aidl = root.join("com/example/IMain.aidl");
    write_aidl(
        &main_aidl,
        r#"
package com.example;
import com.example.IHelper;
interface IMain {
    void run(IHelper helper);
}
"#,
    );

    let helper_aidl = root.join("com/example/IHelper.aidl");
    write_aidl(
        &helper_aidl,
        r#"
package com.example;
interface IHelper {
    void noop();
}
"#,
    );

    let deps = Builder::new()
        .source(&main_aidl)
        .include_dir(root)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    assert!(
        contains_path(&deps, &main_aidl),
        "source not recorded: {deps:?}"
    );
    assert!(
        contains_path(&deps, &helper_aidl),
        "transitively resolved import not recorded: {deps:?}"
    );
    assert!(
        contains_path(&deps, root),
        "include_dir not recorded: {deps:?}"
    );
}

#[test]
fn directory_source_is_recorded() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let a = root.join("com/example/A.aidl");
    write_aidl(
        &a,
        r#"
package com.example;
parcelable A {
    int x;
}
"#,
    );
    let b = root.join("com/example/B.aidl");
    write_aidl(
        &b,
        r#"
package com.example;
parcelable B {
    int y;
}
"#,
    );

    let deps = Builder::new()
        .source(root)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    assert!(
        contains_path(&deps, root),
        "directory source not recorded as dir-level dependency: {deps:?}"
    );
    assert!(
        contains_path(&deps, &a),
        "file A.aidl not recorded: {deps:?}"
    );
    assert!(
        contains_path(&deps, &b),
        "file B.aidl not recorded: {deps:?}"
    );
}

#[test]
fn dependencies_are_sorted_and_deduped() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let main_aidl = root.join("com/example/IMain.aidl");
    write_aidl(
        &main_aidl,
        r#"
package com.example;
import com.example.IHelper;
interface IMain {
    void run(IHelper helper);
}
"#,
    );

    let helper_aidl = root.join("com/example/IHelper.aidl");
    write_aidl(
        &helper_aidl,
        r#"
package com.example;
interface IHelper {
    void noop();
}
"#,
    );

    let deps = Builder::new()
        .source(&main_aidl)
        // Same dir twice — must dedup to a single entry.
        .include_dir(root)
        .include_dir(root)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    let mut sorted = deps.clone();
    sorted.sort();
    assert_eq!(deps, sorted, "dependencies should be returned sorted");

    let occurrences = deps.iter().filter(|d| d.as_path() == root).count();
    assert_eq!(
        occurrences, 1,
        "duplicate include_dir not deduped: {deps:?}"
    );
}

#[test]
fn package_derived_include_is_recorded() {
    // When a source file's `package com.example;` matches its parent
    // path `<root>/com/example/...`, parse_sources synthesises `<root>`
    // as an additional include (so sibling imports resolve without the
    // user calling `.include_dir()`). That synthesised include should
    // be recorded for rerun-if-changed too.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let main_aidl = root.join("com/example/IMain.aidl");
    write_aidl(
        &main_aidl,
        r#"
package com.example;
import com.example.IHelper;
interface IMain {
    void run(IHelper helper);
}
"#,
    );

    let helper_aidl = root.join("com/example/IHelper.aidl");
    write_aidl(
        &helper_aidl,
        r#"
package com.example;
interface IHelper {
    void noop();
}
"#,
    );

    // No explicit .include_dir() — package-derived resolution only.
    let deps = Builder::new()
        .source(&main_aidl)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    assert!(
        contains_path(&deps, &helper_aidl),
        "import unresolvable without package-derived include — \
         either resolution is broken or the synthesised include is \
         not being tracked: {deps:?}"
    );
}

/// An import that resolves under more than one include directory is rejected
/// as ambiguous (AOSP `import_resolver.cpp` "Duplicate files found") — picking
/// one silently could compile a stale copy of the type.
#[test]
fn ambiguous_import_across_include_dirs_is_diagnostic() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let dep = r#"
package com.example;
parcelable Dep {
    int x;
}
"#;
    write_aidl(&root.join("inc_a/com/example/Dep.aidl"), dep);
    write_aidl(&root.join("inc_b/com/example/Dep.aidl"), dep);

    let main_aidl = root.join("src/com/example/IMain.aidl");
    write_aidl(
        &main_aidl,
        r#"
package com.example;
import com.example.Dep;
interface IMain {
    void foo(in Dep d);
}
"#,
    );

    let err = Builder::new()
        .source(&main_aidl)
        .include_dir(root.join("inc_a"))
        .include_dir(root.join("inc_b"))
        .collect_aidl_dependencies()
        .expect_err("duplicate import must be an error");
    let msg = format!("{err}");
    assert!(
        msg.contains("multiple include directories") || msg.contains("Dep"),
        "expected ambiguous-import diagnostic, got: {msg}"
    );

    // With a single include directory the same import resolves cleanly.
    Builder::new()
        .source(&main_aidl)
        .include_dir(root.join("inc_a"))
        .collect_aidl_dependencies()
        .expect("single include dir must resolve");
}
