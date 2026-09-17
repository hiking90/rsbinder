// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Pipe probe (Plan 10-3 AC-3.1 kernel half, AC-3.2 STAGE3).
//!
//! A pipe fd in a parcel is only interesting once it has crossed an
//! address space: the parcel carries one descriptor and the payload goes
//! through the kernel pipe, so the two processes are what make it a
//! stream rather than a buffer.
//!
//! ```text
//! pipe_probe serve <service-name>           # blocks
//! pipe_probe read  <service-name> <bytes>   # ask for a stream this long
//! ```
//!
//! `read` prints one line:
//!
//! ```text
//! RESULT pipe <bytes> OK <checksum>
//! RESULT pipe <bytes> ERROR <detail>
//! ```
//!
//! The wire contract is shared with
//! `example-hello/cpp/pipe_interop.cpp`: descriptor
//! "rsbinder.test.pipe.IProbe", code 1 = OPEN (request: int32 length;
//! reply: a `ParcelFileDescriptor` the service is already filling with
//! `i % 251`).

use std::io::{Read, Write};

use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.pipe.IProbe";
const OPEN: TransactionCode = FIRST_CALL_TRANSACTION;

fn byte_at(i: usize) -> u8 {
    (i % 251) as u8
}

/// Same order-sensitive check the C++ half computes.
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
            OPEN => {
                let len: i32 = reader.read()?;
                let len = usize::try_from(len).map_err(|_| StatusCode::BadValue)?;
                let (read_end, write_end) = ParcelFileDescriptor::pipe()?;
                eprintln!("pipe_probe: streaming {len} bytes");
                // Filling happens on its own thread: a pipe holds about
                // 64 KB, so writing here would block this handler until
                // the caller drained it — and the caller has not been
                // given the fd yet.
                std::thread::spawn(move || {
                    let mut written = 0usize;
                    while written < len {
                        let take = (64 * 1024).min(len - written);
                        let piece: Vec<u8> = (written..written + take).map(byte_at).collect();
                        if (&write_end).write_all(&piece).is_err() {
                            return;
                        }
                        written += take;
                    }
                });
                reply.write(&read_end)
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
    std::io::stdout().flush().ok();
    server.run()
}

fn read(name: &str, len: usize) -> Result<()> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let remote = binder.as_remote().ok_or_else(|| {
        eprintln!("pipe_probe: {name} is local to this process, not a proxy");
        StatusCode::BadType
    })?;

    let mut data = remote.prepare_transact(true)?;
    data.write(&(len as i32))?;
    let mut reply = remote
        .submit_transact(OPEN, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);
    let read_end: ParcelFileDescriptor = reply.read()?;

    let mut got = Vec::with_capacity(len);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match (&read_end).read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                println!("RESULT pipe {len} ERROR read={e}");
                return Ok(());
            }
        };
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    if got.len() != len {
        println!("RESULT pipe {len} ERROR short={}", got.len());
        return Ok(());
    }
    if got.iter().enumerate().any(|(i, b)| *b != byte_at(i)) {
        println!("RESULT pipe {len} ERROR payload-mismatch");
        return Ok(());
    }
    println!("RESULT pipe {len} OK {}", checksum(&got));
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: pipe_probe serve <name> | read <name> <bytes>");
        std::process::exit(2)
    };
    let r = match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 3 => serve(&args[2]),
        Some("read") if args.len() == 4 => match args[3].parse() {
            Ok(len) => read(&args[2], len),
            Err(_) => usage(),
        },
        _ => usage(),
    };
    if let Err(e) = r {
        println!("RESULT pipe ERROR {e:?}");
        eprintln!("pipe_probe: {e:?}");
        std::process::exit(1);
    }
}
