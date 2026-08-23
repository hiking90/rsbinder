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
use rsbinder_aidl::render::{
    interface_stem, render_interface, FnMembers, InterfaceRender, TransactionWrite,
};
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
                descriptor = Some(s.value());
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
        Ok(ts) => ts,
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(args: &Args, item: &ItemTrait) -> syn::Result<TokenStream> {
    let rendered = render_source(args, item)?;

    let file = syn::parse_file(&rendered).map_err(|e| {
        syn::Error::new_spanned(
            &item.ident,
            format!("generated code did not parse ({e}); generated source follows:\n{rendered}"),
        )
    })?;
    let Some(syn::Item::Mod(mut module)) = file.items.into_iter().next() else {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "generated code was not a single module (generator contract changed)",
        ));
    };

    // Renamed because an explicit module would beat the glob re-export to the
    // trait's name; `use super::*;` because the signature's paths are the
    // user's, one level up.
    let mod_ident = syn::Ident::new(&format!("{}_binder", item.ident), item.ident.span());
    module.ident = mod_ident.clone();
    if let Some((_, items)) = module.content.as_mut() {
        items.insert(
            0,
            syn::parse_quote!(
                use super::*;
            ),
        );
    }

    // The proxy passes each argument to `build_parcel_*` and then again to
    // `read_response_*`, so a by-value argument has to be `Copy`. Assert it
    // against the user's own type, or the move error lands on generated
    // tokens with no span into their file.
    let copied = by_value_types(item)?;
    let vis = &item.vis;
    Ok(quote! {
        #(
            const _: fn() = || {
                fn __rsbinder_assert_copy<T: ::core::marker::Copy>() {}
                __rsbinder_assert_copy::<#copied>();
            };
        )*
        #[doc(hidden)]
        #module
        #vis use #mod_ident::*;
    }
    .into())
}

