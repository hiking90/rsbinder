// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Rendering `syn::Type` back to the exact spelling `rsbinder-aidl` emits.
//!
//! The generated code is compared against the `.aidl` path **as text** (plan
//! 2-19 P3 golden gate), so `quote!`'s spacing (`& str`, `Vec < i32 >`) will
//! not do. These printers reproduce `TypeGenerator::type_declaration`'s
//! formatting instead.

use syn::{GenericArgument, PathArguments, Type};

/// What the caller was expanding, so an unsupported type names the macro the
/// user actually wrote.
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

/// Strip the invisible delimiters `syn` wraps a macro-interpolated `$t:ty`
/// in, and any explicit parentheses. Without this a perfectly ordinary type
/// reaches the matches below as `Type::Group` and is refused as unsupported.
pub fn unwrap_group(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => unwrap_group(&g.elem),
        Type::Paren(p) => unwrap_group(&p.elem),
        other => other,
    }
}

/// The type exactly as the signature spells it — `rsbinder-aidl`'s
/// `type_decl_for_func`.
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
            reject_self_path(p)?;
            let mut out = String::new();
            for (i, seg) in p.path.segments.iter().enumerate() {
                if i > 0 || p.path.leading_colon.is_some() {
                    out.push_str("::");
                }
                out.push_str(&seg.ident.to_string());
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

/// The rendered body lands one module deeper than where the user wrote it, so
/// `self::` would name that generated module instead of theirs.
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

/// The owned storage form (`rsbinder-aidl`'s `type_declaration(false)`):
/// references collapse to what they borrow, inside `Option`/`Vec` too.
pub fn owned(ty: &Type) -> syn::Result<String> {
    let ty = unwrap_group(ty);
    Ok(match ty {
        Type::Reference(r) => match &*r.elem {
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

/// Reject shapes whose [`owned`] form cannot be handed back to the signature
/// (`&[&str]`), where rustc would blame generated tokens instead of the type.
/// `Option<&str>` / `Option<&[T]>` stay legal — AIDL's nullable in argument.
pub fn check_supported(ty: &Type) -> syn::Result<()> {
    let ty = unwrap_group(ty);
    match ty {
        Type::Reference(r) => {
            if r.lifetime.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "an explicit lifetime is not supported — the generated trait has none to \
                     bind it to; use a plain `&T`",
                ));
            }
            reject_inner_references(&r.elem)
        }
        Type::Path(p) => {
            let last = p.path.segments.last();
            if let Some(seg) = last {
                if seg.ident == "Option" {
                    if let PathArguments::AngleBracketed(args) = &seg.arguments {
                        if let Some(GenericArgument::Type(Type::Reference(inner))) =
                            args.args.first()
                        {
                            if inner.lifetime.is_some() || inner.mutability.is_some() {
                                return Err(syn::Error::new_spanned(
                                    ty,
                                    "a nullable argument borrows immutably and without a named \
                                     lifetime — use `Option<&str>` or `Option<&[T]>`",
                                ));
                            }
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

/// Reject a reference anywhere in `ty`, itself included — a return value is
/// decoded into a fresh owned value with nothing to borrow from.
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

fn reject_inner_references(ty: &Type) -> syn::Result<()> {
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

/// Initializer for an out parameter whose `Default::default()` would not
/// compile: `Default` stops at length 32 (`TypeGenerator::fixed_array_default`).
pub fn out_default(ty: &Type) -> syn::Result<Option<String>> {
    let mut inner = unwrap_group(ty);
    if let Type::Reference(r) = inner {
        inner = unwrap_group(&r.elem);
    }
    let mut dims = 0usize;
    let mut oversized = false;
    while let Type::Array(a) = inner {
        dims += 1;
        // A named constant would leave `oversized` false and fall back to
        // `Default::default()`, which stops at 32 — the very failure this
        // function exists to avoid, reported against generated tokens. The
        // macro cannot evaluate the constant, so it asks for a literal.
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

/// `Vec<T>`, ignoring any leading references. Structural on purpose: the wire
/// decision this drives must not hinge on how the user spelled the path
/// (`std::vec::Vec<T>` is the same type).
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

/// `Vec<Option<ParcelFileDescriptor>>`, the shape whose write-back needs
/// `.aidl`'s `UNEXPECTED_NULL` guard.
pub fn is_option_pfd_vec(ty: &Type) -> bool {
    let Some(elem) = vec_elem(ty) else {
        return false;
    };
    let Some(inner) = named_generic(elem, "Option").and_then(first_type_arg) else {
        return false;
    };
    matches!(inner, Type::Path(p)
        if p.path.segments.last().is_some_and(|s| s.ident == "ParcelFileDescriptor"))
}

/// Rust spellings of the AIDL types that are scalars on the wire. AOSP's
/// `AidlTypenames::GetArgumentAspect` gives every non-array builtin `in` as its
/// only direction and `AidlTypeSpecifier::CheckValid` refuses `@nullable` on a
/// primitive; `rsbinder-aidl` matches both. `String` shares the direction rule
/// but not the nullability one, so it is listed separately.
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

/// Reject an `out`/`inout` parameter whose type AIDL only ever passes `in`.
///
/// Accepting `&mut String` here would build an interface no `.aidl` can
/// express, breaking the promise that a trait and the equivalent `.aidl`
/// generate the same code. The types that stay legal are the ones AIDL gives a
/// wider direction to: arrays, parcelables and unions.
pub fn check_out_capable(ty: &Type, direction: &str) -> syn::Result<()> {
    let inner = peel(ty);
    let kind = if is_primitive(inner) {
        "a primitive"
    } else if is_string(inner) {
        "`String`"
    } else if plain_name(inner).is_some_and(|n| n == "ParcelFileDescriptor") && direction == "out" {
        // AOSP allows `inout` here but not `out`: a fd is not
        // default-constructible, so there is nothing to hand the callee.
        "`ParcelFileDescriptor`"
    } else {
        return Ok(());
    };
    Err(syn::Error::new_spanned(
        ty,
        format!(
            "{kind} cannot be an `{direction}` parameter — `.aidl` passes it only `in`; \
             return it instead, or use a `Vec<T>` or a parcelable"
        ),
    ))
}

/// Reject `Option<T>` over a type with no null form on the wire.
///
/// `@nullable` is how AIDL spells `Option`, and AIDL allows it only where the
/// wire has a null to write. A scalar has none, so `Option<i32>` is a shape no
/// `.aidl` can express.
pub fn reject_nullable_primitive(ty: &Type) -> syn::Result<()> {
    if let Some(args) = named_generic(peel(ty), "Option") {
        if let Some(inner) = first_type_arg(args) {
            if is_primitive(peel(inner)) {
                return Err(syn::Error::new_spanned(
                    ty,
                    "a primitive has no null form on the wire, so it cannot be nullable — \
                     `.aidl` rejects `@nullable` on one too; use the bare type",
                ));
            }
        }
    }
    let Type::Path(p) = peel(ty) else {
        return Ok(());
    };
    for seg in &p.path.segments {
        if let PathArguments::AngleBracketed(args) = &seg.arguments {
            for arg in &args.args {
                if let GenericArgument::Type(t) = arg {
                    reject_nullable_primitive(t)?;
                }
            }
        }
    }
    Ok(())
}
