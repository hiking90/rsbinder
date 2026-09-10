# Hello World!
This tutorial will guide you through creating a simple Binder service that echoes a string back to the client, and a client program that uses the service.

## Create a new Rust project
Create a library project for the common library used by both the client and service:
```bash
$ cargo new --lib hello
```

## Modify Cargo.toml
In the hello project's Cargo.toml, add the following dependencies:

```toml
[package]
name = "hello"
version = "0.1.0"
publish = false
edition = "2021"

[dependencies]
rsbinder = "0.11"
async-trait = "0.1"
env_logger = "0.11"

[build-dependencies]
rsbinder-aidl = "0.11"
```

## Create an AIDL File
Create an aidl folder in the project's top directory to manage AIDL files:
```bash
$ mkdir -p aidl/hello
$ touch aidl/hello/IHello.aidl
```
> **Directory ↔ package mapping.** The folder name under `aidl/` **must
> match** the `package` declaration at the top of the `.aidl` file. So
> `aidl/hello/IHello.aidl` requires `package hello;` (below), and the
> generated Rust path is `crate::hello::IHello::IHello`. A nested
> package like `package com.example.hello;` would need
> `aidl/com/example/hello/IHello.aidl`. This mirrors Android's AIDL
> convention.

Create the `aidl/hello/IHello.aidl` file with the following contents:
```aidl
package hello;

// Defining the IHello Interface
interface IHello {
    // Defining the echo() Function.
    // The function takes a single parameter of type String and returns a value of type String.
    String echo(in String hello);
}
```
For more information on AIDL syntax, refer to the [Android AIDL documentation](https://source.android.com/docs/core/architecture/aidl).

## Create the build.rs
Create a `build.rs` file in the **project root** (next to `Cargo.toml`) to compile the AIDL file and generate Rust code:
```rust
use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/hello/IHello.aidl"))
        .output(PathBuf::from("hello.rs"))
        .generate()
        .unwrap();
}
```
> **Important**: The `build.rs` file must be placed in the project root directory, **not** inside `src/`. If placed in the wrong location, you will get a compile error: `environment variable OUT_DIR not defined at compile time`. Cargo only recognizes `build.rs` at the project root.

## Create a common library for Client and Service
For the Client and Service, create a library that includes the Rust code generated from AIDL.

Create src/lib.rs and add the following content.
```rust
// Include the code hello.rs generated from AIDL and re-export the
// IHello interface items (the trait, BnHello, BpHello, ...) so both
// the client and the service can use them.
rsbinder::include_aidl!("hello", crate::hello::IHello::*);

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";
```

The `rsbinder::include_aidl!` macro is the one-call form of the manual
include/re-export pair. The above is equivalent to:

```rust
include!(concat!(env!("OUT_DIR"), "/hello.rs"));
pub use crate::hello::IHello::*;
```

## Create a service
Create the `src/bin/` directory and add the service file. Cargo automatically recognizes `.rs` files under `src/bin/` as binary targets, so no `[[bin]]` section is needed in `Cargo.toml`.

Create `src/bin/hello_service.rs`:
```rust
use env_logger::Env;
use rsbinder::*;

use hello::*;

// Define a struct that implements the IHello interface.
struct IHelloService;

// Implement the IHello interface for the IHelloService.
impl Interface for IHelloService {
    // Reimplement the dump method. This is optional.
    fn dump(&self, writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        writeln!(writer, "Dump IHelloService")?;
        Ok(())
    }
}

// Implement the IHello interface for the IHelloService.
impl IHello for IHelloService {
    // Implement the echo method.
    fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Create a binder service.
    println!("Creating service...");
    let service = BnHello::new_binder(IHelloService{});

    // `serve("binder://")` initializes the kernel binder process state,
    // `add` registers the service with the service manager (it takes
    // anything convertible into `SIBinder`), and `run` starts the binder
    // thread pool and joins it — this blocks for the life of the service.
    println!("Serving {SERVICE_NAME}...");
    rsbinder::serve("binder://")?
        .add(SERVICE_NAME, &service)?
        .run()?;
    Ok(())
}
```

> The same three calls with `serve("unix:///tmp/hello.sock")` serve the
> service over a Unix socket instead of kernel binder — see
> [Cross-Transport Services](./cross-transport-services.md). The low-level
> form (`ProcessState::init_default()`, `start_thread_pool()`,
> `hub::add_service()`, `join_thread_pool()`) remains available.

## Create a client
Create the src/bin/hello_client.rs file and configure it as follows.
```rust
#![allow(non_snake_case)]

use env_logger::Env;
use rsbinder::*;
use hello::*;
use hub::{BnServiceCallback, IServiceCallback};
use std::sync::Arc;

struct MyServiceCallback {}

impl Interface for MyServiceCallback {}

impl IServiceCallback for MyServiceCallback {
    fn onRegistration(&self, name: &str, _service: &SIBinder) -> rsbinder::BinderResult<()> {
        println!("MyServiceCallback: {name}");
        Ok(())
    }
}

struct MyDeathRecipient {}

impl DeathRecipient for MyDeathRecipient {
    fn binder_died(&self, _who: &WIBinder) {
        println!("MyDeathRecipient");
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // `connect("binder://<name>")` initializes the kernel binder process
    // state, blocks until the service is registered (the event-driven
    // equivalent of AOSP's `waitForService`), and casts it to the interface.
    let hello: rsbinder::Strong<dyn IHello> = rsbinder::connect(&format!("binder://{SERVICE_NAME}"))?;

    println!("list services:");
    // Service-manager extras stay on `hub`.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{name}");
    }

    let service_callback = BnServiceCallback::new_binder(MyServiceCallback {});
    hub::register_for_notifications(SERVICE_NAME, &service_callback)?;

    // `link_to_death_arc` takes the concrete `Arc<MyDeathRecipient>` directly.
    // Keep `recipient` alive for the link's lifetime (the link holds only a weak ref).
    let recipient = Arc::new(MyDeathRecipient {});
    hello.link_to_death_arc(&recipient)?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    Ok(ProcessState::join_thread_pool()?)
}
```

## Project folder and file structure
```
.
├── Cargo.toml
├── aidl
│   └── hello
│       └── IHello.aidl
├── build.rs
└── src
    ├── bin
    │   ├── hello_client.rs
    │   └── hello_service.rs
    └── lib.rs
```

## Run Hello Service and Client

Before running the service and client, start the service manager. It refuses
to start without an access-control policy, so a local try-out passes
`--insecure-allow-all` — see [Access control](./service-manager.md#access-control)
for the real thing:

```bash
# In terminal 1: Start the service manager
$ rsb_hub --insecure-allow-all
```

Now you can run the service and client:

```bash
# In terminal 2: Run the service
$ cargo run --bin hello_service
```

```bash
# In terminal 3: Run the client
$ cargo run --bin hello_client
```

### Expected Output

**hello_service** output:
```
Creating service...
Serving my.hello...
```

**hello_client** output:
```
list services:
my.hello
manager
MyServiceCallback: my.hello
Result: Hello World!
```

Along the way the client also listed the registered services, registered for
availability notifications, and linked a death recipient.

### Troubleshooting

1. **"ProcessState is not initialized!"** — `rsbinder::serve("binder://")` / `rsbinder::connect("binder://…")` (or the low-level `ProcessState::init_default()`) must run before any other rsbinder API that touches the binder device.
2. **"environment variable OUT_DIR not defined"** — `build.rs` must be in the project root, next to `Cargo.toml`, not inside `src/`.
3. **Client blocks without output** — `rsbinder::connect("binder://…")` waits until the service is registered; make sure the service is running.
4. **Permission errors** — the binder device node defaults to `0600`, root only. Give it a group with `rsb_device binder --group <group> --mode 0660` and make sure you are in that group.
5. **Service manager not found** — check that `rsb_hub` is running, and that it did not exit at startup for want of a policy.

## Next Steps

- **[AIDL Data Types](./aidl-data-types.md)** — how AIDL types map to Rust: primitives, arrays, strings, nullable types
- **[Parcelable](./aidl-parcelable.md)** — custom data structures that cross the Binder boundary
- **[Enum and Union](./aidl-enum-union.md)** — enum and union types in your interfaces
- **[Annotations](./aidl-annotations.md)** — `@RustDerive`, `@Backing`, `@nullable`, and the rest
- **[Interface Macros](./interface-macros.md)** — the same interface as a Rust trait, with no `.aidl` file
- **[Service Patterns](./service-patterns.md)** — `dump()`, default implementations, multi-service processes
- **[Async Service](./async-service.md)** — async/await on the tokio runtime
- **[Callbacks and Interfaces](./callbacks-and-interfaces.md)** — bidirectional communication and death recipients
- **[ParcelFileDescriptor](./parcel-file-descriptor.md)** — passing file descriptors across processes
- **[Error Handling](./error-handling.md)** — service-specific errors, status codes, exceptions
- **[Security & Authorization](./security.md)** — identifying the caller and deciding what it may do
- **[Service Manager (HUB)](./service-manager.md)** — registration, lookup, notifications, debug info
- **[docs.rs/rsbinder](https://docs.rs/rsbinder)** — the full API reference

### Run the Test Suite
The **rsbinder** project includes a comprehensive test suite ported from Android:

```bash
# Terminal 1: Start service manager
$ cargo run --bin rsb_hub -- --insecure-allow-all

# Terminal 2: Start test service
$ cargo run --bin test_service

# Terminal 3: Run tests
$ cargo test -p tests test_client::
```

### Study Real Examples
Check out the **rsbinder** repository for more complex examples:
- **example-hello**: The complete example from this tutorial (note: the package name is `example-hello`, so import paths use `example_hello::*` instead of `hello::*`)
- **tests**: Comprehensive test cases showing various IPC scenarios
- **rsbinder-tools**: Real-world service manager implementation
