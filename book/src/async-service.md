# Async Service

rsbinder supports async/await with the [Tokio](https://tokio.rs/) runtime, making it
straightforward to build non-blocking Binder services. The `tokio` feature is enabled by
default in rsbinder, so no extra feature flags are required for most projects.

If you have not yet read the [Hello, World!](./hello-world.md) chapter, it is recommended to
do so first -- the async concepts here build on the synchronous service and client covered
there.

## Async is transport-agnostic

Async lives in the **AIDL-generated stubs**, not in the transport. The compiler
emits the async trait (`IMyServiceAsyncService`), the async proxy
(`into_async::<Tokio>()`), and the `new_async_binder` constructor *once*, and
the very same async `impl` runs unchanged over **kernel binder or RPC** — including
through the transport-agnostic [`serve` / `connect` entry API](./cross-transport-services.md).

The reason is that the entry API and the transports only ever move a transport-agnostic
`SIBinder` around (register a binder under a name, look one up). Whether that binder is
sync- or async-backed is decided entirely by which constructor produced it
(`new_binder` vs `new_async_binder`) — a fact the transport never sees. So async needs
**no transport-specific code**: pick the constructor, pick the transport, and the two
compose.

This chapter gives one setup recipe, then shows what the RPC variant does and does not
change about it.

> **Under the hood — it is a thread bridge, not a reactor.** rsbinder async is *not*
> epoll-driven non-blocking I/O. On the client a transact runs on
> `tokio::task::spawn_blocking`; on the server each call is wrapped in `rt.block_on(..)`
> and driven from a blocking worker thread (the binder thread pool for kernel, the
> `serve` worker for RPC). This is the same bridge the kernel `binder_tokio` path uses.
> You get async/await ergonomics and real concurrency, backed by blocking worker
> threads — which is sufficient for typical service workloads. A true async I/O reactor
> is deliberately out of scope.

## Sync vs Async at a Glance

The following table summarizes the key differences between a synchronous and an asynchronous
Binder service in rsbinder.

| Aspect | Sync | Async |
|---|---|---|
| Trait name | `IMyService` | `IMyServiceAsyncService` |
| Method signature | `fn method(&self) -> Result<T>` | `async fn method(&self) -> Result<T>` |
| Service creation | `BnXxx::new_binder(impl)` | `BnXxx::new_async_binder(impl, rt())` |
| Remote call (client) | `service.Method()` | `service.clone().into_async::<Tokio>().Method().await` |
| Main loop | `serve(..).run()` | `serve(..).spawn()` + `std::future::pending().await` |
| Runtime | Not needed | Tokio runtime required |

The AIDL compiler generates both the sync trait (`IMyService`) and the async trait
(`IMyServiceAsyncService`) from the same `.aidl` file. You choose which one to implement
depending on whether your service needs async capabilities.

Every row above is **identical across transports** — only the URI passed to `serve`
changes. What does differ is how much slack the runtime flavor has: a current-thread
runtime is usable for kernel binder under a condition spelled out below, and not usable
at all for an RPC server that calls `run()`. Both cases come down to the same rule, so
the recipe that follows uses a multi-threaded runtime and `spawn()` throughout.

## Setting Up the Tokio Runtime

An async Binder service must run inside a Tokio runtime. One recipe covers both
transports:

1. A **multi-threaded** runtime (what `#[tokio::main]` builds by default).
2. `serve(..).add(..)` — the same two calls as a sync service.
3. `spawn()` rather than `run()`, so this task is not blocked.
4. Park on `std::future::pending().await`.

```rust
use rsbinder::*;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // The runtime inbound calls are driven on. Binder threads are outside the
    // runtime, which is the case `Handle::block_on` is built for.
    let rt = TokioRuntime(tokio::runtime::Handle::current());
    let service = BnMyService::new_async_binder(MyAsyncService::default(), rt);

    // `serve("binder://")` initializes the kernel `ProcessState`; `add`
    // registers with the service manager. `spawn` starts the binder thread
    // pool and returns — unlike `run`, which joins it and never comes back.
    // Swap the URI for `unix:///tmp/my.sock` to serve the same service over
    // RPC; nothing else in this file changes, but rsbinder then needs
    // `features = ["rpc"]`.
    let _guard = rsbinder::serve("binder://")?
        .add("com.example.myservice", &service)?
        .spawn()?;

    // Never resolves, so the process stays alive. To shut down on Ctrl-C,
    // await `tokio::signal::ctrl_c()` here instead and declare `tokio/signal`
    // in your own `Cargo.toml` — rsbinder does not pull that feature in.
    std::future::pending::<()>().await;
    Ok(())
}
```

`example-hello/src/bin/hello_async_service.rs` is this code with a real interface.

Things worth naming:

- **`TokioRuntime`** is what `new_async_binder` drives each inbound call with. It wraps
  a `Handle`, not an owned `Runtime`: a binder that owns its runtime panics in
  `Runtime::drop` when the last handle to it is released on a worker thread.
- **`spawn()` over `run()`.** For kernel binder, `spawn` starts one looper and returns;
  the kernel grows the pool up to `max_threads` as load arrives. `run` joins the pool,
  which blocks the calling thread forever — fine for a sync service, wrong here.
- **`pending().await`** is the async counterpart of `ProcessState::join_thread_pool()`.
- **`rt()`** in the snippets below is this one-liner:
  ```rust
  fn rt() -> TokioRuntime<tokio::runtime::Handle> {
      TokioRuntime(tokio::runtime::Handle::current())
  }
  ```

### Advanced: a current-thread runtime

A current-thread runtime works too, under one condition that is easy to miss: **its
owning thread must stay parked inside `Runtime::block_on`.** That is the only thing that
runs the runtime — its timer and IO drivers, and the tasks spawned on it. Park the owner
anywhere else — calling `ProcessState::join_thread_pool()` directly, joining a thread, a
blocking read — and every handler that awaits a timer, IO, or a task it spawned stops
making progress. There is no error; those calls simply never return.

So the recipe is the same shape as above, with the owner parked:

```rust
let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;

