# Enum and Union

AIDL supports two powerful type constructs beyond simple interfaces and parcelables: **enums** and **unions**. Enums provide named integer constants with type safety, while unions represent a value that can be one of several different types. Both are fully supported by rsbinder's AIDL compiler and map naturally to Rust constructs.

This chapter covers how to define enums and unions in AIDL, how they translate to Rust code, and how to use them in practice.

## Enum Types

AIDL enums are backed by a specific integer type, declared using the `@Backing` annotation. Unlike Rust's native enums, AIDL enums map to **newtype structs** wrapping the backing integer. This design preserves wire compatibility and allows values outside the defined set, which is important for forward compatibility in IPC.

### Defining Enums in AIDL

The `@Backing(type=...)` annotation is required and specifies the underlying integer type. Here are examples for each supported backing type:

**Byte-backed enum:**

```aidl
@Backing(type="byte")
enum ByteEnum {
    FOO = 1,
    BAR = 2,
    BAZ,
}
```

**Int-backed enum:**

```aidl
@Backing(type="int")
enum IntEnum {
    FOO = 1000,
    BAR = 2000,
    BAZ,
}
```

**Long-backed enum:**

```aidl
@Backing(type="long")
enum LongEnum {
    FOO = 100000000000,
    BAR = 200000000000,
    BAZ,
}
```

When a value is omitted (as with `BAZ` above), it is automatically assigned the previous value plus one. So `BAZ` would be `3`, `2001`, and `200000000001` respectively.

### Backing Type Mapping

The AIDL backing type determines the Rust integer type used inside the generated newtype struct:

| AIDL Backing Type | Rust Type |
|-------------------|-----------|
| `byte`            | `i8`      |
| `int`             | `i32`     |
| `long`            | `i64`     |

### Using Enums in Rust

Enum values are accessed as associated constants on the generated struct. The generated type implements `Default`, `Debug`, `PartialEq`, `Eq`, and serialization traits automatically.

```rust
// Access enum values as associated constants
let e = ByteEnum::FOO;
let result = service.RepeatByteEnum(e)?;
assert_eq!(result, ByteEnum::FOO);
```

Enums work naturally with arrays and vectors:

```rust
// Enums can be used in arrays
let input = [ByteEnum::FOO, ByteEnum::BAR, ByteEnum::BAZ];
let mut repeated = vec![];
let reversed = service.ReverseByteEnum(&input, &mut repeated)?;
```

Each generated enum type provides an `enum_values()` method that returns a slice of all defined values, which is useful for iteration and validation:

```rust
// enum_values() returns all defined values
let all_values = ByteEnum::enum_values();
```

### Enums in Service Interfaces

Enums are commonly used as parameters and return types in AIDL interfaces:

```aidl
interface ITestService {
    ByteEnum RepeatByteEnum(ByteEnum token);
    IntEnum RepeatIntEnum(IntEnum token);
    LongEnum RepeatLongEnum(LongEnum token);

    ByteEnum[] ReverseByteEnum(in ByteEnum[] input, out ByteEnum[] repeated);
}
```

The generated Rust trait methods use the enum types directly, providing compile-time type safety across the IPC boundary.

## Union Types

AIDL unions represent a tagged value that holds exactly one of several possible fields at a time. They map to Rust `enum` types, which are a natural fit since Rust enums are sum types with variants.

### Defining Unions in AIDL

Here is a union definition from the rsbinder test suite:

```aidl
@RustDerive(Clone=true, PartialEq=true)
union Union {
    int[] ns = {};
    int n;
    int m;
    @utf8InCpp String s;
    @nullable IBinder ibinder;
    @utf8InCpp List<String> ss;
    ByteEnum be;

    const @utf8InCpp String S1 = "a string constant in union";
}
```

Key points about union definitions:

- **The first field is the default.** When a union is default-constructed, it takes the value of the first field. In this example, the default is `ns` initialized to an empty array `{}`.
- **Fields can have different types**, including primitives, strings, arrays, other AIDL types, and even binder references.
- **Constants can be defined** inside unions, independent of the union's variants.
- **`@RustDerive`** is recommended so the generated Rust type supports `Clone` and `PartialEq`.

