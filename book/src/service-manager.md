# Service Manager (HUB)

The service manager is the central registry for Binder services. Every service
that wants to be discoverable by other processes registers itself with the
service manager under a well-known name, and every client that needs a service
looks it up by that name. In rsbinder, the service manager is referred to as
**HUB** and is accessed through the `rsbinder::hub` module.

On Linux, the HUB is provided by the `rsb_hub` binary that ships with the
`rsbinder-tools` crate. On Android, the system's native `servicemanager`
fulfills this role, and rsbinder talks to it using the same Binder protocol.

> **Note**: This chapter covers the **kernel-binder** service manager.
> The [RPC transport](./rpc-transport.md) (binder-over-socket) uses no
> service manager at all — `RpcServer::set_root` publishes the root
> binder directly and `RpcSession::get_root` fetches it. The Android
> 16 **Accessor** pattern bridges the two: a kernel-binder service of
> type `IAccessor` hands a client an RPC socket fd. See
> [RPC Transport](./rpc-transport.md#bridging-rpc-and-the-service-manager-the-accessor-pattern).

## Running the Service Manager (Linux)

Before any service can register or any client can perform lookups, the HUB
process must be running:

```bash
# Build and run the service manager, with no access control (development)
$ cargo run --bin rsb_hub -- --insecure-allow-all
```

`rsb_hub` opens the Binder device, becomes the context manager (handle 0),
and enters a loop that processes registration and lookup requests. It must
remain running for the lifetime of the system's Binder services.

**It also refuses to start without an access-control policy.** Every request
is denied unless a policy allows it, and `--insecure-allow-all` above is the
explicit opt-out for development. See [Access control](#access-control) for
how to write one.

## Registering a Service

To make a service available to other processes, create a Binder object and
register it with `hub::add_service`:

```rust
use rsbinder::*;

// Create the Binder service object
let service = BnMyService::new_binder(MyServiceImpl);

// Register it under a descriptive name. `add_service` accepts anything
// convertible into `SIBinder`, so the typed `Strong<dyn IMyService>` can
// be passed directly — no `.as_binder()` needed.
hub::add_service("com.example.myservice", &service)?;
```

The name passed to `add_service` is the identifier that clients will use to
find the service.

### Registration Rules

The service manager enforces several constraints on service names. These
checks are applied **by the service manager process** (`rsb_hub` on Linux,
`servicemanager` on Android) — the `rsbinder` library itself does not validate
the name on the client side, so the failure surfaces as a `Status` error from
the IPC call rather than a local compile-time or pre-flight check.

- **Maximum length**: 127 characters. Names of 128 characters or longer are
  rejected.
- **Allowed characters**: Alphanumeric characters, dots (`.`), underscores
  (`_`), hyphens (`-`), and forward slashes (`/`). Special characters such as
  `$` are not allowed.
- **Non-empty**: An empty string is rejected.
- **Overwrite rules**: Re-registering the *same* binder under a name, or
  replacing a registration from the *same UID* (e.g. a restarted service
  process), is always allowed. However, `rsb_hub` **rejects** an attempt by a
  different UID to overwrite an existing name with a different binder — a
  likely service-name hijack — unless it was started with the opt-in
  `--allow-cross-uid-overwrite` flag.

```rust
let service = BnFoo::new_binder(FooImpl {});

// Empty names are rejected
assert!(hub::add_service("", &service).is_err());

// Valid name
assert!(hub::add_service("foo", &service).is_ok());

// Maximum length (127 characters)
let long_name = "a".repeat(127);
assert!(hub::add_service(&long_name, &service).is_ok());

// Too long (128 characters)
let too_long = "a".repeat(128);
assert!(hub::add_service(&too_long, &service).is_err());

// Special characters are rejected
assert!(hub::add_service("happy$foo$fo", &service).is_err());
```

## Looking Up Services

rsbinder provides a family of lookup functions. They differ only in *how they
wait* and *how they encode "not registered"* — pick by what your client needs:

| Function | Waits? | Returns | Not registered |
|---|---|---|---|
| `wait_for_interface::<T>` | blocks until registered | `Result<Strong<T>>` | *blocks* |
| `wait_for_service` | blocks until registered | `Option<SIBinder>` | *blocks* |
| `check_interface::<T>` | no | `Result<Strong<T>>` | `Err(NameNotFound)` |
| `check_service` | no | `Option<SIBinder>` | `None` |
| `try_get_interface::<T>` | no | `Result<Option<Strong<T>>>` | `Ok(None)` |
| `try_get_service` | no | `Result<Option<SIBinder>>` | `Ok(None)` |

> **Deprecated**: `hub::get_service` and `hub::get_interface` are
> `#[deprecated]` as of 0.10.0 because their wait behavior was inconsistent
> across Android versions. Use `wait_for_service` / `wait_for_interface` to
> block until the service appears, or `check_service` / `check_interface`
> for a non-blocking lookup.

### Blocking, Type-Safe Lookup with `wait_for_interface`

The most common client pattern is `hub::wait_for_interface`, which blocks
until the service is registered and casts it to the expected AIDL interface
type in one step — the equivalent of AOSP's `waitForService`:

```rust
let service: rsbinder::Strong<dyn IMyService::IMyService> =
    hub::wait_for_interface("com.example.myservice")?;

// Now call methods directly on the typed proxy
let result = service.some_method()?;
```

The wait is unbounded: it returns when the service appears, and fails only
if the service manager itself is unreachable. The wait is event-driven
(registration-callback based) when the process runs a binder thread pool
(`ProcessState::start_thread_pool()`); without one it degrades gracefully to
~1-second polling.

`hub::wait_for_service` is the untyped variant — it returns
`Option<SIBinder>` when you need the raw handle (for example, to inspect the
descriptor or pass it to another API):

```rust
if let Some(binder) = hub::wait_for_service("com.example.myservice") {
    println!("Found service with descriptor: {}", binder.descriptor());
}
```

### Non-Blocking Check with `check_service` / `check_interface`

`hub::check_service` returns immediately, with `None` if the service is not
(yet) registered:

```rust
let binder: Option<SIBinder> = hub::check_service("com.example.myservice");
if binder.is_some() {
    println!("Service is available");
} else {
    println!("Service is not yet registered");
}
```

The typed counterpart `hub::check_interface` casts in one step and returns
`Err(StatusCode::NameNotFound)` immediately when the service is absent.

### Error-Preserving Lookup with `try_get_service` / `try_get_interface`

`check_service` folds "not registered" and "service manager unreachable"
into the same `None`. When you must tell those apart, use
`hub::try_get_service` (or the typed `hub::try_get_interface`), which
returns `Result<Option<_>>`: `Ok(None)` means the service is not registered,
while `Err(..)` means the lookup itself failed (transport or service-manager
error):

```rust
match hub::try_get_service("com.example.myservice") {
    Ok(Some(binder)) => println!("found: {}", binder.descriptor()),
    Ok(None) => println!("service not registered"),
    Err(err) => println!("service manager unreachable: {err:?}"),
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

> **Thread pool required.** `onRegistration` arrives as an *inbound* binder
> transaction, so the client process must call
> `ProcessState::start_thread_pool()` (or park a thread in
> `join_thread_pool()`) — otherwise the callback never fires. This applies
> to any client that receives inbound calls, not just services.

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

On Linux with `rsb_hub`, this **always** returns `false` because `rsb_hub`
has no VINTF parser and no manifest concept ([rsb_hub.rs](https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/src/bin/rsb_hub.rs)
implements `isDeclared` as `Ok(false)`). On Android, it reflects the device's
hardware interface declarations.

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

This feature is available on Android 12 and above. On Android 10 and Android 11,
calling `get_service_debug_info` returns an error.

## Access control

`rsb_hub` gates every entry point on a policy. There is no permissive
default: a name with no matching rule is denied, a rule that does not mention
a permission denies it, and a policy that fails to load stops the process
from starting.

### Why it does not use SELinux

Android's `servicemanager` gates the same three permissions through SELinux.
SELinux is not present on every Linux distribution and does not exist on
macOS, so it cannot be the basis of the model here — and it does not need to
be. Android's policy *model* is SELinux-independent:

```text
(caller identity, service name) -> {add, find, list}
```

SELinux is only one way of expressing "caller identity". `rsb_hub` keeps the
model and swaps that identity for **uid**, which every platform reports and
the binder driver fills in itself (`sender_euid`) at transaction time —
unforgeable and not racy. This is the same conclusion D-Bus reached for the
same problem: uid/group policy files as the portable base, with an LSM as an
optional overlay rather than a prerequisite.

Note what is *not* used: **pid**. Pid reuse races make every pid-derived
attribute (`/proc/<pid>/exe`, cgroup, systemd unit) unsound as an
authorization key — which is why AOSP calls its own field `debugPid`. Groups
are usable because resolving uid → groups through NSS is race-free.

### Permissions

| Permission | Gates |
|---|---|
| `add` | `add_service`, `register_client_callback`, `try_unregister_service` |
| `find` | `check_service`, `wait_for_service`, `get_service`, `register_for_notifications`, `is_declared`, `get_connection_info`, and the per-name filtering of the two `list` calls |
| `list` | `list_services`, `get_service_debug_info` — a single global gate, since there is no name to scope it to |

A denied **lookup** is reported as "not registered" rather than as an error.
That matches AOSP (`getService` returns ok regardless of result) and keeps a
denied caller from using the error to probe which names exist. Every other
denial returns `ExceptionCode::Security`.

The two `list` calls apply *both* gates: the caller must be allowed to
enumerate at all, and each name it would learn about must be one it may
`find`. A name you cannot look up is a name you do not get told about.

### Policy files

`rsb_hub --config <PATH>` takes a `.toml` file or a directory of them
(default: `/etc/rsbinder/hub.d`). A directory is read as every `*.toml` in
it, sorted by file name — so `10-`/`20-` prefixes control precedence the way
they do in any other `.d` directory.

```toml
# /etc/rsbinder/hub.d/10-example.toml

[global]
# Who may enumerate service names at all. Omitted means nobody.
list = { group = ["binder-admin"] }

[[rule]]
name = "com.example.*"                       # trailing `*` only
add  = { user = ["exampled"] }
find = { group = ["example-clients"], uid = [0] }

[[rule]]
name = "*"
add  = "none"
find = "none"
```

A subject is the union of `uid` (numeric), `user` (resolved through NSS at
load time), and `group` (matched against the caller's primary *and*
supplementary groups). The shorthands `"any"` and `"none"` are also accepted;
`"any"` must be written out, since an omitted key always means deny.

**The first rule whose `name` matches decides**, and evaluation stops there —
a later rule never widens what an earlier one refused. That makes the rule
governing any given name unique and greppable, which is worth more in a
security file than the extra expressiveness of last-match-wins. Put
catch-alls last.

Patterns support a trailing `*` and nothing else. A leading or interior `*`
is rejected at load time rather than silently matching nothing.

### Reloading

`SIGHUP` reloads the policy in place:

```bash
$ sudo kill -HUP "$(pidof rsb_hub)"
```

A reload that fails to parse or resolve **keeps the policy already in
force** and logs the error. Dropping to deny-all would take the machine's IPC
down over a typo; falling back to permissive would be worse. The reload also
drops the memoized uid → groups sets, so group-membership changes take effect
with it.

### Layers underneath

The policy is the second of two gates. The first is the binder device node
itself: binder has no in-kernel access control of its own, so
`/dev/binderfs/<name>` decides who can speak binder *at all*. `rsb_device`
creates it `0600` (root only) and takes `--group` / `--mode` to widen that
deliberately — the same model as `/dev/kvm` being `0660 root:kvm`:

```bash
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"
$ sudo rsb_device binder --group binder --mode 0660
```

An LSM (SELinux, AppArmor) can sit on top as a third, AND-ed layer where the
platform provides one, but it is never what makes the first two unnecessary.

## Linux vs. Android Differences

While rsbinder aims for API compatibility across both platforms, there are
important behavioral differences between the Linux HUB (`rsb_hub`) and
Android's native `servicemanager`:

| Aspect                  | Linux (`rsb_hub`)                       | Android (`servicemanager`)              |
|-------------------------|-----------------------------------------|-----------------------------------------|
| **Process**             | User-space `rsb_hub` binary             | System `servicemanager` daemon          |
| **Access control**      | uid/group policy files ([above](#access-control)) | SELinux MAC policy                      |
| **VINTF manifests**     | Not supported (`is_declared` is false)  | Supported and enforced                  |
| **Service debug info**  | Supported                               | Supported (Android 12+; not on 10/11)   |
| **Binder device**       | Must be created with `rsb_device`       | Managed by Android init                 |
| **Version selection**   | Always uses Android 16 protocol         | Auto-detected from SDK version          |
| **Death notifications** | Supported                               | Supported                               |

On Android, rsbinder automatically detects the SDK version and uses the
appropriate service manager protocol (Android 10 through 16). The per-version
API availability matrix:

| API                                | Android 10 | Android 11 | Android 12+ |
|------------------------------------|:---------:|:---------:|:-----------:|
| `check_service` / `wait_for_service` |     ✓     |     ✓     |      ✓      |
| `add_service`                      |     ✓     |     ✓     |      ✓      |
| `list_services`                    |     ✓     |     ✓     |      ✓      |
| `is_declared`                      |   false   |     ✓     |      ✓      |
| `register_for_notifications`       |     ✗     |     ✓     |      ✓      |
| `unregister_for_notifications`     |     ✗     |     ✓     |      ✓      |
| `get_service_debug_info`           |     ✗     |     ✗     |      ✓      |

Where ✗ means the call returns `StatusCode::UnknownTransaction` (or `false`
for the `bool`-returning `is_declared`) because the underlying server protocol
predates the API. Android 10 falls back to the legacy C `IServiceManager`,
which only learned the AIDL-based interface in Android 11; `get_service_debug_info`
was added in Android 12. On Linux, rsbinder always uses the Android 16
protocol — what `rsb_hub` implements — so every row in the Android 12+
column applies, with the caveat that `is_declared` is *always* `false` on
Linux (no VINTF manifest).

## Using the ServiceManager Object Directly

The convenience functions (`hub::add_service`, `hub::check_service`, etc.) use
a global singleton `ServiceManager` under the hood. If you need more control,
you can obtain the `ServiceManager` instance directly:

```rust
use rsbinder::hub;

// `hub::default()` returns `Result<Arc<ServiceManager>, StatusCode>` because
// initialization can fail (e.g., binder device unavailable, unsupported SDK).
// Propagate the error with `?` or handle it with `match`.
let sm = hub::default()?;

// Use methods on the ServiceManager instance
let service = sm.check_service("com.example.myservice");
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

- **Wait, don't poll.** If your client starts before the service it depends
  on, call `wait_for_interface` / `wait_for_service` rather than repeatedly
  calling `check_service` in a loop — the wait is event-driven when a binder
  thread pool is running. Use `register_for_notifications` when you need to
  observe every (re-)registration over time, and remember that the
  notification callback only fires if the process runs a thread pool.

- **Avoid the deprecated `get_service` / `get_interface`.** Their wait
  behavior differed across Android versions; the `wait_*`, `check_*`, and
  `try_get_*` families make the blocking and error semantics explicit.

- **Handle registration failures.** `add_service` can fail if the name is
  invalid or if the caller lacks permission (on Android with SELinux). Always
  check the result.

- **Prefer the typed `*_interface` variants.** `wait_for_interface`,
  `check_interface`, and `try_get_interface` return a strongly-typed proxy
  that provides compile-time guarantees; the `*_service` variants return the
  raw `SIBinder`.

- **Debug with `list_services` and `get_service_debug_info`.** When
  troubleshooting, list all registered services and inspect their debug
  information to verify that services are registered from the expected
  processes.
