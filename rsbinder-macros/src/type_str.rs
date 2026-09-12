// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `syn::Type` printed as `TypeGenerator::type_declaration` does: the golden gate compares text.

use syn::{GenericArgument, PathArguments, Type};

/// The expanding macro, so an unsupported-type error names the one the user wrote.
#[derive(Clone, Copy)]
pub enum Ctx {
    Interface,
    Parcelable,
}

impl Ctx {
    fn unsupported(self) -> &'static str {
        match self {
            Ctx::Interface => "unsupported type in a #[rsbinder::interface] signature",
            Ctx::Parcelable => "unsupported type in a #[derive(Parcelable)] field",
        }
    }
}

/// Where a type sits. A field has no direction of its own, so it answers the
/// direction-dependent rules for itself (`type_declaration(is_struct = true)`).
#[derive(Clone, Copy)]
pub enum Place {
    In,
    Out,
    Inout,
    Return,
    Field,
}

impl Place {
    /// The word the diagnostics use; also `check_array_elements`'s axis.
    fn word(self) -> &'static str {
        match self {
            Place::In => "in",
            Place::Out => "out",
            Place::Inout => "inout",
            Place::Return => "return",
            Place::Field => "field",
        }
    }
}

/// Strip the invisible group a `macro_rules!` `$t:ty` arrives in, and any parentheses.
pub fn unwrap_group(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => unwrap_group(&g.elem),
        Type::Paren(p) => unwrap_group(&p.elem),
        other => other,
    }
}

/// The type as the signature spells it (`rsbinder-aidl`'s `type_decl_for_func`).
pub fn as_written(ty: &Type) -> syn::Result<String> {
    as_written_in(ty, Ctx::Interface)
}

pub fn as_written_in(ty: &Type, ctx: Ctx) -> syn::Result<String> {
    let ty = unwrap_group(ty);
    Ok(match ty {
        Type::Reference(r) => {
            let inner = as_written_in(&r.elem, ctx)?;
            if r.mutability.is_some() {
                format!("&mut {inner}")
            } else {
                format!("&{inner}")
            }
        }
        Type::Slice(s) => format!("[{}]", as_written_in(&s.elem, ctx)?),
        Type::Array(a) => {
            let len = &a.len;
            format!(
                "[{}; {}]",
                as_written_in(&a.elem, ctx)?,
                quote::quote!(#len)
            )
        }
        Type::Tuple(t) if t.elems.is_empty() => "()".to_string(),
        Type::TraitObject(t) => {
            let bounds = &t.bounds;
            format!(
                "dyn {}",
                quote::quote!(#bounds).to_string().replace(' ', "")
            )
        }
        Type::Path(p) => {
            if p.qself.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "qualified paths are not supported",
                ));
            }
            // A derive's codec lands in the user's own scope, where `self::` is theirs.
            if matches!(ctx, Ctx::Interface) {
                reject_self_path(p)?;
            }
            let mut out = String::new();
            for (i, seg) in p.path.segments.iter().enumerate() {
                if i > 0 || p.path.leading_colon.is_some() {
                    out.push_str("::");
                }
                out.push_str(&seg.ident.to_string());
                reject_parenthesized(&seg.arguments)?;
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    let mut rendered = Vec::new();
                    for arg in &args.args {
                        match arg {
                            GenericArgument::Type(t) => rendered.push(as_written_in(t, ctx)?),
                            GenericArgument::Lifetime(l) => rendered.push(format!("'{}", l.ident)),
                            other => {
                                return Err(syn::Error::new_spanned(
                                    other,
                                    "only type and lifetime generic arguments are supported",
                                ))
                            }
                        }
                    }
                    out.push('<');
                    out.push_str(&rendered.join(", "));
                    out.push('>');
                }
            }
            out
        }
        other => return Err(syn::Error::new_spanned(other, ctx.unsupported())),
    })
}

/// The printers walk only `<..>` arguments, so `Fn(i32)` would render as a bare `Fn`.
fn reject_parenthesized(args: &PathArguments) -> syn::Result<()> {
    if let PathArguments::Parenthesized(p) = args {
        return Err(syn::Error::new_spanned(
            p,
            "parenthesized generic arguments have no wire form",
        ));
    }
    Ok(())
}

