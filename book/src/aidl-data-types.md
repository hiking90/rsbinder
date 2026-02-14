# AIDL Data Types

AIDL (Android Interface Definition Language) defines the interface contract between a Binder service and its clients. When you write an `.aidl` file, `rsbinder-aidl` generates Rust code that maps each AIDL type to the corresponding Rust type. Understanding these mappings is essential for implementing services and calling them correctly from client code.

This chapter covers the supported AIDL data types, how they map to Rust, and common patterns you will encounter when working with rsbinder.

## Primitive Types

The following table shows how AIDL primitive types map to Rust types. Input parameters (`in`) are passed by value or by reference, while output parameters (`out`) are passed as mutable references so the service can write results back to the caller.

| AIDL Type | Rust Type (in) | Rust Type (out) | Notes |
|-----------|---------------|-----------------|-------|
| boolean | bool | &mut bool | |
| byte | i8 | &mut i8 | Single values use i8; array Reverse uses u8 |
| char | u16 | &mut u16 | UTF-16 code unit |
| int | i32 | &mut i32 | |
| long | i64 | &mut i64 | |
| float | f32 | &mut f32 | |
| double | f64 | &mut f64 | |
| String | &str | &mut String | |
| @utf8InCpp String | &str | &mut String | Same mapping in rsbinder |
| T[] | &[T] | &mut Vec\<T\> | |
| @nullable T | Option\<&T\> | &mut Option\<T\> | |
| IBinder | &SIBinder | | |
| ParcelFileDescriptor | &ParcelFileDescriptor | | |

Here is an AIDL interface that exercises the primitive types:

```aidl
interface IDataService {
    boolean RepeatBoolean(boolean token);
    byte RepeatByte(byte token);
    int RepeatInt(int token);
    long RepeatLong(long token);
    float RepeatFloat(float token);
    double RepeatDouble(double token);
}
```

The generated Rust trait expects the following signatures. A service implementation simply returns each value back to the caller:

```rust
impl IDataService for MyService {
    fn RepeatBoolean(&self, token: bool) -> rsbinder::status::Result<bool> {
        Ok(token)
    }
    fn RepeatByte(&self, token: i8) -> rsbinder::status::Result<i8> {
        Ok(token)
    }
    fn RepeatInt(&self, token: i32) -> rsbinder::status::Result<i32> {
        Ok(token)
    }
    // ... similar for other types
}
```

Each method returns `rsbinder::status::Result<T>`, which allows the service to return either a value or a `Status` error to the client.

## String Types

AIDL `String` maps to `&str` for input parameters and `String` for return values. This follows Rust's standard convention of borrowing for inputs and returning owned data for outputs.

The `@utf8InCpp` annotation exists in Android AIDL to distinguish between UTF-16 and UTF-8 string encodings in the C++ backend. In Android's C++ Binder, strings are UTF-16 by default and `@utf8InCpp` switches them to `std::string` (UTF-8). In rsbinder, this annotation has no effect because Rust strings are always UTF-8. Both `String` and `@utf8InCpp String` produce the same Rust type mapping.

A simple service method that echoes a string back to the caller looks like this:

```rust
fn RepeatString(&self, input: &str) -> rsbinder::status::Result<String> {
    Ok(input.into())
}
```

Note the use of `.into()` to convert the borrowed `&str` into an owned `String` for the return value. You can also use `input.to_string()` or `input.to_owned()` -- all three are equivalent here.

## Arrays and the Reverse Pattern

A common pattern in AIDL test interfaces is the "Reverse" method. The method receives an input array, copies it into an `out` parameter called `repeated`, and returns the reversed array. This exercises both input and output array handling in a single call.

The AIDL definition looks like this:

```aidl
int[] ReverseInt(in int[] input, out int[] repeated);
```

In the generated Rust trait, the `in` parameter becomes a slice reference (`&[i32]`) and the `out` parameter becomes a mutable reference to a `Vec` (`&mut Vec<i32>`). The return value is also a `Vec`:

```rust
fn ReverseInt(&self, input: &[i32], repeated: &mut Vec<i32>)
    -> rsbinder::status::Result<Vec<i32>>
{
    repeated.clear();
    repeated.extend_from_slice(input);
    Ok(input.iter().rev().cloned().collect())
}
```

On the client side, you pass the input array and a mutable `Vec` to receive the repeated copy. After the call returns, both the `repeated` vector and the return value are populated:

```rust
let input = vec![1, 2, 3];
let mut repeated = vec![];
let reversed = service.ReverseInt(&input, &mut repeated)?;
assert_eq!(repeated, vec![1, 2, 3]);
assert_eq!(reversed, vec![3, 2, 1]);
```

This pattern applies to all array types, including `boolean[]`, `byte[]`, `long[]`, `float[]`, `double[]`, `String[]`, and arrays of parcelable types. The Reverse pattern is particularly useful in testing because it validates that data survives a round trip through Binder serialization and deserialization in both directions.

