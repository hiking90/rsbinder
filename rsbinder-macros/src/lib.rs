// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Define a Binder interface as a Rust trait — no `.aidl`, no `build.rs`.
//!
//! ```ignore
//! use rsbinder::{interface, BinderResult, Strong};
//!
//! #[interface(descriptor = "com.example.IHello")]
//! pub trait IHello {
//!     fn echo(&self, msg: &str) -> BinderResult<String>;
//!     fn fill(&self, out: &mut Vec<i32>) -> BinderResult<()>;   // `&mut` = out
//!     #[oneway] fn ping(&self) -> BinderResult<()>;
//! }
//!
//! rsbinder::serve("binder://")?.add("hello", BnHello::new_binder(Impl))?.run()?;
//! let h: Strong<dyn IHello> = rsbinder::connect("binder://hello")?;
//! ```
//!
//! # Relationship to `.aidl`
//!
//! `.aidl` stays the canonical path (plan 2-17 D3): it is what Android tooling
//! reads, what other languages generate from, and the only path that carries
//! AIDL-only semantics. This macro is for **rsbinder ↔ rsbinder** interfaces,
//! where the wire is private to both ends.
//!
//! It is not a re-implementation. The macro fills the very same
//! [`rsbinder_aidl::render`] structs the AIDL front-end fills and runs the same
//! templates, so a trait here and the equivalent `.aidl` produce **identical**
//! generated code — moving from one to the other never touches a call site.
//! What it cannot express (unions, constants, `@VintfStability`,
//! `@EnforcePermission`, `ParcelableHolder`, generics, nested types) is what
//! needs AIDL semantics; write those in `.aidl`.
//!
//! # What the signature means
//!
//! | Signature | Meaning |
//! |---|---|
//! | `x: T` (must be `Copy`), `x: &T`, `x: &str`, `x: &[T]` | in argument |
//! | `x: &mut T` | **out** argument |
//! | `#[inout] x: &mut T` | written **and** read back |
//! | `#[nonnull] x: &mut Option<T>` | an `out` binder or fd that is not `@nullable` |
//! | `#[deprecated]` / `#[deprecated = "…"]` | AIDL's `@deprecated`, on a trait or a method |
//! | `Option<T>` | nullable |
//! | `#[oneway]` on a method | no reply; must return `BinderResult<()>` |
//!
//! `Option<T>` is AIDL's `@nullable`, which AIDL allows only on the types that
//! have a null representation on the wire. A `#[derive(BinderEnum)]` enum is
//! carried as its `repr` scalar and is not one of them: `Option<Mode>` fails to
//! compile inside the generated code, because the enum has no `SerializeOption`
//! — the same shape `.aidl` rejects at the AIDL level. A primitive has no null
//! either, so `Option<i32>` is refused outright, wherever it sits.
//!
//! **Not everything can be an out parameter.** AIDL passes a primitive, a
//! `String` and an enum `in` only. The first two are refused as `&mut` —
//! `&mut String` even as `&mut Option<String>`, because `@nullable` does not
//! widen the direction — but an enum is not: the macro sees only its name and
//! cannot tell `&mut Mode` from a parcelable's `&mut Config`, which is a legal
//! out parameter. Never take an enum by `&mut`; no `.aidl` can express it.
//!
//! A binder object (`Strong<dyn IFoo>`, `SIBinder`) and a
//! `ParcelFileDescriptor` have no `Default` for the callee to start from, so
//! `.aidl` renders an `out` one as `&mut Option<_>` whether or not it is
//! `@nullable`, and a non-nullable `#[inout]` one bare. Here `&mut Option<_>`
//! is the `@nullable` form, as `Option<T>` is everywhere else: a `None` the
//! service leaves behind goes back as null. The exception is an `out` fd
//! array: in `&mut Vec<Option<ParcelFileDescriptor>>` /
//! `&mut [Option<ParcelFileDescriptor>; N]` the element `Option` is `.aidl`'s
//! own, and one left `None` fails the call with `UNEXPECTED_NULL`;
//! `&mut Option<Vec<Option<_>>>` is the `@nullable` form. For the other
//! `.aidl` form of the scalar — a non-nullable `out IFoo`, whose server
//! answers a `None` the service left behind with `UNEXPECTED_NULL` rather than
//! writing null — mark the parameter `#[nonnull]`. That is the only place the
//! attribute applies; everywhere else the spelling already says which form it
//! is. A `ParcelableHolder` is a parcelable field type only and cannot appear
//! in a signature at all.
//!
//! **An out vector is an array, never a `List`.** An out `&mut Vec<T>` is
//! `.aidl`'s `out T[]`, and `&mut Option<Vec<T>>` its `out @nullable T[]`: the
//! proxy sends the caller's current length (null for `None`) and the service
//! starts from a vector pre-sized to it. `.aidl`'s `out List<T>` renders the
//! very same signature but sends no length and starts the service from an
//! empty vector (`None` when `@nullable`), so the two are not wire-compatible.
//! `out List<T>` has no spelling here; it needs `.aidl`.
//!
//! **Paths resolve inside the generated module.** The body lands in a
//! `{Trait}_binder` module one level below where the macro was written, and it
//! reaches the surrounding scope through a `use super::*;`. A bare name is
//! therefore the user's own — but `super::X` names the module the macro was
//! written in, not its parent, so a signature that means the parent has to say
//! `crate::X`. `self::` is rejected outright for the same reason; `super::` is
//! left legal because it is the only way to name a type the generated module
//! shadows (`BnFoo`, `BpFoo`, `transactions`, the trait's own name).
//!
//! **Spell types directly.** A proc macro cannot see through a type alias or
//! a `use … as` rename, and every check above reads the spelling. `&mut Ids`
//! for `type Ids = Vec<i32>` reads as an opaque named type: it loses the
//! length word `.aidl` writes for an out array (`T[]`), and an aliased
//! `ParcelFileDescriptor` element loses the null guard that keeps a null fd
//! off the wire. Likewise a renamed `ParcelableHolder` or an alias of
//! `Option<i32>` slips past the check that refuses it and generates the code
//! that check exists to prevent, and a renamed `Strong` or
//! `ParcelFileDescriptor` escapes the out-parameter rule above. Path
//! qualification is fine — `std::vec::Vec<i32>` is matched structurally — but
//! an alias or a rename is not.
//!
//! A doc comment on a method documents the declaration, not the generated
//! trait: the render layer carries no doc field, so rustdoc for the emitted
//! `IFoo` comes from the crate-level docs, not from here.
//!
//! # What it emits
//!
//! The generated items — the trait, `BnFoo`, `BpFoo`, `IFooDefault`, and with
//! the `async` feature the `IFooAsync` halves — land in the module where the
//! macro was written, so they are usable without a path. They physically live
//! in a `#[doc(hidden)] mod {Trait}_binder` that is glob re-exported: the
//! generated body reuses `.aidl`'s fixed internal names (`transactions`,
//! `on_transact`, `DEFAULT_IMPL`), which would collide if two interfaces were
//! emitted side by side. That module is an implementation detail — never name
//! it.
//!
//! Transaction codes are assigned by declaration order
//! (`FIRST_CALL_TRANSACTION + i`), exactly as `.aidl` does without explicit
//! codes — so **reordering methods is a wire break**. The descriptor defaults
//! to the bare trait name; pass `descriptor = "…"` for anything shared across
//! crates.
//!
//! The generated code names `rsbinder::` directly, so the dependency has to
//! keep that name — a `package = "rsbinder"` rename will not resolve.

use proc_macro::TokenStream;
use quote::quote;
use rsbinder_aidl::render::{render_interface, FnMembers, InterfaceRender, TransactionWrite};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    FnArg, ItemTrait, Pat, PathArguments, ReturnType, Token, TraitItem, TraitItemFn, Type,
};

mod binder_enum;
mod parcelable;
mod type_str;

/// Argument direction, read off the Rust signature.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    In,
    Out,
    Inout,
}

/// `#[interface(descriptor = "…")]`.
struct Args {
    descriptor: Option<String>,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut descriptor = None;
        if input.is_empty() {
            return Ok(Self { descriptor });
        }
        let metas = Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            if meta.path.is_ident("descriptor") {
                if descriptor.is_some() {
                    return Err(syn::Error::new_spanned(
                        &meta.path,
                        "`descriptor` is given more than once",
                    ));
                }
                let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &meta.value
                else {
                    return Err(syn::Error::new_spanned(
                        &meta.value,
                        "descriptor must be a string literal",
                    ));
                };
                // Spliced unescaped into a generated literal; `.aidl`'s grammar never admits these.
                let value = s.value();
                if let Some(bad) = value.chars().find(|c| "\\\"\n\r".contains(*c)) {
                    return Err(syn::Error::new_spanned(
                        s,
                        format!(
                            "a descriptor cannot contain {bad:?} — it is written verbatim \
                             into the generated source and onto the wire"
                        ),
                    ));
                }
                descriptor = Some(value);
            } else {
                return Err(syn::Error::new_spanned(
                    &meta.path,
                    "unknown argument; only `descriptor = \"…\"` is supported",
                ));
            }
        }
        Ok(Self { descriptor })
    }
}

/// `#[<name>(descriptor = "…")]` on a derive input.
fn attr_descriptor(attrs: &[syn::Attribute], name: &str) -> syn::Result<Option<String>> {
    let mut found = None;
    for attr in attrs {
        if !attr.path().is_ident(name) {
            continue;
        }
        let args: Args = attr.parse_args()?;
        if args.descriptor.is_some() {
            if found.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!("`descriptor` is given more than once; #[{name}(..)] appears twice"),
                ));
            }
            found = args.descriptor;
        }
    }
    Ok(found)
}