/// The body lands one module deeper, so `self::` would name the generated module.
fn reject_self_path(p: &syn::TypePath) -> syn::Result<()> {
    if p.path.leading_colon.is_none() && p.path.segments.first().is_some_and(|s| s.ident == "self")
    {
        return Err(syn::Error::new_spanned(
            p,
            "`self::` is not supported: the generated items live in a module of their own, \
             so the path would resolve there rather than where you wrote it — and for the \
             same reason `super::` names *this* module, not its parent. Use the bare name, \
             or `crate::` to reach the parent",
        ));
    }
    Ok(())
}

/// The owned form (`rsbinder-aidl`'s `type_declaration(false)`), references collapsed.
pub fn owned(ty: &Type) -> syn::Result<String> {
    let ty = unwrap_group(ty);
    Ok(match ty {
        Type::Reference(r) => match unwrap_group(&r.elem) {
            Type::Path(p) if p.path.is_ident("str") => "String".to_string(),
            Type::Slice(s) => format!("Vec<{}>", owned(&s.elem)?),
            inner => owned(inner)?,
        },
        Type::Path(p) => {
            if p.qself.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "qualified paths are not supported",
                ));
            }
            reject_self_path(p)?;
            let mut out = String::new();
            for (i, seg) in p.path.segments.iter().enumerate() {
                if i > 0 || p.path.leading_colon.is_some() {
                    out.push_str("::");
                }
                out.push_str(&seg.ident.to_string());
                reject_parenthesized(&seg.arguments)?;
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    let mut rendered = Vec::new();
                    for arg in &args.args {
                        match arg {
                            GenericArgument::Type(t) => rendered.push(owned(t)?),
                            GenericArgument::Lifetime(l) => rendered.push(format!("'{}", l.ident)),
                            other => {
                                return Err(syn::Error::new_spanned(
                                    other,
                                    "only type and lifetime generic arguments are supported",
                                ))
                            }
                        }
                    }
                    out.push('<');
                    out.push_str(&rendered.join(", "));
                    out.push('>');
                }
            }
            out
        }
        Type::Slice(s) => format!("Vec<{}>", owned(&s.elem)?),
        other => as_written(other)?,
    })
}

/// Every gate a type must pass, keyed by where it sits. The argument, return
/// and parcelable-field paths call only this, so a rule added here is reached
/// from all of them rather than from the one position that prompted it.
pub fn check_type_at(ty: &Type, place: Place) -> syn::Result<()> {
    match place {
        // `check_supported` is the argument-shape gate: it opens with
        // `reject_non_argument` and `check_scalar_names` itself.
        Place::In | Place::Out | Place::Inout => check_supported(ty)?,
        Place::Return => {
            // Not `check_supported`: its `Option<&str>` would silently become `Option<String>` here.
            reject_any_reference(ty)?;
            // `()` is `void`, which only the whole return type may be.
            if !matches!(unwrap_group(ty), Type::Tuple(t) if t.elems.is_empty()) {
                reject_non_argument(ty)?;
            }
            check_scalar_names(ty)?;
        }
        Place::Field => {
            reject_field_references(ty)?;
            // A field has no `void` exception: `.aidl` refuses `void` as a field outright.
            reject_non_argument(ty)?;
            check_scalar_names(ty)?;
        }
    }
    reject_nullable_primitive(ty)?;
    reject_parcelable_holder(ty, place)?;
    if matches!(place, Place::Out | Place::Inout) {
        check_out_capable(ty, place.word())?;
    }
    check_array_elements(ty, place.word())?;
    // Last, so a shape that cannot go on the wire at all keeps its own
    // diagnostic: the rules above each describe one way to be wrong, and this
    // asks the only question that actually matters — whether `.aidl` would
    // have written what you wrote.
    crate::aidl_shape::check_canonical(ty, place)
}

/// A field owns what it carries, so it borrows at no depth.
fn reject_field_references(ty: &Type) -> syn::Result<()> {
    if matches!(unwrap_group(ty), Type::Reference(_)) {
        return Err(syn::Error::new_spanned(
            ty,
            "a parcelable field cannot be a reference — it owns what it carries",
        ));
    }
    reject_inner_references(ty)
}

