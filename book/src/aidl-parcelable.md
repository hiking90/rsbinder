# Parcelable

A parcelable is a user-defined struct that can cross the Binder boundary. You declare it in an `.aidl` file and `rsbinder-aidl` generates the Rust struct and its codec — it is how any non-trivial interface passes structured data.

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

Parcelable types can define constants, including numeric values and bit flags. These are emitted as module-level `pub const` items inside the generated parcelable module — not as associated constants on the struct. The familiar-looking `TypeName::CONSTANT` path therefore resolves through the *module* name (which shares its name with the struct), so you must import the module, not the struct: `use ...::Config;` gives you both `Config::BIT_VERBOSE` (the constant) and `Config::Config` (the struct). Importing the struct directly (`use ...::Config::Config;`) shadows the module path and makes the constants unreachable through it. This pattern is commonly used for configuration values and flag fields.

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

In Rust, the constants are accessed through the module path, and the struct through the module-qualified `Config::Config` spelling:

```rust
use com::example::Config;

let mut cfg = Config::Config::default();
assert_eq!(cfg.retryCount, 5);

cfg.flags = Config::BIT_VERBOSE | Config::BIT_DEBUG;
assert_eq!(cfg.flags, 0x5);
```

This pattern mirrors how the Android test suite defines and uses bit flags within `StructuredParcelable`, where constants like `BIT0`, `BIT1`, and `BIT2` are defined alongside the fields that use them.

## Using Parcelable in Services

Parcelable types are passed to and from service methods as regular parameters. A common pattern is to pass a mutable reference to a parcelable so that the service can fill in or modify its fields. This is based on the `FillOutStructuredParcelable` pattern used in the rsbinder test suite. As with the constants above, `StructuredParcelable` in the snippets below names the generated *module*: the struct is written module-qualified as `StructuredParcelable::StructuredParcelable`, while the bit constants (`StructuredParcelable::BIT0`, `StructuredParcelable::BIT2`) resolve through the module.

Service implementation:

```rust
fn FillOutStructuredParcelable(
    &self,
    parcelable: &mut StructuredParcelable::StructuredParcelable,
) -> rsbinder::BinderResult<()> {
    parcelable.shouldBeJerry = "Jerry".into();
    parcelable.shouldContainThreeFs = vec![parcelable.f, parcelable.f, parcelable.f];
    parcelable.shouldSetBit0AndBit2 =
        StructuredParcelable::BIT0 | StructuredParcelable::BIT2;
    Ok(())
}
```

Client side:

```rust
let mut parcelable = StructuredParcelable::StructuredParcelable {
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
@nullable Empty RepeatNullableParcelable(in @nullable Empty input);
```

Service implementation:

```rust
fn RepeatNullableParcelable(
    &self,
    input: Option<&Empty>,
) -> rsbinder::BinderResult<Option<Empty>> {
    Ok(input.cloned())
}
```

When the client passes `None`, the service receives `None` and can return `None`. When a value is provided, standard `Option` methods like `cloned()`, `map()`, and `as_ref()` work as expected. Note that `cloned()` requires the parcelable type to derive `Clone` via `@RustDerive(Clone=true)`.

## Recursive Structures

AIDL supports self-referential parcelable types. Mark the recursive field `@nullable(heap=true)`: the inner value then lives on the heap, so the struct has a finite, known size. As in AOSP's Rust backend, such a field is `Option<Box<T>>` wherever it is, on a cycle or not. `heap=true` is accepted only on a parcelable or union type; on any other type (a `String`, an array, an interface) it is rejected, as AOSP rejects it.

Every cycle needs at least one such field. Once one field on the cycle is boxed, the other fields on it stay inline, as in AOSP: in `parcelable A { @nullable(heap=true) B b; } parcelable B { A a; }`, `A.b` is `Option<Box<B>>` and `B.a` is a plain `A`. A type reached through an interface handle or a `Vec` element keeps the enclosing type finite and closes no cycle. A cycle made only of non-`@nullable` fields or fixed-size arrays (`T[N]`, which store their elements inline) is rejected with `aidl::recursive_parcelable`, because neither form can be boxed.

rsbinder also accepts a bare `@nullable` field (no `heap=true`) that closes a cycle and boxes it, since Rust can represent it. AOSP's `aidl` rejects that `.aidl` as a recursive parcelable, so rsbinder-aidl prints a `cargo:warning` naming the field; add `heap=true` to keep the file buildable in an Android tree.

AIDL definition (from `RecursiveList.aidl` in the test suite):

```aidl
parcelable RecursiveList {
    int value;
    @nullable(heap=true) RecursiveList next;
}
```