/// Parcel codec for a plain Rust struct — the `.aidl`-free `parcelable`.
///
/// Fields are the wire, in declaration order, so **reordering or inserting a
/// field is a wire break**. The emitted `write_to_parcel` / `read_from_parcel`
/// are the same ones `rsbinder-aidl` emits for the equivalent `parcelable`,
/// including the size-prefixed header and the truncated-read handling that
/// lets an older reader accept a newer writer's extra fields.
///
/// Only the codec is generated, so `Debug`, `Clone` and friends stay yours to
/// derive — but **`Default` is required**: reading a parcelable that arrives
/// as `null` builds one from it. `#[derive(Parcelable, Default, Debug)]` is
/// the normal shape. The descriptor defaults to the type name and is
/// overridden with `#[parcelable(descriptor = "…")]`. `#[deprecated]` and
/// `#[deprecated = "…"]` carry through as AIDL's `@deprecated`, on the struct
/// and on a field alike; the richer Rust forms carry fields `.aidl` has
/// nowhere to put and are refused.
///
/// Fields must be named and owned — owned all the way down, so `Option<&str>`
/// and `Vec<&str>` are out too. A nullable primitive (`Option<i32>`) is
/// refused for the same reason the interface path refuses it: `.aidl` has no
/// `@nullable int`. `ParcelableHolder` and non-nullable binder or fd fields
/// (`Strong<dyn IFoo>`, `SIBinder`, `ParcelFileDescriptor`) are `.aidl`-only
/// shapes: here `Option<_>` on any of them is AIDL's `@nullable`, even though
/// `.aidl` spells the non-nullable field the same way.
///
/// These checks read the field's spelling — a proc macro cannot see through a
/// type alias or a `use … as` rename. A `ParcelableHolder` imported under
/// another name, or a `type MaybeInt = Option<i32>`, is not caught and
/// compiles into the very codec the check exists to refuse; spell them
/// directly.
#[proc_macro_derive(Parcelable, attributes(parcelable))]
pub fn derive_parcelable(item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::DeriveInput);
    match parcelable::expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Parcel codec for a plain Rust enum, carried as its `#[repr(..)]` scalar.
///
/// `#[repr(i8)]`, `#[repr(i32)]` or `#[repr(i64)]` is required — it is the
/// wire format (AIDL `byte`, `int`, `long`) — and every variant needs an
/// explicit value, so what goes on the wire is visible at the declaration
/// instead of implied by declaration order.
///
/// ```ignore
/// #[derive(BinderEnum, Clone, Copy, PartialEq, Eq, Debug)]
/// #[repr(i32)]
/// pub enum Mode { Fast = 0, Safe = 1 }
/// ```
///
/// Nothing beyond the `repr` is required of the type — the codec matches on
/// the variant rather than casting through a shared reference.
///
/// The enum is not nullable: `Option<Mode>` has no wire form here, matching
/// `.aidl`, where `@nullable` on an enum is rejected outright.
///
/// **This enum is closed.** A value no variant declares deserializes to
/// `rsbinder::StatusCode::BadValue`. An `.aidl` enum is open: its generated
/// newtype keeps whatever a newer peer sent, so a reader can pass an unknown
/// value along untouched. Use `.aidl` — or `rsbinder::declare_binder_enum!`,
/// which emits exactly that newtype — when the two ends may be different
/// versions. Within one build of both ends, a real Rust enum is the better
/// type: it matches exhaustively and cannot hold a value you never defined.
#[proc_macro_derive(BinderEnum)]
pub fn derive_binder_enum(item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::DeriveInput);
    match binder_enum::expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as Args);
    let item_trait = syn::parse_macro_input!(item as ItemTrait);
    match expand(&args, &item_trait) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(args: &Args, item: &ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let rendered = render_source(args, item)?;

    let file = syn::parse_file(&rendered).map_err(|e| {
        syn::Error::new_spanned(
            &item.ident,
            format!("generated code did not parse ({e}); generated source follows:\n{rendered}"),
        )
    })?;
    let mut items = file.items.into_iter();
    let (Some(syn::Item::Mod(mut module)), None) = (items.next(), items.next()) else {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "generated code was not a single module (generator contract changed)",
        ));
    };

    // Renamed: a module named like the trait would shadow the trait's glob re-export.
    let raw_name = item.ident.to_string();
    let mod_ident = syn::Ident::new(
        &format!("{}_binder", strip_raw(&raw_name)),
        item.ident.span(),
    );
    module.ident = mod_ident.clone();
    // A `pub` module would make a private trait nameable from outside.
    module.vis = item.vis.clone();

    // The proxy passes a by-value argument twice, so assert `Copy` where the body resolves it.
    let copied = by_value_types(item)?;
    if let Some((_, items)) = module.content.as_mut() {
        items.insert(
            0,
            syn::parse_quote!(
                use super::*;
            ),
        );
        for ty in &copied {
            items.push(syn::parse_quote!(
                const _: fn() = || {
                    fn __rsbinder_assert_copy<T: ::core::marker::Copy>() {}
                    __rsbinder_assert_copy::<#ty>();
                };
            ));
        }
    }

    let vis = &item.vis;
    Ok(quote! {
        #[doc(hidden)]
        #module
        #vis use #mod_ident::*;
    })
}

/// `Ident`'s `Display` keeps the `r#`, and the templates add their own.
pub(crate) fn strip_raw(ident: &str) -> &str {
    ident.strip_prefix("r#").unwrap_or(ident)
}

/// The generated module source, split out for the golden test (plan 2-19 D1).
fn render_source(args: &Args, item: &ItemTrait) -> syn::Result<String> {
    render_source_with(args, item, cfg!(feature = "async"))
}

fn render_source_with(args: &Args, item: &ItemTrait, enabled_async: bool) -> syn::Result<String> {
    // A trait-level `#[cfg]` or `#[oneway]` would vanish in the re-render.
    check_attrs(&item.attrs, &["deprecated"])?;
    // The render layer carries no trait modifier, so an accepted one would vanish.
    if let Some(unsafety) = &item.unsafety {
        return Err(syn::Error::new_spanned(
            unsafety,
            "an `unsafe` binder interface is not supported — the generated trait is safe, \
             so the obligation would be silently dropped",
        ));
    }
    if let Some(auto_token) = &item.modifiers.auto_token {
        return Err(syn::Error::new_spanned(
            auto_token,
            "an `auto` trait carries no methods and cannot be a binder interface",
        ));
    }
    if let Some(where_clause) = &item.generics.where_clause {
        return Err(syn::Error::new_spanned(
            where_clause,
            "a where clause has no meaning on a binder interface — the generated trait \
             requires only `rsbinder::Interface + Send`",
        ));
    }
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "a binder interface cannot be generic — the wire has no way to carry the parameter",
        ));
    }
    if !item.supertraits.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.supertraits,
            "supertraits are not supported; the generated trait already requires \
             `rsbinder::Interface + Send`",
        ));
    }

    let raw = item.ident.to_string();
    let name = strip_raw(&raw).to_string();
    let mut fn_members = Vec::new();
    // `r#type` and `type` are one name.
    let mut seen = std::collections::HashSet::new();
    for (i, trait_item) in item.items.iter().enumerate() {
        let TraitItem::Fn(f) = trait_item else {
            return Err(syn::Error::new_spanned(
                trait_item,
                "only methods are supported; constants and associated types need `.aidl`",
            ));
        };
        let ident = f.sig.ident.to_string();
        if !seen.insert(strip_raw(&ident).to_string()) {
            return Err(syn::Error::new_spanned(
                &f.sig.ident,
                format!("duplicate method name `{}`", strip_raw(&ident)),
            ));
        }
        fn_members.push(make_fn_member(f, i as u32)?);
    }

    let descriptor = args.descriptor.clone().unwrap_or_else(|| name.clone());
    let mut render = InterfaceRender::new(name, descriptor);
    render.fn_members = fn_members;
    render.enabled_async = enabled_async;
    render.deprecated = deprecated_of(&item.attrs)?;

    render_interface(&render)
        .map(|s| s.trim().to_string())
        .map_err(|e| syn::Error::new_spanned(&item.ident, format!("codegen failed: {e}")))
}