/// Matched structurally, so a holder inside `Option<_>` is caught too. Both
/// messages live here so neither can start recommending what the other refuses.
fn reject_parcelable_holder(ty: &Type, place: Place) -> syn::Result<()> {
    if !mentions_parcelable_holder(ty) {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        ty,
        match place {
            Place::Field => {
                "a `ParcelableHolder` field needs the `.aidl` path: its stability is set before \
                 the read and the derived codec would replace the whole field, so a peer's \
                 `@VintfStability` holder can never be decoded"
            }
            _ => {
                "a `ParcelableHolder` cannot appear in a binder signature — `.aidl` refuses it \
                 as an argument or return type; carry it as a field of an `.aidl` parcelable \
                 instead, since `#[derive(Parcelable)]` cannot hold one either (its stability \
                 is set before the read)"
            }
        },
    ))
}

/// Reject what [`owned`] cannot lend back (`&[&str]`); `Option<&str>` is AIDL's nullable `in`.
pub fn check_supported(ty: &Type) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    reject_non_argument(ty)?;
    check_scalar_names(ty)?;
    match ty {
        Type::Reference(r) => {
            if r.lifetime.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "an explicit lifetime is not supported — the generated trait has none to \
                     bind it to; use a plain `&T`",
                ));
            }
            if r.mutability.is_some() && matches!(unwrap_group(&r.elem), Type::Slice(_)) {
                return Err(syn::Error::new_spanned(
                    ty,
                    "`&mut [T]` has no `.aidl` form and nothing to size or decode it into — use \
                     `&mut Vec<T>`, or `&mut [T; N]` for a fixed-size array",
                ));
            }
            if r.mutability.is_none() {
                reject_borrowed_container(&r.elem, false)?;
            }
            reject_inner_references(&r.elem)
        }
        Type::Path(p) => {
            let last = p.path.segments.last();
            if let Some(seg) = last {
                if seg.ident == "Option" {
                    if let PathArguments::AngleBracketed(args) = &seg.arguments {
                        if let Some(Type::Reference(inner)) = first_type_arg(args).map(unwrap_group)
                        {
                            if inner.lifetime.is_some() || inner.mutability.is_some() {
                                return Err(syn::Error::new_spanned(
                                    ty,
                                    "a nullable argument borrows immutably and without a named \
                                     lifetime — use `Option<&str>`, or `Option<&[T]>` for an \
                                     array (whose non-primitive elements each carry an \
                                     `Option` of their own: `Option<&[Option<String>]>`)",
                                ));
                            }
                            reject_borrowed_container(&inner.elem, true)?;
                            return reject_inner_references(&inner.elem);
                        }
                    }
                }
            }
            reject_inner_references(ty)
        }
        other => reject_inner_references(other),
    }
}

