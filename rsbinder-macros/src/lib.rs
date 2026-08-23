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
//! | `Option<T>` | nullable |
//! | `#[oneway]` on a method | no reply; must return `BinderResult<()>` |
//!
//! `Option<T>` is AIDL's `@nullable`, which AIDL allows only on the types that
//! have a null representation on the wire. A `#[derive(BinderEnum)]` enum is
//! carried as its `repr` scalar and is not one of them: `Option<Mode>` fails to
//! compile inside the generated code, because the enum has no `SerializeOption`
//! — the same shape `.aidl` rejects at the AIDL level.
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
//! **Spell out-parameter types directly.** A proc macro cannot see through a
//! type alias, so `&mut Ids` for `type Ids = Vec<i32>` reads as an opaque
//! named type: it loses the length word `.aidl` writes for an out vector, and
//! an aliased `ParcelFileDescriptor` element loses the null guard that keeps a
//! null fd off the wire. Path qualification is fine — `std::vec::Vec<i32>` is
//! matched structurally — but an alias is not.
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
                // The value is spliced into a Rust string literal in the
                // generated source with no re-escaping, so a backslash or a
                // quote would either change the wire descriptor or break the
                // literal — surfacing as a parse failure over the whole
                // generated module instead of a span on this attribute. The
                // `.aidl` path is spared this by its grammar.
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
    for attr in attrs {
        if !attr.path().is_ident(name) {
            continue;
        }
        let args: Args = attr.parse_args()?;
        if args.descriptor.is_some() {
            return Ok(args.descriptor);
        }
    }
    Ok(None)
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
/// overridden with `#[parcelable(descriptor = "…")]`.
///
/// Fields must be named and owned. `ParcelableHolder` and non-nullable binder
/// fields are `.aidl`-only shapes: spell a binder field `Option<Strong<dyn
/// IFoo>>`, which is AIDL's `@nullable`.
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

    // Renamed because an explicit module would beat the glob re-export to the
    // trait's name; `use super::*;` because the signature's paths are the
    // user's, one level up.
    let raw_name = item.ident.to_string();
    let mod_ident = syn::Ident::new(
        &format!("{}_binder", strip_raw(&raw_name)),
        item.ident.span(),
    );
    module.ident = mod_ident.clone();
    // The module is an implementation detail, but a `pub` one would still make
    // a private trait nameable from outside; follow the trait's own visibility.
    module.vis = item.vis.clone();

    // The proxy passes each argument to `build_parcel_*` and then again to
    // `read_response_*`, so a by-value argument has to be `Copy`. Asserted
    // against the user's own type — and from *inside* the module, so a
    // signature path resolves in the same scope the generated body uses it.
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

/// The generated module source, before the module wrapper is stripped — split
/// out so the golden test can hold it against the `.aidl` output (plan 2-19 D1).
fn render_source(args: &Args, item: &ItemTrait) -> syn::Result<String> {
    render_source_with(args, item, cfg!(feature = "async"))
}

