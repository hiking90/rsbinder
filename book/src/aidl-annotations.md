# AIDL Annotations

AIDL annotations modify how the code generator produces Rust code from `.aidl` files. They control everything from trait derivation and backing types to nullability and interface stability. This chapter covers the annotations relevant to the Rust backend in rsbinder, with examples showing how each annotation affects the generated code.

If you are new to AIDL data types, read the [AIDL Data Types](./aidl-data-types.md) chapter first. Annotations build on those type mappings by adding metadata that changes how types are generated, serialized, or constrained.

## @RustDerive

The `@RustDerive` annotation tells the code generator to add Rust `derive` attributes to parcelable and union types. Without this annotation, generated types receive only the minimum set of derives needed for serialization.

```aidl
@RustDerive(Clone=true, PartialEq=true)
parcelable Point {
    int x;
    int y;
}
```

This generates a Rust struct with both `Clone` and `PartialEq` derived:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}
```

### Available Derives

| Derive | Description |
|--------|-------------|
| `Clone` | Enables cloning of the type |
| `PartialEq` | Enables equality comparison |
| `Copy` | Enables bitwise copy (fixed-size types only) |

### Why Clone Is Not Derived by Default

Generated types do **not** derive `Clone` by default. This is intentional because some AIDL types contain fields that cannot be cloned:

- **`ParcelFileDescriptor`** wraps an `OwnedFd`, which represents sole ownership of a file descriptor. Cloning it would require duplicating the file descriptor at the OS level, which is not a simple bitwise copy.
- **`ParcelableHolder`** contains a `Mutex`, which cannot be cloned.

If your parcelable contains only primitive fields and cloneable types, add `@RustDerive(Clone=true)` explicitly. If the type contains a `ParcelFileDescriptor` or `ParcelableHolder` field, attempting to derive `Clone` will produce a compile error.

### Copy Derivation for Fixed-Size Types

For parcelables that contain only primitive fields, you can derive `Copy` in addition to `Clone`. This is typically combined with the `@FixedSize` annotation:

```aidl
@RustDerive(Clone=true, Copy=true, PartialEq=true)
@FixedSize
parcelable IntParcelable {
    int value;
}
```

The `Copy` derive is only valid when all fields are `Copy` types. Using it on a parcelable that contains `String`, arrays, or other heap-allocated types will result in a compile error.

## @Backing

The `@Backing` annotation specifies the underlying integer type for an AIDL enum. This controls both the wire format and the generated Rust type.

```aidl
@Backing(type="byte")
enum Priority {
    LOW = 0,
    MEDIUM = 1,
    HIGH = 2,
}
```

The generated Rust code uses a newtype pattern with the corresponding integer type:

```rust
pub mod Priority {
    #![allow(non_upper_case_globals)]
    pub type Priority = i8;
    pub const LOW: Priority = 0;
    pub const MEDIUM: Priority = 1;
    pub const HIGH: Priority = 2;
}
```

### Supported Backing Types

| AIDL Backing | Rust Type | Size |
|-------------|-----------|------|
| `"byte"` | `i8` | 1 byte |
| `"int"` | `i32` | 4 bytes |
| `"long"` | `i64` | 8 bytes |

If no `@Backing` annotation is specified, the default backing type is `"byte"`.

Choose the smallest backing type that fits your range of values. Enum values are serialized to the Binder transaction as their backing integer type, so a smaller backing type produces a more compact wire format.

## @nullable

The `@nullable` annotation marks a type as optional, indicating that the value may be absent. In Rust, this maps to `Option<T>`.

### Basic Usage

Apply `@nullable` to method parameters, return values, or struct fields:

```aidl
interface IUserService {
    @nullable String getName();
    void setValues(@nullable in int[] values);
}
```

The generated Rust signatures use `Option`:

```rust
fn getName(&self) -> rsbinder::status::Result<Option<String>>;
fn setValues(&self, values: Option<&[i32]>) -> rsbinder::status::Result<()>;
```

When `None` is passed over a Binder transaction, a null marker is written to the parcel. The receiving side deserializes it as `None` without allocating any data.

### Heap-Allocated Nullable: @nullable(heap=true)

For recursive or self-referential types, use `@nullable(heap=true)` to wrap the field in a `Box<T>`:

```aidl
parcelable RecursiveList {
    int value;
    @nullable(heap=true) RecursiveList next;
}
```

This generates:

```rust
pub struct RecursiveList {
    pub value: i32,
    pub next: Option<Box<RecursiveList>>,
}
```

The `Box` indirection is necessary because without it, `RecursiveList` would contain itself directly, making the type infinitely large. The `heap=true` parameter places the inner value on the heap, giving the struct a finite, known size at compile time.

Use `@nullable(heap=true)` only when you need recursive structures. For non-recursive optional fields, plain `@nullable` is sufficient and avoids the extra heap allocation.

## @utf8InCpp

The `@utf8InCpp` annotation exists in Android AIDL to specify UTF-8 encoding for strings in the C++ backend, where the default encoding is UTF-16. In rsbinder, this annotation has **no effect** because Rust strings are always UTF-8.

```aidl
interface ITextService {
    @utf8InCpp String getData();
    @utf8InCpp List<String> getNames();
}
```

Both `String` and `@utf8InCpp String` produce identical Rust type mappings:

| Direction | Rust Type |
|-----------|-----------|
| Input (`in`) | `&str` |
| Output / Return | `String` |

You may encounter this annotation in AIDL files that were originally written for Android's C++ backend. It is safe to keep or remove it when targeting rsbinder; the generated Rust code is identical either way.

## @Descriptor

The `@Descriptor` annotation overrides the interface descriptor string that identifies an interface on the Binder wire protocol. This is useful when renaming an interface while maintaining backward compatibility with existing clients or services.

Every Binder interface has a descriptor string derived from its fully qualified name (e.g., `android.aidl.tests.IOldName`). When you rename an interface, the descriptor changes, breaking compatibility. The `@Descriptor` annotation lets you decouple the source name from the wire descriptor.

Consider an interface that was originally named `IOldName`:

```aidl
// IOldName.aidl
interface IOldName {
    String RealName();
}
```

You can create a new interface `INewName` that uses the same descriptor:

```aidl
// INewName.aidl
@Descriptor(value="android.aidl.tests.IOldName")
interface INewName {
    String RealName();
}
```

Because both interfaces share the same descriptor, they are interchangeable at the Binder level:

```rust
// A service registered as IOldName can be used as INewName
let new_from_old = old_service
    .as_binder()
    .into_interface::<dyn INewName::INewName>();
