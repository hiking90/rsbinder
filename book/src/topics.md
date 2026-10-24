# Topics (one publisher, many subscribers)

**Experimental.** The `rsbinder-topic` crate carries records from one
publisher to any number of subscribers over kernel binder. A binder call sets
each subscription up; after it the records travel through shared memory, and
no binder transaction is made for them:

- The publisher allocates the topic's memory, a ring of records in a sealed
  memfd, and every subscriber maps it read-only. One copy into the ring serves
  all of them.
- A publish does not wait for a subscriber. When the ring is full the oldest
  records are overwritten, and a subscriber that had not read them is told how
  many it lost.
- A subscriber writes one small memfd of its own, its control page, which the
  publisher maps read-only. Nothing a subscriber does reaches the ring or
  another subscriber.

The crate is separate from `rsbinder` and sits in the Experimental
[stability tier](./stability-tiers.md): its API and its shared-memory layout
may change in any release. A publisher and a subscriber of different layout
versions refuse each other (`ITopic::ERROR_VERSION`).

## Topic, stream or callback

| | Topic | [Stream](./streaming.md) | `oneway` callback |
|---|---|---|---|
| Receivers | any number | one | one per registration |
| Who allocates the shared memory | the publisher, once for all | the consumer | none: every value is a transaction |
| A receiver that falls behind | loses the oldest records (`RecvError::Lagged`), or with backpressure holds the publisher up to a timeout and is then taken out | holds the sender back; nothing is lost | calls queue in the receiver's async buffer, and fail once it is full |
| Transports | kernel binder | kernel binder and RPC | kernel binder and RPC |

A topic suits values that many processes read and where the newest matters
more than every one: sensor readings, frames, state that keeps changing. A
receiver that must see every value however slow it is uses a stream. A value
sent now and then, to one process, is a `oneway` method.

## A topic through the service manager

Add `rsbinder-topic` beside `rsbinder`, at the same version. The example below
defines its value type with `#[derive(rsbinder::Parcelable)]`, which needs
`rsbinder`'s `macros` feature:

```toml
[dependencies]
rsbinder = { version = "…", features = ["macros"] }
rsbinder-topic = "…"
```

A topic carries values of one type, a `Payload`: any parcelable, derived or
generated from `.aidl`, or `Bytes`. The two ends compare the type's name when
a subscription is set up; for a derived parcelable that is the descriptor, so
it names the package:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:payload}}
```

Both processes run the binder thread pool (see [Constraints](#constraints)):

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:init}}
```

The publisher makes the topic, registers its binder, and publishes. Any
thread may publish: `publish` takes `&self`.

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:publish}}
```

A subscriber takes a subscription with one twoway call, `Subscriber::request`.
After it, receiving makes no binder call. `recv` waits for a record; every
error but `Closed` leaves the subscription as it was:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_readings.rs:subscribe}}
```

The example runs as two processes, with `rsb_hub` running:

```text
cargo run -p rsbinder-topic --example topic_readings -- publish
cargo run -p rsbinder-topic --example topic_readings -- subscribe
```

A subscriber starts at the first record published after its subscription
(`Start::Latest`), so it misses what came before. `SubscriberPolicy::start`
also offers `Start::Last`, whose first receive returns the newest record, and
`Start::Oldest`, which reads everything the ring still holds.

`PublisherPolicy::ring_bytes` sizes the ring (64 KiB by default). A record is
at most half the ring, and how far a subscriber can fall behind before it
loses records is the ring's size over a record's. `PublisherPolicy::for_payload`
works the size out from the largest value and the number of records to keep.

### Handing a topic out from an interface

A topic does not have to be a service of its own. A method of another
interface can return it as an `IBinder`, and the client passes that binder to
`Subscriber::request`:

```aidl
interface ISensors {
    IBinder readings();
}
```

The application's `.aidl` does not import `rsbinder-topic`'s, and must not
copy it into its own AIDL build: the copy generates types of the same names
that `rsbinder-topic` does not take.

### Who may subscribe

`PublisherPolicy::authorize` takes a check on the caller of the subscription
call (`rsbinder::CallingContext`: uid, pid, SELinux context); a refused caller
gets `EX_SECURITY`. `max_subscribers` (64 by default) and `max_per_uid` bound
how many subscriptions the topic holds. An application that wants more than a
predicate implements `ITopic` itself and hands each admitted request to an
`Acceptor` (see the `Acceptor` rustdoc).

## Finding a topic by name

`Publisher::advertise` makes the topic and registers it under a name, and a
`NamedSubscriber` watches the name and connects to whichever publisher holds
it, the two started in either order:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_named.rs:advertise}}
```

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_named.rs:named}}
```

Where a `Subscriber`'s topic ends with `RecvError::Closed`, a
`NamedSubscriber` returns `RecvError::Disconnected` once (the publisher ended
or died, or another took the name) and waits for the next publisher. Records
do not carry over: what was published between two connections is not counted
as lost, and the `Disconnected` marks the gap. Only being taken out by a topic
with backpressure, `Closed::Evicted`, ends it. The watch uses the service
manager's registration notices, which Android 10's service manager does not
have: there `NamedSubscriber::subscribe` fails.

