// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Blob probe (Plan 10-2 AC-2.2 STAGE3).
//!
//! The rsbinder half of the blob interop harness: one role answers with
//! a blob for a real `libbinder` client to read, the other reads one a
//! real `libbinder` service wrote.
//!
//! ```text
//! blob_probe serve <service-name>           # blocks
//! blob_probe read  <service-name> <bytes>   # ask for a blob of this size
//! ```
//!
//! `read` prints one line:
//!
//! ```text
//! RESULT blob <bytes> <INLINE|SHARED> <checksum>
//! RESULT blob <bytes> ERROR <detail>
//! ```
//!
//! Exit 0 on the blob asked for, 1 otherwise — the convention the C++
//! half already follows.
//!
//! The wire contract is shared with
//! `example-hello/cpp/blob_interop.cpp`: descriptor
//! "rsbinder.test.blob.IProbe", code 1 = GET (request: int32 length;
//! reply: a blob of that length). The payload is `i % 251` so both sides
//! can check it without shipping the bytes back.

use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.blob.IProbe";
const GET: TransactionCode = FIRST_CALL_TRANSACTION;

/// Same generator as the C++ half.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Cheap, order-sensitive check the C++ half computes the same way.
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().enumerate().fold(0u32, |acc, (i, b)| {
        acc.wrapping_add((i as u32) ^ (*b as u32))
    })
}

struct Probe;

impl Interface for Probe {}

impl Remotable for Probe {
    fn descriptor() -> &'static str {
        DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            GET => {
                let len: i32 = reader.read()?;
                let len = usize::try_from(len).map_err(|_| StatusCode::BadValue)?;
                eprintln!("blob_probe: writing a {len}-byte blob");
                // Immutable, which is what Java's `Parcel.writeBlob`
                // always writes — the form a framework peer expects.
                reply.write_blob(&pattern(len), false)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn serve(name: &str) -> Result<()> {
    let probe = Binder::new(Probe);
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&probe))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

/// `Ok(false)` = the peer answered, but not with the blob asked for; the
/// caller turns that into a non-zero exit, as the C++ half does.
fn read(name: &str, len: usize) -> Result<bool> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let remote = binder.as_remote().ok_or_else(|| {
        eprintln!("blob_probe: {name} is local to this process, not a proxy");
        StatusCode::BadType
    })?;

    let mut data = remote.prepare_transact(true)?;
    data.write(&(len as i32))?;
    let mut reply = remote
        .submit_transact(GET, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);

    let blob = reply.read_blob()?;
    if blob.len() != len {
        println!("RESULT blob {len} ERROR length={}", blob.len());
        return Ok(false);
    }
    let form = if blob.inline().is_some() {
        "INLINE"
    } else {
        "SHARED"
    };
    let bytes = blob.to_vec()?;
    if bytes != pattern(len) {
        println!("RESULT blob {len} ERROR payload-mismatch");
        return Ok(false);
    }
    println!("RESULT blob {len} {form} {}", checksum(&bytes));
    Ok(true)
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: blob_probe serve <name> | read <name> <bytes>");
        std::process::exit(2)
    };
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 3 => {
            if let Err(e) = serve(&args[2]) {
                eprintln!("blob_probe: {e:?}");
                std::process::exit(1);
            }
        }
        Some("read") if args.len() == 4 => {
            let len = match args[3].parse() {
                Ok(len) => len,
                Err(_) => usage(),
            };
            match read(&args[2], len) {
                Ok(true) => {}
                Ok(false) => std::process::exit(1),
                Err(e) => {
                    println!("RESULT blob {len} ERROR {e:?}");
                    eprintln!("blob_probe: {e:?}");
                    std::process::exit(1);
                }
            }
        }
        _ => usage(),
    }
}
