// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What the sources must hold that no compiler pass checks.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `line` holds the keyword `unsafe` (not a part of a longer name such as `unsafe_code`).
fn names_unsafe(line: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices("unsafe").any(|(at, word)| {
        let before = line[..at].chars().next_back().is_some_and(ident);
        let after = line[at + word.len()..].chars().next().is_some_and(ident);
        !before && !after
    })
}

/// Shared memory becomes atomics in `shm` alone (plan 12-2).
///
/// The crate root denies `unsafe_code` and `shm.rs` alone expects it, so an `unsafe` block or
/// `unsafe impl` anywhere else does not compile. What the lint misses is a file that opens with
/// `#![allow(unsafe_code)]`, and that file still has to spell `unsafe` to use it. The sources are
/// read at run time, not embedded, so that a new file is seen; where they are not (a device),
/// the test has nothing to read and passes.
#[test]
fn unsafe_lives_in_shm() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src.is_dir() {
        eprintln!("no sources at {}: nothing to check", src.display());
        return;
    }
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.iter().any(|f| f.ends_with("queue.rs")), "{files:?}");
    let mut found = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy();
        if rel == "shm.rs" || rel.starts_with("shm/") {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("a readable source file");
        for (n, line) in text.lines().enumerate() {
            if !line.trim_start().starts_with("//") && names_unsafe(line) {
                found.push(format!("src/{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "`unsafe` outside `shm`:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_keyword_is_told_from_names_that_contain_it() {
    assert!(names_unsafe("let x = unsafe { f() };"));
    assert!(names_unsafe("unsafe impl Send for X {}"));
    assert!(names_unsafe("#![allow(warnings)] unsafe"));
    assert!(!names_unsafe("#![deny(unsafe_code)]"));
    assert!(!names_unsafe("#![deny(unsafe_op_in_unsafe_fn)]"));
    assert!(!names_unsafe("let not_unsafe = 1;"));
}
