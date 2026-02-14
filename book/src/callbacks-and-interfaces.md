# Callbacks and Interfaces

Binder IPC is not limited to one-way requests from client to service. Through callback interfaces, a client can pass a Binder object to a service, and the service can call methods on that object. This enables bidirectional communication across process boundaries without requiring the client to register itself as a separate service.

This chapter covers how to define callback interfaces in AIDL, implement them in Rust, manage collections of callbacks, pass raw `IBinder` objects, work with nested interface types, and monitor remote service lifecycle with death recipients.

## Defining a Callback Interface

A callback interface is a regular AIDL interface. The only difference is in how it is used: instead of being registered with the service manager, it is created by one process and passed to another through a method call.

Here is a minimal callback interface from the rsbinder test suite (`INamedCallback.aidl`):

```aidl
package android.aidl.tests;

interface INamedCallback {
    String GetName();
}
```

This interface defines a single method that returns a string. A service can accept objects implementing this interface, store them, and call `GetName` later -- regardless of whether the callback lives in the same process or a different one.

## Implementing a Callback

Implementing a callback follows the same pattern as implementing any Binder service. Define a struct, implement the `rsbinder::Interface` trait, and implement the generated AIDL trait:

```rust
struct NamedCallback(String);

impl rsbinder::Interface for NamedCallback {}

impl INamedCallback::INamedCallback for NamedCallback {
    fn GetName(&self) -> std::result::Result<String, Status> {
        Ok(self.0.clone())
    }
}
```

This implementation is identical in structure to a top-level service -- the only difference is that this object will be passed to another service rather than registered in the service manager.

## Service-Side Callback Management

A service that works with callbacks typically needs to create, store, and invoke them. The following pattern uses a `HashMap` to cache callbacks by name:

```rust
#[derive(Default)]
struct TestService {
    service_map: Mutex<HashMap<String, rsbinder::Strong<dyn INamedCallback::INamedCallback>>>,
}

impl Interface for TestService {}

impl ITestService::ITestService for TestService {
    fn GetOtherTestService(
        &self,
        name: &str,
    ) -> std::result::Result<rsbinder::Strong<dyn INamedCallback::INamedCallback>, rsbinder::Status>
    {
        let mut service_map = self.service_map.lock().unwrap();
        let other_service = service_map.entry(name.into()).or_insert_with(|| {
            let named_callback = NamedCallback(name.into());
            INamedCallback::BnNamedCallback::new_binder(named_callback)
        });
        Ok(other_service.to_owned())
    }
    // ...
}
```

Key points:

- **`BnNamedCallback::new_binder()`** wraps the struct in a Binder node so it can cross process boundaries. The `Bn` prefix stands for "Binder native" (server-side stub).
- **`Strong<dyn INamedCallback::INamedCallback>`** is a strong reference to a Binder object, equivalent to Android's `sp<INamedCallback>`.
- **`Mutex<HashMap<...>>`** protects the map because Binder calls can arrive on different threads.

### Accepting and Invoking Callbacks

A service can also accept callbacks from the client and invoke methods on them. The `VerifyName` method below receives a callback and calls its `GetName` method:

```rust
fn VerifyName(
    &self,
    service: &rsbinder::Strong<dyn INamedCallback::INamedCallback>,
    name: &str,
) -> std::result::Result<bool, rsbinder::Status> {
    service.GetName().map(|found_name| found_name == name)
}
```

When the client and service are in different processes, calling `service.GetName()` triggers a Binder transaction back to the client process. This is completely transparent to the service code -- the proxy handles all the marshalling.

## Client-Side Usage

From the client side, working with callbacks is straightforward. You request a callback from the service, call methods on it, and pass it back to the service for verification:

```rust
let service = get_test_service();

// Request a callback from the service
let got = service
    .GetOtherTestService("Smythe")
    .expect("error calling GetOtherTestService");

// Call a method on the callback
assert_eq!(got.GetName().as_ref().map(String::as_ref), Ok("Smythe"));

// Pass the callback back to the service for verification
assert_eq!(service.VerifyName(&got, "Smythe"), Ok(true));
```

Even though the `NamedCallback` object was created inside the service process, the client can call `GetName()` on it through Binder IPC. The generated proxy (`BpNamedCallback`) handles serialization and deserialization automatically.

## Callback Arrays

Services can return and accept arrays of callback interfaces. The `GetInterfaceArray` method creates a callback for each name in the input and returns them as a `Vec`. On the client side:

```rust
let names = vec!["Fizz".into(), "Buzz".into()];
let service = get_test_service();

let got = service
    .GetInterfaceArray(&names)
    .expect("error calling GetInterfaceArray");

// Each callback has the correct name
assert_eq!(
    got.iter()
        .map(|s| s.GetName())
        .collect::<std::result::Result<Vec<_>, _>>(),
    Ok(names.clone())
);

// Verify all names in a single call
assert_eq!(
    service.VerifyNamesWithInterfaceArray(&got, &names),
    Ok(true)
);
```

### Nullable Arrays

Callback arrays can also be nullable, where both the array itself and individual elements may be absent:

```rust
let names = vec![Some("Fizz".into()), None, Some("Buzz".into())];
let got = service
    .GetNullableInterfaceArray(Some(&names))
    .expect("error calling GetNullableInterfaceArray");
```

In this case, the service returns `Option<Vec<Option<Strong<dyn INamedCallback::INamedCallback>>>>` -- an optional array where each element is itself optional. The `None` entries in the input produce `None` entries in the output.

## Passing Raw IBinder Objects

You can also pass raw `IBinder` objects through Binder transactions without committing to a specific interface type. The AIDL definitions use the `IBinder` type directly:

```aidl
void TakesAnIBinder(in IBinder input);
void TakesANullableIBinder(in @nullable IBinder input);
void TakesAnIBinderList(in List<IBinder> input);
void TakesANullableIBinderList(in @nullable List<IBinder> input);
```

In Rust, `IBinder` maps to `SIBinder` (a strong Binder reference). You can obtain an `SIBinder` from any typed interface using the `as_binder()` method:

```rust
let service = get_test_service();

// Pass the service's own binder reference
let result = service.TakesAnIBinder(&service.as_binder());
assert!(result.is_ok());

// Pass a list of binder references
let result = service.TakesAnIBinderList(&[service.as_binder()]);
assert!(result.is_ok());

// Nullable binder -- pass None
let result = service.TakesANullableIBinder(None);
assert!(result.is_ok());

// Nullable list with mixed Some/None entries
let result = service.TakesANullableIBinderList(
    Some(&[Some(service.as_binder()), None])
);
assert!(result.is_ok());
```

## Nested Interfaces

AIDL allows you to define interfaces, parcelables, and enums nested inside another interface. This is useful when a callback type is logically scoped to a single service. Here is the `INestedService` definition from the test suite:

```aidl
interface INestedService {
    @RustDerive(PartialEq=true)
    parcelable Result {
        ParcelableWithNested.Status status = ParcelableWithNested.Status.OK;
    }

    Result flipStatus(in ParcelableWithNested p);

    interface ICallback {
        void done(ParcelableWithNested.Status status);
    }
    void flipStatusWithCallback(ParcelableWithNested.Status status, ICallback cb);
}
```

### Implementing a Nested Callback

In the generated Rust code, nested types are accessed through the parent module's namespace:

```rust
#[derive(Debug, Default)]
struct Callback {
    received: Arc<Mutex<Option<ParcelableWithNested::Status::Status>>>,
}

impl Interface for Callback {}

impl INestedService::ICallback::ICallback for Callback {
    fn done(
        &self,
        st: ParcelableWithNested::Status::Status,
    ) -> std::result::Result<(), Status> {
        *self.received.lock().unwrap() = Some(st);
        Ok(())
    }
}
```

### Using a Nested Callback

To create and pass a nested callback to the service:

```rust
let service: rsbinder::Strong<dyn INestedService::INestedService> = hub::get_interface(
    <INestedService::BpNestedService as INestedService::INestedService>::descriptor(),
)
.expect("did not get binder service");

let received = Arc::new(Mutex::new(None));

// Create the callback binder
let cb = INestedService::ICallback::BnCallback::new_binder(Callback {
    received: Arc::clone(&received),
});

// Pass NOT_OK to the service; it should flip it to OK via the callback
let ret = service.flipStatusWithCallback(
    ParcelableWithNested::Status::Status::NOT_OK,
    &cb,
);
assert_eq!(ret, Ok(()));

// Verify the callback was invoked with the flipped status
let received = received.lock().unwrap();
assert_eq!(*received, Some(ParcelableWithNested::Status::Status::OK));
```

