// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `contrib/ndk/rsbinder_stream.h` on the host: `tests/c/stream_check.c`
//! runs both ends of a stream in one process, built with AddressSanitizer
//! and UBSan so a death callback touching an unmapped word after `close`
//! fails the test instead of passing by luck. The same program is also
//! compiled as C++. The wire against rsbinder is `run_stream_interop.sh`'s
//! (an emulator); this is what timing there cannot pin down.
//!
//! Linux only: the harness needs a C compiler where the test runs.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
}

/// Compile `tests/c/stream_check.c` with `compiler` and `args`, run it, expect "ok".
fn build_and_run(compiler_env: &str, default: &str, args: &[&str], out: &str) {
    let compiler = std::env::var(compiler_env).unwrap_or_else(|_| default.to_string());
    let exe = Path::new(env!("CARGO_TARGET_TMPDIR")).join(out);
    let built = Command::new(&compiler)
        .args(args)
        .args(["-Wall", "-Wextra", "-Werror", "-O1", "-g", "-pthread"])
        .arg("-I")
        .arg(repo().join("contrib/ndk"))
        .arg("-I")
        .arg(repo().join("rsbinder-fmq/c"))
        .arg(repo().join("tests/c/stream_check.c"))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap_or_else(|e| panic!("{compiler}: {e} (set ${compiler_env})"));
    assert!(
        built.status.success(),
        "{compiler}:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let ran = Command::new(&exe).output().expect("run stream_check");
    assert!(
        ran.status.success() && ran.stdout == b"ok\n",
        "stream_check: exit {:?}\n{}{}",
        ran.status.code(),
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
}

#[test]
fn stream_header_under_sanitizers() {
    build_and_run(
        "CC",
        "cc",
        &[
            "-std=c11",
            "-pedantic",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
        ],
        "stream_check",
    );
}

#[test]
fn stream_header_as_cxx() {
    build_and_run(
        "CXX",
        "c++",
        &["-std=c++17", "-x", "c++"],
        "stream_check_cxx",
    );
}
