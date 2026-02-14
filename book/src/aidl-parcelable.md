# Parcelable

Parcelable types are user-defined data structures that can be serialized and sent across Binder IPC boundaries. They are defined in AIDL `.aidl` files, and the `rsbinder-aidl` code generator automatically produces Rust structs from them. Parcelable types are the primary way to pass structured data between a Binder service and its clients.

Unlike primitive types (such as `int`, `String`, or `boolean`), which AIDL handles natively, parcelable types let you group related fields into a single, coherent structure. This is essential for any non-trivial service interface.

## Basic Parcelable Definition

A parcelable is declared in its own `.aidl` file using the `parcelable` keyword. Here is a simple example:

```aidl
package com.example;

@RustDerive(Clone=true, PartialEq=true)
parcelable UserProfile {
    int id;
    String name = "Unknown";
    int age = 0;
}
```

When this AIDL file is processed by `rsbinder-aidl`, it generates a Rust struct that you can use directly in your service and client code. Several things to note about the definition above:

- **`@RustDerive(Clone=true, PartialEq=true)`** instructs the code generator to add `#[derive(Clone, PartialEq)]` to the generated Rust struct. By default, generated types do not derive `Clone`, because some AIDL types contain non-cloneable fields (such as `ParcelFileDescriptor` or `ParcelableHolder`). You must opt in explicitly for each type.
- **Default values** (`"Unknown"` for `name`, `0` for `age`) are applied in the generated `Default` trait implementation. Fields without explicit defaults use Rust's default for their type (e.g., `0` for integers, empty string for `String`).
- The generated struct can be used directly as a parameter or return type in service interface methods.

You can then use the generated struct in Rust:

```rust
use com::example::UserProfile::UserProfile;

let profile = UserProfile {
    id: 1,
    name: "Alice".into(),
    age: 30,
};

let default_profile = UserProfile::default();
assert_eq!(default_profile.name, "Unknown");
assert_eq!(default_profile.age, 0);
```

## Constants in Parcelable

Parcelable types can define constants, including numeric values and bit flags. These become associated constants on the generated Rust struct. This pattern is commonly used for configuration values and flag fields.

```aidl
@RustDerive(Clone=true, PartialEq=true)
parcelable Config {
    const int MAX_RETRIES = 5;
    const int BIT_VERBOSE = 0x1;
    const int BIT_DEBUG = 0x4;

    int retryCount = MAX_RETRIES;
    int flags = 0;
    String label = "default";
}
```

In Rust, the constants are accessed as associated constants on the struct:

```rust
use config::Config;

let mut cfg = Config::default();
assert_eq!(cfg.retryCount, 5);

cfg.flags = Config::BIT_VERBOSE | Config::BIT_DEBUG;
assert_eq!(cfg.flags, 0x5);
```

This pattern mirrors how the Android test suite defines and uses bit flags within `StructuredParcelable`, where constants like `BIT0`, `BIT1`, and `BIT2` are defined alongside the fields that use them.

## Using Parcelable in Services

Parcelable types are passed to and from service methods as regular parameters. A common pattern is to pass a mutable reference to a parcelable so that the service can fill in or modify its fields. This is based on the `FillOutStructuredParcelable` pattern used in the rsbinder test suite.

Service implementation:

```rust
fn FillOutStructuredParcelable(
    &self,
    parcelable: &mut StructuredParcelable,
) -> rsbinder::status::Result<()> {
    parcelable.shouldBeJerry = "Jerry".into();
    parcelable.shouldContainThreeFs = vec![parcelable.f, parcelable.f, parcelable.f];
    parcelable.shouldSetBit0AndBit2 =
        StructuredParcelable::BIT0 | StructuredParcelable::BIT2;
    Ok(())
}
```

Client side:

```rust
let mut parcelable = StructuredParcelable {
    f: 17,
    shouldSetBit0AndBit2: 0,
    ..Default::default()
};

service.FillOutStructuredParcelable(&mut parcelable)?;

assert_eq!(parcelable.shouldBeJerry, "Jerry");
assert_eq!(parcelable.shouldContainThreeFs, vec![17, 17, 17]);
assert_eq!(
    parcelable.shouldSetBit0AndBit2,
    StructuredParcelable::BIT0 | StructuredParcelable::BIT2
);
```

The service receives the parcelable by mutable reference, reads existing field values, and populates the remaining fields before returning. The client can then inspect the modified parcelable.

## Nullable Parcelable

The `@nullable` annotation allows a parcelable parameter or return type to be `None`. In the generated Rust code, nullable parcelable types are represented as `Option<T>`.

AIDL declaration:

```aidl
@nullable Empty RepeatNullableParcelable(@nullable in Empty input);
```

Service implementation:

```rust
fn RepeatNullableParcelable(
    &self,
    input: Option<&Empty>,
) -> rsbinder::status::Result<Option<Empty>> {
    Ok(input.cloned())
}
```

When the client passes `None`, the service receives `None` and can return `None`. When a value is provided, standard `Option` methods like `cloned()`, `map()`, and `as_ref()` work as expected. Note that `cloned()` requires the parcelable type to derive `Clone` via `@RustDerive(Clone=true)`.

## Recursive Structures

AIDL supports self-referential parcelable types using the `@nullable(heap=true)` annotation. This is necessary because a struct that directly contains itself would have infinite size. The `heap=true` attribute causes the field to be wrapped in `Box<T>` in the generated Rust code, which places the recursive field on the heap and gives the type a known size at compile time.