This generates a Rust struct where `next` has the type `Option<Box<RecursiveList>>`: `@nullable` makes it `Option`, and `heap=true` adds the `Box`.

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
let mut current: Option<&RecursiveList> = Some(&result);
for n in 0..10 {
    assert_eq!(current.map(|inner| inner.value), Some(n));
    current = current.unwrap().next.as_ref().map(|n| n.as_ref());
}
assert!(current.is_none());
```

Without the `Box` indirection the Rust compiler would reject the type definition because `RecursiveList` would need to contain itself directly, leading to an infinite-size type.

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
- **Passing through over RPC**: an undecoded holder can be passed on over kernel binder, and within one RPC session on the android-16 (v2) wire. On the r34 and android-13 v0/v1 wires, and into a different session or transport, writing it returns `BadType`; decode it with `get_parcelable::<T>()` first. The `ParcelableHolder` rustdoc lists each case.

## Generic Parcelables

A parcelable may take type parameters, as AOSP's
`android.hardware.common.fmq.MQDescriptor<T, Flavor>` does:

```aidl
parcelable MQDescriptor<@FixedSize T, Flavor> {
    GrantorDescriptor[] grantors;
    NativeHandle handle;
    int quantum;
    int flags;
}
```

A parameter never reaches the parcel. It is a compile-time label that ties the
descriptor to an element type and a flavor, so two instantiations produce the
same bytes. The generated struct mirrors AOSP's Rust backend:

```rust
pub struct MQDescriptor<T, Flavor> {
    pub grantors: Vec<GrantorDescriptor>,
    pub handle: NativeHandle,
    pub quantum: i32,
    pub flags: i32,
    pub _phantom_T: core::marker::PhantomData<T>,
    pub _phantom_Flavor: core::marker::PhantomData<Flavor>,
}
```

Every impl (`Default`, `Parcelable`, `Serialize`, `Deserialize`,
`ParcelableMetadata`) is generic with no bound on the parameters. The phantom
fields are `pub` (AOSP keeps them private) so `MQDescriptor { quantum: 4,
..Default::default() }` compiles outside the generated module. Note that
`#[derive(Debug)]` and `@RustDerive` bound the parameters the way any derive
does: `Foo<Bar>: PartialEq` needs `Bar: PartialEq`.

An annotation on a parameter is a requirement on the argument, checked at
every use site: `@FixedSize T` accepts a primitive, an enum, or a `@FixedSize`
parcelable or union (`MQDescriptor<byte, …>` compiles, `MQDescriptor<String, …>`
does not; a generic instantiation such as `Elem<int>` never qualifies, as in
AOSP), and `@VintfStability T` accepts only a `@VintfStability` declaration.
Any other annotation on a parameter is a requirement no argument meets: the
Java-only `@JavaPassthrough` and `@JavaSuppressLint` parse on the declaration,
as in AOSP, but every use of such a generic is rejected. The argument count
must match the declaration.

The generator rejects, with a diagnostic naming the reason:

- a field whose type is a parameter (`parcelable Foo<T> { T value; }`) —
  nothing would be written for it;
- a generic `union` — an enum with an unused parameter has no Rust form;
- a nested declaration inside a generic parcelable — AOSP's rule ("Generic
  types can't have nested types"); a field could only reach it by a path the
  parameter's name would capture;
- a parameter named like something the generated Rust spells unqualified: a
  keyword, `String`, `Vec`, `Option`, `Box`, `Default`, `core`, `std`, a
  primitive, the `rsbinder` crate, or the parcelable itself
  (`parcelable R<R>`). A nested type named `Vec`, `Box`, `Option`, `String`,
  `Default`, `std` or `rsbinder` is not refused and breaks the generated file
  instead (see
  [What the compiler rejects](./aidl-guide.md#what-the-compiler-rejects));
- an array, a `List` or `void` as a type argument;
- an annotation on a type argument (`Q<@nullable Elem>`) — as in AOSP,
  `@nullable` belongs to the whole field: `@nullable Q<Elem>`.

## Tips

- **Always use `@RustDerive(Clone=true)`** if you need to clone parcelable values. This is required for patterns like `input.cloned()` with nullable parameters. Only add it when all fields in the parcelable actually implement `Clone`.

- **Use `@RustDerive(PartialEq=true)`** when you need to compare parcelable instances in assertions or business logic. As with `Clone`, all fields must implement `PartialEq`.

- **Write `@nullable(heap=true)` on a recursive field.** It gives the field the same `Option<Box<T>>` type AOSP's Rust backend generates, and keeps the `.aidl` accepted by AOSP's `aidl`; a bare `@nullable` there works in rsbinder but draws a warning.

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

- **Constants live on the generated module, not the struct.** Import the module: `MyParcelable::MAX_VALUE` is the constant and `MyParcelable::MyParcelable` is the struct. Importing the struct directly hides the constants.
