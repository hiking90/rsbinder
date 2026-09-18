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
- A type argument on a type that takes none (`String<int>`, `IBinder<T>`).
- A `const` whose type is not a primitive, a `String`, or an array of those.

rsbinder keeps `boolean`, `char` and array constants, which AOSP refuses. Then move on to [Service Development](./service-development.md) for the runtime patterns that put these types to work.