/// `()` and a trait object have no wire form; rustc would blame generated tokens.
pub fn reject_non_argument(ty: &Type) -> syn::Result<()> {
    match unwrap_group(ty) {
        Type::Tuple(t) if t.elems.is_empty() => Err(syn::Error::new_spanned(
            ty,
            "`()` has no wire form inside a type — `.aidl` accepts `void` only as the whole \
             return type",
        )),
        Type::TraitObject(_) => Err(syn::Error::new_spanned(
            ty,
            "a trait object has no wire form — pass a binder as `rsbinder::Strong<dyn IFoo>` \
             (`&` it in an argument)",
        )),
        Type::Reference(r) => reject_non_argument(&r.elem),
        Type::Slice(s) => reject_non_argument(&s.elem),
        Type::Array(a) => reject_non_argument(&a.elem),
        // Never below `Strong<dyn IFoo>`: that `dyn` is the one legal trait object.
        Type::Path(p) if named_generic(ty, "Strong").is_none() => {
            for seg in &p.path.segments {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    for arg in &args.args {
                        if let GenericArgument::Type(t) = arg {
                            reject_non_argument(t)?;
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Reject a scalar name `.aidl` never renders: the `.aidl` port would spell it
/// another type, and `u128` puts a width no AIDL peer can decode on the wire.
pub fn check_scalar_names(ty: &Type) -> syn::Result<()> {
    check_scalar_names_at(ty, false)
}

fn check_scalar_names_at(ty: &Type, element: bool) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    match ty {
        Type::Reference(r) => check_scalar_names_at(&r.elem, element),
        Type::Slice(s) => check_scalar_names_at(&s.elem, true),
        Type::Array(a) => check_scalar_names_at(&a.elem, true),
        Type::Path(_) => {
            if let Some(inner) = named_generic(ty, "Vec").and_then(first_type_arg) {
                return check_scalar_names_at(inner, true);
            }
            // An `Option` wraps, so its argument sits in the same place it does.
            if let Some(inner) = named_generic(ty, "Option").and_then(first_type_arg) {
                return check_scalar_names_at(inner, element);
            }
            let Some(name) = plain_name(ty) else {
                return Ok(());
            };
            // `byte` swaps spelling by place — `i8` as a scalar, `u8` as an
            // array element (`array_type_name`) — and neither the other way.
            if element && name == "i8" {
                return Err(syn::Error::new_spanned(
                    ty,
                    "`i8` is not the element spelling `.aidl` renders — a `byte[]` element is \
                     `u8` (`i8` is the scalar spelling, as in a bare `byte`), so the equivalent \
                     `.aidl` would render `u8` here and a call site written against one does not \
                     take the other; use `u8`",
                ));
            }
            if RENDERABLE_SCALARS.contains(&name.as_str())
                || (element && name == "u8")
                || !RUST_SCALARS.contains(&name.as_str())
            {
                return Ok(());
            }
            Err(syn::Error::new_spanned(
                ty,
                format!(
                    "`{name}` is not a spelling `.aidl` renders — AIDL's scalars are `bool`, \
                     `i8` (`byte`), `i32` (`int`), `i64` (`long`), `f32` (`float`), `f64` \
                     (`double`) and `u16` (`char`), so the equivalent `.aidl` would render a \
                     different Rust type here; use {}",
                    scalar_advice(&name)
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// The AIDL scalar to write instead, which every place taking the refused one accepts.
fn scalar_advice(name: &str) -> &'static str {
    match name {
        "u8" => "`i8` (`.aidl`'s `byte`; `u8` is the element spelling, as in `&[u8]`)",
        "char" => "`u16`, which is what `.aidl` renders its own `char` as",
        "i16" | "u32" => "`i32`",
        _ => "`i64`, the widest scalar AIDL has",
    }
}

/// Spellings that compile and carry the same wire as the `in` argument `.aidl`
/// renders, but do not give the same call site.
fn reject_borrowed_container(ty: &Type, nullable: bool) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    if is_primitive(ty) {
        if nullable {
            return Err(syn::Error::new_spanned(
                ty,
                "a primitive has no null form on the wire, so `.aidl` rejects `@nullable` on \
                 one, and it renders an `in` primitive as the bare type; drop both the \
                 `Option` and the `&`",
            ));
        }
        return Err(syn::Error::new_spanned(
            ty,
            "`.aidl` renders a primitive `in` argument as the bare type, never behind a \
             reference, and a call site written against one does not take the other; drop \
             the `&`",
        ));
    }
    let (written, aidl, note) = if plain_name(ty).is_some_and(|n| n == "String") {
        if nullable {
            ("Option<&String>", "Option<&str>", "")
        } else {
            ("&String", "&str", "")
        }
    } else if named_generic(ty, "Vec").is_some() {
        if nullable {
            (
                "Option<&Vec<T>>",
                "Option<&[T]>",
                " (and a `@nullable` array gives every non-primitive element an `Option` of \
                 its own: `Option<&[Option<String>]>`)",
            )
        } else {
            ("&Vec<T>", "&[T]", "")
        }
    } else if named_generic(ty, "Option").is_some() {
        (
            if nullable {
                "Option<&Option<T>>"
            } else {
                "&Option<T>"
            },
            "Option<&T>",
            " (`Option<&str>` for a string, `Option<&[T]>` for an array — whose non-primitive \
             elements each carry an `Option` of their own: `Option<&[Option<String>]>`)",
        )
    } else {
        return Ok(());
    };
    Err(syn::Error::new_spanned(
        ty,
        format!(
            "`{written}` is not the spelling `.aidl` renders for this argument — it renders \
             `{aidl}`{note}, and a call site written against one does not take the other; \
             use `{aidl}`"
        ),
    ))
}

/// Reject a reference anywhere in `ty`, itself included.
pub fn reject_any_reference(ty: &Type) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    if let Type::Reference(_) = ty {
        return Err(syn::Error::new_spanned(
            ty,
            "a return value is decoded into a fresh owned value, so it cannot be a reference — \
             return `String`, `Vec<T>` or the owned type",
        ));
    }
    reject_inner_references(ty)
}

/// Reject any reference in `ty`, itself included; callers pass what sits below
/// the one `&` they allow.
pub fn reject_inner_references(ty: &Type) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    match ty {
        Type::Reference(_) => Err(syn::Error::new_spanned(
            ty,
            "a borrowed type nested inside another type is not supported — the decoded value is \
             owned, so there is nothing for it to borrow from; use the owned form",
        )),
        Type::Slice(s) => reject_inner_references(&s.elem),
        Type::Array(a) => reject_inner_references(&a.elem),
        Type::Path(p) => {
            for seg in &p.path.segments {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    for arg in &args.args {
                        if let GenericArgument::Type(t) = arg {
                            reject_inner_references(t)?;
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Out init for an array past length 32, where `Default` stops (`fixed_array_default`).
pub fn out_default(ty: &Type) -> syn::Result<Option<String>> {
    let mut inner = unwrap_group(ty);
    if let Type::Reference(r) = inner {
        inner = unwrap_group(&r.elem);
    }
    let mut dims = 0usize;
    let mut oversized = false;
    while let Type::Array(a) = inner {
        dims += 1;
        // A named constant would silently fall back to `Default::default()`, which stops at 32.
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(n),
            ..
        }) = &a.len
        else {
            return Err(syn::Error::new_spanned(
                &a.len,
                "a fixed-array length must be an integer literal here — the macro cannot \
                 evaluate a constant, so it cannot tell whether the out parameter needs \
                 `std::array::from_fn` instead of `Default::default()`",
            ));
        };
        if n.base10_parse::<usize>()? > 32 {
            oversized = true;
        }
        inner = unwrap_group(&a.elem);
    }
    if !oversized {
        return Ok(None);
    }
    let mut init = "Default::default()".to_string();
    for _ in 0..dims {
        init = format!("std::array::from_fn(|_| {init})");
    }
    Ok(Some(init))
}

/// The type behind any number of `&`/`&mut`.
fn peel(ty: &Type) -> &Type {
    match unwrap_group(ty) {
        Type::Reference(r) => peel(&r.elem),
        other => other,
    }
}

/// `Path` segment whose last ident is `name`, with its generic arguments.
fn named_generic<'a>(ty: &'a Type, name: &str) -> Option<&'a syn::AngleBracketedGenericArguments> {
    let Type::Path(p) = unwrap_group(ty) else {
        return None;
    };
    let seg = p.path.segments.last()?;
    if seg.ident != name {
        return None;
    }
    match &seg.arguments {
        PathArguments::AngleBracketed(args) => Some(args),
        _ => None,
    }
}

fn first_type_arg(args: &syn::AngleBracketedGenericArguments) -> Option<&Type> {
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// `Vec<T>` behind any references, matched structurally so `std::vec::Vec<T>` counts.
pub fn vec_elem(ty: &Type) -> Option<&Type> {
    named_generic(peel(ty), "Vec").and_then(first_type_arg)
}

/// `Option<Vec<T>>`, ignoring any leading references.
pub fn option_vec_elem(ty: &Type) -> Option<&Type> {
    let inner = named_generic(peel(ty), "Option").and_then(first_type_arg)?;
    vec_elem(inner)
}

/// A length-carrying out parameter: `Vec<T>` or `Option<Vec<T>>`.
pub fn is_variable_array(ty: &Type) -> bool {
    vec_elem(ty).is_some() || option_vec_elem(ty).is_some()
}

/// `Option<T>`, ignoring any leading references.
fn option_inner(ty: &Type) -> Option<&Type> {
    named_generic(peel(ty), "Option").and_then(first_type_arg)
}

/// An `Option<ParcelFileDescriptor>` array element.
fn is_option_pfd(ty: &Type) -> bool {
    let Some(inner) = named_generic(unwrap_group(ty), "Option").and_then(first_type_arg) else {
        return false;
    };
    plain_name(inner).is_some_and(|n| n == "ParcelFileDescriptor")
}

/// The out-fd-array guard's `.flatten()` count (`TypeGenerator::out_array_null_guard_flatten`).
pub fn out_pfd_null_guard(ty: &Type) -> Option<usize> {
    let ty = peel(ty);
    if let Some(elem) = vec_elem(ty) {
        return is_option_pfd(elem).then_some(0);
    }
    let mut dims = 0usize;
    let mut elem = ty;
    while let Type::Array(a) = unwrap_group(elem) {
        dims += 1;
        elem = &a.elem;
    }
    (dims > 0 && is_option_pfd(elem)).then(|| dims - 1)
}

/// `ParcelableHolder` anywhere in `ty`, however wrapped.
pub fn mentions_parcelable_holder(ty: &Type) -> bool {
    match unwrap_group(ty) {
        Type::Path(p) => p.path.segments.iter().any(|seg| {
            seg.ident == "ParcelableHolder"
                || match &seg.arguments {
                    PathArguments::AngleBracketed(args) => args.args.iter().any(
                        |a| matches!(a, GenericArgument::Type(t) if mentions_parcelable_holder(t)),
                    ),
                    _ => false,
                }
        }),
        Type::Reference(r) => mentions_parcelable_holder(&r.elem),
        Type::Slice(s) => mentions_parcelable_holder(&s.elem),
        Type::Array(a) => mentions_parcelable_holder(&a.elem),
        _ => false,
    }
}

/// AIDL scalars: `in` only and never `@nullable` (AOSP `GetArgumentAspect`, `CheckValid`).
const PRIMITIVE_NAMES: &[&str] = &[
    "bool", "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "f32", "f64", "char",
];

/// The scalar names `TypeGenerator::type_decl` renders; `u8` is an array
/// element only, where `array_type_name` rewrites `byte`'s `i8`.
const RENDERABLE_SCALARS: &[&str] = &["bool", "i8", "i32", "i64", "f32", "f64", "u16"];

/// Rust's own scalar spellings, so a name outside `RENDERABLE_SCALARS` is
/// refused as a scalar rather than taken for a user-defined type.
const RUST_SCALARS: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32",
    "u64", "u128", "usize",
];

/// The last path segment of a bare, un-parameterised named type.
fn plain_name(ty: &Type) -> Option<String> {
    let Type::Path(p) = unwrap_group(ty) else {
        return None;
    };
    let seg = p.path.segments.last()?;
    if !matches!(seg.arguments, PathArguments::None) {
        return None;
    }
    Some(seg.ident.to_string())
}

fn is_primitive(ty: &Type) -> bool {
    plain_name(ty).is_some_and(|n| PRIMITIVE_NAMES.contains(&n.as_str()))
}

fn is_string(ty: &Type) -> bool {
    plain_name(ty).is_some_and(|n| n == "String" || n == "str")
}

/// A binder object or fd, which an out parameter has no `Default` to start from.
fn lacks_default(ty: &Type) -> bool {
    plain_name(ty).is_some_and(|n| n == "ParcelFileDescriptor" || n == "SIBinder")
        || named_generic(ty, "Strong").is_some()
}

/// `&mut Option<T>` where the `Option` is the generator's, not a `@nullable`:
/// the one spelling `out T` and `out @nullable T` share.
pub fn out_option_is_ambiguous(ty: &Type) -> bool {
    option_inner(peel(ty)).is_some_and(lacks_default)
}

/// The element an out array is sized or defaulted from, through every dimension.
fn out_array_elem(ty: &Type) -> Option<&Type> {
    let mut elem = option_vec_elem(ty).or_else(|| vec_elem(ty));
    let mut t = elem.unwrap_or(ty);
    while let Type::Array(a) = unwrap_group(t) {
        t = &a.elem;
        elem = Some(t);
    }
    elem
}

/// Reject an array element spelling `.aidl` does not render for this direction.
pub fn check_array_elements(ty: &Type, direction: &str) -> syn::Result<()> {
    let outer = unwrap_group(peel(ty));
    let nullable = option_inner(outer);
    let start = nullable.map_or(outer, |inner| unwrap_group(peel(inner)));
    let fixed = matches!(start, Type::Array(_));

    let mut elem = start;
    let mut is_array = false;
    loop {
        if let Some(inner) = vec_elem(elem) {
            elem = unwrap_group(inner);
            is_array = true;
            continue;
        }
        match unwrap_group(elem) {
            Type::Slice(s) => {
                elem = unwrap_group(&s.elem);
                is_array = true;
            }
            Type::Array(a) => {
                elem = unwrap_group(&a.elem);
                is_array = true;
            }
            _ => break,
        }
    }
    if !is_array {
        return Ok(());
    }
    // A return type's direction is `Direction::None`, which `list_type_decl` and
    // `make_fixed_array` both render down the same arm as `in`.
    let in_like = matches!(direction, "in" | "return");
    let field = direction == "field";
    let (article, label) = match direction {
        "return" => ("a", "returned".to_string()),
        "field" => ("a", "parcelable field".to_string()),
        d => ("an", format!("`{d}`")),
    };
    // A `@nullable` array wraps every non-primitive element except in a
    // fixed-size `in` one; a bare array only where the callee defaults a slot.
    // A field has no direction: `make_fixed_array`'s `is_struct` arm wraps a
    // fixed slot with no `Default` of its own, whatever the nullability.
    let wraps = if field {
        nullable.is_some() || (fixed && lacks_default(option_inner(elem).unwrap_or(elem)))
    } else if nullable.is_some() {
        !(in_like && fixed)
    } else {
        match direction {
            "out" => true,
            "inout" => fixed,
            _ => false,
        }
    };
    let Some(under) = option_inner(elem) else {
        if wraps && (is_string(elem) || lacks_default(elem)) {
            if nullable.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "a nullable {label} array of this type needs `Option<_>` elements — \
                         `.aidl` gives every element of a `@nullable` array its own `Option` \
                         unless the element is a primitive"
                    ),
                ));
            }
            // Only as a field: an `out`/`inout` one gets `check_out_capable`'s message.
            if field {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "a fixed-size {label} array of this type needs `Option<_>` elements — \
                         the field has no value to start each slot from, so that is what \
                         `.aidl` renders"
                    ),
                ));
            }
        }
        return Ok(());
    };
    if wraps && (nullable.is_some() || lacks_default(under)) {
        return Ok(());
    }
    // Only reachable as `in_like && fixed`, the one `@nullable` array that stays bare.
    if nullable.is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "a `@nullable` fixed-size {label} array keeps its elements bare — it is the one \
                 `@nullable` array `.aidl` leaves alone, rendering `@nullable T[N]` as \
                 `Option<&[T; N]>` (`Option<[T; N]>` for a return); drop the element `Option`"
            ),
        ));
    }
    Err(syn::Error::new_spanned(
        ty,
        format!(
            "{article} {label} array cannot have `Option<_>` elements — `.aidl` gives an element \
             its own `Option` in a `@nullable` array (except a fixed-size `in` or returned one, \
             which keeps them bare) and where the slot has no value to start from (an `out` \
             binder or fd array, a fixed-size `#[inout]` one, or a fixed-size parcelable field \
             array of a binder or fd); this array is neither, so drop the element `Option`"
        ),
    ))
}

