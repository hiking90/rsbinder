# Streaming

A binder method returns one value. A method that would return a million rows
either builds the whole list in memory on both sides — and runs into the
transaction size limit long before that — or invents a paging protocol.
`rsbinder::stream` is the third option: the consumer makes an **endpoint** and
hands it to the producer in the one call that opens the stream; the producer
writes items into it and can only run as far ahead as the consumer allows.

It is for a **sequence in which every item matters**. Three neighbours cover
what it is not for:

| What you have | Use | Why |
|---|---|---|
| A sequence of values, all of which must arrive | **`rsbinder::stream`** (this chapter) | Items cannot be dropped, so a slow consumer has to be able to slow the producer down |
| A value that changes, where only the current one matters | A `oneway` callback — see [below](#state-not-a-sequence-use-a-oneway-callback) | Stale values may be dropped, so nothing needs slowing down |
| Bytes — a file, a log, a media stream | [`ParcelFileDescriptor::pipe`](./parcel-file-descriptor.md) | The kernel already does back-pressure on a pipe |
| A large buffer both sides read in place | [Shared memory](./shared-memory.md) | No copy at all |

New in 0.12.0. The same two types run over kernel binder and over
[RPC](./rpc-transport.md), on different wires: over kernel binder the items
travel on a shared-memory **ring** the consumer allocates, over RPC as `oneway`
batches paced by **credit** — see [What is on the wire](#what-is-on-the-wire).
The RPC side has one [requirement on the client](#over-rpc).

## A minimal stream

The method that starts a stream takes the consumer's endpoint —
`rsbinder.stream.StreamEndpoint<T>`, a parcelable shipped in
`rsbinder/aidl/stream/` that `rsbinder-aidl` resolves without a copy of the
file — and returns nothing. The type argument is the item type. Everything
else travels over the stream itself:

```aidl
package demo;

import rsbinder.stream.StreamEndpoint;

parcelable LogLine {
    long timestampMs;
    String text;
}

interface ILogService {
    void tail(in StreamEndpoint<LogLine> endpoint, String tag);
}
```

The service opens a sink on the endpoint and writes from a thread of its own.
The endpoint's type argument makes it a `Sink<LogLine>`. `send` blocks once the
consumer has fallen behind, which is the back-pressure — and the reason the
producer does not run on the binder thread that took the call:

```rust
use rsbinder::stream::{Sink, StreamEndpoint};

impl ILogService for LogService {
    fn r#tail(&self, endpoint: &StreamEndpoint<LogLine>, tag: &str) -> rsbinder::BinderResult<()> {
        let mut sink = Sink::open(endpoint)?;
        let lines = self.open(tag);
        std::thread::spawn(move || {
            for line in lines {
                if sink.send(&line).is_err() {
                    return; // cancelled, or the consumer is gone
                }
            }
            let _ = sink.end();
        });
        Ok(())
    }
}
```

The consumer makes a receiver against its **peer** — a binder in the producer's
process, here the service it is about to call — passes the endpoint it gets
back, and reads:

```rust
use rsbinder::stream::Receiver;

let (mut lines, endpoint) = Receiver::<LogLine>::new(&service.as_binder())?;
service.r#tail(&endpoint, "net")?;
for line in &mut lines {
    println!("{}", line?.text);
}
```

The peer decides two things: which transport the stream runs on (a kernel
proxy makes a ring endpoint, and so does a local object on Linux and Android;
an RPC proxy makes a sink-only one unless the consumer opts in to a ring — see
[A ring over a Unix session](#a-ring-over-a-unix-session) — and so does a local
object elsewhere) and whom the consumer watches for death. Because the opening call is `twoway`,
a refusal — the transport cannot carry the stream, the ring is larger than the
producer accepts, SELinux denies the mapping — comes back as that call's error.

On Android the default SELinux policy grants `memfd_file` access only on a
domain's own memfds, and the ring is the consumer's memfd, which the producer
checks, reads and writes. A producer in another domain therefore needs
`memfd_file { getattr read write map }` on the consumer's domain, besides the
`fd use` any descriptor passed between the two needs — a platform service that
streams to apps carries
`allow <service> appdomain:memfd_file { getattr read write map };` in its own
`.te` — or `Sink::open` fails with `EACCES`.

When the **client is the producer** — an upload — the direction of the call
flips: the client passes a binder of its own (or a `stream::Token` made for the
purpose), the service makes the receiver against it in the handler and returns
the endpoint, and the client opens its sink on the returned value:

```aidl
StreamEndpoint<Chunk> upload(IBinder producer);
```

```rust
// Client
let token = rsbinder::stream::Token::new();
let endpoint = service.r#upload(&token.binder())?;
let mut sink = Sink::open(&endpoint)?; // a Sink<Chunk>
// ... keep `token` alive for as long as the stream runs.
```

Either way the endpoint crosses in the reply or the arguments of one `twoway`
call. A service that wants to start a stream from a `oneway` callback — an
event it pushes to the client — has no reply to carry the endpoint, so the
consumer needs one more `twoway` call to hand it over.

`Receiver<T>` is an `Iterator<Item = BinderResult<T>>` that ends when the
producer does; `recv`, `try_recv` and `recv_timeout` are the same thing one item
at a time, differing only in how long they wait for one. Items are encoded with
the codec [`to_bytes`](./data-serialization.md) uses, so **an item may hold
neither a binder nor a file descriptor**; `send` refuses one that does.

### The item type is checked at compile time

The type argument is where the `.aidl` states what the items are, and both ends
are held to it: the generated method takes a `StreamEndpoint<LogLine>`, only a
`Receiver<LogLine>` makes one, and `Sink::open` on it is a `Sink<LogLine>`. A
consumer that reads the stream as some other type, or a service that writes
one, does not compile.

That matters most for the mismatches nothing else would catch. The items carry
no type tag, only their encoding, so the consumer ends the stream with an error
only when decoding happens to notice — an item's bytes running out
(`NotEnoughData`) or left over (`BadValue`), a null marker where a value is
required (`UnexpectedNull`). A mismatch it does not notice — `int` read as
`float`, two parcelables with the same fields — decodes as the wrong value.

The type argument is not on the wire: the endpoint is the same bytes whatever
it names, so a C++ peer built from the same `.aidl` (where it is
`StreamEndpoint<LogLine>` too, a class template) interoperates. For the same
reason a Rust end may read or write a type of its own that encodes like the
declared one; `StreamEndpoint::cast::<U>()` says so explicitly, and nothing then
checks that the two agree. A service handler receives `&StreamEndpoint<T>`, so
it casts a copy: `Sink::<U>::open(&endpoint.try_clone()?.cast())`. A producer
holding borrowed items needs no cast: `Sink::<str>::open_borrowed` takes a
`StreamEndpoint<String>` and sends `&str` lines without a `String` for each.

`rsbinder-aidl` refuses an array or a `List` as `T` (AOSP's `aidl` accepts
either); wrap one in a parcelable.

## What is on the wire

`StreamEndpoint` has two fields, `ring` and `sink`, and which one carries the
items depends on the transport the two ends share.

### Kernel binder: a ring

The endpoint carries a `ring`: a synchronized Fast Message Queue of bytes
(`android.hardware.common.fmq.MQDescriptor<byte, SynchronizedReadWrite>`, the
same type `libfmq` uses) that the consumer allocates — sealed against
shrinking and charged to its own process — and the producer maps. Each item
is one record written in place: a 4-byte header (kind bit plus payload
length) followed by the item's parcel bytes. A full ring parks the producer on
the ring's futex until the consumer reads; an empty ring parks the consumer
until the producer writes. Before parking, a blocking call looks at the ring
for up to 20 µs (not on a single-core machine, never on an async executor
thread), and the consumer copies every record the producer has committed out
at once and frees the space with one wake. **After the opening call no binder call carries
anything in either direction**: the end of the stream is the last record, and
a cancel is a bit in the ring's EventFlag word. The `sink` binder is still in
the endpoint, but the producer only links to it for death.

The record layout, the end reserve and the three EventFlag bits (`NOT_FULL`,
`NOT_EMPTY`, `CANCEL`) are documented in `StreamEndpoint.aidl`; a C++ peer on
`libfmq` implements against that.

### RPC: two `oneway` interfaces

By default an RPC session carries no shared memory, so the endpoint carries
only the consumer's `sink` (a consumer may opt in to a ring on a Unix session,
[below](#a-ring-over-a-unix-session)), and the stream is two ordinary AIDL interfaces, shipped
beside the parcelable in `rsbinder/aidl/stream/`:

```aidl
package rsbinder.stream;

interface IStreamSink {                      // implemented by the consumer
    // `source` is an IStreamSource, `credits` the window the producer opens
    // with; sent once, first.
    oneway void onStart(IBinder source, int credits);
    oneway void onBatch(in byte[] items, int count);
    oneway void onEnd(int exception, int serviceSpecific, @nullable String message);
}

interface IStreamSource {                    // implemented by the producer
    // `total` batches granted in all since the stream began — a running
    // total, not an amount to add.
    oneway void request(long total);
    oneway void cancel();
}
```

This is the [reactive-streams](https://www.reactive-streams.org/) contract
under binder names: `onStart` / `onBatch` / `onEnd` are `onSubscribe` / `onNext`
/ `onComplete`-or-`onError`, and `IStreamSource` is the `Subscription`. It
differs in one place: reactive-streams' `request(n)` adds `n`, and
`request(total)` here states a running total. A `oneway` call that reports
failure has almost always not arrived, but its sender cannot always tell, and
an amount sent again would then be counted twice or not at all. A total is
counted once whichever happened, which is what lets the two ends agree on how
many batches the producer may send.

**Every call is `oneway`.** A session keeps `oneway` calls to one object in
order, which is what guarantees the source is known before the first batch and
that `onEnd` cannot overtake the last one. It also means neither end ever waits
on the other's threads: granting credit costs the consumer one send, however
busy the producer's process is — a send that waits for a free outgoing
connection on the session, see [Over RPC](#over-rpc).

## Credit, batches and the two policies

Flow control is set on each side when the stream is made:
`Receiver::with_policy(&peer, &ReceiverPolicy { .. })` on the consumer and
`Sink::open_with(&endpoint, &SinkPolicy { .. })` on the producer. Each policy
has fields for both transports; the ones for the transport not in use are
ignored. `Receiver::new` and `Sink::open` take the defaults.

| Policy field | Transport | Default | What it sets |
|---|---|---|---|
| `ReceiverPolicy::ring_bytes` | ring | 64 KiB | The ring the consumer allocates — the whole of the flow control (the producer runs ahead by under twice it), and the bound on the largest item |
| `SinkPolicy::max_ring_bytes` | ring | 4 MiB | The largest ring the producer maps; a bigger one is refused at `Sink::open` |
| `ReceiverPolicy::ring_use` | RPC | `KernelOnly` | Whether an RPC peer may get a ring; see [A ring over a Unix session](#a-ring-over-a-unix-session) |
| `SinkPolicy::max_batch_bytes` | RPC | 16 KiB | The byte threshold at which a pending batch is sent |
| `SinkPolicy::initial_credits` | RPC | 4 | The window the producer opens with — its ceiling on batches in flight |
| `SinkPolicy::send_timeout` | both | `None` | How long one `send`/`send_all`/`flush`/`end` call (or `*_async` future, from its first poll) may wait for ring room or credit before it returns `TimedOut`; the item is not written or queued and the stream stays usable. `Some(Duration::ZERO)` never waits. It does not bound the RPC session's own send deadline |
| `ReceiverPolicy::credit_window` | RPC | 4 | The consumer's grant threshold: a grant leaves once half of it is owed |
| `ReceiverPolicy::max_opening` | RPC | 4 | The widest opening window the consumer accepts |
| `SinkPolicy::ping`, `ReceiverPolicy::ping` | RPC | `Inherit` | Whether a wait that hears nothing from the other end pings it; see [a peer that goes silent](#a-peer-that-goes-silent) |

**On the ring** the ring's size is everything: the producer waits for room once
the ring is full. The consumer copies every record in the ring out at once and
frees the ring right away, so the producer can be ahead of what `recv` has
returned by the ring plus the records the consumer has copied out and not yet
returned — under twice `ring_bytes`, and the consumer's copy buffer grows to
at most the ring's size. The last `END_RESERVE` (256) bytes are never given to items, so the end
record always has room; the largest item is therefore
`ring_bytes - END_RESERVE - 4`, and a stream of large items needs a larger
ring. The memory is allocated up front and charged to the consumer's process.
The default of 64 KiB is sixteen pages; the producer's `max_ring_bytes` protects its
own address space against a consumer that describes a ring of any size. Once
the ring holds a few of the largest items, a larger ring mainly lets the
producer run further ahead; one that holds only one or two makes the two ends
take turns.

**Over RPC** credit is counted in `onBatch` calls. The producer opens with a
window of credit, spends one per batch, and waits when it has none; the
consumer grants credit back for the batches it has taken — half a window at a
time while more batches are already queued, and everything it owes before it
goes to wait.

Only the producer's opening window bounds what is in flight. A grant pays for a
batch the consumer has already drained, one for one, so the ceiling stays
whatever `initial_credits` was; `credit_window` sets the grant threshold — a
grant leaves once half of it is owed, or the whole opening window if that is
less — not how far the producer may run ahead.

**The consumer holds the producer to its credit.** The producer states its
opening window in `onStart`, so the consumer knows everything the producer may
send: that window, plus what has been granted since. A batch beyond it ends the
stream with `EX_ILLEGAL_STATE` instead of being queued, which is what bounds the
memory a producer can make the consumer's process hold. That matters most when
the consumer is the service — an upload — and its producer is a client it has no
reason to trust.

How wide a window a consumer takes is the consumer's to say: `max_opening`. A
producer that opens wider is refused at the start, with `EX_ILLEGAL_ARGUMENT`,
before any item. The two defaults are the same number, so a producer made with
`Sink::open` is always taken by a consumer made with `Receiver::new`; a wider
window has to be asked for on both sides. The producer hears of a refusal as a
cancel — `onStart` is `oneway` — and the reason is in the consumer's error and
in its log.

The batch size is a threshold, not a cap: an item larger than it still goes out,
with the batch it was added to.

## When a batch leaves

Batching is an RPC-path matter. On the ring every accepted item is already in
the ring when `send` returns; nothing is queued, and `flush` returns at once —
unless a dropped `send_async` left a record on the pool, which `flush` waits
for (see [Async](#async)).

Over RPC two things send a batch, and a clock is neither of them: the pending
batch reaching the byte threshold, and a call to `flush` or `end`.

- **Items already in hand** — a query result, a directory listing. The
  threshold does the batching and `end` sends the remainder.
- **Items arriving at their own pace** — an event log, a progress feed. Call
  `flush` when what has been sent so far is what the consumer should have now,
  typically after each event or burst. Without it an item waits for the *next*
  `send` to fill the batch, which for an idle producer is not a bounded wait,
  and the consumer blocked in `recv` cannot tell that from a producer with
  nothing to say. `Sink::pending()` reports how many items are queued.

`flush` on an empty batch returns at once, and on the ring costs nothing beyond
that pool wait, so a producer unsure which path it is on can simply flush after
every item.

## Ending a stream

`end()` finishes the stream: on the ring it writes the end record, for which
room is always reserved, so it waits for the consumer only for a record a
dropped `send_async` left on the pool (see [Async](#async)); over RPC it
flushes first, and that flush blocks for credit. `end_with(&status)` does the
same and hands the consumer an error — the failure the starting method could
have returned, arriving late because that method had already succeeded:

```rust
sink.end_with(&rsbinder::Status::new_service_specific_error(
    QUOTA_EXCEEDED,
    Some("daily quota reached".into()),
))?;
```

On the other side the iterator yields that `Status` as its last `Err`, with the
[service-specific code](./error-handling.md#service-specific-errors) and message
intact, and then ends. `Receiver::end_status()` has the final status afterwards,
including a clean one, and is `None` until the end has been read: a death
notice or terminator that lands while items remain is not yet the end, and
those items come out first. On the ring the message is cut to what the end reserve
holds (244 bytes).

Call `end` on every path. A `Sink` that is dropped without it still ends the
stream, but as `EX_ILLEGAL_STATE` with a message saying how many items were
lost — from the consumer's side that is all a producer that vanished can mean.
The drop does not wait for the consumer, since a `Drop` that blocks would hold
whatever thread is unwinding: over RPC the items that cannot leave without
credit are lost, on the ring a record a dropped `send_async` future left
waiting for room is given up.

## Cancelling, and a peer that dies

`Receiver::cancel()` tells the producer to stop, and dropping the receiver sends
it for you. A parked producer is released; its `send` returns
`StatusCode::InvalidOperation` and `Sink::is_canceled()` is what tells that apart
from a transport problem. From then on every `send` returns it at once without
taking the item, on both paths. `end` still sends the terminator it was given,
without reporting a loss: over RPC the items still queued are dropped. On the
ring the producer sees the cancel before
its next write, or in the wait it is parked in; over RPC a cancel made before
the producer has introduced itself is held and delivered the moment `onStart`
arrives, and the producer sees it at its next `send`.

Back-pressure means that most of the time neither side has a call in flight, so
nothing would fail by itself if the other process went away. Each end therefore
watches the other's binder — the producer the endpoint's `sink`, the consumer
the peer it was made against: a producer parked for room or credit gets
`StatusCode::DeadObject` from `send`, and a consumer blocked in `recv` gets the
same error as the stream's last item. A peer that is not in the producer's
process leaves the producer's death unseen, and a local object is not watched
at all. Over RPC, a session that ends is a death in the same sense.

Death does not take back what already arrived: the consumer reads every item
already in the ring or in a delivered batch before it sees the death, and an
end that reached it first — an end record already in the ring, an `onEnd`
already received — is the end the stream reports, not `DeadObject`. Once the
producer knows the consumer is dead, every `send` returns `DeadObject` at once
without taking the item, on both paths, and so does `end`.

### A peer that goes silent

Over RPC the session has to notice the loss first. It does when a connection
fails, when the peer's host stops acknowledging (TCP keepalive — see
[Timeouts](./rpc-transport.md#timeouts)), or when a reply is overdue, and a
waiting stream has no reply outstanding: every stream method is `oneway`.
Behind a relay that terminates TCP (`adb forward`, `ssh -L`) a peer can vanish
with none of the three happening, and the stream would wait for good; vsock
has no keepalive either.

So a waiting end pings. When a `recv`, or a producer's wait for credit, has
been woken by nothing for a third of the session's reply deadline, it sends
the other end a `PING_TRANSACTION` — the twoway every binder object answers,
so a C++ or NDK peer needs no code for it — and waits again once it is
answered. The consumer pings the producer's `IStreamSource` once `onStart` has
named it, the producer pings the sink. A ring over a Unix session pings the
same way (its consumer pings the binder it was made against), as does a
producer parked on a full one; a ring over kernel binder never pings, since
the kernel reports a death itself. An unanswered ping ends the session
through the reply deadline, like any unanswered call, and the stream then ends
with `DeadObject` through its death link: a peer that vanished is noticed
within about four thirds of the deadline. A stream whose items flow never
pings.

The deadline is the session's: `RpcSession::set_timeout` or
`ClientOptions::timeout` on a client, `RpcServer::set_reply_timeout` or
`ServeOptions::reply_timeout` on a server. A session without one — the
default — never pings, and `PingPolicy::Off` in either policy turns it off for
that end. The ping is not counted against `recv_timeout` or `send_timeout`, so
a call whose peer stops answering returns later than its own bound, by up to
the deadline. An `*_async` wait that may ping waits on the blocking pool (see
below).

## Async

Both ends have `tokio` counterparts, behind the default `tokio` feature. Each
of them makes its wait from the blocking pool — a futex wait on the ring, a
`oneway` send over RPC — so **poll them inside a Tokio runtime context**:
`spawn_blocking` panics outside one. (A call made from inside a transaction
handler runs on the calling thread and is exempt.) A futex cannot be polled,
so on the ring each wait holds one pool thread for as long as it lasts, the
same cost as a thread of its own.

The consumer awaits `recv_async()`. There is no `futures::Stream` impl — that
would tie rsbinder's public API to a `futures-core` major version — so adapt it
where you need one:

```rust
while let Some(line) = lines.recv_async().await {
    handle(line?).await;
}
```

The producer uses `send_async`, `flush_async`, `end_async` and `end_with_async`
from a task. Waiting for the consumer suspends the task instead of parking a
thread — except a wait that may [ping](#a-peer-that-goes-silent) or has a
`send_timeout`, which holds a pool thread while it lasts, as does a
`recv_async` that may ping: there is no timer to suspend a task against.

```rust
let mut sink = Sink::open(endpoint)?;
tokio::spawn(async move {
    while let Some(line) = source.next().await {
        if sink.send_async(&line).await.is_err() {
            return;
        }
    }
    let _ = sink.end_async().await;
});
```

`send_async` encodes the item when it is called rather than when it is first
polled, so the future does not borrow the item and is `Send` whatever the item
type is. `send_async`, `flush_async` and `recv_async` are cancel-safe: a future
dropped mid-wait loses no credit, and no item without the terminator saying so.
A record or batch already handed to the blocking pool still goes out, in its
place, and the next call waits for it first; a send that then fails is not
forgotten with the future — the next call that sends (`flush_async`, or a
`send`/`send_async` that has to wait) returns that failure, or the terminator
does, and the terminator counts those items as lost. On the ring, a
`send_async` future dropped while it still waits for such an earlier record has
handed nothing over: its item is dropped, and the terminator counts it as lost
too. The two terminators are not waits to abandon — a dropped
`end_async` or `end_with_async` leaves the stream to `Drop`, which ends it as
`EX_ILLEGAL_STATE` without waiting for the consumer, so whatever was still
queued is reported lost.

## Over RPC

Each end calls the other from outside any handler — the producer sends batches
from a thread of its own, the consumer sends grants and `cancel` — and each end
watches the other for death. Even the producer's `onStart`, sent from inside
the handler of a download, is `oneway` and so never rides the connection the
request came in on. A default RPC session can do none of this toward the
client: nothing on the client reads its connection except inside its own calls.
The client has to open an **incoming connection**, which takes the android-13+
profile — see
[Callbacks outside a handler](./rpc-transport.md#callbacks-outside-a-handler):

```rust
let client = rsbinder::Client::open_with(
    "unix:///run/demo.sock?profile=android13plus",
    |o, _| o.incoming_connections = Some(1),
)?;
```

Without one, the client's end refuses with `StatusCode::InvalidOperation`
before any call is made — `Receiver::new` when the client consumes,
`Sink::open` when it produces — and logs which option is missing. `Client::caps()` reports
`TransportCaps::CALLBACKS` when the session can carry a stream. A session with
incoming connections has to be ended explicitly, with
`client.session().unwrap().close_session()`: its serving threads keep it
alive after every handle is dropped.

One more thing to size: the consumer's grants, and both ends' pings, go out on
the session's *outgoing* connections, and each waits for a free one. A client that keeps its only
outgoing connection busy with a long `twoway` call delays its own grants for that
long; give such a client a second one with `outgoing_connections`.

### A ring over a Unix session

When both ends are on one host and talk over a Unix socket, the consumer can
have the stream run on a ring instead of calls:

```rust
use rsbinder::stream::{Receiver, ReceiverPolicy, RingUse};

let policy = ReceiverPolicy { ring_use: RingUse::AlsoUnixRpc, ..Default::default() };
let (mut rx, endpoint) = Receiver::<LogLine>::with_policy(&service.as_binder(), &policy)?;
if !rx.uses_ring() {
    // The session could not carry one; the stream runs on calls as usual.
}
```

The ring's memfd crosses the socket as an `SCM_RIGHTS` file descriptor, so the
session has to pass fds both ways: the client opens with
`ClientOptions::fd_mode = Some(FileDescriptorTransportMode::Unix)` and the
server allows it (`ServeOptions::fd_modes` or
`RpcServer::set_supported_fd_modes`). It also needs the incoming connection
above. A session missing any of these — vsock, TCP, TLS, no fd mode, no incoming
connection — gets calls, and a `log::debug!` line names what was missing;
`uses_ring` tells which one a stream got. The ring needs Linux or Android.

Which to choose is a matter of item size. Measured on x86-64, a ring moves
items of a kilobyte or more several times faster than calls, while calls are
faster for small items (`i32`, 64-byte records), where the ring's per-record
work outweighs a batched call. On an arm64 Android emulator the ring was as
fast as calls for `i32` and faster above.

The producer has to implement the ring over RPC; rsbinder's does, and nothing
changes on its side. A producer written to the RPC-only contract refuses the
endpoint, and how that shows depends on the direction: in a download its
refusal is the error of the opening call; in an upload the opening call has
already returned the endpoint, so the consumer learns of it only from the
producer's death or its own `recv_timeout`. A producer that ignores the ring and
sends batches ends the stream with `EX_ILLEGAL_STATE`.

The items do not cross the socket, so to the session a flowing ring looks quiet.
Each end therefore counts its own ring traffic toward the server's idle check
(`RpcServer::set_idle_timeout`): a stream whose items keep coming, or come
more often than the timeout, keeps the session alive. A stalled one does not, as
on calls, unless it pings.

## State, not a sequence: use a `oneway` callback

A temperature, a progress percentage, a connection state: the consumer wants the
**current** value and a stale one is worthless. That is a different problem, and
back-pressure is the wrong tool for it. It exists because items cannot be
dropped, so a slow consumer has to be able to stop the producer. When values
*can* be dropped, nothing has to stop anybody — provided the handler that
receives them never does slow work:

```aidl
interface ITemperatureListener {
    oneway void onChanged(float celsius);
}
```

```rust
struct Listener(tokio::sync::watch::Sender<f32>);

impl ITemperatureListener for Listener {
    fn r#onChanged(&self, celsius: f32) -> rsbinder::BinderResult<()> {
        self.0.send_replace(celsius); // overwrite and return
        Ok(())
    }
}
```

The handler overwrites one slot and returns, so the queue is one value deep no
matter how slow the code reading the `watch::Receiver` is, and the transaction
buffers are freed as fast as they arrive. "The consumer is slow" means its
*processing* is slow, not its binder handler. `oneway` calls to one object arrive
in order, so the last value written is the latest one. This is the ordinary
[callback pattern](./callbacks-and-interfaces.md), and it is what AOSP's own
state listeners do.

Two refinements, for when they apply:

- **A slow link**, where sending every value is itself the cost: put the same
  `watch` on the *producer* side, and have one task send the current value,
  wait for that call to finish, and read the slot again. While the link is
  backed up the slot is simply overwritten, and the transport's own
  back-pressure paces the sender.
- **A frozen Android app** as the listener: the kernel can replace a queued
  `oneway` transaction with a newer one to the same method, which keeps a
  cached app's buffer from filling up. It does so only when the call carries
  `FLAG_UPDATE_TXN`, which a generated proxy never sets — send that one call by
  hand with `submit_transact`, as
  `example-hello/src/bin/update_txn_interop_client.rs` does.

## A C++ or NDK peer

A Java peer is out of scope: AOSP has no Java FMQ.

Compile the three `.aidl` files from `rsbinder/aidl/stream/` with the
platform's `aidl` — `StreamEndpoint.aidl` imports
`android.hardware.common.fmq.MQDescriptor`, so the platform's FMQ AIDL has to be
on the include path — and implement the side you need. The SDK build-tools
`aidl` does not parse the `@FixedSize T` in the current FMQ files; include the
frozen V1 API instead
(`hardware/interfaces/common/fmq/aidl/aidl_api/android.hardware.common.fmq/1`
and `hardware/interfaces/common/aidl/aidl_api/android.hardware.common/1`),
which has the same wire. Which wire you have to
speak is decided by the endpoint you receive or make: a `ring` on kernel
binder, a `sink` alone over RPC.

The generated `StreamEndpoint` is a class template, and the service's methods
take the instance the `.aidl` names — `StreamEndpoint<LogLine>`, or
`StreamEndpoint<int32_t>` for `StreamEndpoint<int>`. The C++ compiler checks
that much. It does not check the items: they are bytes a peer encodes and
decodes itself, so reading them as the type argument names is up to the peer.

**Over kernel binder** the contract is the ring, and `StreamEndpoint.aidl` is
its specification: attach the `MQDescriptor` with `libfmq`
(`AidlMessageQueue<int8_t, SynchronizedReadWrite>`), read or write whole
records — 4-byte little-endian header, top bit the kind, low 31 bits the
payload length — and use the EventFlag bits as documented there. A producer
keeps the last 256 bytes free of item records so the end record always fits,
and waits with the mask `NOT_FULL | CANCEL` rather than `writeBlocking`, which
would not see the cancel. A consumer making the endpoint allocates the ring
itself and puts a binder in `sink` for the producer to link to for death; no
`IStreamSink` method is called on it. What else a `libfmq` peer must do, with
the details in `StreamEndpoint.aidl`:

- Attach with `AidlMessageQueue(desc, /*resetPointers=*/false)`: the other end
  may already have written.
- Wake the other side after every read and every write, as `libfmq` itself
  does, not only on a full-to-not-full change: a waiter may need room for more
  than one record. A read that takes several records at once may wake once
  for all of them, as rsbinder's consumer does.
- As the producer, load `getEventFlagWord()` before each item record and stop
  on `CANCEL` (`libfmq` has no peek), and remember a cancel your wait returned:
  the wait consumes the bit.
- Treat the ring as the other process's memory. A producer applies the checks
  rsbinder applies before it maps (a memfd sealed against shrinking or an
  ashmem region, an EventFlag word, a size bound, a capacity above 260 bytes);
  a consumer checks each record's length against the ring before reading it.

An NDK peer cannot link `libfmq` — it is not part of the NDK, and the device's
`libfmq.so` is built against the platform's C++ library rather than the NDK's.
Two header-only C11 files do that part instead:
`rsbinder-fmq/c/rsbinder_fmq.h` is the ring (attach with the same checks
rsbinder makes, create a sealed memfd, the counters, the EventFlag futex), and
`contrib/ndk/rsbinder_stream.h` is the record layer on it (`rsbs_send`,
`rsbs_end`, `rsbs_recv`, `rsbs_cancel`). The NDK compiles them with no extra
flag; on a 32-bit glibc host they need `-D_FILE_OFFSET_BITS=64`, without which
the queue header stops with `#error`. For death, create the
`AIBinder_DeathRecipient` with `rsbs_on_binder_died`, set
`rsbs_on_unlinked` as its `onUnlinked` (API 33), and link with the stream's
`rsbs_*_death_cookie`: libbinder can still be running the death callback after
`AIBinder_unlinkToDeath` returns, and the cookie keeps what it touches mapped
until `onUnlinked`, so the stream may be closed at any time. Each header has two
C++ templates that convert to and from the NDK backend's generated
`MQDescriptor` and `StreamEndpoint`.
`example-hello/cpp/stream_interop.cpp` is a complete client on the two headers
for both directions, `example-hello/cpp/stream_libfmq_interop.cpp` the same
client with its ring on `libfmq`, and `run_stream_interop.sh` runs both against
rsbinder on an emulator.

**Over RPC** the contract is the two interfaces, and the client opens incoming
connections as [above](#over-rpc) (libbinder's
`ARpcSession_setMaxIncomingThreads`). `example-hello/cpp/stream_rpc_interop.cpp`
implements both ends on the device's libbinder `RpcSession`, and
`run_stream_rpc_interop.sh` runs it against rsbinder:

- **As the consumer**, implement `IStreamSink`. In `onStart`, turn the binder
  into the interface (`IStreamSource::asInterface`,
  NDK `IStreamSource::fromBinder`) and link to its death. In `onBatch`, put the
  bytes into a parcel — C++ `Parcel::setData`, NDK `AParcel_unmarshal` followed by
  `AParcel_setDataPosition(p, 0)` — and read the item type `count` times. Call
  `request(total)` as you drain, where `total` is how many batches you have
  drained since the stream began; if the call fails, call it again later with
  whatever the total is by then. The producer opens with the `credits` that
  `onStart` states, so the first grant can follow the first batch. To bound your
  queue, hold the producer to `credits` plus the highest total you have sent or
  tried to send, and decide in `onStart` how large a `credits` you accept.
- **As the producer**, implement `IStreamSource` and count: each `onBatch` spends
  one credit, `onEnd` spends none, and after `cancel` only `onEnd` may still be
  sent. `request(total)` is worth `total` minus the highest total you have seen,
  and nothing when that is not positive — keep the highest, not a sum. **Open
  with at least one credit of your own, and state it in `onStart`.** An rsbinder
  consumer grants only for batches it has already drained, so a producer that
  starts at zero waits for a grant that nothing can trigger — and it ends the
  stream on a batch sent beyond the stated window plus its grants, or on a
  window wider than it accepts (4 unless it was made with a larger
  `ReceiverPolicy::max_opening`). A batch sent before `onStart` has no credit
  and ends the stream with `EX_ILLEGAL_STATE`, a negative `count` with
  `EX_ILLEGAL_ARGUMENT`. An rsbinder consumer sends `cancel` when its receiver
  is dropped, also after a clean end, so ignore a `cancel` that arrives after
  your `onEnd`. If an `onBatch` fails in a way that does not tell you whether
  it arrived, stop sending batches and send `onEnd`: you no longer know how
  much credit you have.
  `onEnd`'s three arguments are the fields of a `binder::Status`; `-129`, `-127`
  and `-128` are never sent.
