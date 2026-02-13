# Hello World!
This tutorial will guide you through creating a simple Binder service that echoes a string back to the client, and a client program that uses the service.

## Create a new Rust project
Create a new Rust project using Cargo:
```
$ cargo new --lib hello
```
Create a library project for the common library used by both the client and service:

## Modify Cargo.toml
In the hello project's Cargo.toml, add the following dependencies:

```
[package]
name = "hello"
version = "0.1.0"
publish = false
edition = "2021"

[dependencies]
rsbinder = "0.4.0"
lazy_static = "1"
async-trait = "0.1"
env_logger = "0.11"

[build-dependencies]
rsbinder-aidl = "0.4.0"

```
Add rsbinder, lazy_static, and async-trait to [dependencies], and add rsbinder-aidl to [build-dependencies].

## Create an AIDL File
Create an aidl folder in the project's top directory to manage AIDL files:
```bash
$ mkdir -p aidl/hello
$ touch aidl/hello/IHello.aidl
```
The reason for creating an additional **hello** folder is to create a namespace for the **hello** package.

Create the `aidl/hello/IHello.aidl` file with the following contents:
```
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
Create a `build.rs` file in the project root to compile the AIDL file and generate Rust code:
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
This uses **rsbinder-aidl** to specify the AIDL source file (`IHello.aidl`) and the generated Rust file name (`hello.rs`), and then generates the code during the build process.

## Create a common library for Client and Service
For the Client and Service, create a library that includes the Rust code generated from AIDL.

Create src/lib.rs and add the following content.
```
// Include the code hello.rs generated from AIDL.
include!(concat!(env!("OUT_DIR"), "/hello.rs"));

// Set up to use the APIs provided in the code generated for Client and Service.
pub use crate::hello::IHello::*;

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";
```

## Create a service
Let's configure the src/bin/hello_service.rs file as follows.
```
use env_logger::Env;
use rsbinder::*;

use hello::*;

// Define the name of the service to be registered in the HUB(service manager).
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
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with the default binder path and the default max threads.
    println!("Initializing ProcessState...");
    ProcessState::init_default();

    // Start the thread pool.
    // This is optional. If you don't call this, only one thread will be created to handle the binder transactions.
    println!("Starting thread pool...");
    ProcessState::start_thread_pool();

    // Create a binder service.
    println!("Creating service...");
    let service = BnHello::new_binder(IHelloService{});

    // Add the service to binder service manager.
    println!("Adding service to hub...");
    hub::add_service(SERVICE_NAME, service.as_binder())?;

    // Join the thread pool.
    // This is a blocking call. It will return when the thread pool is terminated.
    Ok(ProcessState::join_thread_pool()?)
}
```

## Create a client
Create the src/bin/hello_client.rs file and configure it as follows.
```
use env_logger::Env;
use rsbinder::*;
use hello::*;
use hub::{BnServiceCallback, IServiceCallback};
use std::sync::Arc;

struct MyServiceCallback {}

impl Interface for MyServiceCallback {}

impl IServiceCallback for MyServiceCallback {
    fn onRegistration(&self, name: &str, _service: &SIBinder) -> rsbinder::status::Result<()> {
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

    // Initialize ProcessState with the default binder path and the default max threads.
    ProcessState::init_default();

    println!("list services:");
    // This is an example of how to use service manager.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{}", name);
    }

    let service_callback = BnServiceCallback::new_binder(MyServiceCallback {});
    hub::register_for_notifications(SERVICE_NAME, &service_callback)?;

    // Create a Hello proxy from binder service manager.
    let hello: rsbinder::Strong<dyn IHello> = hub::get_interface(SERVICE_NAME)
        .unwrap_or_else(|_| panic!("Can't find {SERVICE_NAME}"));

    let recipient = Arc::new(MyDeathRecipient {});
    hello
        .as_binder()
        .link_to_death(Arc::downgrade(&(recipient as Arc<dyn DeathRecipient>)))?;

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

Before running the service and client, make sure you have the service manager running:

```bash
# In terminal 1: Start the service manager
$ rsb_hub
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
Initializing ProcessState...
Starting thread pool...
Creating service...
Adding service to hub...
```

**hello_client** output:
```
list services:
my.hello
manager
MyServiceCallback: my.hello
Result: Hello World!
```

The client demonstrates several advanced features:
- **Service Discovery**: Lists all available services
- **Service Callbacks**: Registers for service availability notifications
- **Death Recipients**: Monitors service lifecycle for cleanup
- **Type-safe Proxies**: Uses strongly-typed interface for service calls

### Troubleshooting

If you encounter issues:

1. **"Can't find my.hello"** - Ensure the service is running and registered
2. **Permission errors** - Check that binder device has correct permissions (0666)
3. **Service manager not found** - Verify `rsb_hub` is running
4. **Build errors** - Ensure all dependencies are correctly specified in Cargo.toml

## Next Steps

Congratulations! You've successfully created your first Binder service and client. Here are some next steps to explore:

### Explore Advanced Features
- **Async Services**: Use async/await with tokio runtime
- **Complex Data Types**: Define custom Parcelable structs
- **Service Callbacks**: Implement bidirectional communication
- **Error Handling**: Learn about Status codes and error propagation

### Run the Test Suite
The **rsbinder** project includes a comprehensive test suite with 96 test cases (90 currently passing) ported from Android:

```bash
# Terminal 1: Start service manager
$ cargo run --bin rsb_hub

# Terminal 2: Start test service
$ cargo run --bin test_service

# Terminal 3: Run tests
$ cargo test test_client::
```

### Study Real Examples
Check out the **rsbinder** repository for more complex examples:
- **example-hello**: The complete example from this tutorial
- **tests**: Comprehensive test cases showing various IPC scenarios
- **rsbinder-tools**: Real-world service manager implementation
