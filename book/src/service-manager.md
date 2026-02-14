# Service Manager (HUB)

The service manager is the central registry for Binder services. Every service
that wants to be discoverable by other processes registers itself with the
service manager under a well-known name, and every client that needs a service
looks it up by that name. In rsbinder, the service manager is referred to as
**HUB** and is accessed through the `rsbinder::hub` module.

On Linux, the HUB is provided by the `rsb_hub` binary that ships with the
`rsbinder-tools` crate. On Android, the system's native `servicemanager`
fulfills this role, and rsbinder talks to it using the same Binder protocol.

## Running the Service Manager (Linux)

Before any service can register or any client can perform lookups, the HUB
process must be running:

```bash
# Build and run the service manager
$ cargo run --bin rsb_hub
```

`rsb_hub` opens the Binder device, becomes the context manager (handle 0),
and enters a loop that processes registration and lookup requests. It must
remain running for the lifetime of the system's Binder services.

## Registering a Service

To make a service available to other processes, create a Binder object and
register it with `hub::add_service`:

```rust
use rsbinder::*;

// Create the Binder service object
let service = BnMyService::new_binder(MyServiceImpl);

// Register it under a descriptive name
hub::add_service("com.example.myservice", service.as_binder())?;
```

The name passed to `add_service` is the identifier that clients will use to
find the service.

### Registration Rules

The service manager enforces several constraints on service names:

- **Maximum length**: 127 characters. Names of 128 characters or longer are
  rejected.
- **Allowed characters**: Alphanumeric characters, dots (`.`), underscores
  (`_`), and hyphens (`-`). Special characters such as `$` are not allowed.
- **Non-empty**: An empty string is rejected.
- **Overwrite permitted**: Registering a service with a name that is already
  in use replaces the previous registration.

```rust
let service = BnFoo::new_binder(FooImpl {});

// Empty names are rejected
assert!(hub::add_service("", service.as_binder()).is_err());

// Valid name
assert!(hub::add_service("foo", service.as_binder()).is_ok());

// Maximum length (127 characters)
let long_name = "a".repeat(127);
assert!(hub::add_service(&long_name, service.as_binder()).is_ok());

// Too long (128 characters)
let too_long = "a".repeat(128);
assert!(hub::add_service(&too_long, service.as_binder()).is_err());

// Special characters are rejected
assert!(hub::add_service("happy$foo$fo", service.as_binder()).is_err());
```

## Looking Up Services

rsbinder provides three ways to find a registered service, each suited to a
different use case.

### Type-Safe Lookup with `get_interface`

The most common approach is `hub::get_interface`, which retrieves the service
and casts it to the expected AIDL interface type in one step:

```rust
let service: rsbinder::Strong<dyn IMyService::IMyService> =
    hub::get_interface("com.example.myservice")?;

// Now call methods directly on the typed proxy
let result = service.some_method()?;
```

If the service is not registered, `get_interface` returns an error.

### Raw Binder Lookup with `get_service`

If you need the untyped `SIBinder` handle (for example, to inspect the
descriptor or pass it to another API), use `hub::get_service`:

```rust
let binder: Option<SIBinder> = hub::get_service("com.example.myservice");
if let Some(binder) = binder {
    println!("Found service with descriptor: {}", binder.descriptor());
}
```

Returns `None` if the service is not registered.

### Non-Blocking Check with `check_service`

`hub::check_service` behaves like `get_service` but is intended as a
non-blocking availability check:

```rust
let binder: Option<SIBinder> = hub::check_service("com.example.myservice");
if binder.is_some() {
    println!("Service is available");
} else {
    println!("Service is not yet registered");
}
```

## Listing Registered Services

To enumerate all services currently registered with the HUB, use
`hub::list_services` with a dump priority flag:

```rust
let services = hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT);
for name in &services {
    println!("Available: {}", name);
}
```

The dump priority flag filters which services are returned. The most commonly
used flags are:

| Flag                          | Description                              |
|-------------------------------|------------------------------------------|
| `DUMP_FLAG_PRIORITY_DEFAULT`  | Services with default priority           |
| `DUMP_FLAG_PRIORITY_HIGH`     | High-priority services                   |
| `DUMP_FLAG_PRIORITY_CRITICAL` | Critical system services                 |
| `DUMP_FLAG_PRIORITY_NORMAL`   | Normal-priority services                 |
| `DUMP_FLAG_PRIORITY_ALL`      | All services regardless of priority      |

## Service Notifications

You can register a callback that fires whenever a service with a particular
name is registered (or re-registered). This is useful for clients that start
before the service they depend on is available.

### Defining a Callback

Implement the `hub::IServiceCallback` trait:

```rust
struct MyServiceCallback;

impl rsbinder::Interface for MyServiceCallback {}

impl hub::IServiceCallback for MyServiceCallback {
    fn onRegistration(
        &self,
        name: &str,
        service: &rsbinder::SIBinder,
    ) -> rsbinder::status::Result<()> {
        println!("Service registered: {name}");
        Ok(())
    }
}
```

### Registering and Unregistering

Wrap the callback in a Binder object and pass it to the HUB:

```rust
let callback = hub::BnServiceCallback::new_binder(MyServiceCallback);

// Start receiving notifications
hub::register_for_notifications("com.example.myservice", &callback)?;

// Later, when notifications are no longer needed
hub::unregister_for_notifications("com.example.myservice", &callback)?;
```

The callback will be invoked each time a service matching the given name is
registered, including if it is re-registered after a restart.

## Checking if a Service is Declared

`hub::is_declared` checks whether a service name has been declared in the
system's service configuration (VINTF manifest on Android). This is distinct
from whether the service is currently running:

```rust
let declared = hub::is_declared("com.example.myservice");
if declared {
    println!("Service is declared in the manifest");
} else {
    println!("Service is not declared");
}
```

On Linux with `rsb_hub`, this typically returns `false` because there is no
VINTF manifest. On Android, it reflects the device's hardware interface
declarations.

## Debug Information

For diagnostics, `hub::get_service_debug_info` returns metadata about every
registered service, including its name and the PID of the process hosting it:

```rust
let debug_info = hub::get_service_debug_info()?;
for info in &debug_info {
    println!("Service: {} (pid: {})", info.name, info.debugPid);
}
```

The returned `ServiceDebugInfo` struct has two fields:

- `name` (`String`) -- the registered service name.
- `debugPid` (`i32`) -- the PID of the process that registered the service.

This feature is available on Android 12 and above. On Android 11, calling
`get_service_debug_info` returns an error.

## Linux vs. Android Differences

While rsbinder aims for API compatibility across both platforms, there are
important behavioral differences between the Linux HUB (`rsb_hub`) and
Android's native `servicemanager`:

| Aspect                  | Linux (`rsb_hub`)                       | Android (`servicemanager`)              |
|-------------------------|-----------------------------------------|-----------------------------------------|
| **Process**             | User-space `rsb_hub` binary             | System `servicemanager` daemon          |
| **Access control**      | No SELinux enforcement                  | Full SELinux MAC policy enforcement     |
| **VINTF manifests**     | Not supported (`is_declared` is false)  | Supported and enforced                  |
| **Service debug info**  | Supported                               | Supported (Android 12+)                 |
| **Binder device**       | Must be created with `rsb_device`       | Managed by Android init                 |
| **Version selection**   | Always uses Android 16 protocol         | Auto-detected from SDK version          |
| **Death notifications** | Supported                               | Supported                               |

On Android, rsbinder automatically detects the SDK version and uses the
appropriate service manager protocol (Android 11 through 16). On Linux, it
always uses the Android 16 protocol, which is what `rsb_hub` implements.

## Using the ServiceManager Object Directly

The convenience functions (`hub::add_service`, `hub::get_service`, etc.) use
a global singleton `ServiceManager` under the hood. If you need more control,
you can obtain the `ServiceManager` instance directly:

```rust
use rsbinder::hub;

let sm = hub::default();

// Use methods on the ServiceManager instance
let service = sm.get_service("com.example.myservice");
let services = sm.list_services(hub::DUMP_FLAG_PRIORITY_ALL);
```

This is equivalent to using the free functions but allows you to pass the
service manager as a parameter or store it in a struct.

## Tips and Best Practices

- **Initialize ProcessState first.** Before calling any `hub::` function, you
  must call `ProcessState::init_default()` (or `ProcessState::init()` with a
  custom binder path). Failing to do so will panic at runtime.

- **Use descriptive service names.** Follow a reverse-domain naming convention
  (e.g., `com.example.myservice`) to avoid name collisions with other
  services.

- **Register for notifications instead of polling.** If your client starts
  before the service it depends on, use `register_for_notifications` rather
  than repeatedly calling `get_service` in a loop.

- **Handle registration failures.** `add_service` can fail if the name is
  invalid or if the caller lacks permission (on Android with SELinux). Always
  check the result.

- **Use `get_interface` for type safety.** Prefer `hub::get_interface` over
  `hub::get_service` when you know the expected interface type. It returns a
  strongly-typed proxy that provides compile-time guarantees.

- **Debug with `list_services` and `get_service_debug_info`.** When
  troubleshooting, list all registered services and inspect their debug
  information to verify that services are registered from the expected
  processes.