fn render_source_with(args: &Args, item: &ItemTrait, enabled_async: bool) -> syn::Result<String> {
    // The trait is re-rendered from scratch, so anything not read here is
    // silently dropped: a `#[cfg]` would be emitted regardless of its
    // condition, and a trait-level `#[oneway]` would become twoway.
    check_attrs(&item.attrs, &[])?;
    // The generated trait is re-rendered from the render layer, which carries
    // no modifier — an accepted one would silently vanish, leaving a safe
    // trait a `impl` could satisfy without ever writing `unsafe`.
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
    for (i, trait_item) in item.items.iter().enumerate() {
        let TraitItem::Fn(f) = trait_item else {
            return Err(syn::Error::new_spanned(
                trait_item,
                "only methods are supported; constants and associated types need `.aidl`",
            ));
        };
        fn_members.push(make_fn_member(f, i as u32)?);
    }

    let descriptor = args.descriptor.clone().unwrap_or_else(|| name.clone());
    let mut render = InterfaceRender::new(name, descriptor);
    render.fn_members = fn_members;
    render.enabled_async = enabled_async;

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
    // Modifiers are dropped by the re-render, so the user's `impl` would fail
    // against a signature they never wrote.
    if let Some(tok) = &f.sig.asyncness {
        return Err(syn::Error::new_spanned(
            tok,
            "declare the method as sync; the `async` feature emits the `IFooAsync` halves \
             alongside it",
        ));
    }
    // `Safety::Safe` parses only inside an `extern` block, so a trait method
    // reaches here as either `Default` or `Unsafe`.
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
    // `syn` parses `...` here even outside an `extern` block, and the render
    // layer has no place for it — the argument would vanish from the trait.
    if let Some(variadic) = &f.sig.variadic {
        return Err(syn::Error::new_spanned(
            variadic,
            "a variadic binder method is not supported — the wire carries a fixed \
             argument list",
        ));
    }
    let oneway = has_attr(&f.attrs, "oneway");
    check_attrs(&f.attrs, &["oneway"])?;

    let mut inputs = f.sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if matches!(r.kind, syn::ReceiverKind::Reference(_, _, None)) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                &f.sig,
                "the first parameter must be `&self` — a binder object is shared, never owned \
                 or uniquely borrowed by a call",
            ))
        }
    }

    let mut args = "&self".to_string();
    let mut args_async = "&'a self".to_string();
    let mut func_call_params = String::new();
    let mut write_funcs = Vec::new();
    let mut transaction_decls = Vec::new();
    let mut transaction_write = Vec::new();
    let mut transaction_params = String::new();
    let mut read_onto_params = Vec::new();

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
        // `rsbinder-aidl` prefixes every parameter with `_arg_`; match it so the
        // two paths emit the same signature. Trait parameter names do not bind
        // the `impl`, so this is invisible to users writing a service.
        check_attrs(&pat_ty.attrs, &["inout"])?;
        let raw_arg = pat_ident.ident.to_string();
        let ident = format!("_arg_{}", strip_raw(&raw_arg));
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
        if dir != Dir::In {
            let word = if dir == Dir::Out { "out" } else { "inout" };
            type_str::check_out_capable(&pat_ty.ty, word)?;
        }
        let as_written = type_str::as_written(&pat_ty.ty)?;
        let owned = type_str::owned(&pat_ty.ty)?;

        let arg_str = format!(", {ident}: {as_written}");
        args += &arg_str;
        args_async += &arg_str.replace('&', "&'a ");
        func_call_params += &format!("{ident}, ");

        // Shape decisions read the `syn::Type`, not the rendered string: a
        // qualified `std::vec::Vec<T>` is the same type as `Vec<T>` and must
        // carry the same length word.
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
            // An out `Vec` carries only its length on the request; the server
            // sizes its own buffer from it (AOSP `resizeOutVector`).
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
            // AOSP writes a non-nullable out fd array back only if every
            // element is present; a `None` would otherwise go out as a null fd
            // where `.aidl` raises `UNEXPECTED_NULL`.
            write.needs_null_guard = dir == Dir::Out && type_str::is_option_pfd_vec(&pat_ty.ty);
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
    Ok(member)
}

/// Bridge what the server owns (`owned`) to what the trait asks for
/// (`as_written`), driven off the signature so no type list is needed.
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
    let is_mut_ref = matches!(ty, Type::Reference(r) if r.mutability.is_some());
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
    let Type::Path(p) = &**ty else {
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
    let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
        return Err(syn::Error::new_spanned(ty, "expected `BinderResult<T>`"));
    };
    // Not `check_supported`: that lets `Option<&str>` through for nullable
    // *arguments*, and a return value would then be silently rewritten to
    // `Option<String>`, leaving the user's `impl` to fail with no diagnostic
    // on the declaration.
    type_str::reject_any_reference(inner)?;
    type_str::reject_nullable_primitive(inner)?;
    type_str::owned(inner)
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

fn has_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