fn make_fn_member(f: &TraitItemFn, index: u32) -> syn::Result<FnMembers> {
    if f.default.is_some() {
        return Err(syn::Error::new_spanned(
            &f.sig.ident,
            "a default body has no meaning on a binder interface — the server `impl` provides it",
        ));
    }
    if !f.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &f.sig.generics,
            "generic methods are not supported",
        ));
    }
    if let Some(where_clause) = &f.sig.generics.where_clause {
        return Err(syn::Error::new_spanned(
            where_clause,
            "a where clause on a binder method is not supported",
        ));
    }
    // The re-render drops these, so the user's `impl` would miss its own signature.
    if let Some(tok) = &f.sig.asyncness {
        return Err(syn::Error::new_spanned(
            tok,
            "declare the method as sync; the `async` feature emits the `IFooAsync` halves \
             alongside it",
        ));
    }
    // `Safety::Safe` parses only inside an `extern` block.
    if !matches!(f.sig.safety, syn::Safety::Default) {
        return Err(syn::Error::new_spanned(
            &f.sig.safety,
            "an `unsafe` binder method is not supported",
        ));
    }
    if let Some(tok) = &f.sig.constness {
        return Err(syn::Error::new_spanned(
            tok,
            "a `const` binder method is not supported",
        ));
    }
    if let Some(abi) = &f.sig.abi {
        return Err(syn::Error::new_spanned(
            abi,
            "an explicit ABI has no meaning on a binder method",
        ));
    }
    // `syn` parses `...` even outside an `extern` block; the render layer would drop it.
    if let Some(variadic) = &f.sig.variadic {
        return Err(syn::Error::new_spanned(
            variadic,
            "a variadic binder method is not supported — the wire carries a fixed \
             argument list",
        ));
    }
    let oneway = has_attr(&f.attrs, "oneway");
    check_attrs(&f.attrs, &["oneway", "deprecated"])?;

    const SHARED_RECEIVER: &str = "the first parameter must be `&self` — a binder object is \
                                   shared, never owned or uniquely borrowed by a call";

    let mut inputs = f.sig.inputs.iter();
    let Some(FnArg::Receiver(receiver)) = inputs.next() else {
        return Err(syn::Error::new_spanned(&f.sig, SHARED_RECEIVER));
    };
    let syn::ReceiverKind::Reference(_, lifetime, None) = &receiver.kind else {
        return Err(syn::Error::new_spanned(&f.sig, SHARED_RECEIVER));
    };
    // `&'_ self` is `&self`; a named one would be dropped by the re-render.
    if let Some(lifetime) = lifetime.as_ref().filter(|lt| lt.ident != "_") {
        return Err(syn::Error::new_spanned(
            lifetime,
            "an explicit lifetime on `self` is not supported — the generated trait takes a \
             plain `&self`",
        ));
    }
    check_attrs(&receiver.attrs, &[])?;

    let mut args = "&self".to_string();
    let mut args_async = "&'a self".to_string();
    let mut func_call_params = String::new();
    let mut write_funcs = Vec::new();
    let mut transaction_decls = Vec::new();
    let mut transaction_write = Vec::new();
    let mut transaction_params = String::new();
    let mut read_onto_params = Vec::new();
    let mut arg_names = std::collections::HashSet::new();

    for input in inputs {
        let FnArg::Typed(pat_ty) = input else {
            return Err(syn::Error::new_spanned(input, "unexpected receiver"));
        };
        let Pat::Ident(pat_ident) = &*pat_ty.pat else {
            return Err(syn::Error::new_spanned(
                &pat_ty.pat,
                "parameter patterns are not supported; use a plain name",
            ));
        };
        // `rsbinder-aidl`'s `_arg_` prefix; trait parameter names do not bind the `impl`.
        check_attrs(&pat_ty.attrs, &["inout", "nonnull"])?;
        let raw_arg = pat_ident.ident.to_string();
        let ident = format!("_arg_{}", strip_raw(&raw_arg));
        // AOSP `AidlMethod::CheckValid`; else E0415 lands on generated tokens.
        if !arg_names.insert(ident.clone()) {
            return Err(syn::Error::new_spanned(
                &pat_ident.ident,
                format!("duplicate argument name `{}`", strip_raw(&raw_arg)),
            ));
        }
        let dir = direction(&pat_ty.attrs, &pat_ty.ty)?;
        if oneway && dir != Dir::In {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "a #[oneway] method cannot have an out/inout parameter — there is no reply to \
                 write it back into",
            ));
        }
        type_str::check_supported(&pat_ty.ty)?;
        type_str::reject_nullable_primitive(&pat_ty.ty)?;
        reject_holder_in_signature(&pat_ty.ty)?;
        let word = match dir {
            Dir::In => "in",
            Dir::Out => "out",
            Dir::Inout => "inout",
        };
        if dir != Dir::In {
            type_str::check_out_capable(&pat_ty.ty, word)?;
        }
        type_str::check_array_elements(&pat_ty.ty, word)?;
        // `out T` and `out @nullable T` share one Rust spelling for a binder or a fd.
        let nonnull = has_attr(&pat_ty.attrs, "nonnull");
        if nonnull && !(dir == Dir::Out && type_str::out_option_is_ambiguous(&pat_ty.ty)) {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "#[nonnull] applies only to an `out` binder object or file descriptor, spelled \
                 `&mut Option<_>` — everywhere else the spelling already says which `.aidl` \
                 form this is",
            ));
        }
        let as_written = type_str::as_written(&pat_ty.ty)?;
        let owned = type_str::owned(&pat_ty.ty)?;

        let arg_str = format!(", {ident}: {as_written}");
        args += &arg_str;
        args_async += &arg_str.replace('&', "&'a ");
        func_call_params += &format!("{ident}, ");

        // Structural, not string: `std::vec::Vec<T>` carries the same length word as `Vec<T>`.
        let is_option_vec = type_str::option_vec_elem(&pat_ty.ty).is_some();
        let is_var_array = type_str::is_variable_array(&pat_ty.ty);
        if dir != Dir::Out {
            let param = if as_written.starts_with('&') {
                ident.clone()
            } else {
                format!("&{ident}")
            };
            write_funcs.push(format!("data.write({param})?;"));
        } else if is_option_vec {
            write_funcs.push(format!("data.write_slice_size({ident}.as_deref())?;"));
        } else if is_var_array {
            // Only the length is sent; the server sizes from it (AOSP `resizeOutVector`).
            write_funcs.push(format!("data.write_slice_size(Some({ident}))?;"));
        }

        let (mutable, init) = match dir {
            Dir::Out => (
                "mut ",
                type_str::out_default(&pat_ty.ty)?.unwrap_or_else(|| "Default::default()".into()),
            ),
            Dir::Inout => ("mut ", "_reader.read()?".to_string()),
            Dir::In => ("", "_reader.read()?".to_string()),
        };
        transaction_decls.push(format!("let {mutable}{ident}: {owned} = {init};"));
        if dir == Dir::Out {
            if is_option_vec {
                transaction_decls.push(format!("_reader.resize_nullable_out_vec(&mut {ident})?;"));
            } else if is_var_array {
                transaction_decls.push(format!("_reader.resize_out_vec(&mut {ident})?;"));
            }
        }

        if dir != Dir::In {
            let mut write = TransactionWrite::new(ident.clone());
            // A `None` fd element would go out as a null fd; `.aidl` raises `UNEXPECTED_NULL`.
            if dir == Dir::Out {
                if let Some(flatten) = type_str::out_pfd_null_guard(&pat_ty.ty) {
                    write.needs_null_guard = true;
                    write.null_guard_flatten = flatten;
                }
            }
            write.needs_unwrap = nonnull;
            transaction_write.push(write);
            read_onto_params.push(ident.clone());
        }
        transaction_params += &format!("{}, ", func_call_param(&ident, &as_written, &owned, dir));
    }

    let return_type = return_type(f)?;
    if oneway && return_type != "()" {
        return Err(syn::Error::new_spanned(
            &f.sig.output,
            "a #[oneway] method cannot return a value — there is no reply to read it from",
        ));
    }

    let raw_ident = f.sig.ident.to_string();
    let mut member = FnMembers::new(strip_raw(&raw_ident), index);
    member.args = args;
    member.args_async = args_async;
    member.transaction_has_return = return_type != "()";
    member.return_type = return_type;
    member.write_funcs = write_funcs;
    member.func_call_params = trim_comma(func_call_params);
    member.transaction_decls = transaction_decls;
    member.transaction_write = transaction_write;
    member.transaction_params = trim_comma(transaction_params);
    member.oneway = oneway;
    member.read_onto_params = read_onto_params;
    member.deprecated = deprecated_of(&f.attrs)?;
    Ok(member)
}

/// Bridge `owned` to `as_written` off the signature alone, so no type list is needed.
fn func_call_param(ident: &str, as_written: &str, owned: &str, dir: Dir) -> String {
    if dir != Dir::In {
        return format!("&mut {ident}");
    }
    if as_written == owned {
        return ident.to_string();
    }
    if as_written == "&str" {
        return format!("{ident}.as_str()");
    }
    if as_written.starts_with('&') {
        return format!("&{ident}");
    }
    // `Option<&str>` / `Option<&[T]>` borrow through, anything else by ref.
    if owned.starts_with("Option<Vec<") || owned.starts_with("Option<String>") {
        format!("{ident}.as_deref()")
    } else if owned.starts_with("Option<") {
        format!("{ident}.as_ref()")
    } else {
        format!("&{ident}")
    }
}

fn direction(attrs: &[syn::Attribute], ty: &Type) -> syn::Result<Dir> {
    let is_mut_ref =
        matches!(type_str::unwrap_group(ty), Type::Reference(r) if r.mutability.is_some());
    if has_attr(attrs, "inout") {
        if !is_mut_ref {
            return Err(syn::Error::new_spanned(
                ty,
                "#[inout] needs `&mut T` — the callee has to be able to write it back",
            ));
        }
        return Ok(Dir::Inout);
    }
    Ok(if is_mut_ref { Dir::Out } else { Dir::In })
}

fn return_type(f: &TraitItemFn) -> syn::Result<String> {
    let ReturnType::Type(_, ty) = &f.sig.output else {
        return Err(syn::Error::new_spanned(
            &f.sig,
            "a binder method must return `BinderResult<T>` — every call can fail on the wire",
        ));
    };
    let Type::Path(p) = type_str::unwrap_group(ty) else {
        return Err(syn::Error::new_spanned(ty, "expected `BinderResult<T>`"));
    };
    let last = p.path.segments.last().expect("non-empty path");
    if last.ident != "BinderResult" {
        return Err(syn::Error::new_spanned(
            ty,
            "expected `BinderResult<T>` (aliased as `rsbinder::BinderResult`)",
        ));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "`BinderResult` needs its success type: `BinderResult<()>` for no return",
        ));
    };
    if args.args.len() != 1 {
        return Err(syn::Error::new_spanned(
            ty,
            "`BinderResult` takes exactly one type argument",
        ));
    }
    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
        return Err(syn::Error::new_spanned(ty, "expected `BinderResult<T>`"));
    };
    // Not `check_supported`: its `Option<&str>` would silently become `Option<String>` here.
    type_str::reject_any_reference(inner)?;
    type_str::reject_nullable_primitive(inner)?;
    reject_holder_in_signature(inner)?;
    type_str::owned(inner)
}

