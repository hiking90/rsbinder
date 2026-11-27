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
* **TCP / vsock / TLS RPC** — never through the entry API, which refuses
  the `Unix` fd mode on these transports; writing a region into such a
  parcel fails with `StatusCode::FdsNotAllowed`, exactly like a plain
  `ParcelFileDescriptor`.

## 0. One large value: `Parcel::write_blob`

Before reaching for a region of your own, note that a **byte payload** has a
ready-made form. `Parcel::write_blob` carries it inline when it is at most
16 KB and through a shared region when it is larger, tagging the wire so the
reader knows which it got. The bytes are Java's `Parcel.writeBlob`: an int32
length, then what C++ `Parcel::writeBlob` writes. A C++ peer reads the length
itself and passes it to `Parcel::readBlob(len, …)`, as the JNI layer under
Java does:

```rust
parcel.write_blob(&payload, false)?;      // false: the reader may not write it

let blob = parcel.read_blob()?;
let bytes = blob.to_vec()?;               // whichever form it took
```

A reader that takes a large blob on every call keeps one `Vec` and passes it
to `blob.read_into(&mut buf)`. A new `Vec` of that size is memory the
allocator takes from the kernel, a page fault for each page as the copy
reaches it; the `Vec` handed back has its pages already. To read a shared blob
where it lies, `blob.mapped()?` maps the region on its first call and returns
`None` for an inline one. `read_blob` checks a blob's shape, not that its
region can be read: a region the kernel will not let your process read or map
is the error of the first `to_vec`, `read_into` or `mapped`, where AOSP
`Parcel::readBlob` maps as it reads and fails there.

Two properties make it the right default for "a big lump of bytes":

- **The transport decides, not your code.** Where fds cannot travel — vsock,
  TLS, or a data-only parcel — the payload simply goes inline, as AOSP does
  when `mAllowFds` is false. There is nothing to configure and no error to
  handle. `Parcel::allow_fds()` answers the same question if you want to know
  in advance.
- **An immutable blob is sealed** (`F_SEAL_FUTURE_WRITE`), so the reader maps
  a snapshot it cannot modify. Pass `true` to leave the reader's copy
  writable (`BLOB_ASHMEM_MUTABLE`). The writer keeps no handle on the region,
  so nothing the reader writes comes back; for a buffer both sides share, use
  a region of your own (below).