/// The generated module source, before the module wrapper is stripped.
///
/// Split out so the golden test can compare it against what `rsbinder-aidl`
/// writes for the equivalent `.aidl` — that equality is the contract this
/// macro exists to keep (plan 2-19 D1).
fn render_source(args: &Args, item: &ItemTrait) -> syn::Result<String> {
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

    let name = item.ident.to_string();
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
    let stem = interface_stem(&name);
    let render = InterfaceRender {
        crate_name: "rsbinder".to_string(),
        module: name.clone(),
        bn_name: format!("Bn{stem}"),
        bp_name: format!("Bp{stem}"),
        namespace: descriptor,
        name,
        fn_members,
        enabled_async: cfg!(feature = "async"),
        ..Default::default()
    };

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
    let oneway = has_attr(&f.attrs, "oneway");
    check_attrs(&f.attrs, &["oneway"])?;

    let mut inputs = f.sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
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
        let ident = format!("_arg_{}", pat_ident.ident);
        let dir = direction(&pat_ty.attrs, &pat_ty.ty)?;
        if oneway && dir != Dir::In {
            return Err(syn::Error::new_spanned(
                &pat_ty.ty,
                "a #[oneway] method cannot have an out/inout parameter — there is no reply to \
                 write it back into",
            ));
        }
        type_str::check_supported(&pat_ty.ty)?;
        let as_written = type_str::as_written(&pat_ty.ty)?;
        let owned = type_str::owned(&pat_ty.ty)?;

        let arg_str = format!(", {ident}: {as_written}");
        args += &arg_str;
        args_async += &arg_str.replace('&', "&'a ");
        func_call_params += &format!("{ident}, ");

        if dir != Dir::Out {
            let param = if as_written.starts_with('&') {
                ident.clone()
            } else {
                format!("&{ident}")
            };
            write_funcs.push(format!("data.write({param})?;"));
        } else if owned.starts_with("Option<Vec<") {
            write_funcs.push(format!("data.write_slice_size({ident}.as_deref())?;"));
        } else if is_variable_array(&owned) {
            // An out `Vec` carries only its length on the request; the server
            // sizes its own buffer from it (AOSP `resizeOutVector`).
            write_funcs.push(format!("data.write_slice_size(Some({ident}))?;"));
        }

        let (mutable, init) = match dir {
            Dir::Out => (
                "mut ",
                type_str::out_default(&pat_ty.ty).unwrap_or_else(|| "Default::default()".into()),
            ),
            Dir::Inout => ("mut ", "_reader.read()?".to_string()),
            Dir::In => ("", "_reader.read()?".to_string()),
        };
        transaction_decls.push(format!("let {mutable}{ident}: {owned} = {init};"));
        if dir == Dir::Out {
            if owned.starts_with("Option<Vec<") {
                transaction_decls.push(format!("_reader.resize_nullable_out_vec(&mut {ident})?;"));
            } else if is_variable_array(&owned) {
                transaction_decls.push(format!("_reader.resize_out_vec(&mut {ident})?;"));
            }
        }

        if dir != Dir::In {
            transaction_write.push(TransactionWrite {
                identifier: ident.clone(),
                // AOSP writes a non-nullable out fd array back only if every
                // element is present; a `None` would otherwise go out as a
                // null fd where `.aidl` raises `UNEXPECTED_NULL`.
                needs_null_guard: dir == Dir::Out
                    && owned.starts_with("Vec<Option<")
                    && owned.contains("ParcelFileDescriptor"),
            });
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

    Ok(FnMembers {
        identifier: f.sig.ident.to_string(),
        args,
        args_async,
        transaction_has_return: return_type != "()",
        return_type,
        write_funcs,
        func_call_params: trim_comma(func_call_params),
        transaction_decls,
        transaction_write,
        transaction_params: trim_comma(transaction_params),
        oneway,
        read_onto_params,
        transaction_code: index,
        has_explicit_code: false,
        enforce_permission_check: None,
    })
}

/// How the server hands a decoded argument to the user's `impl`.
///
/// The server owns what it read (`owned`), and the trait asks for the type the
/// signature spells (`as_written`); this bridges the two. Driving it off the
/// signature rather than off a list of known types is what makes a
/// `#[derive(BinderEnum)]` enum — passed by value, like any `Copy` type — work
/// without the macro having to recognise it.
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
    if matches!(inner, Type::Reference(_)) {
        return Err(syn::Error::new_spanned(
            inner,
            "a return value is decoded into a fresh owned value, so it cannot be a reference — \
             return `String`, `Vec<T>` or the owned type",
        ));
    }
    type_str::check_supported(inner)?;
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

/// Anything unrecognised would be silently dropped — the trait is re-rendered
/// from scratch, so a mistyped `#[oneway]` would quietly become a twoway call
/// and a `#[cfg]` would be emitted regardless of its condition.
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

/// A `Vec` is the only length-carrying out parameter shape in v1 scope.
fn is_variable_array(owned: &str) -> bool {
    owned.starts_with("Vec<") || owned.starts_with("Option<Vec<")
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
    use std::path::PathBuf;

    /// `Builder` reads `OUT_DIR` from the environment, which is process-wide:
    /// two golden tests generating at once would race on it.
    static GENERATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Generate `aidl` with `rsbinder-aidl` and return just the interface
    /// module, without the file header.
    fn from_aidl(aidl: &str, module: &str) -> String {
        let _guard = GENERATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "rsbinder_macros_golden_{module}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let src = dir.join(format!("{module}.aidl"));
        std::fs::write(&src, aidl).expect("write aidl");
        std::env::set_var("OUT_DIR", &dir);

        rsbinder_aidl::Builder::new()
            .source(src)
            .include_dir(&dir)
            .output(PathBuf::from("golden.rs"))
            .set_async_support(cfg!(feature = "async"))
            .generate()
            .expect("aidl generate");

        let text = std::fs::read_to_string(dir.join("golden.rs")).expect("read generated");
        extract_module(&text, module)
    }

    /// Pull `pub mod {module} { … }` out of a generated file and dedent it.
    ///
    /// A packaged `.aidl` nests the interface under its package modules, so
    /// the block arrives indented; the macro emits it at column 0.
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

    fn from_macro(tokens: proc_macro2::TokenStream) -> String {
        let item: ItemTrait = syn::parse2(tokens).expect("parse trait");
        render_source(&Args { descriptor: None }, &item).expect("render")
    }

    #[track_caller]
    fn assert_same(aidl: &str, module: &str, tokens: proc_macro2::TokenStream) {
        let expected = from_aidl(aidl, module);
        let actual = from_macro(tokens);
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
            panic!("generated code differs\n{report}\n--- aidl ---\n{expected}\n--- macro ---\n{actual}");
        }
    }

    /// Same as [`from_aidl`] but with sibling files in the include dir, so a
    /// fixture can reference another interface.
    fn from_aidl_files(files: &[(&str, &str)], main: &str, module: &str) -> String {
        let _guard = GENERATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "rsbinder_macros_golden_multi_{module}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
            std::fs::write(&path, body).expect("write aidl");
        }
        std::env::set_var("OUT_DIR", &dir);
        rsbinder_aidl::Builder::new()
            .source(dir.join(main))
            .include_dir(&dir)
            .output(PathBuf::from("golden_multi.rs"))
            .set_async_support(cfg!(feature = "async"))
            .generate()
            .expect("aidl generate");
        let text = std::fs::read_to_string(dir.join("golden_multi.rs")).expect("read generated");
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
        let expected = from_aidl_files(
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
        );
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IGolden5 {
                fn register(
                    &self,
                    cb: &rsbinder::Strong<dyn super::IGolden5Cb::IGolden5Cb>,
                ) -> BinderResult<()>;
                fn fetch(
                    &self,
                ) -> BinderResult<rsbinder::Strong<dyn super::IGolden5Cb::IGolden5Cb>>;
            }
        })
        .unwrap();
        let actual = render_source(
            &Args {
                descriptor: Some("com.example.IGolden5".to_string()),
            },
            &item,
        )
        .expect("render");
        assert_eq!(expected, actual, "generated code differs");
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
        );
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
        assert_eq!(expected, actual, "generated parcelable differs");
    }

    /// A method taking a parcelable and an enum. The enum is the case that
    /// forced argument passing to follow the signature: `.aidl` treats an enum
    /// as a scalar and hands it to the service by value, which a list of known
    /// primitive type names would never have covered.
    #[test]
    fn parcelable_and_enum_arguments() {
        let expected = from_aidl_files(
            &[
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
            ],
            "com/example/IGolden7.aidl",
            "IGolden7",
        );
        let item: ItemTrait = syn::parse2(quote! {
            pub trait IGolden7 {
                fn apply(
                    &self,
                    cfg: &super::GoldenCfg::GoldenCfg,
                    mode: super::GoldenMode::GoldenMode,
                ) -> BinderResult<super::GoldenCfg::GoldenCfg>;
            }
        })
        .unwrap();
        let actual = render_source(
            &Args {
                descriptor: Some("com.example.IGolden7".to_string()),
            },
            &item,
        )
        .expect("render");
        assert_eq!(expected, actual, "generated code differs");
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
}
