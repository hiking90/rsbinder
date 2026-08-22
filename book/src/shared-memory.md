# Shared Memory

Binder transactions copy their payload through the kernel, and a single
transaction is capped at the binder buffer size (1 MiB per process by
default). For frames, audio chunks, or any buffer you would rather not
copy per call, Android services share **memory** instead: the buffer is an
anonymous shared region, only its file descriptor crosses the binder, and
both processes map the same pages.

rsbinder's `shared_memory` module provides that pattern on Linux, Android
**and macOS**, wire-compatible with Android's own types:

| rsbinder | Android equivalent | on the wire |
|---|---|---|
| `SharedMemory` | `android.os.SharedMemory`, NDK `ASharedMemory` | one `ParcelFileDescriptor` |
| `MemoryHeapBase` / `BnMemoryHeap` / `BpMemoryHeap` | libbinder `IMemoryHeap` (`android.utils.IMemoryHeap`) | handwritten AOSP transaction |
| `MemoryBase` / `BnMemory` / `BpMemory` | libbinder `IMemory` (`android.utils.IMemory`) | handwritten AOSP transaction |
| `MemoryDealer` / `Allocation` | libbinder `MemoryDealer` | `IMemory` windows on one heap |

The full example lives in
[`example-hello/src/bin/shm_service.rs`](https://github.com/hiking90/rsbinder/blob/master/example-hello/src/bin/shm_service.rs)
and
[`shm_client.rs`](https://github.com/hiking90/rsbinder/blob/master/example-hello/src/bin/shm_client.rs);
this page walks through it.

## Which transports can carry a region?

A shared region is a file descriptor, so it travels wherever an fd can:

* **Kernel binder** (Linux with binderfs, Android) — always.
* **Unix-socket RPC** — when both sides opt into fd passing
  (`FileDescriptorTransportMode::Unix`, see below).
* **TCP / vsock / TLS RPC** — never; writing a region into such a parcel
  fails with `StatusCode::BadType`, exactly like a plain
  `ParcelFileDescriptor`.

## 1. One region: `SharedMemory` + `ParcelFileDescriptor`

This is the simplest and the most portable form — it is what Android apps
do with `SharedMemory`. In AIDL the region is just a `ParcelFileDescriptor`:

```aidl
package shm;

interface IShm {
    ParcelFileDescriptor getRegion();
    void regionWritten(int offset, int len);   // "data is ready" signal
    IBinder nextFrame(int seq);                 // see section 2
}
```

### Service side

```rust
use rsbinder::shared_memory::SharedMemory;

struct ShmService {
    region: SharedMemory,
    // …
}

impl ShmService {
    fn new() -> rsbinder::Result<Self> {
        let region = SharedMemory::create(64 * 1024)?;   // page-rounded
        region.write_at(0, b"hello from shm_service")?;  // through our own mapping
        Ok(Self { region })
    }
}

impl IShm for ShmService {
    fn getRegion(&self) -> rsbinder::status::Result<ParcelFileDescriptor> {
        // dup the fd for the parcel; the service keeps its mapping
        Ok(self.region.to_parcel_fd()?)
    }

    fn regionWritten(&self, offset: i32, len: i32) -> rsbinder::status::Result<()> {
        let mut buf = vec![0u8; len as usize];
        self.region.read_at(offset as usize, &mut buf)?;   // the client's bytes
        Ok(())
    }
    // …
}
```

### Client side

```rust
use rsbinder::shared_memory::SharedMemory;

let shm: Strong<dyn IShm> = broker.get_interface(SERVICE_NAME)?;

let pfd = shm.getRegion()?;
let region = SharedMemory::from_fd(pfd.into())?;   // size recovered from the fd

let mut greeting = [0u8; 22];
region.read_at(0, &mut greeting)?;                  // same physical pages

region.write_at(4096, b"hello back")?;              // visible to the service at once
shm.regionWritten(4096, 10)?;                       // tell it to look
```

`SharedMemory::from_fd` accepts anything Android would send here: a memfd,
a macOS POSIX shm object, or a legacy `/dev/ashmem` fd (its size is read
with `ASHMEM_GET_SIZE`). A region that was sealed read-only by the owner
maps read-only automatically (`is_read_only()`), and `write_at` then
returns `PermissionDenied`.

### Synchronisation is yours

The library does not serialise access to the pages: a region is, by
definition, writable by the other process at any time. Use an ordinary
binder call as the hand-off signal (`regionWritten` above), the way AOSP
services do, and prefer the copying accessors `read_at` / `write_at` over
holding a `&[u8]` into the region.

## 2. Many buffers: `MemoryDealer` + `IMemory`

Allocating a region per frame means an fd and an `mmap` per frame on
both sides. AOSP's `MemoryDealer` avoids that: one heap is created and
mapped once, and each frame is an `(offset, size)` window of it — an
`android.utils.IMemory` binder whose reply names the shared heap.

### Service side

```rust
use rsbinder::shared_memory::{Allocation, MemoryDealer};

struct ShmService {
    dealer: Arc<MemoryDealer>,
    frames: Mutex<VecDeque<Allocation>>,   // bounded; dropping returns the block
}

fn nextFrame(&self, seq: i32) -> rsbinder::status::Result<SIBinder> {
    let frame = self.dealer.allocate_page_aligned(4096)?;   // NoMemory when full
    frame.write_at(0, format!("frame #{seq}").as_bytes())?;
    let binder = frame.export();                             // the IMemory binder
    let mut frames = self.frames.lock().unwrap();
    frames.push_back(frame);
    if frames.len() > 4 {
        frames.pop_front();   // Allocation::drop → block back to the dealer
    }
    Ok(binder)
}
```

The allocator is AOSP's `SimpleBestFitAllocator`: 32-byte granules,
best-fit, neighbours coalesce on free, and `allocate_page_aligned` for
buffers that must start on a page. Peers that still hold an exported
binder keep reading the window after the `Allocation` is dropped; only
the dealer's bookkeeping changes — so keep a frame alive until the
consumer is done with it.

### Client side

```rust
use rsbinder::shared_memory::{BpMemory, HeapCache};

let cache = HeapCache::new();        // one per session
for seq in 0..6 {
    let binder = shm.nextFrame(seq)?;
    let frame = BpMemory::new_with_cache(binder, cache.clone());
    let heap = frame.resolve()?;     // GET_MEMORY; HEAP_ID + mmap only the first time
    let mut buf = [0u8; 40];
    frame.read_at(0, &mut buf)?;     // offset is relative to the window
    assert_eq!(cache.len(), 1);      // one mapped heap behind every frame
}
```

Without the cache each `BpMemory` maps the heap on its own (still
correct, just more VMAs); with it, frames from the same dealer resolve to
bare offsets.

Because the transactions are the handwritten AOSP layout, the same
binders work against a C++ `interface_cast<IMemory>` client or a C++
`MemoryHeapBase` server — that interop is part of rsbinder's test gate
(`example-hello/cpp/run_imemory_interop.sh`).

## Running the example

Kernel binder (Linux with `rsb_hub` running, or an Android device):

```bash
cargo run -p example-hello --bin shm_service kernel &
cargo run -p example-hello --bin shm_client kernel
```

Unix-socket RPC (works on macOS too; fd passing needs the `rpc` feature):

```bash
cargo run -p example-hello --features rpc --bin shm_service rpc &
cargo run -p example-hello --features rpc --bin shm_client rpc
```

Over RPC both ends must opt into fd passing — the service with
`host.server().set_supported_fd_modes(&[FileDescriptorTransportMode::Unix])`
and the client with
`broker.session().negotiate_fd_transport(FileDescriptorTransportMode::Unix)`
before its first lookup. Without that, `getRegion()` fails with
`BadType`.

Expected client output:

```text
shm_client: region 65536 bytes, read-only=false, says "hello from shm_service"
shm_client: frame #0 @ offset 0 (window 0+4096 of a 1048576 byte heap, 1 mapped heap(s))
shm_client: frame #1 @ offset 4096 …       (16384 on a 16 KiB-page macOS)
…
shm_client: frame #5 @ offset 0 …          ← a recycled block
```

## What the kernel enforces

| property | Linux / Android (memfd) | macOS (POSIX shm) |
|---|---|---|
| size cannot change after creation | `F_SEAL_SHRINK \| F_SEAL_GROW` | inherent: one `ftruncate` only |
| read-only for peers (`FLAG_READ_ONLY`, `seal_read_only`) | `F_SEAL_FUTURE_WRITE` | the owner exports an `O_RDONLY` fd |
| receiver can verify | `F_GET_SEALS` | `fcntl(F_GETFL)` |

`MemoryHeapBase::seals()` reports these as the same `SEAL_*` bits on every
platform, and `MappedHeap::from_fd_strict` rejects a sender whose claims
the kernel does not back. One macOS difference to know: sealing read-only
*after* a writable fd was already handed out does not revoke that fd —
create the region with `FLAG_READ_ONLY` when peers must never write.