runtime.block_on(async {
    let rt = TokioRuntime(tokio::runtime::Handle::current());
    let service = BnMyService::new_async_binder(MyAsyncService::default(), rt);
    let _guard = rsbinder::serve("binder://")?.add("com.example.myservice", &service)?.spawn()?;
    std::future::pending::<()>().await;
    Ok(())
})
```

`tests/src/bin/test_service_async.rs` is built this way, and the integration suite runs the
whole client test suite against it — on a manual workflow dispatch, not on every pull
request. The multi-threaded recipe carries no such condition to get wrong, which is why it
is the default advice.

### Calling a local async service through its sync handle

`new_async_binder` hands back a **synchronous** `Strong<dyn IMyService>`. Each of its
methods is implemented as `block_on(inner.method())`, so calling it from inside the
runtime — a gateway that calls its own service, start-up code, a test — re-enters the
runtime. On a multi-threaded runtime rsbinder handles that by releasing the worker's core
first.

Two independent things decide what such a call does. Read them as one list and you will
attribute a failure to the wrong one.

**Where you call from** decides whether the call runs. Tokio refuses a `block_on` on a
thread already executing inside a runtime, and `block_in_place` — which steps back out of
one — exists only for the multi-threaded runtime. That is the whole rule:

| The calling thread | Result |
|---|---|
| Not executing inside a runtime | runs |
| Inside a multi-threaded runtime | runs, after releasing the core |
| Inside a multi-threaded runtime, within a `LocalSet` | panic — `block_in_place` is refused there |
| Inside a current-thread runtime | panic — "Cannot start a runtime from within a runtime" |

Applying it takes some care, because "inside a runtime" is not "on a runtime's thread":

- A `spawn_blocking` pool thread is *outside* the runtime it belongs to, either flavor.
  Those calls run.
- A `Runtime::block_on` body is *inside* — so a current-thread runtime's
  `Runtime::block_on` body is the last row, not the first.
- **An async handler is inside** for as long as `block_on` is driving it, and inside
  *the handle's* runtime whatever thread it started on. A service whose `TokioRuntime`
  holds a current-thread handle therefore cannot reach a second local service through a
  sync handle from inside a handler — that is the last row, and it is the gateway pattern
  exactly. Use `into_async` there, or give the service a multi-threaded handle.

**Which runtime the handle points at** decides whether the call finishes, whatever the
answer above. The future itself is polled on the *calling* thread either way, and that is
all `Handle::block_on` does. Whatever the handler needs the runtime itself to advance — a
timer, IO readiness, or a task it spawned there — needs that runtime to be running:

| The `TokioRuntime` handle | Result |
|---|---|
| A multi-threaded runtime | completes — its workers are always running |
| A current-thread runtime with a thread parked in `Runtime::block_on` | completes — that parked call is what runs it |
| A current-thread runtime with nobody parked in it | **never returns**, for a handler that awaits any of the three above. One that needs none of them — ready on the first poll, or suspending only on `yield_now` — still completes |

The cost is one core release per call made from a worker thread; from the other running
positions the call is inline. A released core is offered to the `spawn_blocking` pool,
which spawns a thread on demand — so it goes unclaimed only when the pool is at its
`max_blocking_threads` cap with every thread busy. Even then the call completes; what
stops is the rest of a *single-worker* runtime, until it returns. Two workers avoid that.

On a path that calls repeatedly, convert the handle once instead and await it. That route
dispatches straight to the async service and never touches `block_on`:

```rust
let svc = local_sync_handle.into_async::<Tokio>();
svc.method().await?;
```

## Async Over RPC

Because async lives in the generated stubs and `rsbinder::serve` / `connect` only move
an `SIBinder`, the *same* async service runs over RPC with no new code. You `add` the
result of `new_async_binder` and the client converts the looked-up proxy with `into_async`:

```rust
use async_trait::async_trait;
use rsbinder::*;

