# AIDL Guide

AIDL (Android Interface Definition Language) is the contract between a Binder service and its clients. You describe the interface — its methods, parameters, and data types — in a `.aidl` file, and the `rsbinder-aidl` compiler generates the Rust code on both sides. This guide walks through the parts of AIDL that you will actually use when writing services with rsbinder.

If you have not yet seen rsbinder running end-to-end, read [Hello, World!](./hello-world.md) first — it shows where AIDL fits into a complete project.

> With rsbinder on both ends, [Interface Macros](./interface-macros.md) declares the same interface as a Rust trait instead — same compiler, same generated code. Read this guide either way: it describes the type system both paths share.

## Chapters

- **[Data Types](./aidl-data-types.md)** — Primitive, string, array, list, map, and interface types, and how each one maps to Rust on the `in` / `out` / `inout` side. Start here.
- **[Parcelable](./aidl-parcelable.md)** — Defining user-supplied structs that can cross the Binder boundary, including nullable fields and default values.
- **[Enum and Union](./aidl-enum-union.md)** — Backed enums (newtype structs in Rust, for wire-stable forward compatibility) and unions (tagged variants).
- **[Annotations](./aidl-annotations.md)** — `@RustDerive`, `@nullable`, `@Backing`, `@JavaDerive`-equivalents, and the other annotations the Rust backend honors.

Read them in that order the first time; *Annotations* is a reference to come back to when the generated Rust does not look the way you expected.

## What the compiler rejects

`rsbinder-aidl` rejects the `.aidl` that AOSP's `aidl` rejects, so a contract written here also builds for the other backends. Each diagnostic names the rule. Besides the `@FixedSize` and `@VintfStability` rules in [Annotations](./aidl-annotations.md), it rejects:

- `ParcelableHolder` anywhere but a parcelable field: as a method argument or return type, a union member, an array or `List` element, or `@nullable`.
- `void` anywhere but a method return type.
- A `union` with no fields (`const` members do not count).
- A duplicate method name in an interface, or a duplicate argument name in a method.
- A type argument on a type that takes none (`String<int>`, `IBinder<T>`, `Plain<int>` on a non-generic parcelable), or the wrong number of them.
- A type argument that does not meet its parameter's requirement (`MQDescriptor<String, …>` where the parameter is `@FixedSize T`), or that is an array, a `List`, `void` or `ParcelableHolder`.
- A generic parcelable field whose type is one of the parameters (`parcelable Foo<T> { T value; }`), and a generic `union`. See [Generic Parcelables](./aidl-parcelable.md#generic-parcelables).
- A `const` whose type is not a primitive, a `String`, or an array of those.

rsbinder keeps `boolean`, `char` and array constants, which AOSP refuses.

`rsbinder-aidl` also refuses an AIDL name that collides with an item the generated Rust adds next to it, with a diagnostic rather than a rustc duplicate-definition error in the generated file under `OUT_DIR`. For an interface `IFoo`, those items are `BnFoo`, `BpFoo`, `IFooDefault`, `IFooDefaultRef` and a `transactions` module, with async codegen also `BnFooAdapter`, `IFooAsync` and `IFooAsyncService`, the module-level `DEFAULT_IMPL` and `on_transact`, plus `VERSION` and `HASH` when a version or hash is set; a union adds `Tag`, and an enum (a union's `Tag` included) adds the methods `get` and `enum_values`. A nested type, constant, enumerator or union field with one of those names — `const int VERSION = 1;` in a versioned interface, a nested `parcelable Tag` in a union, a union field named `get` — needs another name. AOSP's `aidl` also refuses a union named `Tag` and a nested `Tag` in a union; the other names come from the Rust backend.

Type names starting with `__Rsb` are refused as well, which AOSP's `aidl` accepts. The async codegen declares its type parameters and a helper struct (`__RsbPool`, `__RsbService`, `__RsbRuntime`, `__RsbAsyncWrapper`) in the scope where method signatures are resolved, so a nested type of the same name would resolve to the generated item.

The generated Rust also spells some names without a path, and those names are not refused: a nested type named `Vec`, `Box`, `Option`, `String`, `Default` or `rsbinder`. A nested type becomes a module next to the signatures, fields and bodies of the type that contains it, which write `BinderResult<Vec<i32>>`, `Option<String>`, `Box<…>`, `Default::default()` and `rsbinder::…`, so each of those names resolves to the nested type and the generated file under `OUT_DIR` fails with a rustc error, not an AIDL diagnostic. A fixed-size array field longer than 32 elements is initialized through `std::array::from_fn`, so a nested type named `std` breaks that field the same way. AOSP's Rust backend writes `Vec` and `Box` with a full path and accepts such a contract; here the nested type needs another name. A generic parcelable's type parameter with one of these names is refused (see [Generic Parcelables](./aidl-parcelable.md#generic-parcelables)).

Then move on to [Service Development](./service-development.md) for the runtime patterns that put these types to work.