/// The trait is re-rendered from scratch, so an unrecognised attribute would be
/// dropped: a mistyped `#[oneway]` becoming a twoway call, a `#[cfg]` ignored.
fn check_attrs(attrs: &[syn::Attribute], allowed: &[&str]) -> syn::Result<()> {
    for attr in attrs {
        if attr.path().is_ident("doc") || allowed.iter().any(|a| attr.path().is_ident(a)) {
            continue;
        }
        return Err(syn::Error::new_spanned(
            attr,
            format!(
                "unsupported attribute; #[rsbinder::interface] understands only {} here",
                allowed
                    .iter()
                    .map(|a| format!("#[{a}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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
    //! The contract: a trait through this macro and the equivalent `.aidl`
    //! through `rsbinder-aidl` must produce the **same** module source. If this
    //! drifts, moving an interface from one path to the other stops being a
    //! no-op for call sites, which is the whole reason the macro reuses the
    //! render layer instead of emitting its own code (plan 2-19 D1).

    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temp directory that removes itself, so a `cargo test` run leaves
    /// nothing behind in `/tmp`.
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

    /// Generate `aidl` with `rsbinder-aidl` and return just the interface
    /// module, without the file header.
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

    /// Pull `pub mod {module} { … }` out of a generated file and dedent it —
    /// a packaged `.aidl` nests it, the macro emits it at column 0.
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

    /// Both `enabled_async` settings, because the golden gate is what pins the
    /// two front-ends together and a `cfg!` would leave one half unchecked.
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

    /// Same as [`from_aidl`] but with sibling files in the include dir, so a
    /// fixture can reference another interface.
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
        // Cross-file: pins the `super::Mod::Type` path a packaged import
        // produces, which a single-file fixture never exercises.
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

    /// A self-referencing interface — the callback pattern users reach for
    /// first, and the one shape where the two paths could disagree on how the
    /// declaring interface names itself.
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

    /// The same contract for data types: `#[derive(Parcelable)]` must produce
    /// the codec `.aidl` produces for the equivalent `parcelable`. The derive
    /// then drops the struct and `Default` from this module (a derive adds to
    /// a type, it cannot redeclare it) — what is compared here is the whole
    /// module, so the parts that do survive are pinned too.
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

    /// A method taking a parcelable and an enum. The enum is the case that
    /// forced argument passing to follow the signature: `.aidl` treats an enum
    /// as a scalar and hands it to the service by value, which a list of known
    /// primitive type names would never have covered.
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

    /// The `_arg_` prefix and the templates' own `r#` would otherwise stack up
    /// into `_arg_r#type` / `fn r#r#type`, neither of which lexes.
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

    /// The trait is re-rendered, so an attribute left on it would vanish —
    /// a `#[cfg]` emitted regardless of its condition, a trait-level
    /// `#[oneway]` silently becoming twoway.
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

    /// A modifier the re-render drops would leave the user's `impl` failing
    /// against a signature they never wrote.
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

    /// `syn` keeps the `mut` of `&mut self` inside the receiver kind, not
    /// beside it, so a shape-only check has to name the kind.
    #[test]
    fn rejects_non_shared_receivers() {
        for recv in [
            quote!(self),
            quote!(mut self),
            quote!(&mut self),
            quote!(self: Box<Self>),
        ] {
            let err = reject(quote! {
                pub trait IBad {
                    fn go(#recv) -> BinderResult<()>;
                }
            });
            assert!(err.contains("`&self`"), "{recv}: {err}");
        }
    }

    /// `check_supported` lets `Option<&str>` through for nullable *arguments*;
    /// as a return type it would be rewritten to `Option<String>` with no
    /// diagnostic on the declaration.
    #[test]
    fn rejects_borrowed_return_inside_option() {
        let err = reject(quote! {
            pub trait IBad {
                fn go(&self) -> BinderResult<Option<&str>>;
            }
        });
        assert!(err.contains("borrow"), "{err}");
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

    /// The out-vector wire decisions read the `syn::Type`, so a qualified path
    /// spells the same type and must carry the same length word.
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

    /// Same for `Option<Vec<T>>` and for the out-fd-array null guard, whose
    /// absence would put a null fd where `.aidl` raises `UNEXPECTED_NULL`.
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

    /// The proxy passes a by-value argument twice, so the macro asserts `Copy`
    /// against the user's own type. Pinned here rather than in `tests/ui`: the
    /// resulting rustc diagnostic is compiler wording that moves between
    /// releases.
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
        // Both halves are pinned: that the bound is `Copy` — swapping it for a
        // bound every type meets would otherwise go unnoticed — and that the
        // user's own type is what gets asserted.
        assert!(
            expanded.contains("T : :: core :: marker :: Copy"),
            "{expanded}"
        );
        assert!(
            expanded.contains("__rsbinder_assert_copy :: < String > ()"),
            "{expanded}"
        );
    }

    /// The assertion has to sit inside the generated module: it names types
    /// straight from the signature, which the rendered body resolves there.
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

    /// `#[doc(hidden)]` hides the module from docs, not from paths: a private
    /// trait must not become nameable through it.
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
