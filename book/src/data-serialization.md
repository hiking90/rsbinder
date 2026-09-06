# Storing Values (Data Serialization)

An AIDL definition is a schema, and the Rust that `rsbinder-aidl` generates
from it is already a complete serializer. Until now it had nowhere to write
except a transaction, so a project that wanted to *save* one of its own
types reached for protobuf or serde and ended up maintaining a second
description of the same data.

Two functions remove that second description:

```rust
let bytes = rsbinder::to_bytes(&settings)?;
std::fs::write("settings.bin", &bytes)?;

let settings: Settings = rsbinder::from_bytes(&std::fs::read("settings.bin")?)?;
```

Both need the `rpc` feature — not because anything here talks to a socket,
but because the encoder runs in the same session-less parcel mode the RPC
transport uses, and that mode is what refuses binders and file descriptors.

## What you can store

Anything that implements `Serialize` works, but a type you intend to keep
should be a parcelable — either generated from `.aidl` or declared with the
derive:

```rust
#[derive(rsbinder::Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "myapp.Settings")]
pub struct Settings {
    pub volume: i32,
    pub name: String,
    pub tags: Vec<String>,
}
```

The derive matters for a reason worth being explicit about. A parcelable
writes a length header before its fields, and that header is the entire
forward-compatibility story: a reader built against an older definition
stops at the boundary the writer wrote, and a reader built against a newer
one defaults the fields that were never written. So this works:

| Writer | Reader | Result |
|---|---|---|
| v2 (two extra fields) | v1 | v1's fields intact, the extras skipped |
| v1 | v2 (two extra fields) | v1's fields intact, the extras default |

`rsbinder::to_bytes(&42i32)` is four bytes with no header and no way to
evolve. Wrap what you store in a parcelable.

The one rule for schema changes is the one every positional format has:
**append only.** Reordering or removing a field changes what the bytes at
each position mean, and nothing will tell you — the reader will decode
whatever is there.

## What you cannot store

Binders and file descriptors. Neither means anything outside the process
that produced it: a binder handle is an index into that process's table, and
an fd is an index into its descriptor table. Storing either would produce
bytes that look valid and refer to nothing.

So a value containing one is refused at the point it is written — before any
`dup` — rather than encoded:

```rust
// Err(StatusCode::FdsNotAllowed); the file descriptor is never duplicated.
let bytes = rsbinder::to_bytes(&value_with_an_fd)?;
```

A binder is `BadType` rather than `FdsNotAllowed` — the two conditions are
distinct, and the codes match what Android's `libbinder` returns for each.

The refusal runs in the other direction too. Bytes that merely *look* like a
binder object are never turned into one: the decoder has no object table to
resolve them against and returns an error instead of fabricating a
reference. That property is what makes it safe to read a file you did not
write.

## Reading is strict

`from_bytes` requires the input to be consumed exactly:

| Input | Result |
|---|---|
| Ends inside the value | `Err(NotEnoughData)` |
| Decodes with bytes left over | `Err(BadValue)` |

Leftover bytes usually mean the file was written as a different type, and
returning the partially-decoded value would hide that behind a
plausible-looking wrong answer.

## The bytes are the IPC bytes

There is no separate storage format. `to_bytes` produces exactly what a peer
would receive for the same value over kernel binder or RPC — same codec,
same layout — and since the wire is fixed little-endian, the same bytes on
any architecture. A file written on an aarch64 phone reads on an x86_64
server.

What this buys you: a value can move between a file, a socket and a
transaction without being re-encoded, and one schema describes all three.

## What this is not

It is not a protobuf replacement. The format is positional and untagged,
strings are UTF-16 (roughly double the bytes of UTF-8), and everything is
padded to a 4-byte boundary. There is no self-describing type information,
so nothing can tell you that a file holds a `Settings` rather than a
`Preferences` with a compatible prefix — that is your framing to add if you
need it.

It is for dropping a type you have *already described in AIDL* onto disk,
and it is very good at that.
