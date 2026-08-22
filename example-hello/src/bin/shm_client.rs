// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory example client — see `shm_service` for the service side.
//!
//!     cargo run -p example-hello --bin shm_client kernel
//!     cargo run -p example-hello --features rpc --bin shm_client rpc

use env_logger::Env;
use example_hello::shm::*;
use rsbinder::service::{kernel, Broker};
use rsbinder::shared_memory::{BpMemory, HeapCache, IMemory, IMemoryHeap, SharedMemory};
use rsbinder::*;

fn talk<B: Broker>(broker: &B) -> rsbinder::Result<()> {
    let shm: Strong<dyn IShm> = broker.get_interface(SERVICE_NAME)?;

    // ---- 1. Whole region: one fd, mapped on both sides -------------------
    let pfd = shm.getRegion()?;
    let region = SharedMemory::from_fd(pfd.into())?; // size comes from the fd
    let mut greeting = [0u8; 22];
    region.read_at(0, &mut greeting)?;
    println!(
        "shm_client: region {} bytes, read-only={}, says {:?}",
        region.size(),
        region.is_read_only(),
        String::from_utf8_lossy(&greeting)
    );
    let reply = b"hello back from shm_client";
    region.write_at(4096, reply)?; // lands in the service's mapping directly
    shm.regionWritten(4096, reply.len() as i32)?; // the "ready" signal

    // ---- 2. Frames: many IMemory windows, one heap mapping ---------------
    let cache = HeapCache::new(); // keep one per session
    for seq in 0..6 {
        let binder = shm.nextFrame(seq)?;
        let frame = BpMemory::new_with_cache(binder, cache.clone());
        let heap = frame.resolve()?; // HEAP_ID + mmap only on the first frame
        let mut buf = vec![0u8; 40];
        frame.read_at(0, &mut buf)?;
        let text = String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(40)])
            .into_owned();
        println!(
            "shm_client: {text} (window {}+{} of a {} byte heap, {} mapped heap(s))",
            frame.offset(),
            frame.size(),
            heap.size(),
            cache.len()
        );
    }
    Ok(())
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    match std::env::args().nth(1).as_deref() {
        Some("kernel") => {
            let broker = kernel::Broker::new()?;
            talk(&broker)?;
        }
        #[cfg(feature = "rpc")]
        Some("rpc") => {
            use rsbinder::rpc::FileDescriptorTransportMode;
            use rsbinder::service::rpc;
            let broker = rpc::Broker::unix(RPC_SOCKET)?;
            // Opt into fd passing before the first lookup; without it the
            // service's `getRegion()` reply is rejected with BadType.
            broker
                .session()
                .negotiate_fd_transport(FileDescriptorTransportMode::Unix)?;
            talk(&broker)?;
        }
        _ => {
            eprintln!("usage: shm_client <kernel|rpc>   (rpc needs --features rpc)");
            std::process::exit(2);
        }
    }
    Ok(())
}
