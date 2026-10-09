// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `cargo:rerun-if-changed` set, read through `Builder::collect_aidl_dependencies`.

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
        // The walk records `root/com`, and so does the import dir `com` under the inferred `root`.
        .source(root)
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");

    let mut sorted = deps.clone();
    sorted.sort();
    assert_eq!(deps, sorted, "dependencies should be returned sorted");

    let com = root.join("com");
    let occurrences = deps.iter().filter(|d| **d == com).count();
    assert_eq!(
        occurrences, 1,
        "`root/com` recorded twice is not deduped: {deps:?}"
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
    // `<root>/com` holds every `com.example.*` candidate; the inferred root is not watched.
    assert!(
        contains_path(&deps, &root.join("com")),
        "import dir under the package-derived root not recorded for rerun: {deps:?}"
    );
    assert!(!contains_path(&deps, root), "{deps:?}");
}

/// A directory under `tmp` reached through a symlink, so raw and canonical paths differ.
fn linked_dir(tmp: &TempDir, name: &str) -> PathBuf {
    let real = tmp.path().join("real");
    fs::create_dir_all(real.join(name)).unwrap();
    #[cfg(unix)]
    {
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        link.join(name)
    }
    #[cfg(not(unix))]
    real.join(name)
}

/// Whether cargo's mtime scan of `deps` reaches `path`; it recurses into dirs and follows symlinks.
fn rescans(deps: &[PathBuf], path: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut seen = std::collections::HashSet::new();
    let mut pending: Vec<PathBuf> = deps.iter().map(|d| fs::canonicalize(d).unwrap()).collect();
    while let Some(dep) = pending.pop() {
        if dep == path {
            return true;
        }
        if !dep.is_dir() || !seen.insert(dep.clone()) {
            continue;
        }
        for entry in fs::read_dir(&dep).unwrap() {
            if let Ok(key) = fs::canonicalize(entry.unwrap().path()) {
                pending.push(key);
            }
        }
    }
    false
}

/// cargo reruns the build script on every build while a recorded path is missing.
fn assert_all_exist(deps: &[PathBuf]) {
    for dep in deps {
        assert!(dep.exists(), "missing {dep:?} reruns every build: {deps:?}");
    }
}

/// A standalone crate whose `target/` (and so `OUT_DIR`) is inside the crate root.
struct StandaloneCrate {
    root: PathBuf,
    out_dir: PathBuf,
    /// Written by cargo after the build script ran, as `deps/*.rlib` is.
    artifact: PathBuf,
}

impl StandaloneCrate {
    fn new(root: PathBuf) -> Self {
        let out_dir = root.join("target/debug/build/hello-0123/out");
        fs::create_dir_all(&out_dir).unwrap();
        let artifact = root.join("target/debug/deps/libhello.rlib");
        write_aidl(&artifact, "");
        Self {
            root,
            out_dir,
            artifact,
        }
    }

    fn assert_target_not_rescanned(&self, deps: &[PathBuf]) {
        assert_all_exist(deps);
        let generated = self.out_dir.join("rsbinder_generated_aidl.rs");
        write_aidl(&generated, "");
        for written in [&self.artifact, &generated] {
            assert!(
                !rescans(deps, written),
                "{written:?} is rescanned, so every build reruns: {deps:?}"
            );
        }
    }
}

const BUILD_SCRIPT_CHILD: &str = "collect_as_a_build_script";

