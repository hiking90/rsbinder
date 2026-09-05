// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Shared memory over Unix-socket RPC (Plan 4-7a E3 / AC-4.7a.7).
//!
//! The server publishes an `android.utils.IMemory` window onto a
//! `MemoryHeapBase`; the client resolves it through `BpMemory` →
//! `BpMemoryHeap` (two wire-faithful transactions, heap fd via
//! `SCM_RIGHTS`) and both sides read what the other wrote through
//! their own mapping. Also pins the no-opt-in failure mode.
//!
//! Runs on every host with a backing store (Linux, Android, macOS —
//! the latter through the `shm_open` backend).

#![cfg(all(
    feature = "rpc",
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]

use std::sync::Arc;
use std::thread;

use rsbinder::rpc::{FileDescriptorTransportMode as FdMode, RpcServer, RpcSession};
use rsbinder::shared_memory::{
    export_heap, BpMemory, BpMemoryHeap, HeapCache, IMemory, IMemoryHeap, MemoryBase, MemoryDealer,
    MemoryHeapBase, FLAG_READ_ONLY,
};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result as RsResult, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

fn page() -> usize {
    rustix::param::page_size()
}

/// One bound server + a connector for it. Linux/macOS use a filesystem
/// socket; Android uses an abstract name (the `shell` SELinux domain
/// cannot bind filesystem sockets under `/data/local/tmp`).
struct Bound {
    server: Arc<RpcServer>,
    #[cfg(not(target_os = "android"))]
    path: std::path::PathBuf,
    #[cfg(target_os = "android")]
    name: Vec<u8>,
}