assert!(new_from_old.is_ok());
```

This is particularly useful during interface migrations where you want to rename types in your codebase without requiring all clients and services to update simultaneously.

## @VintfStability

The `@VintfStability` annotation marks a type or interface as part of the Vendor Interface (VINTF). VINTF-stable types are subject to stricter compatibility rules to ensure that vendor and system partitions can be updated independently.

```aidl
@VintfStability
parcelable VintfData {
    int value;
}
```

### Stability Rules

VINTF-stable types enforce the following constraints:

- A VINTF-stable parcelable can only contain fields whose types are also VINTF-stable.
- A VINTF-stable interface can only use VINTF-stable types in its method signatures.
- Attempting to embed a non-VINTF type inside a VINTF-stable type will produce a `StatusCode::BadValue` error at runtime.

These rules exist to guarantee that the serialization format of VINTF types remains stable across system updates. On Android, this is critical for maintaining compatibility between the framework and vendor HAL implementations.

On Linux, the `@VintfStability` annotation is recognized by the code generator but the stability enforcement depends on how the service manager is configured.

## @FixedSize

The `@FixedSize` annotation indicates that a parcelable has a fixed serialization size, meaning its wire format is always the same number of bytes regardless of the field values.

```aidl
@FixedSize
parcelable FixedPoint {
    int x;
    int y;
}
```

### Constraints

Fixed-size parcelables can only contain:

- Primitive types (`boolean`, `byte`, `char`, `int`, `long`, `float`, `double`)
- Other `@FixedSize` parcelables
- Enums with a `@Backing` annotation

They cannot contain:

- `String` or `@utf8InCpp String`
- Arrays (`T[]`)
- `ParcelFileDescriptor`
- `IBinder`
- Any variable-length type

### Relationship with @RustDerive(Copy=true)

The `@FixedSize` annotation is a prerequisite for deriving `Copy` in Rust, because only types with a fixed memory layout can be safely copied with a bitwise copy:

```aidl
@RustDerive(Clone=true, Copy=true, PartialEq=true)
@FixedSize
parcelable Coordinate {
    double latitude;
    double longitude;
}
```

Without `@FixedSize`, adding `Copy` to the derive list may compile but violates the intended semantics. Always pair `Copy` with `@FixedSize` to make the intent explicit.

## Summary

The following table provides a quick reference for all annotations covered in this chapter.

| Annotation | Applies To | Rust Effect |
|------------|------------|-------------|
| `@RustDerive` | parcelable, union | Adds `derive` attributes (`Clone`, `Copy`, `PartialEq`) |
| `@Backing` | enum | Sets the backing integer type (`i8`, `i32`, `i64`) |
| `@nullable` | field, param, return | Maps to `Option<T>` |
| `@nullable(heap=true)` | field | Maps to `Option<Box<T>>` for recursive types |
| `@utf8InCpp` | String | No effect in Rust (strings are always UTF-8) |
| `@Descriptor` | interface | Overrides the wire descriptor string |
| `@VintfStability` | parcelable, interface | Enforces VINTF stability rules |
| `@FixedSize` | parcelable | Restricts fields to fixed-size types, enables `Copy` |

When writing AIDL files for rsbinder, the most commonly used annotations are `@RustDerive` (for ergonomic Rust types), `@Backing` (for enums), and `@nullable` (for optional values). The remaining annotations are important for interoperability with Android or for specific use cases like recursive types and interface migration.
