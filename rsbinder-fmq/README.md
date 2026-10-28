# rsbinder-fmq

AOSP-compatible Fast Message Queue (FMQ) in Rust: the shared-memory ring
Android's `libfmq` builds, with the same layout, counters and futex protocol,
so one end can be C++ on a device and the other this crate. It does not
depend on binder — `rsbinder` adds the AIDL `MQDescriptor` parcelable and the
conversion to this crate's `Descriptor`.

```rust
use rsbinder_fmq::{AttachPolicy, MessageQueue, NOT_EMPTY, NOT_FULL};

// The side that allocates: a ring of 1024 bytes with an EventFlag word.
let mut producer = MessageQueue::<u8>::create(1024, true)?;
let descriptor = producer.descriptor()?;          // fds + grantors; send it to the peer

// The side that receives the descriptor.
let policy = AttachPolicy { max_capacity: 1 << 20, require_seal: true, require_event_flag: true };
let mut consumer = MessageQueue::<u8>::attach(&descriptor, &policy)?;

producer.write_blocking(b"hello", NOT_FULL, NOT_EMPTY, None)?;
let mut buf = [0u8; 5];
consumer.read_blocking(&mut buf, NOT_EMPTY, NOT_FULL, None)?;
assert_eq!(&buf, b"hello");
# Ok::<(), rsbinder_fmq::Error>(())
```

That is libfmq's synchronized flavor: one writer, one reader, and a full ring
refuses the write. The unsynchronized flavor — one writer, any number of
readers, a full ring overwrites its oldest elements — is `UnsyncWriter` and
`UnsyncReader`, with the counter discipline of `android-17.0.0_r1`:

```rust
use rsbinder_fmq::{AttachPolicy, UnsyncCreate, UnsyncRead, UnsyncReader, UnsyncStart, UnsyncWriter};

// The writer allocates; `read_only_readers` seals the memory so that no reader can write it.
let mut writer = UnsyncWriter::<u32>::create(64, &UnsyncCreate::new().read_only_readers(true))?;
let descriptor = writer.descriptor()?;            // send it to each reader

let policy = AttachPolicy { max_capacity: 1 << 16, require_seal: true, require_event_flag: false };
let mut reader = UnsyncReader::<u32>::attach(&descriptor, &policy, UnsyncStart::Now)?;

writer.write(&[1, 2, 3])?;                        // never waits for a reader
let mut buf = [0u32; 3];
match reader.read(&mut buf)? {
    UnsyncRead::Done => assert_eq!(buf, [1, 2, 3]),
    UnsyncRead::Empty => {}                       // fewer than three are there yet
    UnsyncRead::Lost { elements } => eprintln!("lapped by the writer: skipped {elements} elements"),
}
# Ok::<(), rsbinder_fmq::Error>(())
```

A reader keeps its read counter in its own process and maps the queue
read-only; one that falls a whole ring behind is told how many elements it
skipped and resumes half a ring behind the writer. `read_latest` takes the
newest elements and drops the backlog. A libfmq reader of such a queue must
attach with `resetPointers = false` (the default resets the writer's
counters).

Scope: both flavors, primitive elements, Linux and
Android targets with 64-bit atomics (for macOS, WASI and a Linux target
without those atomics the crate compiles and the queue's constructors return
`Unsupported`). `UnsyncReader::attach` needs `shm::RoWord64` as well and
returns `Unsupported` on a target without that type (32-bit x86 without
SSE2), and `UnsyncWriter::create` with `read_only_readers` returns it on a
kernel without `F_SEAL_FUTURE_WRITE` (mainline Linux before 5.1). A
descriptor from a peer is checked against an
`AttachPolicy` before anything is mapped, and the counters are checked on
every operation. Every access to the shared memory is a Rust atomic, so
breaking a flavor's rule — a second writer, or a second reader of a
synchronized queue; the peer, another handle, C code — makes the
counters fail their check or the elements wrong, never undefined behavior;
nothing detects it, as with libfmq. See the crate documentation for the
full contract.

The pieces the queue is built from are public as `rsbinder_fmq::shm`, for
shared-memory protocols other than FMQ: `create` / `add_seals` / `seals` (a
memfd and its seals), `Mapping` (a shared mapping at any byte offset,
read-only or read-write), `Run` (copies in and out of shared memory through
atomics, so a peer's concurrent store is not a data race), `futex_wait` /
`futex_wake` (non-private, so they cross processes), `RoRun` / `RoWord32` /
`RoWord64` (bytes or a word in a read-only mapping, with no method that
stores) and
`verify_received` (refuses a peer's fd unless it is a sealed memfd with every
page in place, so that the peer cannot shrink the file or leave a page
missing under the mapping). memfd, seals and futex
are Linux and Android. `Mapping` needs only `mmap`, so it works on macOS too
(on WASI `Mapping::map` returns `Unsupported`), and `Run` only atomics.
`RoWord64` exists on the 64-bit architectures, on 32-bit x86 with SSE2 and on
32-bit ARM with 64-bit atomics.

For a peer that cannot link `libfmq` — an NDK app, where it is not available —
`c/rsbinder_fmq.h` is the synchronized queue as one C11 header: attach with
the same descriptor and policy checks, create a sealed memfd, the counters
and the EventFlag protocol, plus two C++ templates for the NDK backend's
`MQDescriptor`. Its `mmap` step does not have the two refusals of
`shm::Mapping::map`: a hugetlbfs fd, and on a 32-bit target a ring mapping
of more than `isize::MAX` bytes. `tests/c_header.rs`
runs it against this crate in both roles. It needs a 64-bit `off_t`: on
32-bit glibc compile with `-D_FILE_OFFSET_BITS=64` (the header stops with
`#error` otherwise); 64-bit glibc, musl and the NDK need no such flag.

Part of [rsbinder](https://github.com/hiking90/rsbinder). Apache-2.0.
