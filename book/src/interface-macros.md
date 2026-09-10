# Interface Macros (no `.aidl`)

`#[rsbinder::interface]` declares a Binder interface as a Rust trait — no
`.aidl` file, no `build.rs`, no generated-code directory:

```toml
rsbinder = { version = "0.11", features = ["macros"] }
```

```rust
use rsbinder::{interface, BinderResult, Strong};

#[interface(descriptor = "com.example.IHello")]
pub trait IHello {
    fn echo(&self, msg: &str) -> BinderResult<String>;
    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()>;   // `&mut` = out
    #[oneway]
    fn ping(&self) -> BinderResult<()>;
}

// `Hello` is your `impl IHello`, plus `impl Interface for Hello {}`.
rsbinder::serve("binder://")?.add("hello", BnHello::new_binder(Hello))?.run()?;
let hello: Strong<dyn IHello> = rsbinder::connect("binder://hello")?;
```

`BnHello`, `BpHello` and `IHelloDefault` appear where you wrote the trait, so
the service impl, the registration and the call sites read exactly as they do
on the `.aidl` path.

The macro is not a second code generator: it fills the same
`rsbinder_aidl::render` structs the AIDL front-end fills and runs the same
templates. A trait here and the equivalent `.aidl` produce **byte-for-byte
identical** code, enforced by golden tests. Moving between the two never
touches a call site and never changes a byte on the wire.

The feature is off by default, because it pulls the AIDL compiler in as a
proc-macro dependency — a build-time cost that a crate consuming generated
code has no reason to carry.

## Which path to use

| Use | When |
|---|---|
| `#[rsbinder::interface]` | rsbinder on both ends, wire private to both |
| `.aidl` | An Android service or client, another language on the other end, or a published interface |

`.aidl` is also the only path that carries unions, interface constants,
`@VintfStability`, `@EnforcePermission`, `ParcelableHolder` fields, generics
and nested types. The macro has no syntax for those; where a Rust declaration
could be mistaken for one, it is a compile error naming the reason.

## What a signature means

| Signature | Meaning |
|---|---|
| `x: T` (must be `Copy`), `x: &T`, `x: &str`, `x: &[T]` | `in` argument |
| `x: &mut T` | **`out`** — the server fills the caller's value |
| `#[inout] x: &mut T` | written **and** read back |
| `Option<T>` | `@nullable` |
| `#[oneway]` on a method | no reply; must return `BinderResult<()>` |

What the wire cannot carry is a compile error where you wrote it:

- A primitive cannot be `out` — `.aidl` passes it only `in`. Return it, or use
  a `Vec<T>` or a parcelable.
- A primitive has no null form, so `Option<i32>` is refused; so is
  `Option<Mode>` for a derived enum, which travels as its `repr` scalar.
- A borrowed type nested inside another (`&[&str]`) has nothing to borrow from
  once decoded. Use the owned form.

Methods are declared `fn`, not `async fn`. The `async` feature emits the
`IFooAsync` halves from the same declaration.

## Data types

```rust
use rsbinder::{BinderEnum, Parcelable};

#[derive(Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "com.example.Config")]
pub struct Config {
    pub name: String,
    pub retries: i32,
    pub extra: Option<Vec<u8>>,
}

#[derive(BinderEnum, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Mode {
    #[default]
    Fast = 0,
    Safe = 1,
}
```

`#[derive(Parcelable)]` emits only the codec, leaving `Default`, `Debug` and
the rest to the usual derives. `Default` is required: decoding starts from it,
which is how a field an older writer never wrote gets a value. Named fields
only — field order is wire order, so **append only**, as in `.aidl`.

`#[derive(BinderEnum)]` needs `#[repr(i8|i32|i64)]`, the AIDL backing type that
goes on the wire, and an explicit value per variant so the wire value is
visible at the declaration.

> **A derived enum is closed.** An undeclared value decodes as `BadValue`,
> where an `.aidl` enum carries it through. For a peer that may send a value
> your build has never heard of, use `.aidl` or `declare_binder_enum!`.

Either type works with [`to_bytes` / `from_bytes`](./data-serialization.md).

## Rules that bite

**Reordering methods is a wire break.** Transaction codes follow declaration
order (`FIRST_CALL_TRANSACTION + i`), as in `.aidl` without explicit codes.
Append at the end.

**Set the descriptor for anything shared.** It defaults to the bare trait name,
which is fine inside one crate and not across two.

**Spell out-parameter types directly.** A proc macro cannot see through a type
alias, so `&mut Ids` for `type Ids = Vec<i32>` reads as an opaque named type
and loses the length word `.aidl` writes for an out vector. Path qualification
(`std::vec::Vec<i32>`) is matched structurally and is fine; an alias is not.

**A bare path in a signature is your own, `super::` is not.** The generated
body lands in a module one level below where you wrote the macro and reaches
your scope through `use super::*;`, so `super::X` names *that* module — a
signature meaning the parent has to say `crate::X`.
