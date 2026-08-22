// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared-memory example service.
//!
//! Two ways to share memory over binder, side by side:
//!
//! * `getRegion()` — one [`SharedMemory`] region handed over as a
//!   `ParcelFileDescriptor` (the `android.os.SharedMemory` wire form).
//!   The client maps the same pages and both sides read/write them;
//!   `regionWritten()` is the ordinary binder call used as the
//!   "data is ready" signal.
//! * `nextFrame()` — frames carved out of one [`MemoryDealer`] heap and
//!   handed over as `android.utils.IMemory` binders: no fd and no mmap
//!   per frame once the client has mapped the heap.
//!
//! Run over the kernel binder (needs `rsb_hub` / Android servicemanager):
//!
//!     cargo run -p example-hello --bin shm_service kernel
//!
//! or over a Unix-socket RPC session (fd passing needs the `rpc` feature
//! and the client opting into `FileDescriptorTransportMode::Unix`):
//!
//!     cargo run -p example-hello --features rpc --bin shm_service rpc

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use env_logger::Env;
use example_hello::shm::*;
use rsbinder::service::{kernel, Registry};
use rsbinder::shared_memory::{Allocation, MemoryDealer, SharedMemory};
use rsbinder::*;

/// How many frames stay alive after `nextFrame` returns them. Dropping an
/// `Allocation` returns its block to the dealer, so with a bounded queue
/// the heap is reused instead of exhausted.
const LIVE_FRAMES: usize = 4;

struct ShmService {
    region: SharedMemory,
    dealer: Arc<MemoryDealer>,
    frames: Mutex<VecDeque<Allocation>>,
}

impl ShmService {
    fn new() -> rsbinder::Result<Self> {
        let region = SharedMemory::create(REGION_SIZE)?;
        region.write_at(0, b"hello from shm_service")?;
        Ok(Self {
            region,
            dealer: MemoryDealer::new(DEALER_SIZE, 0)?,
            frames: Mutex::new(VecDeque::new()),
        })
    }
}

impl Interface for ShmService {}

impl IShm for ShmService {
    fn getRegion(&self) -> rsbinder::status::Result<ParcelFileDescriptor> {
        // `to_parcel_fd` dups the fd; the service keeps its own mapping.
        Ok(self.region.to_parcel_fd()?)
    }

    fn regionWritten(&self, offset: i32, len: i32) -> rsbinder::status::Result<()> {
        let (offset, len) = (offset as usize, len as usize);
        let mut buf = vec![0u8; len];
        self.region.read_at(offset, &mut buf)?;
        println!(
            "shm_service: client wrote {len} bytes at {offset}: {:?}",
            String::from_utf8_lossy(&buf)
        );
        Ok(())
    }

    fn nextFrame(&self, seq: i32) -> rsbinder::status::Result<SIBinder> {
        // 4 KiB per frame, page-aligned as a real pixel buffer would be.
        let frame = self.dealer.allocate_page_aligned(4096)?;
        let payload = format!("frame #{seq} @ offset {}", frame.offset());
        frame.write_at(0, payload.as_bytes())?;
        frame.write_at(payload.len(), &[0])?;
        let binder = frame.export();
        let mut frames = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        frames.push_back(frame);
        if frames.len() > LIVE_FRAMES {
            frames.pop_front(); // returns that block to the dealer
        }
        println!(
            "shm_service: frame #{seq} served; dealer free = {} bytes",
            self.dealer.free_space()
        );
        Ok(binder)
    }
}

fn register<R: Registry>(reg: &R) -> rsbinder::Result<()> {
    let binder = BnShm::new_binder(ShmService::new()?).as_binder();
    reg.add_service(SERVICE_NAME, binder)
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    match std::env::args().nth(1).as_deref() {
        Some("kernel") => {
            let host = kernel::Host::new()?;
            register(&host)?;
            println!("shm_service: serving {SERVICE_NAME} over kernel binder");
            host.serve()?;
        }
        #[cfg(feature = "rpc")]
        Some("rpc") => {
            use rsbinder::rpc::FileDescriptorTransportMode;
            use rsbinder::service::rpc;
            let host = rpc::Host::unix(RPC_SOCKET)?;
            // Shared memory is an fd: the session must be allowed to carry
            // fds (SCM_RIGHTS). TCP / vsock / TLS sessions cannot.
            host.server()
                .set_supported_fd_modes(&[FileDescriptorTransportMode::Unix]);
            register(&host)?;
            println!("shm_service: serving {SERVICE_NAME} over RPC at {RPC_SOCKET}");
            host.serve()?;
        }
        _ => {
            eprintln!("usage: shm_service <kernel|rpc>   (rpc needs --features rpc)");
            std::process::exit(2);
        }
    }
    Ok(())
}