struct IHelloService;
impl Interface for IHelloService {}

#[async_trait]
impl IHelloAsyncService for IHelloService {
    async fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        Ok(echo.to_owned())
    }
}
```

### The server needs a multi-threaded runtime

An RPC async server is driven by a **blocking serve worker** that calls
`rt.block_on(handler)` from a thread that is not the runtime's own. That part is fine for
either flavor — it is the same position a binder looper is in.

What rules out `current_thread` here is `run()`: it blocks the calling thread in the
accept loop. On a current-thread runtime that thread is the runtime's owner, and parking
it anywhere other than `Runtime::block_on` leaves nothing running that runtime — the
condition from [Advanced: a current-thread runtime](#advanced-a-current-thread-runtime)
above. Handlers would then hang the first time one awaits a timer, IO, or a task it
spawned. A multi-threaded runtime has no owner thread to strand, so `run()` is safe on
it.

```rust
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // RPC async servers need a MULTI-THREAD runtime: run() below parks this
    // thread in the accept loop, which a current-thread runtime cannot survive.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let rt = TokioRuntime(runtime.handle().clone());

    println!("serving over RPC at /tmp/hello.sock");
    rsbinder::serve("unix:///tmp/hello.sock")?
        .add("hello", BnHello::new_async_binder(IHelloService {}, rt))?
        .run()?; // blocks, driving this one socket
    Ok(())
}
```

Use `spawn()` instead of `run()` if you want the socket driven on a background thread
while `main` does other work (for instance from inside `runtime.block_on`).

### The client converts the looked-up proxy

The client side is the ordinary `connect` followed by `into_async` — or
`connect_async::<dyn IHelloAsync<Tokio>>(uri).await`, which does the blocking connect on
`spawn_blocking`:

```rust
let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

