# ParcelFileDescriptor

Binder IPC typically transfers structured data -- integers, strings, parcelables -- but
sometimes you need to pass a **file descriptor** from one process to another. The
`ParcelFileDescriptor` type makes this possible by wrapping an `OwnedFd` so that it
can be serialized into a Binder `Parcel`, sent across process boundaries, and
deserialized on the other side.

Common use cases include sending pipe endpoints to a service so it can stream data
back, sharing access to an open file or socket, and implementing `dump()` for
diagnostic output.

## Creating a ParcelFileDescriptor

`ParcelFileDescriptor::new()` accepts any type that implements `Into<OwnedFd>`,
including `std::fs::File`, `OwnedFd`, and the file descriptors returned by
`rustix::pipe::pipe()`.

```rust
use std::fs::File;

// From an existing File
let file = File::open("/dev/null").unwrap();
let pfd = rsbinder::ParcelFileDescriptor::new(file);

// From a pipe created with rustix
let (reader, writer) = rustix::pipe::pipe().unwrap();
let read_pfd = rsbinder::ParcelFileDescriptor::new(reader);
let write_pfd = rsbinder::ParcelFileDescriptor::new(writer);
```

Since 0.10.0 `ParcelFileDescriptor` also implements `From<std::fs::File>`
and `From<OwnedFd>`, so the common concrete cases work with plain
conversions — `let pfd: ParcelFileDescriptor = file.into();`. The generic
`new()` covers any other `Into<OwnedFd>` source.

## Sending a File Descriptor to a Service

A typical pattern is to create a pipe, wrap one end in a `ParcelFileDescriptor`,
send it to a service method, and then read from or write to the other end locally.

The following example is based on the `test_parcel_file_descriptor` test in the
rsbinder test suite:

```rust
use std::io::{Read, Write};

let (mut read_file, write_file) = build_pipe();
let write_pfd = rsbinder::ParcelFileDescriptor::new(write_file);

// Send the write end to the service; it returns a (duplicated) copy
let result_pfd = service.RepeatParcelFileDescriptor(&write_pfd)?;

// Write through the returned file descriptor
file_from_pfd(&result_pfd).write_all(b"Hello")?;

// Read from the original pipe's read end
let mut buf = [0u8; 5];
read_file.read_exact(&mut buf)?;
assert_eq!(&buf, b"Hello");
```

Because the service duplicates the descriptor before returning it (see the next
section), both the caller and the service hold independent handles to the same
underlying pipe.

## Duplicating File Descriptors in a Service

When a service receives a `ParcelFileDescriptor`, it usually needs to **duplicate**
the descriptor before returning it or storing it. This avoids ownership conflicts
and ensures each side can close its handle independently.

Since 0.10.0 the type has a built-in `ParcelFileDescriptor::try_clone()`
that duplicates the underlying descriptor with `fcntl(F_DUPFD_CLOEXEC)`
(the duplicate is always close-on-exec, matching AOSP
`ParcelFileDescriptor::dup`), so no hand-rolled `dup` helper is needed.
A service method that repeats a file descriptor back to the caller is then
straightforward:

```rust
use rsbinder::ParcelFileDescriptor;

fn RepeatParcelFileDescriptor(
    &self,
    read: &ParcelFileDescriptor,
) -> rsbinder::BinderResult<ParcelFileDescriptor> {
    Ok(read.try_clone()?)
}
```

## Working with File Descriptor Arrays

AIDL interfaces can accept and return arrays of `ParcelFileDescriptor`. The
pattern for reversing an array -- a common test case -- illustrates how to
combine `try_clone()` with iterator combinators:

```rust
fn ReverseParcelFileDescriptorArray(
    &self,
    input: &[ParcelFileDescriptor],
    repeated: &mut Vec<Option<ParcelFileDescriptor>>,
) -> rsbinder::BinderResult<Vec<ParcelFileDescriptor>> {
    repeated.clear();
    for fd in input {
        repeated.push(Some(fd.try_clone()?));
    }
    input.iter().rev().map(|fd| Ok(fd.try_clone()?)).collect()
}
```

The `repeated` output parameter receives a copy of the input in the original
order, while the return value contains the input in reverse order. Every
descriptor is duplicated so that each `Vec` owns its own set of file handles.

## Helper Functions

Two small helpers cover most of what application code needs.

### build_pipe

Creates a Unix pipe and returns both ends as `std::fs::File` values. `pipe()`
hands back `OwnedFd`s and `File` converts from one, so no `unsafe` is involved:

```rust
use std::fs::File;

fn build_pipe() -> (File, File) {
    let (reader, writer) = rustix::pipe::pipe().expect("error creating pipe");
    (File::from(reader), File::from(writer))
}
```

### file_from_pfd

Converts a `ParcelFileDescriptor` reference into a `File` suitable for use
with the standard `Read` and `Write` traits. The descriptor is cloned first
so the original `ParcelFileDescriptor` remains valid:

```rust
use std::fs::File;
use rsbinder::ParcelFileDescriptor;

fn file_from_pfd(fd: &ParcelFileDescriptor) -> File {
    fd.as_ref()
        .try_clone()
        .expect("failed to clone file descriptor")
        .into()
}
```

## File Descriptors over RPC

`ParcelFileDescriptor` also crosses the socket-based
[RPC transport](./rpc-transport.md) (the `rpc` Cargo feature). Unlike
kernel binder, FD passing over RPC is **opt-in and negotiated per
connection**: the server declares the modes it accepts with
`RpcServer::set_supported_fd_modes`, and the client requests one with
`RpcSession::negotiate_fd_transport` (or the `fd_mode` knob on the
`RpcUnixClientConfig` builder). Descriptors then travel out-of-band via
`SCM_RIGHTS`, which requires a Unix-domain socket and the android-14+
wire version. Service and client code using `ParcelFileDescriptor` is
otherwise unchanged. See
[RPC Transport](./rpc-transport.md#capabilities) for details.

## Tips

- **Each side owns its own descriptor.** Serializing a `ParcelFileDescriptor`
  makes the kernel duplicate the fd for the receiver, so the two handles are
  independent and close order does not matter.

- **`ParcelFileDescriptor` is not `Clone`** — it wraps an `OwnedFd`, which owns
  the descriptor. Use `try_clone()`, and use it before storing or re-returning
  a descriptor you received.

- **`try_clone()` can fail** when the process is out of file descriptors.
  Propagate it with `?` rather than `unwrap()`; a `StatusCode` converts into
  `Status` automatically.
