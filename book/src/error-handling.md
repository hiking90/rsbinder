# Error Handling

Binder IPC introduces failure modes that do not exist in ordinary function calls:
the remote process may crash, the kernel driver may reject a transaction, or the
service may intentionally signal an application-level error. rsbinder represents
all of these cases through two complementary types -- `StatusCode` for
transport-level errors and `Status` for richer application-level errors that
include exception codes and optional messages.

## Core Types

### `rsbinder::status::Result<T>`

Every AIDL-generated method returns this type:

```rust
pub type Result<T> = std::result::Result<T, Status>;
```

This is a standard `Result` whose error variant is a `Status`.

### `StatusCode`

`StatusCode` represents low-level transport errors that occur before or during
a Binder transaction. These are defined in `rsbinder::StatusCode` (re-exported
from `rsbinder::error::StatusCode`).

Commonly encountered values:

| Variant              | Meaning                                      |
|----------------------|----------------------------------------------|
| `Ok`                 | Operation completed successfully              |
| `Unknown`            | An unspecified error occurred                 |
| `BadValue`           | Invalid parameter value                       |
| `UnknownTransaction` | The transaction code is not recognized        |
| `PermissionDenied`   | Caller does not have permission               |
| `DeadObject`         | The remote process has died                   |
| `FailedTransaction`  | The transaction could not be completed        |
| `NoMemory`           | Out of memory                                 |
| `BadType`            | Wrong data type encountered                   |
| `NotEnoughData`      | The parcel did not contain enough data        |

A `StatusCode` can be converted directly into a `Status`:

```rust
let status: rsbinder::Status = rsbinder::StatusCode::PermissionDenied.into();
```

### `Status`

`Status` combines three pieces of information:

- **Exception code** (`ExceptionCode`) -- categorizes the error (e.g.
  `ServiceSpecific`, `Security`, `NullPointer`).
- **Status code** (`StatusCode`) -- provides transport-level detail.
- **Message** (`Option<String>`) -- an optional human-readable description.

Key methods on `Status`:

```rust
// Check the category of the error
status.exception_code()      // -> ExceptionCode

// Get the transport error (only meaningful when exception is TransactionFailed)
status.transaction_error()   // -> StatusCode

// Get the service-specific error code (only meaningful when exception is ServiceSpecific)
status.service_specific_error() // -> i32

// Check if the status represents success
status.is_ok()               // -> bool
```

### `ExceptionCode`

`ExceptionCode` classifies the kind of error:

| Variant                | Meaning                                    |
|------------------------|--------------------------------------------|
| `None`                 | No error                                   |
| `Security`             | Security / permission violation            |
| `BadParcelable`        | Malformed parcelable data                  |
| `IllegalArgument`      | Invalid argument provided                  |
| `NullPointer`          | Unexpected null value                      |
| `IllegalState`         | Operation invalid for current state        |
| `UnsupportedOperation` | Requested operation is not supported       |
| `ServiceSpecific`      | Application-defined error with custom code |
| `TransactionFailed`    | Low-level transaction failure              |

## Returning Errors from a Service

### Service-Specific Errors

The most common way for a service to report an application-level error is
through `Status::new_service_specific_error`. This sets the exception code to
`ServiceSpecific` and carries an integer error code whose meaning is defined
by the service:

```rust
fn ThrowServiceException(
    &self,
    code: i32,
) -> rsbinder::status::Result<()> {
    Err(rsbinder::Status::new_service_specific_error(code, None))
}
```

You can also attach an optional message:

```rust
Err(rsbinder::Status::new_service_specific_error(
    -1,
    Some("resource not found".into()),
))
```

### Unimplemented Methods

When a service does not support a particular transaction (for example, a method
added in a newer version of the AIDL interface), return `UnknownTransaction`:

```rust
fn UnimplementedMethod(
    &self,
    _arg: i32,
) -> rsbinder::status::Result<i32> {
    // Indicate that this method is not implemented
    Err(rsbinder::StatusCode::UnknownTransaction.into())
}
```

The `.into()` conversion automatically creates a `Status` with the
`TransactionFailed` exception code.

### Permission Errors

If your service enforces access control, return `PermissionDenied`:

```rust
fn restricted_operation(&self) -> rsbinder::status::Result<()> {
    let caller = rsbinder::thread_state::CallingContext::default();
    if caller.uid != ALLOWED_UID {
        return Err(rsbinder::StatusCode::PermissionDenied.into());
    }
    Ok(())
}
```

## Handling Errors on the Client Side

### Checking for Service-Specific Errors

When calling a service method, always check the `Result` for errors.
For service-specific errors, inspect the `exception_code()` first, then
retrieve the integer code:

```rust
let result = service.ThrowServiceException(-1);
assert!(result.is_err());

let status = result.unwrap_err();
assert_eq!(
    status.exception_code(),
    rsbinder::ExceptionCode::ServiceSpecific,
);
assert_eq!(status.service_specific_error(), -1);
```

### Distinguishing Error Categories

A practical error-handling pattern checks the exception code to determine
how to react:

```rust
match service.some_method() {
    Ok(value) => {
        // Success
    }
    Err(status) => {
        match status.exception_code() {
            rsbinder::ExceptionCode::ServiceSpecific => {
                let code = status.service_specific_error();
                eprintln!("Service error {code}: {status}");
            }
            rsbinder::ExceptionCode::TransactionFailed => {
                let transport_err = status.transaction_error();
                eprintln!("Transport error: {transport_err:?}");
            }
            rsbinder::ExceptionCode::Security => {
                eprintln!("Permission denied: {status}");
            }
            other => {
                eprintln!("Unexpected error ({other}): {status}");
            }
        }
    }
}
```

### Detecting a Dead Service

If the remote process crashes, methods will fail with `DeadObject`:

```rust
let result = service.some_method();
if let Err(ref status) = result {
    if status.transaction_error() == rsbinder::StatusCode::DeadObject {
        eprintln!("Service has died, attempting reconnection...");
    }
}
```

## Converting Between Error Types

rsbinder provides `From` implementations that make conversions between
`StatusCode`, `ExceptionCode`, and `Status` straightforward:

```rust
// StatusCode -> Status
let status: rsbinder::Status = rsbinder::StatusCode::BadValue.into();

// ExceptionCode -> Status
let status: rsbinder::Status = rsbinder::ExceptionCode::IllegalArgument.into();

// Status -> StatusCode (extracts the transport error code)
let code: rsbinder::StatusCode = status.into();
```

These conversions are used most often in the `Err(...)` return position of
service methods, where `.into()` converts a `StatusCode` into the expected
`Status` type automatically.

## Tips and Best Practices

- **Prefer service-specific errors for application logic.** Use
  `Status::new_service_specific_error` when the error is meaningful to the
  caller (e.g., "item not found", "quota exceeded"). Define your error codes
  as constants so both the service and client can reference them.

- **Use `StatusCode` for infrastructure problems.** Return raw `StatusCode`
  values like `PermissionDenied` or `BadValue` for errors that relate to the
  IPC mechanism rather than your application's business logic.

- **Always check `exception_code()` first.** The meaning of
  `service_specific_error()` and `transaction_error()` depends on the
  exception code. Calling them without checking the exception may return
  default (zero) values.

- **Handle `DeadObject` gracefully.** In long-running clients, the remote
  service may restart. Consider using death notifications
  (`link_to_death`) to detect service restarts and re-establish connections.

- **`Status` implements `Display` and `std::error::Error`.** You can use it
  with `?` in functions that return `Box<dyn std::error::Error>` or with
  logging macros for human-readable diagnostics.