/// Runs `Builder` in a child process with a build script's cwd and `OUT_DIR`, absolute paths out.
fn collect_as_build_script(cwd: &Path, out_dir: &Path, args: &[&str]) -> Vec<PathBuf> {
    // `OUT_DIR` is process-wide; setting it in this process would race the other tests.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([BUILD_SCRIPT_CHILD, "--exact", "--ignored", "--nocapture"])
        .current_dir(cwd)
        .env("OUT_DIR", out_dir)
        .env("RSB_RERUN_ARGS", args.join("\n"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deps: Vec<PathBuf> = stdout
        .lines()
        .filter_map(|line| Some(cwd.join(line.split_once("DEP:")?.1)))
        .collect();
    assert!(!deps.is_empty(), "the child ran no builder: {stdout}");
    deps
}

#[test]
#[ignore = "spawned by collect_as_build_script with RSB_RERUN_ARGS and OUT_DIR set"]
fn collect_as_a_build_script() {
    let Ok(args) = std::env::var("RSB_RERUN_ARGS") else {
        return;
    };
    let mut builder = Builder::new();
    for arg in args.lines() {
        builder = match arg.split_once('=') {
            Some(("source", p)) => builder.source(p),
            Some(("include", p)) => builder.include_dir(p),
            Some(("dest", p)) => builder.dest_dir(p),
            _ => panic!("unknown argument {arg:?}"),
        };
    }
    for dep in builder.collect_aidl_dependencies().expect("collect") {
        println!("DEP:{}", dep.display());
    }
}

/// `hello/IHello.aidl` in `package hello` makes the crate root (`.`, holding `target/`) a root.
#[test]
fn package_root_at_the_crate_root_does_not_rescan_target() {
    let tmp = TempDir::new().unwrap();
    let krate = StandaloneCrate::new(linked_dir(&tmp, "hello_crate"));
    let root = &krate.root;
    write_aidl(
        &root.join("hello/IHello.aidl"),
        "package hello; import hello.IWorld; interface IHello { IWorld get(); }",
    );
    write_aidl(
        &root.join("hello/IWorld.aidl"),
        "package hello; interface IWorld { void ping(); }",
    );

    let deps = collect_as_build_script(root, &krate.out_dir, &["source=hello/IHello.aidl"]);

    krate.assert_target_not_rescanned(&deps);
    assert!(rescans(&deps, &root.join("hello/IWorld.aidl")), "{deps:?}");
    let added = root.join("hello/IAdded.aidl");
    write_aidl(&added, "package hello; interface IAdded { void f(); }");
    assert!(
        rescans(&deps, &added),
        "a new import candidate goes unnoticed: {deps:?}"
    );
}

/// The crate dir is itself `<root>/hello` (a `.aidl` at the crate root in `package hello`).
#[test]
fn crate_dir_named_after_the_package_is_not_rescanned() {
    let tmp = TempDir::new().unwrap();
    let krate = StandaloneCrate::new(linked_dir(&tmp, "hello"));
    let root = &krate.root;
    let ihello = root.join("IHello.aidl");
    write_aidl(
        &ihello,
        "package hello; import hello.IWorld; interface IHello { IWorld get(); }",
    );
    let iworld = root.join("IWorld.aidl");
    write_aidl(&iworld, "package hello; interface IWorld { void ping(); }");

    let source = format!("source={}", ihello.display());
    let deps = collect_as_build_script(root, &krate.out_dir, &[&source]);

    krate.assert_target_not_rescanned(&deps);
    assert!(
        rescans(&deps, &ihello) && rescans(&deps, &iworld),
        "{deps:?}"
    );
}

/// AOSP `-I .` in a crate holding `target/`: only the import dirs under `.` are watched.
#[test]
fn include_dir_dot_holding_target_watches_its_import_dirs() {
    let tmp = TempDir::new().unwrap();
    let krate = StandaloneCrate::new(linked_dir(&tmp, "dot_crate"));
    let root = &krate.root;
    write_aidl(
        &root.join("aidl/hello/IHello.aidl"),
        "package hello; import other.IDep; interface IHello { IDep get(); }",
    );
    write_aidl(
        &root.join("other/IDep.aidl"),
        "package other; interface IDep { void f(); }",
    );
    let lib_rs = root.join("src/lib.rs");
    write_aidl(&lib_rs, "");

    let deps = collect_as_build_script(
        root,
        &krate.out_dir,
        &["include=.", "source=aidl/hello/IHello.aidl"],
    );

    krate.assert_target_not_rescanned(&deps);
    assert!(
        !rescans(&deps, &lib_rs),
        "unrelated sources rerun the build: {deps:?}"
    );
    let added = root.join("other/IAdded.aidl");
    write_aidl(&added, "package other; interface IAdded { void f(); }");
    assert!(
        rescans(&deps, &added),
        "a new import candidate goes unnoticed: {deps:?}"
    );
}

/// `source(".")` in a crate holding `target/`: the walk neither enters nor records `target/`.
#[test]
fn directory_source_holding_target_skips_target() {
    let tmp = TempDir::new().unwrap();
    let krate = StandaloneCrate::new(linked_dir(&tmp, "walk_crate"));
    let root = &krate.root;
    write_aidl(
        &root.join("aidl/hello/IHello.aidl"),
        "package hello; interface IHello { void ping(); }",
    );
    // Another crate's build output: walked, it would be one more `hello.IHello`.
    let stray = root.join("target/debug/build/other-4567/out/hello/IHello.aidl");
    write_aidl(&stray, "package hello; interface IHello { void pong(); }");

    let deps = collect_as_build_script(root, &krate.out_dir, &["source=."]);

    krate.assert_target_not_rescanned(&deps);
    assert!(!rescans(&deps, &stray), "{deps:?}");
    let added = root.join("aidl/hello/IAdded.aidl");
    write_aidl(&added, "package hello; interface IAdded { void f(); }");
    assert!(
        rescans(&deps, &added),
        "a new source goes unnoticed: {deps:?}"
    );
}

/// A symlink to `target/` in a watched dir: cargo's scan follows it, so that dir is not recorded.
#[cfg(unix)]
#[test]
fn symlink_to_target_keeps_its_dir_unrecorded() {
    for args in [
        &["source=aidl"][..],
        &["include=aidl", "source=aidl/hello/IHello.aidl"],
    ] {
        let tmp = TempDir::new().unwrap();
        let krate = StandaloneCrate::new(linked_dir(&tmp, "symlink_crate"));
        let root = &krate.root;
        let ihello = root.join("aidl/hello/IHello.aidl");
        write_aidl(&ihello, "package hello; interface IHello { void ping(); }");
        write_aidl(
            &root.join("aidl/other/IOther.aidl"),
            "package other; interface IOther { void f(); }",
        );
        std::os::unix::fs::symlink(root.join("target"), root.join("aidl/hello/build")).unwrap();

        let deps = collect_as_build_script(root, &krate.out_dir, args);

        krate.assert_target_not_rescanned(&deps);
        assert!(rescans(&deps, &ihello), "{args:?}: {deps:?}");
        if args[0] == "source=aidl" {
            let added = root.join("aidl/other/IAdded.aidl");
            write_aidl(&added, "package other; interface IAdded { void f(); }");
            assert!(
                rescans(&deps, &added),
                "a clean sibling stays watched: {deps:?}"
            );
        }
    }
}

/// `build.build-dir` outside the crate puts `OUT_DIR` there; the crate's `target/` is tagged.
#[test]
fn target_dir_apart_from_out_dir_is_not_rescanned() {
    let tmp = TempDir::new().unwrap();
    let root = &linked_dir(&tmp, "build_dir_crate");
    let out_dir = tmp.path().join("build-dir/debug/build/hello-0123/out");
    fs::create_dir_all(&out_dir).unwrap();
    // cargo writes this tag when it creates a target dir; uplifted artifacts land there later.
    write_aidl(
        &root.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    let artifact = root.join("target/debug/libhello.rlib");
    write_aidl(&artifact, "");
    let stray = root.join("target/debug/hello/IHello.aidl");
    write_aidl(&stray, "package hello; interface IHello { void pong(); }");
    write_aidl(
        &root.join("aidl/hello/IHello.aidl"),
        "package hello; import other.IDep; interface IHello { IDep get(); }",
    );
    write_aidl(
        &root.join("other/IDep.aidl"),
        "package other; interface IDep { void f(); }",
    );

    for args in [
        &["include=.", "source=aidl/hello/IHello.aidl"][..],
        &["source=."][..],
    ] {
        let deps = collect_as_build_script(root, &out_dir, args);
        assert_all_exist(&deps);
        assert!(
            !rescans(&deps, &artifact),
            "{args:?}: `target/` reruns every build: {deps:?}"
        );
        assert!(!rescans(&deps, &stray), "{args:?}: {deps:?}");
        assert!(
            rescans(&deps, &root.join("other/IDep.aidl")),
            "{args:?}: {deps:?}"
        );
    }
}

/// `include_dir(".")` with `dest_dir("gen")` not created yet: the first build records the same set.
#[test]
fn missing_dest_dir_records_the_same_set_before_and_after_the_first_build() {
    let tmp = TempDir::new().unwrap();
    // A workspace member: `target/` (and so `OUT_DIR`) is outside the crate.
    let root = &linked_dir(&tmp, "member");
    let out_dir = tmp.path().join("target/debug/build/member-89ab/out");
    fs::create_dir_all(&out_dir).unwrap();
    write_aidl(
        &root.join("hello/IHello.aidl"),
        "package hello; import hello.IWorld; interface IHello { IWorld get(); }",
    );
    write_aidl(
        &root.join("hello/IWorld.aidl"),
        "package hello; interface IWorld { void ping(); }",
    );
    let args = ["include=.", "source=hello/IHello.aidl", "dest=gen"];

    let first = collect_as_build_script(root, &out_dir, &args);
    let generated = root.join("gen/rsbinder_generated_aidl.rs");
    write_aidl(&generated, "");
    let second = collect_as_build_script(root, &out_dir, &args);

    assert_eq!(
        first, second,
        "the first build's set differs from the next one's"
    );
    assert_all_exist(&first);
    assert!(
        !rescans(&first, &generated),
        "the output reruns every build: {first:?}"
    );
    assert!(
        rescans(&first, &root.join("hello/IWorld.aidl")),
        "{first:?}"
    );
}

/// AOSP `-I` accepts a missing dir; recording it would make cargo rerun every build.
#[test]
fn missing_include_dir_is_accepted_and_not_recorded() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let main_aidl = root.join("hello/IHello.aidl");
    write_aidl(
        &main_aidl,
        "package hello; interface IHello { void ping(); }",
    );
    let missing = root.join("vendr");

    let deps = Builder::new()
        .source(&main_aidl)
        .include_dir(&missing)
        .collect_aidl_dependencies()
        .expect("a missing include dir is not an error");

    assert!(!contains_path(&deps, &missing), "{deps:?}");
    assert_all_exist(&deps);
}

/// An include dir holding `dest_dir` output, created or not: only `<root>/hello` is watched.
#[test]
fn include_dir_holding_the_output_is_not_recorded() {
    let tmp = TempDir::new().unwrap();
    let root = &linked_dir(&tmp, "root");
    let main_aidl = root.join("hello/IHello.aidl");
    write_aidl(
        &main_aidl,
        "package hello; import hello.IWorld; interface IHello { IWorld get(); }",
    );
    write_aidl(
        &root.join("hello/IWorld.aidl"),
        "package hello; interface IWorld { void ping(); }",
    );
    let out = root.join("target/out");
    let generated = out.join("rsbinder_generated_aidl.rs");

    for created in [false, true] {
        if created {
            write_aidl(&generated, "");
        }
        let deps = Builder::new()
            .source(&main_aidl)
            .include_dir(root)
            .dest_dir(&out)
            .collect_aidl_dependencies()
            .expect("collect_aidl_dependencies");
        assert!(!contains_path(&deps, root), "created={created}: {deps:?}");
        assert!(
            contains_path(&deps, &root.join("hello")),
            "created={created}: {deps:?}"
        );
        if created {
            assert!(!rescans(&deps, &generated), "{deps:?}");
        }
    }
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
