// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! FMQ-over-binder probe (plan 12-fmq F2): `tests::fmq_peer` as a
//! process, so a peer built on AOSP's libfmq and libbinder_ndk
//! (`example-hello/cpp/fmq_interop.cpp`) can sit at either end.
//!
//! ```text
//! fmq_probe serve  <name>
//! fmq_probe client <name> <server-queue|client-queue|corrupt|all> [count] [capacity]
//! ```
//!
//! `client` prints one `RESULT` line per case (three for `corrupt`) and
//! exits non-zero when one does not hold;
//! `example-hello/cpp/run_fmq_interop.sh` drives it.

#[cfg(any(target_os = "linux", target_os = "android"))]
fn main() {
    use tests::fmq_peer::{self, DEFAULT_CAPACITY, DEFAULT_COUNT};

    fn usage() -> ! {
        eprintln!(
            "usage: fmq_probe serve <name> | fmq_probe client <name> <server-queue|client-queue|corrupt|all> [count] [capacity]"
        );
        std::process::exit(2);
    }
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, default: i32| -> i32 {
        args.get(i)
            .map(|s| s.parse().unwrap_or_else(|_| usage()))
            .unwrap_or(default)
    };
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 3 => {
            if let Err(e) = fmq_peer::serve(&args[2]) {
                eprintln!("fmq_probe: serve failed: {e:?}");
                std::process::exit(1);
            }
        }
        Some("client") if args.len() >= 4 => {
            let count = arg(4, DEFAULT_COUNT);
            let capacity = arg(5, DEFAULT_CAPACITY);
            let peer = match fmq_peer::connect(&args[2]) {
                Ok(peer) => peer,
                Err(e) => {
                    println!("RESULT connect {} err:{e:?}", args[2]);
                    std::process::exit(1);
                }
            };
            let outcomes = match args[3].as_str() {
                "server-queue" => vec![fmq_peer::server_queue(&peer, count, capacity)],
                "client-queue" => vec![fmq_peer::client_queue(&peer, count, capacity)],
                "corrupt" => fmq_peer::corrupt(&peer, capacity),
                "all" => fmq_peer::run_all(&peer, count, capacity),
                _ => usage(),
            };
            let mut failed = false;
            for outcome in &outcomes {
                println!("{}", outcome.line);
                failed |= !outcome.ok;
            }
            if failed {
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn main() {
    eprintln!("fmq_probe: the kernel binder and rsbinder::fmq exist on Linux and Android only");
    std::process::exit(2);
}