The 16 KB boundary is AOSP's `BLOB_INPLACE_LIMIT`. A writer that wants
another one passes it: `parcel.write_blob_with_limit(&payload, false, limit)`.
The reader does not change, because the tag on the wire says which form the
blob took. [What each form costs](#what-each-form-costs) has the sizes at
which one overtakes the other, and what bounds an inline payload.

Reach for the sections below when you need the region itself: a window into a
larger heap, a lifetime longer than one call, or many buffers out of one
allocation.

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
    fn getRegion(&self) -> rsbinder::BinderResult<ParcelFileDescriptor> {
        // dup the fd for the parcel; the service keeps its mapping
        Ok(self.region.to_parcel_fd()?)
    }

    fn regionWritten(&self, offset: i32, len: i32) -> rsbinder::BinderResult<()> {
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

let shm: Strong<dyn IShm> = client.get::<dyn IShm>(SERVICE_NAME)?;

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

fn nextFrame(&self, seq: i32) -> rsbinder::BinderResult<SIBinder> {
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
let mut kept = None;                 // the heap, held between frames
for seq in 0..6 {
    let binder = shm.nextFrame(seq)?;
    let frame = BpMemory::new_with_cache(binder, cache.clone());
    let heap = frame.resolve()?;     // GET_MEMORY; HEAP_ID + mmap only the first time
    let mut buf = [0u8; 40];
    frame.read_at(0, &mut buf)?;     // offset is relative to the window
    assert_eq!(cache.len(), 1);      // one mapped heap behind every frame
    kept = Some(heap);
}
```

Without the cache each `BpMemory` maps the heap on its own (still
correct, just more VMAs); with it, frames from the same dealer resolve to
bare offsets.

**Hold the heap between frames.** The cache keeps a heap weakly, as AOSP's
`HeapCache` does: the mapping lives while a `BpMemory` or the
`Arc<BpMemoryHeap>` that `resolve` returned is alive. A receiver that drops
each frame before it takes the next, and keeps nothing else, drops the
mapping with it, and the next frame asks for the heap and maps it again.
Keeping the last `resolve` result, as `kept` does above, leaves one mapping
for the whole run. On the machine of the table below a 64 KiB round trip was
95.5 µs without it and 53.9 µs with it.

## What each form costs

One call that carries a payload to a service and gets it back, four ways, by
payload size. Every row copies the payload the same number of times (the
caller writes it and reads the reply, the service reads it and writes the
reply), so the differences are what each form adds around the copies.

| payload | in the transaction (`byte[]`) | blob (`write_blob`) | `SharedMemory` kept | `MemoryDealer` frame |
|---|---|---|---|---|
| 16 KiB | 13.8 µs | 13.9 µs | 11.2 µs | 43.2 µs |
| 32 KiB | 17.5 µs | 48.3 µs | 12.8 µs | 46.9 µs |
| 64 KiB | 40.4 µs | 69.1 µs | 15.9 µs | 53.9 µs |
| 128 KiB | 99.9 µs | 104 µs | 23.0 µs | 64.2 µs |
| 256 KiB | 219 µs | 279 µs | 36.9 µs | 73.5 µs |
| 512 KiB | 477 µs | 551 µs | 65.7 µs | 99.3 µs |
| 1 MiB | does not fit | 879 µs | 123 µs | 156 µs |
| 4 MiB | does not fit | 2.40 ms | 0.455 ms | 1.23 ms |
| 16 MiB | does not fit | 14.5 ms | 6.21 ms | 6.47 ms |

Median round trip over kernel binder, three runs, on one x86_64 laptop CPU
(Ryzen 7 8845HS, Linux 7.2, 2 MiB huge pages for shared memory, `performance`
governor, caller and service each on a core of its own). The numbers are one
machine's; the order of the columns is what carries over. The benchmark is
`bench/ipc-compare` (`run.sh region`).

- **In the transaction** is the cheapest form for a payload that is sent once
  and is small: no region, no fd. Its cost grows fastest with size, and it
  ends at the binder buffer: 1 MiB less two pages for the whole receiving
  process, shared by every transaction it is handling, half of that for
  oneway calls. A call that does not fit fails with `FailedTransaction`.
- **A blob** past its inline limit makes a region for every call, so it pays
  for an fd and for the region's pages each time. Just past 16 KiB that is
  more than the bytes cost in the transaction, which is the case for
  `write_blob_with_limit` with a higher limit: at 32 KiB the inline form took
  17.5 µs and the region 48.3 µs, and the two met near 128 KiB. Choose a limit
  against the buffer above, counting the calls a service may be handling at
  once. Past the buffer a blob is the form that needs no setup. A reader of
  many large blobs uses `read_into` with a `Vec` it keeps.
- **A `SharedMemory` kept** across calls pays for its pages once. The call
  carries a length and each side copies in and out of the mapping. It is the
  cheapest form at every size here, and it asks for a protocol of your own:
  who writes when, and when the other side is done reading.
- **A `MemoryDealer` frame** costs two binder objects a call (the frame's
  `IMemory` each way) on top of the copies, about 40 µs here, and then grows
  like the kept region. It gives many buffers out of one heap, each with a
  lifetime of its own.

For a stream of values to many readers, a region per call is the wrong shape
whatever its size: that is what [Topics](./topics.md) are for.

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

Over RPC both ends must opt into fd passing before connecting. Through the
entry API it is an option on the connect; on an android-14+ session you
built yourself it is `RpcClientConfig::fd_mode` on the client and
`RpcServer::set_supported_fd_modes` on the server, since the connection
header fixes the mode. Only an r34 session negotiates it afterwards, with
`RpcSession::negotiate_fd_transport`:

```rust
// service
rsbinder::serve(&uri)?
    .with(|o| o.fd_modes = Some(vec![FileDescriptorTransportMode::Unix]))
    .add(SERVICE_NAME, BnShm::new_binder(ShmService::new()?))?
    .run()?;

// client
let client = rsbinder::Client::open_with(&uri, |o, _endpoint| {
    o.fd_mode = Some(FileDescriptorTransportMode::Unix);
})?;
```

Both options are rejected on an endpoint that cannot carry fds, so ask
`endpoint.supports_fd_passing()` first when the URI is not fixed (as
[`shm_client.rs`](https://github.com/hiking90/rsbinder/blob/master/example-hello/src/bin/shm_client.rs)
does). Without the opt-in, `getRegion()` fails with `FdsNotAllowed`.

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
