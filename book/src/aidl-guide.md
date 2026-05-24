# AIDL Guide

AIDL (Android Interface Definition Language) is the contract between a Binder service and its clients. You describe the interface — its methods, parameters, and data types — in a `.aidl` file, and the `rsbinder-aidl` compiler generates the Rust code on both sides. This guide walks through the parts of AIDL that you will actually use when writing services with rsbinder.

If you have not yet seen rsbinder running end-to-end, read [Hello, World!](./hello-world.md) first — it shows where AIDL fits into a complete project.

## Chapters

- **[Data Types](./aidl-data-types.md)** — Primitive, string, array, list, map, and interface types, and how each one maps to Rust on the `in` / `out` / `inout` side. Start here.
- **[Parcelable](./aidl-parcelable.md)** — Defining user-supplied structs that can cross the Binder boundary, including nullable fields and default values.
- **[Enum and Union](./aidl-enum-union.md)** — Backed enums (newtype structs in Rust, for wire-stable forward compatibility) and unions (tagged variants).
- **[Annotations](./aidl-annotations.md)** — `@RustDerive`, `@nullable`, `@Backing`, `@JavaDerive`-equivalents, and the other annotations the Rust backend honors.

## How to use this guide

The chapters are roughly ordered by how often you need each topic when building a new service:

1. Skim **Data Types** so you know what AIDL primitives translate to in Rust.
2. Read **Parcelable** when you need to pass structured data.
3. Reach for **Enum and Union** when designing variant payloads or constant sets.
4. Consult **Annotations** as a reference whenever the generated Rust does not look the way you expect.

Once you are comfortable with the AIDL surface, move on to [Service Development](./service-development.md) for the runtime patterns that put these types to work.
