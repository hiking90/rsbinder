// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Large shared-memory probe (Plan 4-7a STAGE3, large-heap half).
//!
//! The rsbinder half of the large `IMemory` cases in
//! `example-hello/cpp/run_imemory_interop.sh`: a heap many times the
//! kernel's 4 MB transaction limit, shared with a real `libbinder` peer
//! and checked end to end in both directions.
//!
//! ```text
//! shm_probe serve <service-name> <bytes>   # blocks
//! shm_probe read  <service-name> <bytes>   # map a peer's heap, check it, write the tail
//! shm_probe tail  <service-name> <bytes>   # print the 16-byte tail a peer wrote
//! ```
//!
//! `serve` publishes an `IMemory` window covering a whole heap of
//! `<bytes>` filled with the shared pattern. `read` checks the geometry
//! and every byte of a peer's window, writes [`RUST_TAIL`] into its last
//! 16 bytes, and reads that back through a second, independently resolved
//! proxy. It prints one line:
//!
//! ```text
//! RESULT shm <bytes> OK <checksum>
//! RESULT shm <bytes> ERROR <detail>
//! ```
//!
//! `tail` prints `RESULT shm-tail <bytes> <text>`. Exit 0 on success, 1
//! otherwise. The pattern (`i % 251`) and the checksum are shared with
//! `example-hello/cpp/imemory_interop.cpp`.

use std::sync::Arc;

use rsbinder::shared_memory::{
    export_heap, BpMemory, IMemory, IMemoryHeap, MemoryBase, MemoryHeapBase,
};
use rsbinder::*;

const RUST_TAIL: &[u8; 16] = b"rust-large-tail\0";
const TAIL_LEN: usize = 16;
const CHUNK: usize = 1 << 20;

fn byte_at(i: usize) -> u8 {
    (i % 251) as u8
}

/// Same order-sensitive sum as the C++ half and `blob_probe`.
fn checksum_step(acc: u32, off: usize, bytes: &[u8]) -> u32 {
    bytes.iter().enumerate().fold(acc, |acc, (i, b)| {
        acc.wrapping_add(((off + i) as u32) ^ (*b as u32))
    })
}

fn serve(name: &str, len: usize) -> Result<()> {
    let server = rsbinder::serve("binder://")?;
    let heap = Arc::new(MemoryHeapBase::new(len, 0)?);
    let mut off = 0;
    while off < len {
        let take = CHUNK.min(len - off);
        let piece: Vec<u8> = (off..off + take).map(byte_at).collect();
        heap.write_at(off, &piece)?;
        off += take;
    }
    let memory = Arc::new(MemoryBase::new(
        heap.clone(),
        export_heap(heap.clone()),
        0,
        len,
    )?);
    let server = server.add(name, memory.export())?;
    println!("SERVING {name} {len}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn open(name: &str) -> Result<BpMemory> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let bp = BpMemory::new(binder);
    bp.resolve()?;
    Ok(bp)
}

/// `Ok(Err(detail))` = the peer answered, but its heap is not the one
/// asked for.
fn read(name: &str, len: usize) -> Result<std::result::Result<u32, String>> {
    let bp = open(name)?;
    let heap = bp.resolve()?;
    if heap.size() < len || bp.offset() != 0 || bp.size() != len {
        return Ok(Err(format!(
            "geometry heap={} offset={} size={}",
            heap.size(),
            bp.offset(),
            bp.size()
        )));
    }
    let mut sum = 0u32;
    let mut buf = vec![0u8; CHUNK];
    let mut off = 0;
    while off < len {
        let take = CHUNK.min(len - off);
        bp.read_at(off, &mut buf[..take])?;
        if let Some(i) = (0..take).find(|&i| buf[i] != byte_at(off + i)) {
            return Ok(Err(format!("payload-mismatch at {}", off + i)));
        }
        sum = checksum_step(sum, off, &buf[..take]);
        off += take;
    }
    bp.write_at(len - TAIL_LEN, RUST_TAIL)?;
    let again = open(name)?;
    let mut back = [0u8; TAIL_LEN];
    again.read_at(len - TAIL_LEN, &mut back)?;
    if &back != RUST_TAIL {
        return Ok(Err("tail-not-visible".into()));
    }
    Ok(Ok(sum))
}

fn tail(name: &str, len: usize) -> Result<String> {
    let bp = open(name)?;
    let mut back = [0u8; TAIL_LEN];
    bp.read_at(len - TAIL_LEN, &mut back)?;
    let end = back.iter().position(|&b| b == 0).unwrap_or(TAIL_LEN);
    Ok(String::from_utf8_lossy(&back[..end]).into_owned())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: shm_probe serve|read|tail <name> <bytes>");
        std::process::exit(2)
    };
    if args.len() != 4 {
        usage();
    }
    let name = &args[2];
    let len: usize = match args[3].parse() {
        Ok(len) if len >= TAIL_LEN => len,
        _ => usage(),
    };
    match args[1].as_str() {
        "serve" => {
            if let Err(e) = serve(name, len) {
                eprintln!("shm_probe: {e:?}");
                std::process::exit(1);
            }
        }
        "read" => match read(name, len) {
            Ok(Ok(sum)) => println!("RESULT shm {len} OK {sum}"),
            Ok(Err(detail)) => {
                println!("RESULT shm {len} ERROR {detail}");
                std::process::exit(1);
            }
            Err(e) => {
                println!("RESULT shm {len} ERROR {e:?}");
                std::process::exit(1);
            }
        },
        "tail" => match tail(name, len) {
            Ok(text) => println!("RESULT shm-tail {len} {text}"),
            Err(e) => {
                println!("RESULT shm-tail {len} ERROR {e:?}");
                std::process::exit(1);
            }
        },
        _ => usage(),
    }
}