impl Bound {
    fn new(tag: &str) -> Self {
        let uniq = format!(
            "rsb_shm_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        #[cfg(not(target_os = "android"))]
        {
            let mut path = std::env::temp_dir();
            path.push(format!("{uniq}.sock"));
            let server = RpcServer::setup_unix_server(&path).expect("bind");
            Self { server, path }
        }
        #[cfg(target_os = "android")]
        {
            let name = uniq.into_bytes();
            let server = RpcServer::setup_unix_server_abstract(&name).expect("bind abstract");
            // The android13plus client below speaks the versioned wire.
            server.set_android13plus(1);
            Self { server, name }
        }
    }

    /// Start serving; returns the join handle.
    fn run(&self) -> thread::JoinHandle<()> {
        let bg = self.server.run_background();
        #[cfg(not(target_os = "android"))]
        for _ in 0..400 {
            if self.path.exists() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        bg
    }

    /// Connect a client, negotiating `Unix` fd mode when `fd_mode`.
    fn connect(&self, fd_mode: bool) -> RpcSession {
        #[cfg(not(target_os = "android"))]
        {
            let client = RpcSession::setup_unix_client(&self.path).expect("connect");
            if fd_mode {
                assert_eq!(
                    client.negotiate_fd_transport(FdMode::Unix).unwrap(),
                    FdMode::Unix
                );
            }
            client
        }
        #[cfg(target_os = "android")]
        {
            use rsbinder::rpc::RpcUnixClientConfig;
            let mut cfg = RpcUnixClientConfig::abstract_name(&self.name, 1);
            if fd_mode {
                cfg = cfg.fd_mode(FdMode::Unix);
            }
            RpcSession::setup_unix_client_android13plus_with_config(cfg).expect("connect abstract")
        }
    }

    fn finish(self, bg: thread::JoinHandle<()>) {
        self.server.stop_accepting();
        let _ = bg.join();
        #[cfg(not(target_os = "android"))]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Server state: the owner heap plus the exported `IMemory` root.
struct Served {
    heap: Arc<MemoryHeapBase>,
    window_offset: usize,
    window_size: usize,
    root: rsbinder::SIBinder,
}

fn serve(flags: u32) -> Served {
    let heap = Arc::new(MemoryHeapBase::new(page() * 4, flags).unwrap());
    let heap_binder = export_heap(heap.clone());
    let window_offset = page();
    let window_size = page() * 2;
    let mem =
        Arc::new(MemoryBase::new(heap.clone(), heap_binder, window_offset, window_size).unwrap());
    Served {
        heap,
        window_offset,
        window_size,
        root: mem.export(),
    }
}

#[test]
fn imemory_window_roundtrips_over_uds_with_fd_mode() {
    let bound = Bound::new("ok");
    bound.server.set_supported_fd_modes(&[FdMode::Unix]);
    let served = serve(0);
    bound
        .server
        .set_root(served.root.clone())
        .expect("set_root");
    let bg = bound.run();

    let client = bound.connect(true);
    let root = client.get_root().expect("get_root");

    // Server writes inside the window before the client maps.
    served
        .heap
        .write_at(served.window_offset + 16, b"server->client")
        .unwrap();

    let bp = BpMemory::new(root);
    let heap = bp.resolve().expect("GET_MEMORY + HEAP_ID + mmap");
    assert_eq!(bp.offset(), served.window_offset);
    assert_eq!(bp.size(), served.window_size);
    assert_eq!(heap.size(), page() * 4, "heap geometry travelled intact");
    assert_eq!(heap.flags(), 0);
    assert_eq!(IMemory::memory(&bp).size(), page() * 4);

    let mut buf = [0u8; 14];
    bp.read_at(16, &mut buf).unwrap();
    assert_eq!(&buf, b"server->client");

    // Client writes through its own mapping; the owner sees it.
    bp.write_at(page(), b"client->server").unwrap();
    let mut back = [0u8; 14];
    served
        .heap
        .read_at(served.window_offset + page(), &mut back)
        .unwrap();
    assert_eq!(&back, b"client->server");

    // Window bounds are enforced on the proxy side.
    assert_eq!(
        bp.read_at(served.window_size - 1, &mut [0u8; 2])
            .unwrap_err(),
        StatusCode::BadValue
    );

    // The heap mapping is resolved once and shared.
    let again = bp.resolve().unwrap();
    assert!(Arc::ptr_eq(&heap, &again));

    drop(bp);
    drop(client);
    bound.finish(bg);
}

#[test]
fn read_only_heap_is_read_only_at_the_client() {
    let bound = Bound::new("ro");
    bound.server.set_supported_fd_modes(&[FdMode::Unix]);
    let served = serve(FLAG_READ_ONLY);
    // The owner may still write.
    served.heap.write_at(served.window_offset, b"ro").unwrap();
    bound
        .server
        .set_root(served.root.clone())
        .expect("set_root");
    let bg = bound.run();

    let client = bound.connect(true);
    let bp = BpMemory::new(client.get_root().unwrap());
    let heap = bp.resolve().unwrap();
    assert_eq!(heap.flags(), FLAG_READ_ONLY);
    // Kernel-backed on every supported host: memfd F_SEAL_FUTURE_WRITE on
    // Linux/Android, an O_RDONLY shm fd on macOS (plan 4-7b).
    let seals = heap.map().unwrap().seals().expect("protections readable");
    assert_ne!(seals & rsbinder::shared_memory::SEAL_FUTURE_WRITE, 0);
    let mut b = [0u8; 2];
    bp.read_at(0, &mut b).unwrap();
    assert_eq!(&b, b"ro");
    assert_eq!(
        bp.write_at(0, b"xx").unwrap_err(),
        StatusCode::PermissionDenied
    );

    drop(bp);
    drop(client);
    bound.finish(bg);
}

/// A direct `IMemoryHeap` root (no `IMemory` indirection) also works,
/// and the heap proxy is usable through the trait surface.
#[test]
fn bare_imemoryheap_root_maps() {
    let bound = Bound::new("heap");
    bound.server.set_supported_fd_modes(&[FdMode::Unix]);
    let heap = Arc::new(MemoryHeapBase::new(page(), 0).unwrap());
    bound
        .server
        .set_root(export_heap(heap.clone()))
        .expect("set_root");
    let bg = bound.run();

    let client = bound.connect(true);
    let bp = BpMemoryHeap::new(client.get_root().unwrap());
    assert_eq!(bp.size(), 0, "not yet mapped");
    let m = bp.map().unwrap();
    assert_eq!(bp.size(), page());
    heap.write_at(0, b"bare").unwrap();
    let mut b = [0u8; 4];
    m.read_at(0, &mut b).unwrap();
    assert_eq!(&b, b"bare");
    let head = bp.base().unwrap().slice(0, 4).unwrap().to_vec();
    assert_eq!(head, b"bare");

    drop(m);
    drop(bp);
    drop(client);
    bound.finish(bg);
}

/// Without mutual fd-mode opt-in the `HEAP_ID` reply cannot carry the
/// fd: the server-side serialize fails and the client sees a clean
/// error, never a panic or a bogus mapping.
#[test]
fn heap_fd_rejected_without_fd_mode() {
    let bound = Bound::new("nofd");
    let served = serve(0);
    bound
        .server
        .set_root(served.root.clone())
        .expect("set_root");
    let bg = bound.run();

    let client = bound.connect(false);
    let bp = BpMemory::new(client.get_root().unwrap());
    let err = bp.resolve().unwrap_err();
    assert!(
        matches!(err, StatusCode::BadType | StatusCode::FailedTransaction),
        "unexpected error {err:?}"
    );
    assert_eq!(bp.size(), 0);

    drop(bp);
    drop(client);
    bound.finish(bg);
}

/// A tiny handwritten service handing out dealer allocations: code 1 →
/// reply = one `IMemory` binder per call (index in the request).
struct BnAllocs(Vec<rsbinder::shared_memory::Allocation>);
impl Remotable for BnAllocs {
    fn descriptor() -> &'static str {
        "rsbinder.test.IAllocs"
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> RsResult<()> {
        match code {
            FIRST_CALL_TRANSACTION => {
                let i: i32 = reader.read()?;
                let a = self.0.get(i as usize).ok_or(StatusCode::BadValue)?;
                reply.write(&a.export())
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> RsResult<()> {
        Ok(())
    }
}

/// Plan 4-7a Phase D: three dealer allocations cross the session as
/// three `IMemory` binders; with a `HeapCache` the client maps the heap
/// once and reads each block at its own offset.
#[test]
fn dealer_allocations_share_one_mapping_through_heap_cache() {
    let bound = Bound::new("dealer");
    bound.server.set_supported_fd_modes(&[FdMode::Unix]);
    let dealer = MemoryDealer::new(page() * 4, 0).unwrap();
    let allocs: Vec<_> = (0..3)
        .map(|i| {
            let a = dealer.allocate(page() / 2 + i * 64).unwrap();
            a.write_at(0, format!("block-{i}").as_bytes()).unwrap();
            a
        })
        .collect();
    let offsets: Vec<usize> = allocs.iter().map(|a| a.offset()).collect();
    bound
        .server
        .set_root(Interface::as_binder(&Binder::new(BnAllocs(allocs))))
        .expect("set_root");
    let bg = bound.run();

    let client = bound.connect(true);
    let root = client.get_root().unwrap();
    let rp = (*root)
        .as_any()
        .downcast_ref::<rsbinder::rpc::RpcProxy>()
        .expect("RpcProxy");
    let cache = HeapCache::new();
    let mut heaps = Vec::new();
    for i in 0..3i32 {
        let mut data = rp.build_request("rsbinder.test.IAllocs").unwrap();
        data.write(&i).unwrap();
        let mut reply = rp
            .transact(FIRST_CALL_TRANSACTION, &data, 0)
            .unwrap()
            .unwrap();
        let mem_binder: rsbinder::SIBinder = reply.read().unwrap();
        let bp = BpMemory::new_with_cache(mem_binder, cache.clone());
        let heap = bp.resolve().unwrap();
        assert_eq!(bp.offset(), offsets[i as usize]);
        let mut buf = [0u8; 7];
        bp.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, format!("block-{i}").as_bytes());
        heaps.push((bp, heap));
    }
    // One heap proxy (one HEAP_ID, one mmap) behind all three windows.
    assert!(Arc::ptr_eq(&heaps[0].1, &heaps[1].1));
    assert!(Arc::ptr_eq(&heaps[1].1, &heaps[2].1));
    assert_eq!(cache.len(), 1);
    assert_eq!(heaps[0].1.size(), page() * 4);

    // Client writes into block 2; the owner sees it at the block's offset.
    heaps[2].0.write_at(8, b"from-client").unwrap();
    let mut back = [0u8; 11];
    dealer.heap().read_at(offsets[2] + 8, &mut back).unwrap();
    assert_eq!(&back, b"from-client");
    drop(heaps);
    assert!(cache.is_empty(), "weak entries are pruned once unused");
    drop(client);
    bound.finish(bg);
}