The key detail is the fully-qualified path for the nested callback's Binder node: `INestedService::ICallback::BnCallback`. This follows the Rust module hierarchy generated from the AIDL nesting structure.

### Service-Side Nested Callback Handling

On the service side, the nested callback is received as a typed strong reference and can be invoked directly:

```rust
impl INestedService::INestedService for NestedService {
    fn flipStatusWithCallback(
        &self,
        st: ParcelableWithNested::Status::Status,
        cb: &rsbinder::Strong<dyn INestedService::ICallback::ICallback>,
    ) -> std::result::Result<(), Status> {
        if st == ParcelableWithNested::Status::Status::OK {
            cb.done(ParcelableWithNested::Status::Status::NOT_OK)
        } else {
            cb.done(ParcelableWithNested::Status::Status::OK)
        }
    }
}
```

The service flips the status and calls `done` on the callback. If the callback lives in a different process, this triggers a Binder transaction back to the caller.

## Death Recipients

When a client holds a reference to a remote Binder object, it may need to know if the remote process dies. In rsbinder, you implement the `DeathRecipient` trait and register it with a Binder reference.

### Implementing a Death Recipient

```rust
use rsbinder::*;
use std::sync::{Arc, Mutex};
use std::fs::File;
use std::io::Write;

struct MyDeathRecipient {
    write_file: Mutex<File>,
}

impl DeathRecipient for MyDeathRecipient {
    fn binder_died(&self, _who: &WIBinder) {
        let mut writer = self.write_file.lock().unwrap();
        writer.write_all(b"binder_died\n").unwrap();
    }
}
```

The `binder_died` method is called when the remote process hosting the Binder object terminates. The `_who` parameter is a `WIBinder` (weak Binder reference) identifying which Binder object died.

### Registering and Unregistering

Death recipients are registered using `link_to_death` and unregistered using `unlink_to_death`. Both methods take a `Weak<dyn DeathRecipient>` reference:

```rust
let recipient = Arc::new(MyDeathRecipient {
    write_file: Mutex::new(write_file),
});

// Register for death notification
service
    .as_binder()
    .link_to_death(Arc::downgrade(
        &(recipient.clone() as Arc<dyn DeathRecipient>),
    ))
    .unwrap();

// Unregister when no longer needed
service
    .as_binder()
    .unlink_to_death(Arc::downgrade(
        &(recipient.clone() as Arc<dyn DeathRecipient>),
    ))
    .unwrap();
```

The cast `recipient.clone() as Arc<dyn DeathRecipient>` is necessary to convert from the concrete type to the trait object before calling `Arc::downgrade`. The weak reference ensures that the death recipient does not keep the Binder object alive -- if all strong references are dropped, the Binder object can be cleaned up normally.

Note that death notifications only work for **remote** Binder objects. Calling `link_to_death` on a local Binder object (one in the same process) will return an error because there is no remote process to monitor.

## Tips

Here are key points to keep in mind when working with callbacks and interfaces in rsbinder:

- **Callbacks are full Binder objects.** They cross process boundaries transparently. A callback created in the client process can be invoked by the service process through a standard Binder transaction.

- **Use `BnXxx::new_binder()` to create callback objects.** The `Bn` (Binder native) wrapper converts your Rust struct into a Binder node that can be sent through Binder transactions. The corresponding `Bp` (Binder proxy) is used automatically on the receiving side.

- **Use `Mutex` to protect shared state.** Binder method calls can arrive on any thread in the thread pool. Any mutable state in your callback or service struct must be protected by `Mutex`, `RwLock`, or another synchronization primitive.

- **Nested types use fully-qualified Rust paths.** A callback `ICallback` nested inside `INestedService` is accessed as `INestedService::ICallback::ICallback` for the trait and `INestedService::ICallback::BnCallback` for the Binder node constructor.

- **Death recipients use `Weak` references.** The `link_to_death` API takes `Weak<dyn DeathRecipient>` to avoid preventing cleanup of the death recipient itself. Keep a strong `Arc` reference alive for as long as you want to receive notifications.

- **`as_binder()` converts typed interfaces to raw `SIBinder`.** This is useful when you need to pass a Binder reference to a method that accepts `IBinder`, or when you need to call Binder-level methods like `link_to_death` or `ping_binder`.

- **Callback equality works through Binder identity.** Two `Strong<dyn T>` references are equal if they point to the same Binder object. This allows you to compare callbacks received from different sources to determine if they refer to the same underlying implementation.