/// Reject an `out`/`inout` parameter whose type `.aidl` passes `in` only.
pub fn check_out_capable(ty: &Type, direction: &str) -> syn::Result<()> {
    let inner = peel(ty);
    // An out parameter's own `Option` can hide a `String`.
    let named = option_inner(inner).unwrap_or(inner);
    let in_only = if is_primitive(named) {
        "a primitive"
    } else if is_string(named) {
        "`String`"
    } else {
        // A fixed-size `inout` array declares each slot the way an `out` one does.
        let fixed = matches!(inner, Type::Array(_));
        if (direction == "out" || (direction == "inout" && fixed))
            && out_array_elem(inner).is_some_and(lacks_default)
        {
            return Err(syn::Error::new_spanned(
                ty,
                format!(
                    "an `{direction}` array of this type needs `Option<_>` elements — the callee \
                     has no value to start each one from, so that is what `.aidl` renders"
                ),
            ));
        }
        return Ok(());
    };
    Err(syn::Error::new_spanned(
        ty,
        format!(
            "{in_only} cannot be an `{direction}` parameter — `.aidl` passes it only `in`; \
             return it instead, or use a `Vec<T>` or a parcelable"
        ),
    ))
}

/// Reject `Option<T>` over a scalar, which has no null form on the wire.
pub fn reject_nullable_primitive(ty: &Type) -> syn::Result<()> {
    let ty = peel(ty);
    if let Some(inner) = option_inner(ty) {
        if is_primitive(peel(inner)) {
            return Err(syn::Error::new_spanned(
                ty,
                "a primitive has no null form on the wire, so it cannot be nullable — \
                 `.aidl` rejects `@nullable` on one too; use the bare type",
            ));
        }
    }
    // `&[Option<i32>]` is the `Vec<Option<i32>>` shape, so slices and arrays recurse too.
    match ty {
        Type::Path(p) => {
            for seg in &p.path.segments {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    for arg in &args.args {
                        if let GenericArgument::Type(t) = arg {
                            reject_nullable_primitive(t)?;
                        }
                    }
                }
            }
        }
        Type::Slice(s) => reject_nullable_primitive(&s.elem)?,
        Type::Array(a) => reject_nullable_primitive(&a.elem)?,
        _ => {}
    }
    Ok(())
}
