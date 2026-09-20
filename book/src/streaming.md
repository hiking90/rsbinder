# Streaming

A binder method returns one value. A method that would return a million rows
either builds the whole list in memory on both sides — and runs into the
transaction size limit long before that — or invents a paging protocol.
`rsbinder::stream` is the third option: the consumer hands the service a
**sink** to push batches into and grants **credit** back as it drains them, so
the service can only run as far ahead as the consumer allows.

It is for a **sequence in which every item matters**. Three neighbours cover
what it is not for:

| What you have | Use | Why |
|---|---|---|
| A sequence of values, all of which must arrive | **`rsbinder::stream`** (this chapter) | Items cannot be dropped, so a slow consumer has to be able to slow the producer down |
| A value that changes, where only the current one matters | A `oneway` callback — see [below](#state-not-a-sequence-use-a-oneway-callback) | Stale values may be dropped, so nothing needs slowing down |
| Bytes — a file, a log, a media stream | [`ParcelFileDescriptor::pipe`](./parcel-file-descriptor.md) | The kernel already does back-pressure on a pipe |
| A large buffer both sides read in place | [Shared memory](./shared-memory.md) | No copy at all |

New in 0.13.0. It works the same over kernel binder and over
[RPC](./rpc-transport.md), with one [requirement on the RPC client](#over-rpc).

## A minimal stream

The method that starts a stream takes the consumer's sink as an `IBinder` and
returns nothing — everything else travels over the stream itself:

```aidl
package demo;

parcelable LogLine {
    long timestampMs;
    String text;
}

interface ILogService {
    // `sink` is an rsbinder.stream.IStreamSink.
    void tail(IBinder sink, String tag);
}
```

The service wraps the sink and writes from a thread of its own. `send` blocks
once the consumer's credit is used up, which is the back-pressure — and the
reason the producer does not run on the binder thread that took the call:

```rust
use rsbinder::stream::Sink;

impl ILogService for LogService {
    fn r#tail(&self, sink: &rsbinder::SIBinder, tag: &str) -> rsbinder::BinderResult<()> {
        let mut sink = Sink::<LogLine>::new(sink)?;
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

The consumer makes a receiver, passes its sink, and reads:

```rust
use rsbinder::stream::Receiver;

let (mut lines, sink) = Receiver::<LogLine>::new();
service.r#tail(&sink, "net")?;
for line in &mut lines {
    println!("{}", line?.text);
}
```

`Receiver<T>` is an `Iterator<Item = BinderResult<T>>` that ends when the
producer does; `recv`, `try_recv` and `recv_timeout` are the same thing one item
at a time, differing only in how long they wait for one — each may still send a
grant it owes before it answers. The two ends need not share a Rust type — items are encoded with the
codec [`to_bytes`](./data-serialization.md) uses, so they only have to agree on
the wire. For the same reason **an item may hold neither a binder nor a file
descriptor**; `send` refuses one that does.

## What is on the wire

Two ordinary AIDL interfaces, shipped in `rsbinder/aidl/stream/`, so a C++ or
Java process can be either end without rsbinder on its side:

```aidl
package rsbinder.stream;

interface IStreamSink {                      // implemented by the consumer
    oneway void onStart(IBinder source);     // an IStreamSource; sent once, first
    oneway void onBatch(in byte[] items, int count);
    oneway void onEnd(int exception, int serviceSpecific, @nullable String message);
}

interface IStreamSource {                    // implemented by the producer
    oneway void request(int credits);
    oneway void cancel();
}
```

This is the [reactive-streams](https://www.reactive-streams.org/) contract
under binder names: `onStart` / `onBatch` / `onEnd` are `onSubscribe` / `onNext`
/ `onComplete`-or-`onError`, and `IStreamSource` is the `Subscription`.

**Every call is `oneway`.** Both transports keep `oneway` calls to one object in
order — the kernel per node, an RPC session per address — which is what
guarantees the source is known before the first batch and that `onEnd` cannot
overtake the last one. It also means neither end ever waits on the other's
threads: granting credit costs the consumer one local send, however busy the
producer's process is. On the kernel that send is all it costs; over RPC it also
waits for a free outgoing connection on the session — see [Over
RPC](#over-rpc).

## Credit, batches and the two constants

Credit is counted in `onBatch` calls. The producer opens with a window of
credit, spends one per batch, and waits when it has none; the consumer grants
credit back for the batches it has taken — half a window at a time while more
batches are already queued, and everything it owes before it goes to wait.

Only the producer's opening window bounds what is in flight. A grant pays for a
batch the consumer has already drained, one for one, so the ceiling stays
whatever `initial_credits` was; `Receiver::with_credit_window(n)` sets the grant
threshold — a grant leaves once half of `n` is owed — not how far the producer
may run ahead.

| Constant | Value | Set it with |
|---|---|---|
| `DEFAULT_MAX_BATCH_BYTES` | 16 KB | `Sink::with_limits(sink, max_batch_bytes, initial_credits)` |
| `DEFAULT_CREDIT_WINDOW` | 4 batches | `initial_credits` in the same call — the producer's ceiling — and `Receiver::with_credit_window(n)` for the consumer's grant threshold (half of `n`) |

The values are sized against the kernel driver. A process's `oneway`
transactions share half its binder mapping — about 512 KB at the default 1 MB —
and once less than a tenth of the mapping is free (about 102 KB) the driver
starts checking individual senders for spam. A full default window is 64 KB in
flight, which stays under that line, so one stream never trips the check by
itself. Raise either number and `initial_credits × batch` is the budget to watch;
exhausting the space does not slow a sender down, it fails the call.

The batch size is a threshold, not a cap: an item larger than it still goes out,
in a batch of its own.

## When a batch leaves

Two things send a batch, and a clock is neither of them: the pending batch
reaching the byte threshold, and a call to `flush` or `end`.

- **Items already in hand** — a query result, a directory listing. The
  threshold does the batching and `end` sends the remainder.
- **Items arriving at their own pace** — an event log, a progress feed. Call
  `flush` when what has been sent so far is what the consumer should have now,
  typically after each event or burst. Without it an item waits for the *next*
  `send` to fill the batch, which for an idle producer is not a bounded wait,
  and the consumer blocked in `recv` cannot tell that from a producer with
  nothing to say. `Sink::pending()` reports how many items are queued.

`flush` on an empty batch returns at once, so a producer unsure which kind it is
can simply flush after every item.

## Ending a stream

`end()` flushes and finishes. `end_with(&status)` does the same and hands the
consumer an error — the failure the starting method could have returned, arriving
late because that method had already succeeded:

```rust
sink.end_with(&rsbinder::Status::new_service_specific_error(
    QUOTA_EXCEEDED,
    Some("daily quota reached".into()),
))?;
```

On the other side the iterator yields that `Status` as its last `Err`, with the
[service-specific code](./error-handling.md#service-specific-errors) and message
intact, and then ends. `Receiver::end_status()` has the final status afterwards,
including a clean one.

Call `end` on every path. A `Sink` that is dropped without it still sends what
it has credit for and still ends the stream, but as `EX_ILLEGAL_STATE` with a
message saying how many items were lost — from the consumer's side that is all a
producer that vanished can mean. That flush does not wait for credit, since a
`Drop` that blocks would hold whatever thread is unwinding.

## Cancelling, and a peer that dies

`Receiver::cancel()` tells the producer to stop, and dropping the receiver sends
it for you. A producer waiting for credit is released; its `send` returns
`StatusCode::InvalidOperation` and `Sink::is_canceled()` is what tells that apart
from a transport problem. A cancel made before the producer has introduced
itself is held and delivered the moment `onStart` arrives.

Back-pressure means that most of the time neither side has a call in flight, so
nothing would fail by itself if the other process went away. Each end therefore
watches the other's binder: a producer waiting for credit gets
`StatusCode::DeadObject` from `send`, and a consumer blocked in `recv` gets the
same error as the stream's last item. Over RPC, a session that ends is a death
in the same sense.

## Async

Both ends have `tokio` counterparts, behind the default `tokio` feature. Each
of them sends from the blocking pool, so **poll them inside a Tokio runtime
context** — `spawn_blocking` panics outside one. (A call made from inside a
transaction handler runs on the calling thread and is exempt.)

The consumer awaits `recv_async()`. There is no `futures::Stream` impl — that
would tie rsbinder's public API to a `futures-core` major version — so adapt it
where you need one:

```rust
while let Some(line) = lines.recv_async().await {
    handle(line?).await;
}
```

The producer uses `send_async`, `flush_async`, `end_async` and `end_with_async`
from a task. Waiting for credit suspends the task instead of parking a thread,
and each batch is sent from the blocking pool, the way every
[generated async proxy](./async-service.md) sends:

```rust
let mut sink = Sink::<LogLine>::new(sink)?;
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
dropped mid-wait loses no item and no credit. A batch already handed to the
blocking pool still goes out, and a send that then fails is not forgotten with
the future — the next call that actually sends a batch (`flush_async`, or a
`send`/`send_async` that fills one) returns that failure, or the terminator
does, and the terminator counts those items as lost. The two terminators are not
waits to abandon — a dropped `end_async` or `end_with_async` leaves the stream
to `Drop`, which ends it as `EX_ILLEGAL_STATE` without waiting for credit, so
whatever was still queued is reported lost.

## Over RPC

The producer calls the consumer from a thread of its own, outside any handler,
and a default RPC session cannot carry a call in that direction. The client has
to open an **incoming connection** — see
[Callbacks outside a handler](./rpc-transport.md#callbacks-outside-a-handler):

```rust
let client = rsbinder::Client::open_with(uri, |o, _| o.incoming_connections = Some(1))?;
```

Without one, `Sink::new` refuses the stream with `StatusCode::InvalidOperation`
and logs which option is missing, so the starting method fails at once instead
of the first batch failing later. `Client::caps()` reports
`TransportCaps::CALLBACKS` when the session can carry a stream. A session with
incoming connections has to be ended with `close_session()`.

One more thing to size: the consumer's grants go out on the session's *outgoing*
connections, and a grant waits for a free one. A client that keeps its only
outgoing connection busy with a long `twoway` call delays its own grants for that
long; give such a client a second one with `outgoing_connections`.

## State, not a sequence: use a `oneway` callback

A temperature, a progress percentage, a connection state: the consumer wants the
**current** value and a stale one is worthless. That is a different problem, and
credit is the wrong tool for it. Credit exists because items cannot be dropped,
so a slow consumer has to be able to stop the producer. When values *can* be
dropped, nothing has to stop anybody — provided the handler that receives them
never does slow work:

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

## A C++ or Java peer

Compile the two `.aidl` files from `rsbinder/aidl/stream/` with the platform's
`aidl` and implement the side you need.

- **As the consumer**, implement `IStreamSink`. In `onStart`, turn the binder
  into the interface (`IStreamSource::asInterface`,
  `IStreamSource.Stub.asInterface`) and link to its death. In `onBatch`, put the
  bytes into a parcel — C++ `Parcel::setData`, Java `Parcel.unmarshall` followed
  by `setDataPosition(0)`, NDK `AParcel_unmarshal` followed by
  `AParcel_setDataPosition(p, 0)` — and read the item type `count` times. Call
  `request(n)` as you drain. An rsbinder producer opens with a window of its
  own, so the first grant can follow the first batch; a producer written to
  start at zero needs one from `onStart`.
- **As the producer**, implement `IStreamSource` and count: each `onBatch` spends
  one credit, `request(n)` adds `n`, `onEnd` spends none, and after `cancel` only
  `onEnd` may still be sent. **Open with at least one credit of your own.** An
  rsbinder consumer grants only for batches it has already drained, so a
  producer that starts at zero waits for a grant that nothing can trigger.
  `onEnd`'s three arguments are the fields of a `binder::Status`; `-129`, `-127`
  and `-128` are never sent.
