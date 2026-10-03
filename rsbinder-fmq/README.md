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

Scope: the `SynchronizedReadWrite` flavor, primitive elements, Linux and
Android (elsewhere the crate compiles and every constructor returns
`Unsupported`). A descriptor from a peer is checked against an
`AttachPolicy` before anything is mapped, and the counters are checked on
every operation. Every access to the shared memory is a Rust atomic, so a
second writer or reader — the peer, another handle, C code — makes the
counters fail their check or the elements wrong, never undefined behavior;
nothing detects it, as with libfmq. See the crate documentation for the
full contract.

For a peer that cannot link `libfmq` — an NDK app, where it is not available —
`c/rsbinder_fmq.h` is the same queue as one C11 header: attach with the same
checks, create a sealed memfd, the counters and the EventFlag protocol, plus
two C++ templates for the NDK backend's `MQDescriptor`. `tests/c_header.rs`
runs it against this crate in both roles. It needs a 64-bit `off_t`: on
32-bit glibc compile with `-D_FILE_OFFSET_BITS=64` (the header stops with
`#error` otherwise); 64-bit glibc, musl and the NDK need no such flag.

Part of [rsbinder](https://github.com/hiking90/rsbinder). Apache-2.0.