/// A holder's stability is set before the read, which a signature cannot express.
fn reject_holder_in_signature(ty: &Type) -> syn::Result<()> {
    if !type_str::mentions_parcelable_holder(ty) {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        ty,
        "a `ParcelableHolder` cannot appear in a binder signature — `.aidl` refuses it as an \
         argument or return type; carry it as a field of a parcelable instead",
    ))
}

/// Argument types the signature takes by value.
fn by_value_types(item: &ItemTrait) -> syn::Result<Vec<Type>> {
    let mut out = Vec::new();
    for trait_item in &item.items {
        let TraitItem::Fn(f) = trait_item else {
            continue;
        };
        for input in &f.sig.inputs {
            let FnArg::Typed(pat_ty) = input else {
                continue;
            };
            if !type_str::as_written(&pat_ty.ty)?.starts_with('&') {
                out.push((*pat_ty.ty).clone());
            }
        }
    }
    Ok(out)
}

/// `#[deprecated]` / `#[deprecated = "…"]` as `.aidl` renders `@deprecated`.
pub(crate) fn deprecated_of(attrs: &[syn::Attribute]) -> syn::Result<String> {
    let Some(attr) = attrs.iter().find(|a| a.path().is_ident("deprecated")) else {
        return Ok(String::new());
    };
    match &attr.meta {
        syn::Meta::Path(_) => Ok(rsbinder_aidl::render::deprecated_attr(Some(&String::new()))),
        syn::Meta::NameValue(nv) => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "a deprecation note must be a string literal",
                ));
            };
            let note = s.value();
            Ok(rsbinder_aidl::render::deprecated_attr(Some(&note)))
        }
        syn::Meta::List(list) => Err(syn::Error::new_spanned(
            list,
            "`.aidl` carries only `#[deprecated]` and `#[deprecated = \"…\"]`; the other fields \
             of the Rust attribute have no AIDL form and would be dropped",
        )),
    }
}

fn has_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

/// The re-render drops an unrecognised attribute, e.g. a mistyped `#[oneway]` or a `#[cfg]`.
fn check_attrs(attrs: &[syn::Attribute], allowed: &[&str]) -> syn::Result<()> {
    for attr in attrs {
        if attr.path().is_ident("doc") {
            continue;
        }
        if let Some(name) = allowed.iter().find(|a| attr.path().is_ident(a)) {
            // `#[deprecated]` carries its note; `deprecated_of` checks its shape.
            if *name != "deprecated" && !matches!(attr.meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!("#[{name}] takes no arguments"),
                ));
            }
            continue;
        }
        let expected = if allowed.is_empty() {
            "no attribute is supported here".to_string()
        } else {
            format!(
                "#[rsbinder::interface] understands only {} here",
                allowed
                    .iter()
                    .map(|a| format!("#[{a}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(syn::Error::new_spanned(
            attr,
            format!("unsupported attribute; {expected}"),
        ));
    }
    Ok(())
}

fn trim_comma(mut s: String) -> String {
    if s.ends_with(", ") {
        s.truncate(s.len() - 2);
    }
    s
}

