# rsbinder-macros

`#[rsbinder::interface]` — declare a Binder interface as a Rust trait, with no
`.aidl` file and no `build.rs`.

```rust
use rsbinder::{interface, BinderResult, Strong};

#[interface(descriptor = "com.example.IHello")]
pub trait IHello {
    fn echo(&self, msg: &str) -> BinderResult<String>;
    fn fill(&self, out: &mut Vec<i32>) -> BinderResult<()>;   // `&mut` = out
    #[oneway]
    fn ping(&self) -> BinderResult<()>;
}

rsbinder::serve("binder://")?.add("hello", BnHello::new_binder(Impl))?.run()?;
let hello: Strong<dyn IHello> = rsbinder::connect("binder://hello")?;
```

This is for **rsbinder ↔ rsbinder** interfaces, where the wire is private to
both ends. `.aidl` stays the canonical path — it is what Android tooling reads
and what other language backends generate from — and anything needing AIDL
semantics (unions, constants, `@VintfStability`, `@EnforcePermission`,
`ParcelableHolder`, generics, nested types) still belongs there.

The macro is not a second code generator. It fills the same
`rsbinder_aidl::render` structs the AIDL front-end fills and runs the same
templates, so a trait here and the equivalent `.aidl` produce **identical**
generated code — moving from one to the other never touches a call site. That
equality is enforced by golden tests in this crate.

Enable it through `rsbinder`:

```toml
rsbinder = { version = "0.11", features = ["macros"] }
```

Licensed under the Apache License, Version 2.0.