AIDL definition (from `RecursiveList.aidl` in the test suite):

```aidl
parcelable RecursiveList {
    int value;
    @nullable(heap=true) RecursiveList next;
}
```

This generates a Rust struct where `next` has the type `Option<Box<RecursiveList>>`. The `@nullable` part makes it `Option`, and `heap=true` makes it `Box`-wrapped. Together they enable a linked-list pattern.

Rust usage (based on the `test_reverse_recursive_list` test):

```rust
// Build a linked list: [9, 8, 7, ..., 0]
let mut head = None;
for n in 0..10 {
    let node = RecursiveList {
        value: n,
        next: head,
    };
    head = Some(Box::new(node));
}

// Send to service for reversal
let result = service.ReverseList(head.as_ref().unwrap())?;

// Traverse the reversed list: [0, 1, ..., 9]
let mut current: Option<&RecursiveList> = result.as_ref();
for n in 0..10 {
    assert_eq!(current.map(|inner| inner.value), Some(n));
    current = current.unwrap().next.as_ref().map(|n| n.as_ref());
}
assert!(current.is_none());
```

Without `heap=true`, the compiler would reject the type definition because `RecursiveList` would need to contain itself directly, leading to an infinite-size type. The `Box` indirection solves this by storing the nested value behind a pointer.

## ExtendableParcelable and ParcelableHolder

`ExtendableParcelable` is a pattern that uses `ParcelableHolder` to support type-safe, extensible data. A `ParcelableHolder` field can hold any parcelable type, allowing you to extend a parcelable without changing its base definition. This is useful for versioned interfaces where new fields may be added in the future.

AIDL definitions:

```aidl
parcelable ExtendableParcelable {
    int a;
    @utf8InCpp String b;
    ParcelableHolder ext;
    long c;
    ParcelableHolder ext2;
}

parcelable MyExt {
    int a;
    @utf8InCpp String b;
}
```

Setting an extension (based on the `test_repeat_extendable_parcelable` test):

```rust
use std::sync::Arc;

let ext = Arc::new(MyExt {
    a: 42,
    b: "EXT".into(),
});

let mut ep = ExtendableParcelable {
    a: 1,
    b: "a".into(),
    c: 42,
    ..Default::default()
};

ep.ext.set_parcelable(Arc::clone(&ext))
    .expect("error setting parcelable");
```

Sending through a service and retrieving the extension:

```rust
let mut ep2 = ExtendableParcelable::default();
service.RepeatExtendableParcelable(&ep, &mut ep2)?;

assert_eq!(ep2.a, ep.a);
assert_eq!(ep2.b, ep.b);
assert_eq!(ep2.c, ep.c);

let ret_ext = ep2.ext.get_parcelable::<MyExt>()
    .expect("error getting parcelable");
assert!(ret_ext.is_some());

let ret_ext = ret_ext.unwrap();
assert_eq!(ret_ext.a, 42);
assert_eq!(ret_ext.b, "EXT");
```

Key points about `ParcelableHolder`:

- **Type erasure**: The `ParcelableHolder` stores the extension in a type-erased manner. You must specify the concrete type when calling `get_parcelable::<T>()`.
- **Arc wrapping**: Extensions are set using `Arc<T>`, which allows shared ownership of the extension data.
- **Multiple holders**: A single parcelable can have multiple `ParcelableHolder` fields (as shown with `ext` and `ext2` above), each holding a different extension type.
- **Versioning**: This mechanism is particularly useful for forward compatibility. Older code that does not know about newer extension types can still deserialize the base parcelable and pass the `ParcelableHolder` through without losing data.

## Tips

Here are some practical guidelines when working with parcelable types in rsbinder:

- **Always use `@RustDerive(Clone=true)`** if you need to clone parcelable values. This is required for patterns like `input.cloned()` with nullable parameters. Only add it when all fields in the parcelable actually implement `Clone`.

- **Use `@RustDerive(PartialEq=true)`** when you need to compare parcelable instances in assertions or business logic. As with `Clone`, all fields must implement `PartialEq`.

- **`@nullable(heap=true)` is required for recursive types.** Without it, the compiler will reject the type due to infinite size. Use this annotation on any self-referential field.

- **Default values in AIDL translate to Rust's `Default` trait.** When you write `int count = 5;` in AIDL, calling `MyParcelable::default()` in Rust will produce a struct with `count` set to `5`.

- **Use `..Default::default()` for partial initialization.** When constructing a parcelable where you only need to set a few fields, use Rust's struct update syntax to fill the rest with defaults:
  ```rust
  let ep = ExtendableParcelable {
      a: 1,
      b: "hello".into(),
      ..Default::default()
  };
  ```

- **ParcelableHolder extensions are type-erased.** Always use `get_parcelable::<T>()` with the correct concrete type to extract the extension. If the wrong type is specified, the deserialization will fail.

- **Place each parcelable in its own `.aidl` file.** Following the AIDL convention, each parcelable type should be defined in a separate file whose name matches the type name (e.g., `UserProfile.aidl` for `parcelable UserProfile`).

- **Constants are scoped to the parcelable.** When you define `const int MAX_VALUE = 100;` inside a parcelable, access it in Rust as `MyParcelable::MAX_VALUE`. This keeps related constants close to the data they describe.
