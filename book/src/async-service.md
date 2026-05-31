# Async Service

rsbinder supports async/await with the [Tokio](https://tokio.rs/) runtime, making it
straightforward to build non-blocking Binder services. The `tokio` feature is enabled by
default in rsbinder, so no extra feature flags are required for most projects. This chapter
explains how to implement async Binder services, how they differ from their synchronous
counterparts, and the patterns you will encounter when working with them.

If you have not yet read the [Hello, World!](./hello-world.md) chapter, it is recommended to
do so first -- the async concepts here build on the synchronous service and client covered
there.

## Async is transport-agnostic

Async lives in the **AIDL-generated stubs**, not in the transport. The compiler
emits the async trait (`IMyServiceAsyncService`), the async proxy
(`into_async::<Tokio>()`), and the `new_async_binder` constructor *once*, and
the very same async `impl` runs unchanged over **kernel binder or RPC** — including
through the transport-agnostic [`service` facade](./cross-transport-services.md).

The reason is that the facade and the transports only ever move a transport-agnostic
`SIBinder` around (register a binder under a name, look one up). Whether that binder is
sync- or async-backed is decided entirely by which constructor produced it
(`new_binder` vs `new_async_binder`) — a fact the transport never sees. So async needs
**no transport-specific code**: pick the constructor, pick the transport, and the two
compose.

This chapter starts with the kernel-binder setup, then shows the RPC and unified-facade
variants — which differ only in the bootstrap and the runtime flavor.

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
| Main loop | `ProcessState::join_thread_pool()` | `std::future::pending().await` |
| Runtime | Not needed | Tokio runtime required |

The AIDL compiler generates both the sync trait (`IMyService`) and the async trait
(`IMyServiceAsyncService`) from the same `.aidl` file. You choose which one to implement
depending on whether your service needs async capabilities.

The trait, proxy, and constructor rows above are **identical across transports** — only the
"Main loop" and "Runtime" rows are the kernel-binder values shown here. Over RPC the main
loop is `rpc::Host::serve()` (or `serve_background()`) and the runtime must be
**multi-threaded**; see [Async Over RPC and the Unified Facade](#async-over-rpc-and-the-unified-facade)
below.

## Setting Up the Tokio Runtime (kernel binder)

An async Binder service must run inside a Tokio runtime. The standard pattern for **kernel
binder** is:

1. Initialize the Binder process state and thread pool (same as sync).
2. Build a Tokio runtime.
3. Inside the runtime, create and register async services.
4. Yield to the runtime with `std::future::pending().await`.

Here is the full setup, based on
`tests/src/bin/test_service_async.rs`:

```rust
use rsbinder::*;

fn rt() -> TokioRuntime<tokio::runtime::Handle> {
    TokioRuntime(tokio::runtime::Handle::current())
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Initialize Binder -- same as the sync case.
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    // Build a single-threaded Tokio runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        // Create and register the async service (see next section).
        let service = BnMyService::new_async_binder(
            MyAsyncService::default(), rt(),
        );
        hub::add_service("com.example.myservice", service.as_binder())
            .expect("Could not register service");

        // Yield to the runtime. This keeps the process alive and
        // drives the Tokio event loop on the current thread.
        std::future::pending().await
    })
}
```

> `ProcessState::init_default()` returns `Result<&'static ProcessState, …>`,
> so a `main` that calls it should also return `Result` (or call
> `.expect()` / `.unwrap()` for the very first line of a quick demo).

There are several things to note here:

- **`rt()`** is a small helper that wraps the current Tokio runtime handle in a
  `TokioRuntime`. Every call to `new_async_binder` requires a `TokioRuntime` so it knows
  where to spawn async work.
- **`new_current_thread()`** creates a single-threaded Tokio runtime. This is the recommended
  choice for Binder services because the Binder thread pool already provides its own threads.
- **`std::future::pending().await`** is a future that never resolves. It keeps the `block_on`
  call (and therefore the process) alive indefinitely, which is the async equivalent of the
  synchronous `ProcessState::join_thread_pool()`.

## Async Over RPC and the Unified Facade

Because async lives in the generated stubs and the [`service`
facade](./cross-transport-services.md) only moves an `SIBinder`, the *same* async service
runs over RPC with no new code. You register the result of `new_async_binder` through the
generic `Registry`, and the client converts the looked-up proxy with `into_async`:

```rust
use async_trait::async_trait;
use rsbinder::service::{rpc, Registry, Broker};
use rsbinder::*;

struct IHelloService;
impl Interface for IHelloService {}

#[async_trait]
impl IHelloAsyncService for IHelloService {
    async fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

/// Registration is written once, generic over the transport — identical to the
/// sync facade example in "Cross-Transport Services". Only the binder
/// constructor differs (`new_async_binder` instead of `new_binder`).
fn register<R: Registry>(
    reg: &R,
    rt: TokioRuntime<tokio::runtime::Handle>,
) -> rsbinder::Result<()> {
    let binder = BnHello::new_async_binder(IHelloService {}, rt).as_binder();
    reg.add_service("hello", binder)
}
```

### The server needs a multi-threaded runtime

This is the one substantive difference from the kernel setup. An RPC async server is driven
by a **blocking `serve` worker** that calls `rt.block_on(handler)`. That `block_on` runs on a
thread that is *not* the runtime's own, so the runtime must be a **multi-threaded** one — a
`current_thread` runtime cannot be driven from an outside thread while it is also being kept
alive elsewhere. (Kernel binder gets away with `current_thread` only because the binder
thread pool, not a `serve` loop, supplies the worker threads.)

```rust
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // RPC async servers require a MULTI-THREAD runtime — the blocking serve
    // worker calls `rt.block_on(handler)` from a non-runtime thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();

    let host = rpc::Host::unix("/tmp/hello.sock")?;
    register(&host, TokioRuntime(handle))?;
    println!("serving over RPC at /tmp/hello.sock");
    host.serve()?; // blocks, driving this one socket
    Ok(())
}
```

Use [`rpc::Host::serve_background()`](./cross-transport-services.md) instead of `serve()`
if you want the socket driven on a background thread while `main` does other work.

### The client converts the looked-up proxy

The client side is the ordinary facade lookup followed by `into_async`:

```rust
let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

let broker = rpc::Broker::unix("/tmp/hello.sock")?;
let hello = broker.get_interface::<dyn IHello>("hello")?; // sync Strong<dyn IHello>

runtime.block_on(async {
    let hello = hello.into_async::<Tokio>();
    println!("{}", hello.echo("hi").await?);
    Ok::<_, Box<dyn std::error::Error>>(())
})?;
```

> The blocking bootstrap — `Broker::unix`, `get_interface` — is done *before* entering the
> runtime; only the transacts themselves are `.await`ed. This mirrors the verified end-to-end
> test `tests/tests/rpc_async.rs`, which drives the generated async `Bp*` against an async
> service over a real Unix-socket session, including many in-flight calls on one shared
> session under genuine async concurrency.

### Switching transports

To run the exact same async service over **kernel binder** instead, swap only the host
construction and the runtime flavor — the `register` helper and the service `impl` are
unchanged:

```rust
let host = kernel::Host::new()?;      // instead of rpc::Host::unix(..)
register(&host, rt())?;
host.serve()?;                        // joins the process-wide binder pool
```

Be aware that the two transports' trust boundaries differ: see
[Security & Authorization](./security.md) — `@EnforcePermission` denies over RPC, and
`get_calling_uid()` is the kernel-vouched peer uid on Unix RPC (fail-closed on uid-less
transports). The facade makes the transport swap easy; it does not make the security models
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
    fn echo(&self, input: &str) -> rsbinder::status::Result<String> {
        Ok(input.to_owned())
    }

    fn RepeatInt(&self, token: i32) -> rsbinder::status::Result<i32> {
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
    async fn echo(&self, input: &str) -> rsbinder::status::Result<String> {
        // You can use .await on async operations here.
        Ok(input.to_owned())
    }

    async fn RepeatInt(&self, token: i32) -> rsbinder::status::Result<i32> {
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
) -> rsbinder::status::Result<bool> {
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

## Creating Nested Async Binders

An async service can create and return new async binders, for instance when implementing a
factory-style method. Pass the same `rt()` helper:

```rust
async fn GetOtherTestService(
    &self,
    name: &str,
) -> rsbinder::status::Result<rsbinder::Strong<dyn INamedCallback::INamedCallback>> {
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
    hub::add_service(service_name, service.as_binder())
        .expect("Could not register service");

    let versioned_service = BnFooInterface::new_async_binder(
        FooInterface, rt(),
    );
    hub::add_service(versioned_service_name, versioned_service.as_binder())
        .expect("Could not register service");

    let nested_service = INestedService::BnNestedService::new_async_binder(
        NestedService, rt(),
    );
    hub::add_service(nested_service_name, nested_service.as_binder())
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
        ) -> BoxFuture<'b, rsbinder::status::Result<$type>>
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
        ) -> BoxFuture<'d, rsbinder::status::Result<Vec<$type>>>
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

    async fn RepeatString(&self, input: &str) -> rsbinder::status::Result<String> {
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

## Tips and Best Practices

- **Use `#[async_trait]`** from the `async-trait` crate for all async trait implementations.
  This is required because Rust does not yet have native async trait support in all contexts
  that rsbinder needs.

- **`into_async::<Tokio>()`** converts a synchronous proxy into an async proxy. Always use
  this when calling another Binder service from an async context, rather than making blocking
  calls that could stall the Tokio runtime.

- **`std::future::pending().await`** is the idiomatic way to keep an async service process
  alive. Unlike the sync approach where `ProcessState::join_thread_pool()` blocks the main
  thread, the async approach yields to Tokio so the runtime can drive spawned tasks.

- **The `rt()` helper should capture the current runtime handle.** Define it as a function
  that returns `TokioRuntime(tokio::runtime::Handle::current())` and call it from within the
  Tokio runtime context.

- **Both sync and async services can coexist in the same process.** You can register some
  services with `new_binder` and others with `new_async_binder`. They share the same Binder
  thread pool.

- **Prefer `new_current_thread()`** for the Tokio runtime builder. The Binder thread pool
  handles multi-threaded transaction dispatching already, so a multi-threaded Tokio runtime
  is typically unnecessary.

- **Avoid blocking the Tokio runtime.** If your async service method must perform a
  CPU-intensive or blocking operation, use `tokio::task::spawn_blocking` to move that work
  off the async executor thread.

## Summary

Async services in rsbinder follow the same structure as sync services, with a few additional
steps:

1. Build a Tokio runtime and run your service setup inside `runtime.block_on(async { ... })`.
2. Define an `rt()` helper that wraps `tokio::runtime::Handle::current()`.
3. Implement `IMyServiceAsyncService` with `#[async_trait]` instead of `IMyService`.
4. Create binders with `BnXxx::new_async_binder(impl, rt())`.
5. Use `into_async::<Tokio>()` when calling other Binder services from async code.
6. Keep the process alive with `std::future::pending().await` (kernel binder) or
   `rpc::Host::serve()` / `serve_background()` (RPC).

Because async lives in the generated stubs, the same async service runs over **either
transport** — register the `new_async_binder` result through the [`service`
facade](./cross-transport-services.md)'s `Registry` and look it up through `Broker`. The only
RPC-specific requirement is a **multi-threaded** runtime, because the blocking `serve` worker
calls `rt.block_on(handler)`.

For complete working examples, see `tests/src/bin/test_service_async.rs` (kernel binder) and
`tests/tests/rpc_async.rs` (RPC) in the rsbinder repository.