### Using Unions in Rust

The AIDL union generates a Rust enum. Because AIDL types are organized into modules, the union type and its enum variants live inside a module named after the union. Variants are accessed as `Union::Union::VariantName(...)`:

```rust
// Default value is the first field
assert_eq!(Union::Union::default(), Union::Union::Ns(vec![]));

// Creating union variants
let u1 = Union::Union::N(42);
let u2 = Union::Union::S("hello".into());
let u3 = Union::Union::Be(ByteEnum::FOO);
```

Constants defined inside a union are accessed directly on the module, not through a variant:

```rust
// Constants defined in the union
let s = Union::S1;  // "a string constant in union"

// Using a constant as a union variant value
let u = Union::Union::S(Union::S1.to_string());
```

### Union Tags

Each union has an associated `Tag` enum that identifies which variant is currently active. Tags are useful when you need to inspect or communicate which field a union holds without extracting the value itself.

```rust
let result = service.GetUnionTags(&[
    Union::Union::N(0),
    Union::Union::Ns(vec![]),
])?;
assert_eq!(result, vec![Union::Tag::n, Union::Tag::ns]);
```

Tags can also be used in match expressions when implementing service logic. Here is an example from the test service implementation:

```rust
fn GetUnionTags(
    &self,
    input: &[Union::Union],
) -> Result<Vec<Union::Tag>> {
    Ok(input.iter().map(|u| match u {
        Union::Union::Ns(_) => Union::Tag::ns,
        Union::Union::N(_) => Union::Tag::n,
        Union::Union::M(_) => Union::Tag::m,
        Union::Union::S(_) => Union::Tag::s,
        Union::Union::Ibinder(_) => Union::Tag::ibinder,
        Union::Union::Ss(_) => Union::Tag::ss,
        Union::Union::Be(_) => Union::Tag::be,
    }).collect())
}
```

### Unions Containing Enums (EnumUnion)

Unions can contain enum types as fields and specify default values using enum constants:

```aidl
@RustDerive(Clone=true, PartialEq=true)
union EnumUnion {
    IntEnum intEnum = IntEnum.FOO;
    LongEnum longEnum;
    /** @deprecated do not use this */
    int deprecatedField;
}
```

In this example, the default value is the first field (`intEnum`) initialized to `IntEnum.FOO`. In Rust:

```rust
assert_eq!(EnumUnion::default(), EnumUnion::IntEnum(IntEnum::FOO));
```

Note that the `@deprecated` Javadoc annotation on `deprecatedField` will generate a `#[deprecated]` attribute in the Rust code, so using that variant will produce a compiler warning.

### Nested Unions

Unions can also be nested inside other unions:

```aidl
union UnionInUnion {
    EnumUnion first;
    int second;
}
```

This allows building complex tagged-value hierarchies that are fully type-safe on the Rust side.

## Tips and Best Practices

- **Always specify `@Backing`** for enums. The AIDL compiler requires it, and the choice of backing type affects both the wire format and the Rust integer type.
- **The union default is always the first field.** Order your fields accordingly, placing the most common or natural default first.
- **Use `@RustDerive(Clone=true, PartialEq=true)`** on unions so they can be compared and cloned in Rust. Without this, you cannot use `==` or `.clone()` on union values.
- **Union constants are module-level**, not variants. Access them as `Union::S1`, not through any variant.
- **Enum `enum_values()`** returns all defined constants, which is useful for exhaustive testing or validation loops.
- **Forward compatibility**: Because AIDL enums are backed by integers, a service may receive values not defined in the current enum. Design your code to handle unknown values gracefully.
- **Tag enums** use lowercase field names (e.g., `Union::Tag::ns`, not `Union::Tag::Ns`), matching the original AIDL field names.

## Further Reading

- [Android AIDL documentation](https://source.android.com/docs/core/architecture/aidl) -- the upstream reference for AIDL syntax and semantics
- [AIDL annotation reference](https://source.android.com/docs/core/architecture/aidl/aidl-annotations) -- details on `@Backing`, `@RustDerive`, and other annotations
- The rsbinder test suite at `tests/aidl/` and `tests/src/test_client.rs` contains comprehensive examples of enum and union usage