## Losing records, and backpressure

Without backpressure a publish never waits. A subscriber that falls more than
a ring behind gets `RecvError::Lagged(n)` for the `n` records written over and
then the next record. `try_latest` and `recv_latest` skip to the newest
record and report nothing skipped as lost.

`PublisherPolicy::backpressure` makes a publish wait, up to
`Backpressure::timeout`, for its subscribers to read what it would overwrite.
Past the timeout the subscribers in the way are taken out: each is told, and
its receives return `Closed::Evicted`. `Backpressure::window` also bounds how
many records a subscriber may be behind. The wait holds the topic's lock and
sleeps the publishing thread, so from async code a publish runs on a blocking
thread. The `Backpressure` rustdoc lists what waits with it.

## Waiting

| Call | No record unread |
|---|---|
| `try_recv`, `try_latest` | `RecvError::Empty` at once |
| `recv`, `recv_latest` | waits for one |
| `recv_timeout` | waits, then `RecvError::Timeout` |
| `recv_async` (`tokio` feature) | on the blocking pool; with `Wake::Fd`, the reactor |
| `Wait`, `wait_any` (`Wake::Fd`) | the caller polls |

A waiting subscriber first watches the ring for up to 20 µs, then sleeps. By
default (`Wake::Futex`) it sleeps on a word of the topic's memory, and a
publish wakes every such subscriber with one system call, and makes none when
none sleeps. A subscriber made with `Wake::Fd` is woken through a socket of
its own, one system call per waiting subscriber, and has a file descriptor to
poll: one thread waits for several subscriptions with `wait_any` or an event
loop, and `recv_async` holds no thread while it waits. An `Interrupter` ends a
wait from another thread.

## Large values: loans and samples

A value too large to copy twice goes through a pool. With
`PublisherPolicy::pool` the publisher writes a value straight into a slot of a
second memfd, and the ring carries only a record that names the slot:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:lend}}
```

A `Bytes` subscriber receives a lent value as any other, or where it lies with
`recv_sample`, `try_recv_sample`, `try_latest_sample` and `recv_sample_async`.
The `Sample` pins its slot until it is dropped, and the publisher lends no
pinned slot. By default the subscriber reads the slot with atomic loads:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:sample}}
```

The atomic copy is the default because the pool is memory the publisher's
process can write. A publisher that breaks the protocol and writes a pinned
slot can then make the subscriber read wrong bytes, and nothing worse: each
byte is loaded once, into the subscriber's own buffer, so a check made on the
copy still holds when the copy is used. A `&[u8]` over the slot would not have
that property — the bytes could change between a check and the use, which is
undefined behavior in Rust.

A subscriber that can vouch for the publisher's process — this system's own
service, say, and not a process less privileged than the subscriber — calls
`unsafe` `trust_publisher`. Its samples then deref to `[u8]` in place, and its
copies are plain memory copies:

```rust,ignore
{{#include ../../rsbinder-topic/examples/topic_frames.rs:trusted}}
```

```text
cargo run -p rsbinder-topic --example topic_frames -- publish
cargo run -p rsbinder-topic --example topic_frames -- subscribe-trusted
```

`PoolPolicy::depth` (2 by default) is how many of the newest values stay
readable for a subscriber behind: each publish takes back the older ones,
which such a subscriber then gets as `Lagged`. A held `Sample` keeps its value
however old it grows. With backpressure nothing is taken back, and a loan
waits for a slot instead.

## Constraints

- **Loss is the default.** A subscriber that falls behind loses records;
  backpressure trades that for a publisher that waits and takes slow
  subscribers out.
- **One publisher.** Any thread of the publisher's process may publish; no
  other process writes the topic.
- **Kernel binder only.** `Subscriber::request` refuses an RPC binder
  (`InvalidOperation`). A subscriber of a publisher in its own process makes
  no binder call and needs no binder device.
- **The binder thread pool.** A subscriber's process runs it
  (`ProcessState::start_thread_pool`) to learn that the publisher died —
  without it a wait for a record never ends — and that it was taken out. A
  `NamedSubscriber` needs it for the service manager's notice of the name. The
  publisher's process needs it to answer subscription calls.
- **Linux or Android.** The kernel must have `F_SEAL_FUTURE_WRITE` (mainline
  5.1; the Android common kernels from 4.14), and the target must be able to
  load a 64-bit word from a read-only mapping
  (`rsbinder::fmq::shm::RoWord64::PRESENT`). Elsewhere the crate compiles and
  every constructor returns `Unsupported`.
- **No binders or file descriptors in a value.** Neither means anything in the
  subscriber's process; the encoding refuses them (`BadType`,
  `FdsNotAllowed`).
- **Bounded mappings.** The publisher sizes the ring and the pool, and a
  subscriber maps them whole, so `SubscriberPolicy::max_ring_bytes` (16 MiB)
  and `max_pool_bytes` (256 MiB) refuse larger ones.
- **Do not copy the `.aidl`.** A topic is handed out as an `IBinder`; see
  [above](#handing-a-topic-out-from-an-interface).
