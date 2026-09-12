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

/// Reject what [`owned`] cannot lend back (`&[&str]`); `Option<&str>` is AIDL's nullable `in`.
pub fn check_supported(ty: &Type) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    reject_non_argument(ty)?;
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
            reject_non_argument(&r.elem)?;
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
                                     lifetime — use `Option<&str>` or `Option<&[T]>`",
                                ));
                            }
                            reject_non_argument(&inner.elem)?;
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
fn reject_non_argument(ty: &Type) -> syn::Result<()> {
    match unwrap_group(ty) {
        Type::Tuple(t) if t.elems.is_empty() => Err(syn::Error::new_spanned(
            ty,
            "`()` is not an argument type — `.aidl` accepts `void` only as a return type",
        )),
        Type::TraitObject(_) => Err(syn::Error::new_spanned(
            ty,
            "a trait object has no wire form — pass a binder as `&rsbinder::Strong<dyn IFoo>`",
        )),
        _ => Ok(()),
    }
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

/// Reject an array whose elements are `Option<_>` where `.aidl` renders them bare.
///
/// An element `Option` is the generator's, not a `@nullable`: it appears only
/// where the callee has to default each slot, which is an `out` array or a
/// fixed-size `inout` one, and only for a binder or a fd. A `@nullable` array
/// is `Option<Vec<_>>` / `Option<[_; N]>`, and what it does to its elements
/// depends on the element type, which a name alone does not give — so a
/// wrapped array is left alone.
pub fn check_array_elements(ty: &Type, direction: &str) -> syn::Result<()> {
    let outer = unwrap_group(peel(ty));
    if option_inner(outer).is_some() {
        return Ok(());
    }
    let fixed = matches!(outer, Type::Array(_));

    let mut elem = outer;
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
    let Some(under) = option_inner(elem) else {
        return Ok(());
    };
    let renderable = lacks_default(under)
        && match direction {
            "out" => true,
            "inout" => fixed,
            _ => false,
        };
    if renderable {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        ty,
        format!(
            "an `{direction}` array cannot have `Option<_>` elements — `.aidl` spells a nullable \
             array `Option<Vec<_>>`, and gives elements their own `Option` only where the callee \
             must default each one: an `out` binder or fd array, or a fixed-size `#[inout]` one"
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
        // `inner`, not `named`: `&mut Option<_>` is the legal spelling.
        let needs_option = direction == "out" && lacks_default(inner);
        if needs_option {
            return Err(syn::Error::new_spanned(
                ty,
                "an `out` parameter of this type has to be spelled `&mut Option<_>` — the \
                 callee has no value to start from, so that is what `.aidl` renders; add the \
                 `Option` (`.aidl`'s `out @nullable`), or make it `#[inout]`",
            ));
        }
        if direction == "out" && out_array_elem(inner).is_some_and(lacks_default) {
            return Err(syn::Error::new_spanned(
                ty,
                "an `out` array of this type needs `Option<_>` elements — the callee has no \
                 value to start each one from, so that is what `.aidl` renders",
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
