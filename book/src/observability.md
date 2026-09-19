# Observability

Three pieces tell you what a binder service is doing and for whom:

| Piece | What it answers | Android counterpart |
|---|---|---|
| **Work source** | On whose behalf is this call being made? | `IPCThreadState::setCallingWorkSourceUid`, Java `Binder.setCallingWorkSourceUid` |
| **Transaction names** | Which AIDL method is transaction code 7? | `aidl --trace` |
| **Transaction observers** | What did every incoming call cost, and did it fail? | Java `Binder.setObserver` / `BinderCallsStats` |

None of them changes the wire format. The work source rides in a field every
kernel binder request header already has; names and observers are local to
the process. All three are new in 0.13.0.

## Work source

A service that does work for someone else — a media server decoding for an
app, a proxy forwarding a request — can say so. The **work source** is a uid
attached to outgoing calls, separate from the calling uid the kernel reports,
so the next service can attribute cost to the uid that asked for the work
rather than to the process that forwarded it.

```rust
use rsbinder::{restore_calling_work_source, set_calling_work_source_uid};

let token = set_calling_work_source_uid(app_uid);
let result = decoder.decode(&frame);  // the request header carries app_uid
restore_calling_work_source(token);
```

On the receiving side the handler reads it:

```rust
fn decode(&self, frame: &Frame) -> rsbinder::status::Result<Image> {
    let attributed_to = rsbinder::get_calling_work_source_uid(); // u32::MAX: unset
    // ...
}
```

The rules follow AOSP:

- The work source is per thread. A client thread can set it outside any
  transaction, and every kernel binder call that thread makes carries it.
- A handler sees what its caller sent. **A received value is not forwarded**
  on the handler's own outgoing calls unless the handler sets it again, so a
  work source never travels further than the service that chose to pass it
  on.
- When the handler returns, the thread's previous value comes back; nothing a
  handler sets leaks into the next call served on that thread.
- `clear_calling_work_source` sends "unset" explicitly,
  `clear_propagate_work_source` stops sending it, and
  `should_propagate_work_source` reports which is in effect.

**RPC has no work-source field** — AOSP's RPC wire has none either. Inside an
RPC handler `get_calling_work_source_uid` reports the unset value; a value set
there still propagates to the kernel binder calls the handler makes. The
functions work in a pure-RPC process too.

## Transaction names

Generated code identifies methods by transaction code. To let tools name them,
generate the interface with a name table, as AOSP does with `aidl --trace`:

```rust
// build.rs
rsbinder_aidl::Builder::new()
    .source(PathBuf::from("aidl/IDecoder.aidl"))
    .trace(true)
    .output(PathBuf::from("decoder.rs"))
    .generate()?;
```

The generated service then answers `Remotable::transaction_name(code)` with the
AIDL method name, including the stable-AIDL meta methods
`getInterfaceVersion` and `getInterfaceHash`. The table is off by default, as
in AOSP, because it costs one `&'static str` per method. Hand-written
`Remotable`s and interfaces declared with the
[interface macros](./interface-macros.md) keep the default, which returns
`None`.

With `trace(true)` the generated proxies also open a client-side span per call
when the `tracing` feature is on (see [below](#tracing-spans)).

## Transaction observers

A `TransactionObserver` is called around every transaction the process serves,
on kernel binder and over RPC: `on_transact` before the handler runs and
`on_reply` after it returns, before the reply is sent.

```rust
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use rsbinder::observe::{set_observer, TransactionObserver, TxnContext};

struct SlowCallLog;

impl TransactionObserver for SlowCallLog {
    fn on_transact(&self, _ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        None
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        _tag: Option<Box<dyn Any + Send>>,
        result: &rsbinder::Result<()>,
        elapsed: Duration,
    ) {
        if elapsed > Duration::from_millis(100) {
            log::warn!(
                "{} {} took {elapsed:?} for uid {}: {result:?}",
                ctx.descriptor,
                ctx.method.unwrap_or("?"),
                ctx.calling_uid,
            );
        }
    }
}

set_observer(Some(Arc::new(SlowCallLog)));
```

`TxnContext` carries the descriptor, the code, the method name (with
`trace(true)`), the one-way flag, the caller's uid and pid, and the transport
(`TransportCaps::KERNEL` or the RPC session's capabilities). Whatever
`on_transact` returns comes back to `on_reply` as `tag` — a start marker or a
span, for example.

What an implementation can rely on:

- Both calls run on the thread that serves the transaction, with no rsbinder
  lock held, so an observer may make binder calls of its own. Its time is
  added to the caller's latency.
- Every `on_transact` is followed by exactly one `on_reply`, one-way calls
  included.
- A panic in either call is caught and logged; it does not change the
  transaction's result.
- `result` is the transport-level result. An AIDL method that returns an
  exception or a service-specific error writes it into the reply and still
  reports `Ok(())` here.
- The kernel reports no sender pid for a one-way call, so `calling_pid` is `0`
  there.
- Over RPC, the session answers `INTERFACE` and `PING` itself, so those two
  meta transactions are observed on kernel binder only.

There is **one observer per process**, because kernel binder serves every
object from one process-wide thread pool. Filter on `ctx.descriptor` or
`ctx.transport` to watch a subset, and write an observer that forwards to
several to combine them. With none installed, a dispatch pays one atomic load.

### Provided observers

- **`LogObserver`** writes one `debug` line per transaction, target
  `rsbinder::observe`.
- **`StatsObserver`** keeps, per descriptor and code, the call count,
  transport-error count, total and maximum handler time, and a power-of-two
  microsecond latency histogram, plus how many transactions are being served
  at once and the peak of that number:

  ```rust
  use rsbinder::observe::{set_observer, StatsObserver};

  let stats = Arc::new(StatsObserver::new());
  set_observer(Some(stats.clone()));
  // ... serve ...
  let snapshot = stats.snapshot();
  for m in &snapshot.methods {
      println!("{} {:?}: {} calls, max {:?}", m.descriptor, m.method, m.calls, m.max);
  }
  println!("peak concurrency {}", snapshot.max_in_flight);
  ```

  A peak equal to the thread-pool size is a hint, not proof, that the pool ran
  out: a callback nested on the same thread counts twice.

## Tracing spans

The `tracing` feature (off by default) produces spans named as AOSP names its
ATrace sections:

```text
AIDL::rust::<descriptor>::<method>::server
AIDL::rust::<descriptor>::<method>::client
```

A code the name table does not cover is written `#<code>`, as in AOSP. Because
a `tracing` span name must be a `&'static str`, the string is the `name` field
of a span called `aidl` (target `rsbinder::aidl`, level `TRACE`).

- **Server spans** come from `observe::TracingObserver`. Install it like any
  other observer; the span covers the handler, so spans the handler opens —
  including the client span of a nested AIDL call — are its children.
- **Client spans** come from proxies generated with `trace(true)`. The span is
  created where the call is made, so its parent is the caller's current span,
  for the async proxy as well.

```toml
[dependencies]
rsbinder = { version = "0.13", features = ["tracing"] }
```

```rust
tracing_subscriber::fmt()
    .with_env_filter("rsbinder::aidl=trace")
    .init();
rsbinder::observe::set_observer(Some(Arc::new(rsbinder::observe::TracingObserver)));
```

Without the feature the generated client hook is a plain function call, and
code generated without `trace(true)` is unchanged.