#[cfg(test)]
mod golden {
    //! A trait here and the equivalent `.aidl` must render the same module source (plan 2-19 D1).

    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temp directory that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rsbinder_macros_golden_{tag}_{}_{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The interface module `rsbinder-aidl` generates from `aidl`, without the file header.
    fn from_aidl(aidl: &str, module: &str, enabled_async: bool) -> String {
        let dir = TempDir::new(module);
        let src = dir.path().join(format!("{module}.aidl"));
        std::fs::write(&src, aidl).expect("write aidl");

        rsbinder_aidl::Builder::new()
            .source(src)
            .include_dir(dir.path())
            .dest_dir(dir.path())
            .output(PathBuf::from("golden.rs"))
            .set_async_support(enabled_async)
            .generate()
            .expect("aidl generate");

        let text = std::fs::read_to_string(dir.path().join("golden.rs")).expect("read generated");
        extract_module(&text, module)
    }

    /// `pub mod {module} { … }`, dedented: a packaged `.aidl` nests it, the macro does not.
    fn extract_module(text: &str, module: &str) -> String {
        let needle = format!("pub mod {module} {{");
        let start_line = text
            .lines()
            .position(|l| l.trim_start().starts_with(&needle))
            .unwrap_or_else(|| panic!("no `pub mod {module}` in:\n{text}"));
        let lines: Vec<&str> = text.lines().collect();
        let indent = lines[start_line].len() - lines[start_line].trim_start().len();

        let mut depth = 0usize;
        let mut end_line = start_line;
        for (i, line) in lines.iter().enumerate().skip(start_line) {
            depth += line.matches('{').count();
            depth -= line.matches('}').count();
            if depth == 0 {
                end_line = i;
                break;
            }
        }

        lines[start_line..=end_line]
            .iter()
            .map(|l| {
                if l.len() >= indent {
                    &l[indent..]
                } else {
                    l.trim_start()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }

    fn from_macro(tokens: proc_macro2::TokenStream, enabled_async: bool) -> String {
        let item: ItemTrait = syn::parse2(tokens).expect("parse trait");
        render_source_with(&Args { descriptor: None }, &item, enabled_async).expect("render")
    }

    /// Both `enabled_async` settings: a `cfg!` would leave one half unchecked.
    #[track_caller]
    fn assert_same(aidl: &str, module: &str, tokens: proc_macro2::TokenStream) {
        for enabled_async in [false, true] {
            assert_same_for(aidl, module, tokens.clone(), enabled_async);
        }
    }

    #[track_caller]
    fn assert_same_for(
        aidl: &str,
        module: &str,
        tokens: proc_macro2::TokenStream,
        enabled_async: bool,
    ) {
        let expected = from_aidl(aidl, module, enabled_async);
        let actual = from_macro(tokens, enabled_async);
        if expected != actual {
            let mut report = String::new();
            for diff in expected
                .lines()
                .zip(actual.lines())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
            {
                let (i, (a, b)) = diff;
                report.push_str(&format!("line {i}:\n  aidl : {a}\n  macro: {b}\n"));
            }
            if expected.lines().count() != actual.lines().count() {
                report.push_str(&format!(
                    "line count: aidl {} vs macro {}\n",
                    expected.lines().count(),
                    actual.lines().count()
                ));
            }
            panic!("generated code differs (enabled_async = {enabled_async})\n{report}\n--- aidl ---\n{expected}\n--- macro ---\n{actual}");
        }
    }

    /// [`assert_same`] for a multi-file fixture carrying an explicit descriptor.
    #[track_caller]
    fn assert_same_files(
        files: &[(&str, &str)],
        main: &str,
        module: &str,
        descriptor: &str,
        tokens: proc_macro2::TokenStream,
    ) {
        for enabled_async in [false, true] {
            let expected = from_aidl_files(files, main, module, enabled_async);
            let item: ItemTrait = syn::parse2(tokens.clone()).expect("parse trait");
            let actual = render_source_with(
                &Args {
                    descriptor: Some(descriptor.to_string()),
                },
                &item,
                enabled_async,
            )
            .expect("render");
            assert_eq!(
                expected, actual,
                "generated code differs (enabled_async = {enabled_async})"
            );
        }
    }

    /// [`from_aidl`] with sibling files, so a fixture can reference another interface.
    fn from_aidl_files(
        files: &[(&str, &str)],
        main: &str,
        module: &str,
        enabled_async: bool,
    ) -> String {
        let dir = TempDir::new(&format!("multi_{module}"));
        for (rel, body) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().expect("relative path")).expect("mkdir");
            std::fs::write(&path, body).expect("write aidl");
        }
        rsbinder_aidl::Builder::new()
            .source(dir.path().join(main))
            .include_dir(dir.path())
            .dest_dir(dir.path())
            .output(PathBuf::from("golden_multi.rs"))
            .set_async_support(enabled_async)
            .generate()
            .expect("aidl generate");
        let text =
            std::fs::read_to_string(dir.path().join("golden_multi.rs")).expect("read generated");
        extract_module(&text, module)
    }

    #[test]
    fn scalars_and_strings() {
        assert_same(
            r#"
interface IGolden1 {
    String echo(in String msg);
    int add(in int a, in int b);
    boolean flag(in boolean b);
}
"#,
            "IGolden1",
            quote! {
                pub trait IGolden1 {
                    fn echo(&self, msg: &str) -> BinderResult<String>;
                    fn add(&self, a: i32, b: i32) -> BinderResult<i32>;
                    fn flag(&self, b: bool) -> BinderResult<bool>;
                }
            },
        );
    }

    #[test]
    fn oneway_and_void() {
        assert_same(
            r#"
interface IGolden2 {
    oneway void ping();
    void nudge(in long n);
}
"#,
            "IGolden2",
            quote! {
                pub trait IGolden2 {
                    #[oneway]
                    fn ping(&self) -> BinderResult<()>;
                    fn nudge(&self, n: i64) -> BinderResult<()>;
                }
            },
        );
    }

    #[test]
    fn out_and_inout_vectors() {
        assert_same(
            r#"
interface IGolden3 {
    void fill(out int[] values);
    void bump(inout int[] values);
    void take(in int[] values);
}
"#,
            "IGolden3",
            quote! {
                pub trait IGolden3 {
                    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()>;
                    fn bump(&self, #[inout] values: &mut Vec<i32>) -> BinderResult<()>;
                    fn take(&self, values: &[i32]) -> BinderResult<()>;
                }
            },
        );
    }

    /// `out List<T>` renders the same signature without the length word, so pin the array side.
    #[test]
    fn an_out_vec_is_the_array_form_not_the_list_form() {
        assert_same(
            r#"
interface IGolden12 {
    void fill(out String[] values);
    void maybe(out @nullable String[] values);
}
"#,
            "IGolden12",
            quote! {
                pub trait IGolden12 {
                    fn fill(&self, values: &mut Vec<String>) -> BinderResult<()>;
                    fn maybe(&self, values: &mut Option<Vec<Option<String>>>) -> BinderResult<()>;
                }
            },
        );
    }

    #[test]
    fn nullable_arguments_and_returns() {
        assert_same(
            r#"
interface IGolden4 {
    @nullable String maybe(in @nullable String msg, in @nullable byte[] blob);
}
"#,
            "IGolden4",
            quote! {
                pub trait IGolden4 {
                    fn maybe(
                        &self,
                        msg: Option<&str>,
                        blob: Option<&[u8]>,
                    ) -> BinderResult<Option<String>>;
                }
            },
        );
    }

    #[test]
    fn binder_objects() {
        // Cross-file, to pin the `super::Mod::Type` path a packaged import produces.
        assert_same_files(
            &[
                (
                    "com/example/IGolden5.aidl",
                    "package com.example;\nimport com.example.IGolden5Cb;\n\
                     interface IGolden5 {\n    void register(in IGolden5Cb cb);\n\
                         IGolden5Cb fetch();\n}\n",
                ),
                (
                    "com/example/IGolden5Cb.aidl",
                    "package com.example;\ninterface IGolden5Cb {\n    void hit();\n}\n",
                ),
            ],
            "com/example/IGolden5.aidl",
            "IGolden5",
            "com.example.IGolden5",
            quote! {
                pub trait IGolden5 {
                    fn register(
                        &self,
                        cb: &rsbinder::Strong<dyn super::IGolden5Cb::IGolden5Cb>,
                    ) -> BinderResult<()>;
                    fn fetch(
                        &self,
                    ) -> BinderResult<rsbinder::Strong<dyn super::IGolden5Cb::IGolden5Cb>>;
                }
            },
        );
    }

    /// A fixed-size out fd array keeps the guard, one `.flatten()` per extra dimension.
    #[test]
    fn out_fd_arrays_keep_the_null_guard_at_every_arity() {
        assert_same(
            r#"
interface IGolden8 {
    void variable(out ParcelFileDescriptor[] fds);
    void fixed(out ParcelFileDescriptor[3] fds);
    void nested(out ParcelFileDescriptor[2][3] fds);
}
"#,
            "IGolden8",
            quote! {
                pub trait IGolden8 {
                    fn variable(
                        &self,
                        fds: &mut Vec<Option<rsbinder::ParcelFileDescriptor>>,
                    ) -> BinderResult<()>;
                    fn fixed(
                        &self,
                        fds: &mut [Option<rsbinder::ParcelFileDescriptor>; 3],
                    ) -> BinderResult<()>;
                    fn nested(
                        &self,
                        fds: &mut [[Option<rsbinder::ParcelFileDescriptor>; 3]; 2],
                    ) -> BinderResult<()>;
                }
            },
        );
    }

    /// Every out/inout shape `.aidl` renders, in one interface. A `Default`-less
    /// element, a `@nullable` wrapper and a fixed dimension all meet in the out
    /// rules, so a rule that is right for one row is easily wrong for the next.
    ///
    /// The first three are `@nullable` because that is what `&mut Option<T>`
    /// means here: `out IFoo` spells the same Rust type but makes the server
    /// answer `UNEXPECTED_NULL` on `None`, and the macro cannot say which.
    #[test]
    fn every_out_and_inout_shape_matches_aidl() {
        assert_same_files(
            &[
                (
                    "p/IGolden10.aidl",
                    "package p;\nimport p.IGolden10Cb;\nimport p.Golden10Cfg;\n\
                     interface IGolden10 {\n\
                     \x20   void a(out @nullable ParcelFileDescriptor v);\n\
                     \x20   void b(out @nullable IBinder v);\n\
                     \x20   void c(out @nullable IGolden10Cb v);\n\
                     \x20   void d(out Golden10Cfg v);\n\
                     \x20   void e(out int[] v);\n\
                     \x20   void f(out String[] v);\n\
                     \x20   void g(out ParcelFileDescriptor[] v);\n\
                     \x20   void h(out IGolden10Cb[] v);\n\
                     \x20   void i(out Golden10Cfg[] v);\n\
                     \x20   void j(out int[3] v);\n\
                     \x20   void k(out ParcelFileDescriptor[3] v);\n\
                     \x20   void l(out ParcelFileDescriptor[2][3] v);\n\
                     \x20   void m(out Golden10Cfg[3] v);\n\
                     \x20   void n(out @nullable Golden10Cfg v);\n\
                     \x20   void o(out @nullable int[] v);\n\
                     \x20   void p(out @nullable String[] v);\n\
                     \x20   void q(out @nullable ParcelFileDescriptor[] v);\n\
                     \x20   void r(out @nullable int[3] v);\n\
                     \x20   void s(out @nullable Golden10Cfg[3] v);\n\
                     \x20   void t(inout ParcelFileDescriptor v);\n\
                     \x20   void u(inout IBinder v);\n\
                     \x20   void w(inout ParcelFileDescriptor[] v);\n\
                     \x20   void x(inout IGolden10Cb[] v);\n\
                     \x20   void y(inout @nullable Golden10Cfg v);\n\
                     \x20   void z(inout @nullable ParcelFileDescriptor[] v);\n}\n",
                ),
                (
                    "p/IGolden10Cb.aidl",
                    "package p;\ninterface IGolden10Cb {\n    void hit();\n}\n",
                ),
                (
                    "p/Golden10Cfg.aidl",
                    "package p;\nparcelable Golden10Cfg {\n    int a;\n}\n",
                ),
            ],
            "p/IGolden10.aidl",
            "IGolden10",
            "p.IGolden10",
            quote! {
                pub trait IGolden10 {
                    fn a(&self, v: &mut Option<rsbinder::ParcelFileDescriptor>) -> BinderResult<()>;
                    fn b(&self, v: &mut Option<rsbinder::SIBinder>) -> BinderResult<()>;
                    fn c(
                        &self,
                        v: &mut Option<rsbinder::Strong<dyn super::IGolden10Cb::IGolden10Cb>>,
                    ) -> BinderResult<()>;
                    fn d(&self, v: &mut super::Golden10Cfg::Golden10Cfg) -> BinderResult<()>;
                    fn e(&self, v: &mut Vec<i32>) -> BinderResult<()>;
                    fn f(&self, v: &mut Vec<String>) -> BinderResult<()>;
                    fn g(
                        &self,
                        v: &mut Vec<Option<rsbinder::ParcelFileDescriptor>>,
                    ) -> BinderResult<()>;
                    fn h(
                        &self,
                        v: &mut Vec<Option<rsbinder::Strong<dyn super::IGolden10Cb::IGolden10Cb>>>,
                    ) -> BinderResult<()>;
                    fn i(&self, v: &mut Vec<super::Golden10Cfg::Golden10Cfg>) -> BinderResult<()>;
                    fn j(&self, v: &mut [i32; 3]) -> BinderResult<()>;
                    fn k(
                        &self,
                        v: &mut [Option<rsbinder::ParcelFileDescriptor>; 3],
                    ) -> BinderResult<()>;
                    fn l(
                        &self,
                        v: &mut [[Option<rsbinder::ParcelFileDescriptor>; 3]; 2],
                    ) -> BinderResult<()>;
                    fn m(&self, v: &mut [super::Golden10Cfg::Golden10Cfg; 3]) -> BinderResult<()>;
                    fn n(
                        &self,
                        v: &mut Option<super::Golden10Cfg::Golden10Cfg>,
                    ) -> BinderResult<()>;
                    fn o(&self, v: &mut Option<Vec<i32>>) -> BinderResult<()>;
                    fn p(&self, v: &mut Option<Vec<Option<String>>>) -> BinderResult<()>;
                    fn q(
                        &self,
                        v: &mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>,
                    ) -> BinderResult<()>;
                    fn r(&self, v: &mut Option<[i32; 3]>) -> BinderResult<()>;
                    fn s(
                        &self,
                        v: &mut Option<[Option<super::Golden10Cfg::Golden10Cfg>; 3]>,
                    ) -> BinderResult<()>;
                    fn t(
                        &self,
                        #[inout] v: &mut rsbinder::ParcelFileDescriptor,
                    ) -> BinderResult<()>;
                    fn u(&self, #[inout] v: &mut rsbinder::SIBinder) -> BinderResult<()>;
                    fn w(
                        &self,
                        #[inout] v: &mut Vec<rsbinder::ParcelFileDescriptor>,
                    ) -> BinderResult<()>;
                    fn x(
                        &self,
                        #[inout] v: &mut Vec<rsbinder::Strong<dyn super::IGolden10Cb::IGolden10Cb>>,
                    ) -> BinderResult<()>;
                    fn y(
                        &self,
                        #[inout] v: &mut Option<super::Golden10Cfg::Golden10Cfg>,
                    ) -> BinderResult<()>;
                    fn z(
                        &self,
                        #[inout] v: &mut Option<Vec<Option<rsbinder::ParcelFileDescriptor>>>,
                    ) -> BinderResult<()>;
                }
            },
        );
    }

    /// `#[nonnull]` is the other half of the ambiguous spelling: the same
    /// `&mut Option<T>` that means `out @nullable T` without it.
    #[test]
    fn nonnull_out_binders_match_the_non_nullable_aidl() {
        assert_same_files(
            &[
                (
                    "p/IGolden11.aidl",
                    "package p;\nimport p.IGolden11Cb;\n\
                     interface IGolden11 {\n\
                     \x20   void a(out ParcelFileDescriptor v);\n\
                     \x20   void b(out IBinder v);\n\
                     \x20   void c(out IGolden11Cb v);\n}\n",
                ),
                (
                    "p/IGolden11Cb.aidl",
                    "package p;\ninterface IGolden11Cb {\n    void hit();\n}\n",
                ),
            ],
            "p/IGolden11.aidl",
            "IGolden11",
            "p.IGolden11",
            quote! {
                pub trait IGolden11 {
                    fn a(
                        &self,
                        #[nonnull] v: &mut Option<rsbinder::ParcelFileDescriptor>,
                    ) -> BinderResult<()>;
                    fn b(
                        &self,
                        #[nonnull] v: &mut Option<rsbinder::SIBinder>,
                    ) -> BinderResult<()>;
                    fn c(
                        &self,
                        #[nonnull] v: &mut Option<
                            rsbinder::Strong<dyn super::IGolden11Cb::IGolden11Cb>,
                        >,
                    ) -> BinderResult<()>;
                }
            },
        );
    }

    /// Anywhere else the spelling already says which `.aidl` form it is, so the
    /// attribute would be a second, silent source of truth.
    #[test]
    fn rejects_nonnull_where_the_spelling_is_not_ambiguous() {
        for decl in [
            quote!(
                fn go(&self, #[nonnull] v: &mut Option<Cfg>) -> BinderResult<()>;
            ),
            quote!(
                fn go(
                    &self,
                    #[nonnull] v: &mut Vec<Option<rsbinder::ParcelFileDescriptor>>,
                ) -> BinderResult<()>;
            ),
            quote!(
                fn go(
                    &self,
                    #[nonnull]
                    #[inout]
                    v: &mut Option<rsbinder::SIBinder>,
                ) -> BinderResult<()>;
            ),
            quote!(
                fn go(&self, #[nonnull] v: Option<&rsbinder::SIBinder>) -> BinderResult<()>;
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    #decl
                }
            });
            assert!(err.contains("#[nonnull] applies only"), "{err}");
        }
    }

    /// `@deprecated` reaches the trait and the method alike, and renders the
    /// same attribute `.aidl` renders.
    #[test]
    fn deprecation_matches_aidl() {
        assert_same(
            r#"
/**
 * @deprecated use IGolden12Next
 */
interface IGolden12 {
    /**
     * @deprecated use b
     */
    void a();
    void b();
}
"#,
            "IGolden12",
            quote! {
                #[deprecated = "use IGolden12Next"]
                pub trait IGolden12 {
                    #[deprecated = "use b"]
                    fn a(&self) -> BinderResult<()>;
                    fn b(&self) -> BinderResult<()>;
                }
            },
        );
    }

    /// The richer Rust forms carry fields `.aidl` has nowhere to put.
    #[test]
    fn rejects_a_deprecation_the_wire_cannot_carry() {
        let err = reject(quote! {
            #[deprecated(since = "1.0", note = "use b")]
            pub trait IBad {
                fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("no AIDL form"), "{err}");
    }

    /// Every `in` array shape `.aidl` renders. The element `Option` follows a
    /// different rule here than for `out`, so the two tables are both needed.
    #[test]
    fn every_in_array_shape_matches_aidl() {
        assert_same_files(
            &[
                (
                    "p/IGolden14.aidl",
                    "package p;\nimport p.IGolden14Cb;\nimport p.Golden14Cfg;\n\
                     interface IGolden14 {\n\
                     \x20   void a(in int[] v);\n\
                     \x20   void b(in @nullable int[] v);\n\
                     \x20   void c(in String[] v);\n\
                     \x20   void d(in @nullable String[] v);\n\
                     \x20   void e(in Golden14Cfg[] v);\n\
                     \x20   void f(in @nullable Golden14Cfg[] v);\n\
                     \x20   void g(in ParcelFileDescriptor[] v);\n\
                     \x20   void h(in @nullable ParcelFileDescriptor[] v);\n\
                     \x20   void i(in IGolden14Cb[] v);\n\
                     \x20   void j(in @nullable IGolden14Cb[] v);\n\
                     \x20   void k(in IBinder[] v);\n\
                     \x20   void l(in @nullable IBinder[] v);\n\
                     \x20   void m(in Golden14Cfg[3] v);\n\
                     \x20   void n(in @nullable Golden14Cfg[3] v);\n\
                     \x20   void o(in ParcelFileDescriptor[3] v);\n\
                     \x20   void p(in @nullable ParcelFileDescriptor[3] v);\n}\n",
                ),
                (
                    "p/IGolden14Cb.aidl",
                    "package p;\ninterface IGolden14Cb {\n    void hit();\n}\n",
                ),
                (
                    "p/Golden14Cfg.aidl",
                    "package p;\nparcelable Golden14Cfg {\n    int a;\n}\n",
                ),
            ],
            "p/IGolden14.aidl",
            "IGolden14",
            "p.IGolden14",
            quote! {
                pub trait IGolden14 {
                    fn a(&self, v: &[i32]) -> BinderResult<()>;
                    fn b(&self, v: Option<&[i32]>) -> BinderResult<()>;
                    fn c(&self, v: &[String]) -> BinderResult<()>;
                    fn d(&self, v: Option<&[Option<String>]>) -> BinderResult<()>;
                    fn e(&self, v: &[super::Golden14Cfg::Golden14Cfg]) -> BinderResult<()>;
                    fn f(
                        &self,
                        v: Option<&[Option<super::Golden14Cfg::Golden14Cfg>]>,
                    ) -> BinderResult<()>;
                    fn g(&self, v: &[rsbinder::ParcelFileDescriptor]) -> BinderResult<()>;
                    fn h(
                        &self,
                        v: Option<&[Option<rsbinder::ParcelFileDescriptor>]>,
                    ) -> BinderResult<()>;
                    fn i(
                        &self,
                        v: &[rsbinder::Strong<dyn super::IGolden14Cb::IGolden14Cb>],
                    ) -> BinderResult<()>;
                    fn j(
                        &self,
                        v: Option<&[Option<rsbinder::Strong<dyn super::IGolden14Cb::IGolden14Cb>>]>,
                    ) -> BinderResult<()>;
                    fn k(&self, v: &[rsbinder::SIBinder]) -> BinderResult<()>;
                    fn l(&self, v: Option<&[Option<rsbinder::SIBinder>]>) -> BinderResult<()>;
                    fn m(&self, v: &[super::Golden14Cfg::Golden14Cfg; 3]) -> BinderResult<()>;
                    fn n(
                        &self,
                        v: Option<&[super::Golden14Cfg::Golden14Cfg; 3]>,
                    ) -> BinderResult<()>;
                    fn o(&self, v: &[rsbinder::ParcelFileDescriptor; 3]) -> BinderResult<()>;
                    fn p(
                        &self,
                        v: Option<&[rsbinder::ParcelFileDescriptor; 3]>,
                    ) -> BinderResult<()>;
                }
            },
        );
    }

    /// An element `Option` that `.aidl` spells bare lets the service send a
    /// null element a conforming peer cannot decode.
    #[test]
    fn rejects_an_array_of_options_aidl_would_spell_bare() {
        for decl in [
            // `in` never gives an element its own `Option`.
            quote!(
                fn go(&self, v: &[Option<rsbinder::ParcelFileDescriptor>]) -> BinderResult<()>;
            ),
            // `out` gives one only to a binder or a fd.
            quote!(
                fn go(&self, v: &mut Vec<Option<String>>) -> BinderResult<()>;
            ),
            // A variable `inout` array is read in fully populated.
            quote!(
                fn go(
                    &self,
                    #[inout] v: &mut Vec<Option<rsbinder::ParcelFileDescriptor>>,
                ) -> BinderResult<()>;
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    #decl
                }
            });
            assert!(err.contains("`Option<_>` elements"), "{err}");
        }
    }

    /// The one shape where the two paths could disagree on how an interface names itself.
    #[test]
    fn self_referencing_interface() {
        assert_same(
            r#"
interface IGolden6 {
    void register(in IGolden6 cb);
    IGolden6 fetch();
}
"#,
            "IGolden6",
            quote! {
                pub trait IGolden6 {
                    fn register(
                        &self,
                        cb: &rsbinder::Strong<dyn IGolden6>,
                    ) -> BinderResult<()>;
                    fn fetch(&self) -> BinderResult<rsbinder::Strong<dyn IGolden6>>;
                }
            },
        );
    }

    /// Compared before the derive drops the struct and `Default`, so the whole module is pinned.
    #[test]
    fn parcelable_codec_matches_aidl() {
        let input: syn::DeriveInput = syn::parse2(quote! {
            pub struct GoldenConfig {
                pub name: String,
                pub retries: i32,
                pub verbose: bool,
                pub timeoutNanos: i64,
                pub ratio: f64,
                pub extra: Option<Vec<u8>>,
                pub tags: Vec<String>,
            }
        })
        .unwrap();
        let actual = parcelable::render_source(&input).expect("render");
        for enabled_async in [false, true] {
            let expected = from_aidl(
                r#"
parcelable GoldenConfig {
    String name;
    int retries;
    boolean verbose;
    long timeoutNanos;
    double ratio;
    @nullable byte[] extra;
    String[] tags;
}
"#,
                "GoldenConfig",
                enabled_async,
            );
            assert_eq!(expected, actual, "generated parcelable differs");
        }
    }

    /// An enum is a scalar in `.aidl`, so it is passed by value.
    #[test]
    fn parcelable_and_enum_arguments() {
        let files = [
            (
                "com/example/IGolden7.aidl",
                "package com.example;\nimport com.example.GoldenCfg;\n\
                 import com.example.GoldenMode;\n\
                 interface IGolden7 {\n\
                 \x20   GoldenCfg apply(in GoldenCfg cfg, in GoldenMode mode);\n}\n",
            ),
            (
                "com/example/GoldenCfg.aidl",
                "package com.example;\nparcelable GoldenCfg {\n    String name;\n\
                 \x20   int retries;\n}\n",
            ),
            (
                "com/example/GoldenMode.aidl",
                "package com.example;\n@Backing(type=\"int\")\n\
                 enum GoldenMode {\n    FAST = 0,\n    SAFE = 1,\n}\n",
            ),
        ];
        assert_same_files(
            &files,
            "com/example/IGolden7.aidl",
            "IGolden7",
            "com.example.IGolden7",
            quote! {
                pub trait IGolden7 {
                    fn apply(
                        &self,
                        cfg: &super::GoldenCfg::GoldenCfg,
                        mode: super::GoldenMode::GoldenMode,
                    ) -> BinderResult<super::GoldenCfg::GoldenCfg>;
                }
            },
        );
    }

    #[test]
    fn rejects_generic_trait() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IBad<T> {
                fn go(&self) -> BinderResult<()>;
            }
        })
        .unwrap();
        let err = render_source(&Args { descriptor: None }, &item).unwrap_err();
        assert!(err.to_string().contains("cannot be generic"), "{err}");
    }

    #[test]
    fn rejects_oneway_with_return() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IBad {
                #[oneway]
                fn go(&self) -> BinderResult<i32>;
            }
        })
        .unwrap();
        let err = render_source(&Args { descriptor: None }, &item).unwrap_err();
        assert!(err.to_string().contains("cannot return a value"), "{err}");
    }

    #[test]
    fn rejects_missing_receiver() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IBad {
                fn go() -> BinderResult<()>;
            }
        })
        .unwrap();
        let err = render_source(&Args { descriptor: None }, &item).unwrap_err();
        assert!(err.to_string().contains("&self"), "{err}");
    }

    #[test]
    fn descriptor_defaults_to_trait_name_and_can_be_overridden() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IHello {
                fn go(&self) -> BinderResult<()>;
            }
        })
        .unwrap();
        let bare = render_source(&Args { descriptor: None }, &item).unwrap();
        assert!(bare.contains(r#""IHello""#), "{bare}");
        let named = render_source(
            &Args {
                descriptor: Some("com.example.IHello".into()),
            },
            &item,
        )
        .unwrap();
        assert!(named.contains(r#""com.example.IHello""#), "{named}");
    }

    fn render(tokens: proc_macro2::TokenStream) -> String {
        let item: ItemTrait = syn::parse2(tokens).expect("parse trait");
        render_source(&Args { descriptor: None }, &item).expect("render")
    }

    fn reject(tokens: proc_macro2::TokenStream) -> String {
        let item: ItemTrait = syn::parse2(tokens).expect("parse trait");
        render_source(&Args { descriptor: None }, &item)
            .unwrap_err()
            .to_string()
    }

    /// The `_arg_` prefix and the templates' `r#` must not stack into `_arg_r#` / `r#r#`.
    #[test]
    fn raw_identifiers_are_not_double_escaped() {
        let s = render(quote! {
            pub trait IRaw {
                fn r#type(&self, r#match: i32) -> BinderResult<()>;
            }
        });
        assert!(!s.contains("r#r#"), "{s}");
        assert!(!s.contains("_arg_r#"), "{s}");
        assert!(s.contains("fn r#type("), "{s}");
        assert!(s.contains("_arg_match"), "{s}");
        syn::parse_file(&s).unwrap_or_else(|e| panic!("does not parse: {e}\n{s}"));
    }

    /// A raw trait name has to survive into `Bn`/`Bp` and the module name.
    #[test]
    fn raw_trait_name_renders() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait r#type {
                fn go(&self) -> BinderResult<()>;
            }
        })
        .unwrap();
        let s = render_source(&Args { descriptor: None }, &item).expect("render");
        assert!(s.contains("pub trait r#type"), "{s}");
        assert!(s.contains("Bntype"), "{s}");
        assert!(!s.contains("r#r#"), "{s}");
        syn::parse_file(&s).unwrap_or_else(|e| panic!("does not parse: {e}\n{s}"));
    }

    /// The re-render would drop these: a `#[cfg]` ignored, a `#[oneway]` turned twoway.
    #[test]
    fn rejects_trait_level_attributes() {
        let err = reject(quote! {
            #[cfg(feature = "x")]
            pub trait IBad {
                fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("unsupported attribute"), "{err}");

        let err = reject(quote! {
            #[oneway]
            pub trait IBad {
                fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("unsupported attribute"), "{err}");
    }

    #[test]
    fn rejects_where_clauses() {
        let err = reject(quote! {
            pub trait IBad where Self: Sized {
                fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("where clause"), "{err}");

        let err = reject(quote! {
            pub trait IBad {
                fn go(&self) -> BinderResult<()> where Self: Sized;
            }
        });
        assert!(err.contains("where clause"), "{err}");
    }

    /// The re-render drops modifiers, so the user's `impl` would miss its own signature.
    #[test]
    fn rejects_signature_modifiers() {
        let err = reject(quote! {
            pub trait IBad {
                async fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("sync"), "{err}");

        let err = reject(quote! {
            pub trait IBad {
                unsafe fn go(&self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("unsafe"), "{err}");
    }

    /// `syn` keeps `&mut self`'s `mut` inside the receiver kind, so the check names the kind.
    #[test]
    fn rejects_non_shared_receivers() {
        for recv in [
            quote!(self),
            quote!(mut self),
            quote!(&mut self),
            quote!(&'static self),
            quote!(self: Box<Self>),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(#recv) -> BinderResult<()>;
                }
            });
            assert!(err.contains("`&self`"), "{recv}: {err}");
        }
        // Already a shared borrow, so the reason has to be the lifetime.
        let err = reject(quote! {
            pub trait IBad {
                fn go(&'static self) -> BinderResult<()>;
            }
        });
        assert!(err.contains("explicit lifetime on `self`"), "{err}");
        // `&'_ self` is the same type as `&self`, so nothing is lost.
        render(quote! {
            pub trait IOk {
                fn go(&'_ self) -> BinderResult<()>;
            }
        });
    }

    /// A receiver's attribute is dropped by the re-render like any other.
    #[test]
    fn rejects_an_attribute_on_the_receiver() {
        for recv in [
            quote!(
                #[oneway]
                &self
            ),
            quote!(
                #[cfg(feature = "never")]
                &self
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(#recv) -> BinderResult<()>;
                }
            });
            assert!(err.contains("unsupported attribute"), "{recv}: {err}");
        }
    }

    /// As a return type, `Option<&str>` would silently become `Option<String>`.
    #[test]
    fn rejects_borrowed_return_inside_option() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self) -> BinderResult<Option<&str>>;
            }
        });
        assert!(err.contains("borrow"), "{err}");
    }

    /// `@nullable` does not widen a `String`'s direction.
    #[test]
    fn rejects_out_nullable_string() {
        for decl in [
            quote!(
                fn go(&self, s: &mut Option<String>) -> BinderResult<()>;
            ),
            quote!(
                fn go(&self, #[inout] s: &mut Option<String>) -> BinderResult<()>;
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    #decl
                }
            });
            assert!(err.contains("`String` cannot be an"), "{err}");
        }
    }

    /// A bare out binder object or fd has no `.aidl` form and no `Default` to start from.
    #[test]
    fn rejects_a_bare_out_binder_object_or_fd() {
        for ty in [
            quote!(&mut rsbinder::Strong<dyn IOther>),
            quote!(&mut rsbinder::ParcelFileDescriptor),
            quote!(&mut rsbinder::SIBinder),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(&self, v: #ty) -> BinderResult<()>;
                }
            });
            assert!(err.contains("&mut Option<_>"), "{ty}: {err}");
        }
        // Bare `#[inout]` and `out` `Option` are both `.aidl` forms.
        render(quote! {
            pub trait IOk {
                fn a(&self, #[inout] v: &mut rsbinder::Strong<dyn IOther>) -> BinderResult<()>;
                fn b(&self, #[inout] v: &mut rsbinder::ParcelFileDescriptor) -> BinderResult<()>;
                fn c(&self, v: &mut Option<rsbinder::Strong<dyn IOther>>) -> BinderResult<()>;
                fn d(&self, v: &mut Option<rsbinder::ParcelFileDescriptor>) -> BinderResult<()>;
            }
        });
    }

    /// Each element of an out array needs a `Default` to be sized or started from.
    #[test]
    fn rejects_an_out_array_of_bare_binder_objects_or_fds() {
        for ty in [
            quote!(&mut Vec<rsbinder::ParcelFileDescriptor>),
            quote!(&mut Option<Vec<rsbinder::ParcelFileDescriptor>>),
            quote!(&mut Vec<rsbinder::Strong<dyn IOther>>),
            quote!(&mut [rsbinder::SIBinder; 3]),
            quote!(&mut [[rsbinder::ParcelFileDescriptor; 3]; 2]),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(&self, v: #ty) -> BinderResult<()>;
                }
            });
            assert!(err.contains("`Option<_>` elements"), "{ty}: {err}");
        }
        // `.aidl`'s non-nullable `inout` array is the bare form.
        render(quote! {
            pub trait IOk {
                fn a(&self, #[inout] v: &mut Vec<rsbinder::ParcelFileDescriptor>)
                    -> BinderResult<()>;
            }
        });
    }

    /// The macro's `&mut Option<_>` out is `.aidl`'s `out @nullable`.
    #[test]
    fn out_binder_objects_and_fds_are_the_nullable_form() {
        assert_same(
            r#"
interface IGolden9 {
    void take_cb(out @nullable IGolden9 cb);
    void take_fd(out @nullable ParcelFileDescriptor fd);
    void take_binder(out @nullable IBinder b);
}
"#,
            "IGolden9",
            quote! {
                pub trait IGolden9 {
                    fn take_cb(
                        &self,
                        cb: &mut Option<rsbinder::Strong<dyn IGolden9>>,
                    ) -> BinderResult<()>;
                    fn take_fd(
                        &self,
                        fd: &mut Option<rsbinder::ParcelFileDescriptor>,
                    ) -> BinderResult<()>;
                    fn take_binder(&self, b: &mut Option<rsbinder::SIBinder>) -> BinderResult<()>;
                }
            },
        );
    }

    /// `macro_rules!` hands a `$t:ty` over wrapped in an invisible group.
    #[test]
    fn a_macro_interpolated_type_renders_like_the_bare_one() {
        let str_ty = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote!(str));
        let ref_str = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote!(&str));
        assert_same(
            r#"
interface IGolden10 {
    void take(in String s, in @nullable String t);
}
"#,
            "IGolden10",
            quote! {
                pub trait IGolden10 {
                    fn take(&self, s: &#str_ty, t: Option<#ref_str>) -> BinderResult<()>;
                }
            },
        );
    }

    /// A whole interpolated parameter or return type keeps its direction and shape.
    #[test]
    fn a_macro_interpolated_parameter_keeps_its_direction() {
        let vec_ty = proc_macro2::Group::new(proc_macro2::Delimiter::None, quote!(&mut Vec<i32>));
        let ret_ty =
            proc_macro2::Group::new(proc_macro2::Delimiter::None, quote!(BinderResult<()>));
        assert_same(
            r#"
interface IGolden11 {
    void fill(out int[] v);
    void bump(inout int[] v);
}
"#,
            "IGolden11",
            quote! {
                pub trait IGolden11 {
                    fn fill(&self, v: #vec_ty) -> #ret_ty;
                    fn bump(&self, #[inout] v: #vec_ty) -> #ret_ty;
                }
            },
        );
    }

    /// No wire form; rustc would otherwise blame generated tokens.
    #[test]
    fn rejects_argument_shapes_with_no_wire_form() {
        for (ty, needle) in [
            (quote!(()), "`()` is not an argument type"),
            (quote!(dyn IOther), "trait object"),
            (quote!(&dyn IOther), "trait object"),
            (quote!(Option<&dyn IOther>), "trait object"),
            (quote!(&mut [i32]), "`&mut [T]`"),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(&self, v: #ty) -> BinderResult<()>;
                }
            });
            assert!(err.contains(needle), "{ty}: {err}");
        }
    }

    /// `&[Option<i32>]` is the `Vec<Option<i32>>` shape, and as inexpressible.
    #[test]
    fn rejects_a_nullable_primitive_behind_a_slice_or_array() {
        for ty in [
            quote!(&[Option<i32>]),
            quote!([Option<i32>; 4]),
            quote!(&Vec<Option<i32>>),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(&self, v: #ty) -> BinderResult<()>;
                }
            });
            assert!(err.contains("cannot be nullable"), "{ty}: {err}");
        }
    }

    /// `rsbinder-aidl` refuses a holder in a signature, however wrapped.
    #[test]
    fn rejects_a_parcelable_holder_in_a_signature() {
        for decl in [
            quote!(
                fn go(&self, h: &rsbinder::ParcelableHolder) -> BinderResult<()>;
            ),
            quote!(
                fn go(&self, h: &[rsbinder::ParcelableHolder]) -> BinderResult<()>;
            ),
            quote!(
                fn go(&self) -> BinderResult<rsbinder::ParcelableHolder>;
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    #decl
                }
            });
            assert!(err.contains("`ParcelableHolder` cannot appear"), "{err}");
        }
    }

    /// A duplicate would collide in generated tokens, away from the user's span.
    #[test]
    fn rejects_duplicate_names() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self, a: i32, a: i32) -> BinderResult<()>;
            }
        });
        assert!(err.contains("duplicate argument name `a`"), "{err}");

        // The `r#` is not part of the name: the templates add their own.
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self, r#type: i32, r#type: i32) -> BinderResult<()>;
            }
        });
        assert!(err.contains("duplicate argument name `type`"), "{err}");

        let err = reject(quote! {
            pub trait IBad {
                fn go(&self) -> BinderResult<()>;
                fn go(&self, a: i32) -> BinderResult<()>;
            }
        });
        assert!(err.contains("duplicate method name `go`"), "{err}");
    }

    /// The receiver is the one place that allows nothing, and an empty
    /// allow-list must not render as an empty `understands only` list.
    #[test]
    fn an_empty_allow_list_names_no_attribute() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(#[allow(dead_code)] &self) -> BinderResult<()>;
            }
        });
        assert_eq!(
            err, "unsupported attribute; no attribute is supported here",
            "{err}"
        );
    }

    /// The descriptor is the wire name, so a second one is refused, not picked.
    #[test]
    fn rejects_a_repeated_descriptor() {
        let err = match syn::parse2::<Args>(quote!(descriptor = "a", descriptor = "b")) {
            Ok(_) => panic!("a repeated `descriptor` must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("more than once"), "{err}");
    }

    /// The re-render reads no attribute argument and no second type argument.
    #[test]
    fn rejects_what_the_re_render_would_drop() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self) -> BinderResult<i32, String>;
            }
        });
        assert!(err.contains("exactly one type argument"), "{err}");

        for decl in [
            quote!(
                #[oneway(false)]
                fn go(&self) -> BinderResult<()>;
            ),
            quote!(
                #[oneway = "no"]
                fn go(&self) -> BinderResult<()>;
            ),
            quote!(
                fn go(&self, #[inout(no)] v: &mut Vec<i32>) -> BinderResult<()>;
            ),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    #decl
                }
            });
            assert!(err.contains("takes no arguments"), "{err}");
        }
    }

    /// `self::` would resolve inside the generated module, not the user's.
    #[test]
    fn rejects_self_paths() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self, cfg: &self::Config) -> BinderResult<()>;
            }
        });
        assert!(err.contains("`self::`"), "{err}");
    }

    /// Matched structurally, so a qualified `Vec` carries the same length word.
    #[test]
    fn qualified_vec_is_still_a_length_carrying_out_vector() {
        let bare = render(quote! {
            pub trait IOut {
                fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()>;
            }
        });
        let qualified = render(quote! {
            pub trait IOut {
                fn fill(&self, values: &mut std::vec::Vec<i32>) -> BinderResult<()>;
            }
        });
        assert!(bare.contains("write_slice_size"), "{bare}");
        assert!(
            qualified.contains("write_slice_size"),
            "a qualified `Vec` must still write the length word:\n{qualified}"
        );
        assert!(qualified.contains("resize_out_vec"), "{qualified}");
    }

    /// Same for `Option<Vec<T>>`, and for the guard that keeps a null fd off the wire.
    #[test]
    fn qualified_paths_keep_the_nullable_and_fd_guards() {
        let s = render(quote! {
            pub trait IOut {
                fn fill(&self, v: &mut core::option::Option<std::vec::Vec<i32>>)
                    -> BinderResult<()>;
            }
        });
        assert!(s.contains("resize_nullable_out_vec"), "{s}");

        let s = render(quote! {
            pub trait IOut {
                fn fds(
                    &self,
                    v: &mut std::vec::Vec<Option<rsbinder::ParcelFileDescriptor>>,
                ) -> BinderResult<()>;
            }
        });
        assert!(s.contains("iter().any(Option::is_none)"), "{s}");
    }

    /// Pinned here, not in `tests/ui`: the rustc diagnostic's wording moves between releases.
    #[test]
    fn by_value_argument_is_asserted_copy() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IBad {
                fn go(&self, s: String) -> BinderResult<()>;
            }
        })
        .unwrap();
        let expanded = expand(&Args { descriptor: None }, &item)
            .expect("expand")
            .to_string();
        // Both the `Copy` bound and the asserted type, so neither can be weakened unnoticed.
        assert!(
            expanded.contains("T : :: core :: marker :: Copy"),
            "{expanded}"
        );
        assert!(
            expanded.contains("__rsbinder_assert_copy :: < String > ()"),
            "{expanded}"
        );
    }

    /// The assertion names signature types, so it must resolve them where the body does.
    #[test]
    fn copy_assertion_shares_the_generated_module_scope() {
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IScoped {
                fn go(&self, mode: super::Mode::Mode) -> BinderResult<()>;
            }
        })
        .unwrap();
        let expanded = expand(&Args { descriptor: None }, &item).expect("expand");
        let file: syn::File = syn::parse2(expanded).expect("expansion parses");
        let module = file
            .items
            .iter()
            .find_map(|i| match i {
                syn::Item::Mod(m) => Some(m),
                _ => None,
            })
            .expect("generated module");
        let body = quote!(#module).to_string();
        assert!(body.contains("__rsbinder_assert_copy"), "{body}");
    }

    /// `#[doc(hidden)]` hides the module from docs, not from paths.
    #[test]
    fn generated_module_follows_the_trait_visibility() {
        let item: ItemTrait = syn::parse2(quote! {
            trait IPrivate {
                fn go(&self) -> BinderResult<()>;
            }
        })
        .unwrap();
        let expanded = expand(&Args { descriptor: None }, &item).expect("expand");
        let file: syn::File = syn::parse2(expanded).expect("expansion parses");
        let module = file
            .items
            .iter()
            .find_map(|i| match i {
                syn::Item::Mod(m) => Some(m),
                _ => None,
            })
            .expect("generated module");
        assert!(
            matches!(module.vis, syn::Visibility::Inherited),
            "a private trait must not get a `pub` module"
        );
    }
}