runtime.block_on(async {
    let hello = rsbinder::connect_async::<dyn IHelloAsync<Tokio>>("unix:///tmp/hello.sock#hello").await?;
    println!("{}", hello.echo("hi").await?);
    Ok::<_, Box<dyn std::error::Error>>(())
})?;
```

> The async-over-RPC *bridge* shown here (`new_async_binder` + the async proxy over a real
> Unix-socket session, multi-thread runtime) is verified end-to-end by
> `tests/tests/rpc_async.rs` — including many in-flight calls on one shared session under
> genuine async concurrency. That test drives the bridge through the lower-level
> `RpcSession` API directly; `serve` / `connect` are a thin wrapper over exactly that path.

### Switching transports

To run the exact same async service over **kernel binder** instead, swap only the URI —
the service `impl` is unchanged. Keep the multi-threaded runtime: `run()` parks the calling
thread in the binder pool, which a current-thread runtime's owner cannot afford (see
[Advanced: a current-thread runtime](#advanced-a-current-thread-runtime)) — use `spawn()`
there.

```rust
rsbinder::serve("binder://")?                      // instead of "unix:///tmp/hello.sock"
    .add("hello", BnHello::new_async_binder(IHelloService {}, rt))?  // the `rt` bound above
    .run()?;                                       // joins the process-wide binder pool
```

Be aware that the two transports' trust boundaries differ: see
[Security & Authorization](./security.md) — `@EnforcePermission` denies over RPC, and
`get_calling_uid()` is the kernel-vouched peer uid on Unix RPC (fail-closed on uid-less
transports). The URI makes the transport swap easy; it does not make the security models
identical.

## Implementing an Async Service

An async service struct implements the generated `IMyServiceAsyncService` trait using the
`#[async_trait]` attribute macro. Compare this with the sync version side by side.

### Sync service

```rust
use rsbinder::*;

#[derive(Default)]
struct MyService;

impl Interface for MyService {}

impl IMyService::IMyService for MyService {
    fn echo(&self, input: &str) -> rsbinder::BinderResult<String> {
        Ok(input.to_owned())
    }

    fn RepeatInt(&self, token: i32) -> rsbinder::BinderResult<i32> {
        Ok(token)
    }
}
```

### Async service

```rust
use async_trait::async_trait;
use rsbinder::*;

#[derive(Default)]
struct MyAsyncService;

impl Interface for MyAsyncService {}

#[async_trait]
impl IMyService::IMyServiceAsyncService for MyAsyncService {
    async fn echo(&self, input: &str) -> rsbinder::BinderResult<String> {
        // You can use .await on async operations here.
        Ok(input.to_owned())
    }

    async fn RepeatInt(&self, token: i32) -> rsbinder::BinderResult<i32> {
        Ok(token)
    }
}
```

The differences are minimal:

1. Add `use async_trait::async_trait;` and annotate the `impl` block with `#[async_trait]`.
2. Implement `IMyServiceAsyncService` instead of `IMyService`.
3. Prefix each method with `async`.
4. When creating the binder, use `BnXxx::new_async_binder(impl, rt())` instead of
   `BnXxx::new_binder(impl)`.

Inside async methods you can `.await` any future -- call other async services, perform async
I/O, use `tokio::time::sleep`, and so on.

## Calling Other Services Asynchronously

When an async service needs to call another Binder service, the proxy it receives is
synchronous by default. Use `into_async::<Tokio>()` to convert it into an async proxy so
you can `.await` the result.

### Async version

Based on `tests/src/bin/test_service_async.rs`:

```rust
async fn VerifyName(
    &self,
    service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
    name: &str,
) -> rsbinder::BinderResult<bool> {
    service
        .clone()
        .into_async::<Tokio>()
        .GetName()
        .await
        .map(|found_name| found_name == name)
}
```

### Sync version for comparison

From `tests/src/bin/test_service.rs`:

```rust
fn VerifyName(
    &self,
    service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
    name: &str,
) -> std::result::Result<bool, rsbinder::Status> {
    service.GetName().map(|found_name| found_name == name)
}
```

The key difference is:

- **Sync**: Call `service.GetName()` directly.
- **Async**: Clone the service reference, convert with `.into_async::<Tokio>()`, call
  `.GetName()`, and `.await` the result.

The `.clone()` is necessary because `into_async` consumes the `Strong` reference. This is a
cheap reference-count increment, not a deep copy.

## Cancellation and Nesting

