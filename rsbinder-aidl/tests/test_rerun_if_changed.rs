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

    // `root` is already package-derived; only a separate dir exercises `include_dir`.
    let extra = root.join("extra");
    fs::create_dir_all(&extra).unwrap();

    let deps = Builder::new()
        .source(&main_aidl)
        .include_dir(&extra)
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
        contains_path(&deps, &extra),
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
    // `root` is also the package-derived include; only the walk records its subdirectory.
    assert!(
        contains_path(&deps, &root.join("com")),
        "walked subdirectory not recorded: {deps:?}"
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
        // The walk and the package-derived include both record `root`; the result has it once.
        .source(root)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    let mut sorted = deps.clone();
    sorted.sort();
    assert_eq!(deps, sorted, "dependencies should be returned sorted");

    let occurrences = deps.iter().filter(|d| d.as_path() == root).count();
    assert_eq!(
        occurrences, 1,
        "`root` recorded twice is not deduped: {deps:?}"
    );
}

#[test]
fn package_derived_include_is_recorded() {
    // parse_sources adds `<root>` as an include when the package matches the path; record it too.
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
        "import unresolvable without package-derived include: {deps:?}"
    );
    // Without the include *directory*, adding a sibling .aidl would never rerun the build.
    assert!(
        contains_path(&deps, root),
        "package-derived include dir not recorded for rerun: {deps:?}"
    );
}

/// An import under two include dirs is an error, as in AOSP `import_resolver.cpp`: no silent pick.
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
        msg.contains("multiple include directories"),
        "expected ambiguous-import diagnostic, got: {msg}"
    );

    // With a single include directory the same import resolves cleanly.
    Builder::new()
        .source(&main_aidl)
        .include_dir(root.join("inc_a"))
        .collect_aidl_dependencies()
        .expect("single include dir must resolve");
}

/// A package-derived dir added after an import resolved still makes it ambiguous, in any order.
#[test]
fn ambiguous_import_does_not_depend_on_source_order() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let dep = "package com.x; parcelable Dep { int x; }";
    write_aidl(&root.join("a/com/x/Dep.aidl"), dep);
    write_aidl(&root.join("b/com/x/Dep.aidl"), dep);
    let ia = root.join("a/com/x/IA.aidl");
    write_aidl(
        &ia,
        "package com.x; import com.x.Dep; interface IA { void f(in Dep d); }",
    );
    let ib = root.join("b/com/x/IB.aidl");
    write_aidl(&ib, "package com.x; interface IB { void g(); }");

    for (first, second) in [(&ia, &ib), (&ib, &ia)] {
        let err = Builder::new()
            .source(first)
            .source(second)
            .collect_aidl_dependencies()
            .expect_err("`com.x.Dep` is under both `a` and `b`");
        assert!(
            format!("{err}").contains("multiple include directories"),
            "{first:?} then {second:?}: {err}"
        );
    }
}

/// An import whose include dir comes from a later source still resolves, in any order.
#[test]
fn import_found_through_a_later_source_does_not_depend_on_order() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let ia = root.join("x/com/a/IA.aidl");
    write_aidl(
        &ia,
        "package com.a; import com.b.Dep; interface IA { void f(in Dep d); }",
    );
    let dep = root.join("y/com/b/Dep.aidl");
    write_aidl(&dep, "package com.b; parcelable Dep { int x; }");

    for (first, second) in [(&ia, &dep), (&dep, &ia)] {
        Builder::new()
            .source(first)
            .source(second)
            .collect_aidl_dependencies()
            .unwrap_or_else(|e| panic!("{first:?} then {second:?}: {e:?}"));
    }
}

/// AOSP `FindImportFile`: an exact file outranks an enclosing type, in any `source()` order.
#[test]
fn exact_import_file_outranks_an_enclosing_match_in_any_order() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let foo = root.join("a/p/IFoo.aidl");
    write_aidl(
        &foo,
        "package p; import p.IOuter.Inner; interface IFoo { void f(in Inner i); }",
    );
    write_aidl(
        &root.join("a/p/IOuter.aidl"),
        "package p; interface IOuter { parcelable Inner { int x; } }",
    );
    let bar = root.join("b/p/IBar.aidl");
    write_aidl(&bar, "package p; interface IBar { void g(); }");
    let exact = root.join("b/p/IOuter/Inner.aidl");
    write_aidl(&exact, "package p.IOuter; parcelable Inner { int y; }");

    for (first, second) in [(&foo, &bar), (&bar, &foo)] {
        let deps = Builder::new()
            .source(first)
            .source(second)
            .collect_aidl_dependencies()
            .unwrap_or_else(|e| panic!("{first:?} then {second:?}: {e:?}"));
        assert!(deps.contains(&exact), "{first:?} then {second:?}: {deps:?}");
        assert!(
            !deps.contains(&root.join("a/p/IOuter.aidl")),
            "{first:?} then {second:?}: {deps:?}"
        );
    }
}

/// A mistyped `source()` path is reported by name, next to the other sources' errors.
#[test]
fn missing_source_is_reported_with_other_errors() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let bad = root.join("com/x/IBad.aidl");
    write_aidl(
        &bad,
        "package com.x; import com.x.Missing; interface IBad { void f(); }",
    );

    let err = Builder::new()
        .source(&bad)
        .source(root.join("com/x/IHelo.aidl"))
        .collect_aidl_dependencies()
        .expect_err("a missing source is an error");
    let msg = format!("{err:?}");
    assert!(msg.contains("does not exist"), "{msg}");
    assert!(
        msg.contains("com.x.Missing"),
        "the earlier import error is kept: {msg}"
    );
}