## Nullable Types

The `@nullable` annotation indicates that a parameter or return value may be absent. In Rust, this maps naturally to `Option<T>`.

For input parameters, a nullable array becomes `Option<&[T]>`. For return values, it becomes `Option<Vec<T>>`. This allows both the client and service to represent the absence of a value without resorting to sentinel values or empty collections.

AIDL definition:

```aidl
@nullable int[] RepeatNullableIntArray(@nullable in int[] input);
```

Rust service implementation:

```rust
fn RepeatNullableIntArray(&self, input: Option<&[i32]>)
    -> rsbinder::status::Result<Option<Vec<i32>>>
{
    Ok(input.map(<[i32]>::to_vec))
}
```

Client usage:

```rust
let result = service.RepeatNullableIntArray(Some(&[1, 2, 3]));
assert_eq!(result, Ok(Some(vec![1, 2, 3])));

let result = service.RepeatNullableIntArray(None);
assert_eq!(result, Ok(None));
```

When `None` is passed, the Binder transaction sends a null marker and the service receives `None`. When a value is present, it is serialized and deserialized normally.

The `@nullable` annotation can also be applied to `String`, `IBinder`, and parcelable types. Without `@nullable`, these types must always be present -- passing a null value will result in a transaction error.

## Parameter Direction: in, out, and inout

AIDL parameters have a direction tag that controls how data flows between client and service. This affects both the wire format (what data is serialized into the Binder transaction) and the generated Rust method signatures.

### `in` (default)

Data flows from the client to the service. This is the default direction and does not need to be specified explicitly (though you can write it for clarity). In Rust, `in` parameters are passed by value for primitives or by reference for complex types like arrays and strings.

```aidl
void Process(in int[] data);   // explicit 'in'
void Process(int[] data);      // same as above, 'in' is the default
```

For primitive types like `int` and `boolean`, the `in` direction simply means the value is copied into the Binder transaction. For complex types like arrays, a slice reference (`&[T]`) is used so the data is serialized without requiring the caller to give up ownership.

### `out`

Data flows from the service back to the client. The client provides a mutable container and the service fills it with data. In Rust, `out` parameters are passed as `&mut` references. The initial contents of the container are not sent to the service -- only the service's written data is transmitted back.

```aidl
void GetData(out int[] result);
```

In Rust, this generates a `&mut Vec<i32>` parameter. The caller should provide an empty or pre-allocated vector; the service is responsible for populating it.

### `inout`

Data flows in both directions. The client sends initial data to the service, the service may modify it, and the modified data is sent back. In Rust, `inout` parameters are also passed as `&mut` references, but unlike `out` parameters, the initial value is serialized and sent to the service.

```aidl
void Transform(inout int[] data);
```

Use `inout` when the service needs to read the existing value and modify it in place. Prefer `in` or `out` when data only needs to flow in one direction, as this avoids unnecessary serialization overhead.

> **Note**: Primitive types (`boolean`, `byte`, `char`, `int`, `long`, `float`, `double`) do not require a direction tag. Direction tags are only meaningful for non-primitive types such as arrays, strings, and parcelable types.

## Tips

Here are a few practical details to keep in mind when working with AIDL data types in rsbinder:

- **The `byte` type has a subtle difference between single values and arrays.** A single `byte` parameter maps to `i8` (signed), but when used in the `ReverseByte` pattern, array elements use `u8` (unsigned). This matches Android's Binder behavior where byte arrays are treated as unsigned.

- **Rust strings are always UTF-8, so `@utf8InCpp` has no special behavior.** In Android's C++ backend, this annotation switches between `String16` (UTF-16) and `std::string` (UTF-8). Since Rust's `String` type is inherently UTF-8, both `String` and `@utf8InCpp String` produce identical code.

- **Arrays in AIDL map to slices for input and `Vec` for output.** Input arrays use `&[T]`, which is efficient because no allocation is needed on the caller side. Output arrays and return values use `Vec<T>`, giving the service ownership of the returned data.

- **Nullable types use `Option`.** This is idiomatic Rust and avoids the null pointer pitfalls found in C++ and Java Binder implementations. Always check for `None` on the client side when calling methods that return nullable types.

- **Direction tags affect performance.** An `inout` parameter requires serialization in both directions. If you only need data to flow one way, use `in` or `out` to reduce the amount of data copied over the Binder transaction.

- **Return values are always `Result`.** Every AIDL method in rsbinder returns `rsbinder::status::Result<T>`, allowing services to report errors using `Status` codes. Even void methods return `rsbinder::status::Result<()>`.

- **`char` is UTF-16, not UTF-8.** The AIDL `char` type maps to Rust's `u16`, representing a single UTF-16 code unit. This is not the same as Rust's native `char` type, which is a Unicode scalar value. Be mindful of this difference when working with character data.

For more information on AIDL syntax and features, refer to the [Android AIDL documentation](https://source.android.com/docs/core/architecture/aidl).