Two properties of the async proxy surprise people who arrive from reactor-based async.
Neither is going to change: the first follows from the binder wire having no cancel
message, the second from how the kernel driver prevents deadlock.

### Dropping a future does not cancel the call

A generated proxy method is not wrapped in an `async` block. It issues the transaction
**when you call it**, before the returned future is ever polled. So losing a `timeout` or
a `select!` race — or dropping the future without polling it once — discards the reply,
not the call:

```rust
// The remote side runs `transfer` either way. Only the reply is thrown away.
let _ = tokio::time::timeout(Duration::from_millis(50), svc.transfer(amount)).await;
```

Wrapping a non-idempotent call in a timeout and treating expiry as "it did not happen" is
therefore wrong. The blocking thread stays occupied until the reply arrives, so repeated
timeouts against an unresponsive server fill the `spawn_blocking` pool (512 threads by
default). Cap it with `Builder::max_blocking_threads`, and bound the wait at the source:
over RPC, `RpcServer::set_reply_timeout` and the session read deadline. Kernel binder has
no such bound.

### A call made inside a handler runs inline

While a thread is handling a transaction, an async proxy call on that thread executes
**at the call site**, synchronously, before the first poll. This is deliberate: the
kernel's deadlock prevention needs the nested outbound call to come from the thread that
is holding the inbound one, and RPC needs the same thread to keep its session pin.

The consequence is that the usual combinators do nothing there:

```rust
// Inside a handler: expires never — the call has already completed.
let r = tokio::time::timeout(d, other.call()).await;

// Inside a handler: sequential, not concurrent.
let (a, b) = tokio::join!(first.call(), second.call());
```

To get real concurrency, leave the transaction context with `tokio::spawn` and call from
there. That task's calls are no longer nested, so they also lose the deadlock avoidance —
do not call back into a service that is currently blocked waiting on you.

## Creating Nested Async Binders

An async service can create and return new async binders, for instance when implementing a
factory-style method. Pass the same `rt()` helper:

```rust
async fn GetOtherTestService(
    &self,
    name: &str,
) -> rsbinder::BinderResult<rsbinder::Strong<dyn INamedCallback::INamedCallback>> {
    let mut service_map = self.service_map.lock().unwrap();
    let other_service = service_map.entry(name.into()).or_insert_with(|| {
        let named_callback = NamedCallback(name.into());
        INamedCallback::BnNamedCallback::new_async_binder(named_callback, rt())
    });
    Ok(other_service.to_owned())
}
```

This pattern is taken directly from the test suite. Note that `new_async_binder` can be
called from any async context as long as a `TokioRuntime` is provided.

## Registering Multiple Async Services

A single process can host multiple async services. Register each one before yielding to the
runtime:

```rust
runtime.block_on(async {
    let service = BnTestService::new_async_binder(
        TestService::default(), rt(),
    );
    hub::add_service(service_name, &service)
        .expect("Could not register service");

    let versioned_service = BnFooInterface::new_async_binder(
        FooInterface, rt(),
    );
    hub::add_service(versioned_service_name, &versioned_service)
        .expect("Could not register service");

    let nested_service = INestedService::BnNestedService::new_async_binder(
        NestedService, rt(),
    );
    hub::add_service(nested_service_name, &nested_service)
        .expect("Could not register service");

    // All services are now registered. Yield to the runtime.
    std::future::pending().await
})
```

All services share the same Tokio runtime and the same Binder thread pool. Incoming Binder
transactions are dispatched to the correct service automatically.

## Advanced: BoxFuture Pattern for Macros

When using declarative macros (`macro_rules!`) to reduce boilerplate in an `#[async_trait]`
impl block, there is a subtlety: `async_trait` transforms `async fn` into functions returning
a pinned boxed future, but it does not apply this transformation to functions produced by
macro expansion. The workaround is to return a `BoxFuture` manually.

This is the pattern used in the rsbinder test suite:

