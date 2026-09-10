# AIDL Guide

AIDL (Android Interface Definition Language) is the contract between a Binder service and its clients. You describe the interface — its methods, parameters, and data types — in a `.aidl` file, and the `rsbinder-aidl` compiler generates the Rust code on both sides. This guide walks through the parts of AIDL that you will actually use when writing services with rsbinder.

If you have not yet seen rsbinder running end-to-end, read [Hello, World!](./hello-world.md) first — it shows where AIDL fits into a complete project.

> With rsbinder on both ends, [Interface Macros](./interface-macros.md) declares the same interface as a Rust trait instead — same compiler, same generated code. Read this guide either way: it describes the type system both paths share.

## Chapters

- **[Data Types](./aidl-data-types.md)** — Primitive, string, array, list, map, and interface types, and how each one maps to Rust on the `in` / `out` / `inout` side. Start here.
- **[Parcelable](./aidl-parcelable.md)** — Defining user-supplied structs that can cross the Binder boundary, including nullable fields and default values.
- **[Enum and Union](./aidl-enum-union.md)** — Backed enums (newtype structs in Rust, for wire-stable forward compatibility) and unions (tagged variants).
- **[Annotations](./aidl-annotations.md)** — `@RustDerive`, `@nullable`, `@Backing`, `@JavaDerive`-equivalents, and the other annotations the Rust backend honors.

Read them in that order the first time; *Annotations* is a reference to come back to when the generated Rust does not look the way you expected. Then move on to [Service Development](./service-development.md) for the runtime patterns that put these types to work.