```rust
type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

macro_rules! impl_repeat {
    ($repeat_name:ident, $type:ty) => {
        fn $repeat_name<'a, 'b>(
            &'a self,
            token: $type,
        ) -> BoxFuture<'b, rsbinder::BinderResult<$type>>
        where
            'a: 'b,
            Self: 'b,
        {
            Box::pin(async move { Ok(token) })
        }
    };
}

macro_rules! impl_reverse {
    ($reverse_name:ident, $type:ty) => {
        fn $reverse_name<'a, 'b, 'c, 'd>(
            &'a self,
            input: &'b [$type],
            repeated: &'c mut Vec<$type>,
        ) -> BoxFuture<'d, rsbinder::BinderResult<Vec<$type>>>
        where
            'a: 'd,
            'b: 'd,
            'c: 'd,
            Self: 'd,
        {
            Box::pin(async move {
                repeated.clear();
                repeated.extend_from_slice(input);
                Ok(input.iter().rev().cloned().collect())
            })
        }
    };
}
```

These macros can then be used inside the `#[async_trait]` impl block alongside regular `async
fn` methods:

```rust
#[async_trait]
impl ITestService::ITestServiceAsyncService for TestService {
    impl_repeat! {RepeatInt, i32}
    impl_reverse! {ReverseInt, i32}

    async fn RepeatString(&self, input: &str) -> rsbinder::BinderResult<String> {
        Ok(input.into())
    }

    // ... other methods
}
```

The lifetime annotations (`'a: 'b`, `Self: 'b`) are necessary to satisfy the borrow checker
when the future captures `&self` and method arguments. This is an advanced pattern -- you
will only need it when combining declarative macros with async trait implementations.

Compare with the sync macro version, which is simpler because no future is involved:

```rust
macro_rules! impl_repeat {
    ($repeat_name:ident, $type:ty) => {
        fn $repeat_name(
            &self, token: $type,
        ) -> std::result::Result<$type, rsbinder::Status> {
            Ok(token)
        }
    };
}
```

## Tips

- **Prefer `new_multi_thread()`.** `new_current_thread()` is correct only while its
  owning thread stays parked in `Runtime::block_on`; park it elsewhere and nothing runs
  that runtime — no error, just handlers that never return. An RPC server that calls
  `run()` parks it elsewhere by definition. A current-thread handle also rules out
  reaching a second local service through a sync handle from inside a handler.
- **Never block inside an async method.** A CPU-bound or blocking call belongs
  in `tokio::task::spawn_blocking`, and a call to another binder service
  belongs behind `into_async::<Tokio>()` — a blocking proxy call from an async
  method stalls the executor thread.
- **Budget threads in both directions.** Outbound, each async call in flight holds one
  blocking-pool thread plus one kernel binder thread until the reply lands. Inbound, each
  handler holds one binder thread for the whole `block_on`, so awaiting slow IO inside a
  handler drains the pool at `max_threads` and nothing else gets served.
  `Builder::max_blocking_threads` caps the outbound half.
- **Sync and async services coexist in one process.** Register some with
  `new_binder` and others with `new_async_binder`; they share the same binder
  thread pool.

## Summary

Async services in rsbinder follow the same structure as sync services, with a few additional
steps:

1. Build a multi-threaded Tokio runtime — `#[tokio::main]` is one.
2. Implement `IMyServiceAsyncService` with `#[async_trait]` instead of `IMyService`.
3. Create binders with `BnXxx::new_async_binder(impl, TokioRuntime(Handle::current()))`.
4. Register them with `serve(uri).add(name, &svc)` and start with `spawn()`, not `run()`.
5. Keep the process alive with `std::future::pending().await`.
6. Use `into_async::<Tokio>()` when calling other Binder services from async code —
   including a local service of your own, where the sync handle would re-enter `block_on`.

Because async lives in the generated stubs, the same async service runs over **either
transport** — `add` the `new_async_binder` result to `rsbinder::serve(uri)` and look it up
with [`connect`](./cross-transport-services.md). Nothing in the recipe changes but the URI —
a non-`binder://` URI additionally needs rsbinder's `rpc` feature, which is not on by default.

For complete working examples, see `tests/src/bin/test_service_async.rs` (kernel binder) and
`tests/tests/rpc_async.rs` (RPC) in the rsbinder repository.
