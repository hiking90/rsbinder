// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Rendering `syn::Type` back to the exact spelling `rsbinder-aidl` emits.
//!
//! The generated code is compared against the `.aidl` path **as text** (plan
//! 2-19 P3 golden gate), so `quote!`'s spacing (`& str`, `Vec < i32 >`) will
//! not do. These printers reproduce `TypeGenerator::type_declaration`'s
//! formatting instead.

use syn::{GenericArgument, PathArguments, Type};

/// The type exactly as the signature spells it — `rsbinder-aidl`'s
/// `type_decl_for_func`.
pub fn as_written(ty: &Type) -> syn::Result<String> {
    Ok(match ty {
        Type::Reference(r) => {
            let inner = as_written(&r.elem)?;
            if r.mutability.is_some() {
                format!("&mut {inner}")
            } else {
                format!("&{inner}")
            }
        }
        Type::Slice(s) => format!("[{}]", as_written(&s.elem)?),
        Type::Array(a) => {
            let len = &a.len;
            format!("[{}; {}]", as_written(&a.elem)?, quote::quote!(#len))
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
                            GenericArgument::Type(t) => rendered.push(as_written(t)?),
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
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "unsupported type in a #[rsbinder::interface] signature",
            ))
        }
    })
}

/// The owned storage form — `rsbinder-aidl`'s `type_declaration(false)`, the
/// type the server declares before `read()` and the proxy reads into.
///
/// References collapse to what they borrow from, inside `Option`/`Vec` too:
/// `&str` → `String`, `&[T]` → `Vec<T>`, `Option<&str>` → `Option<String>`.
pub fn owned(ty: &Type) -> syn::Result<String> {
    Ok(match ty {
        Type::Reference(r) => match &*r.elem {
            Type::Path(p) if p.path.is_ident("str") => "String".to_string(),
            Type::Slice(s) => format!("Vec<{}>", owned(&s.elem)?),
            inner => owned(inner)?,
        },
        Type::Path(p) => {
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

/// Reject the shapes whose owned form cannot be handed back to the signature.
///
/// [`owned`] folds references away, including inside `Option`/`Vec`, so a type
/// like `&[&str]` would have the server declare `Vec<String>` and then pass
/// `&Vec<String>` to a trait asking for `&[&str]`. The compiler would report
/// that against generated tokens with no span into the user's file, so refuse
/// it here instead, where the error can point at the type.
///
/// `Option<&str>` and `Option<&[T]>` stay legal: those are AIDL's nullable in
/// arguments, and `func_call_param` bridges them with `as_deref`.
pub fn check_supported(ty: &Type) -> syn::Result<()> {
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

fn reject_inner_references(ty: &Type) -> syn::Result<()> {
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
/// compile: `Default` stops at length 32, so a longer fixed array needs one
/// `std::array::from_fn` per dimension (mirrors `rsbinder-aidl`'s
/// `TypeGenerator::fixed_array_default`).
pub fn out_default(ty: &Type) -> Option<String> {
    let mut inner = ty;
    if let Type::Reference(r) = inner {
        inner = &r.elem;
    }
    let mut dims = 0usize;
    let mut oversized = false;
    while let Type::Array(a) = inner {
        dims += 1;
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(n),
            ..
        }) = &a.len
        {
            if n.base10_parse::<usize>().is_ok_and(|n| n > 32) {
                oversized = true;
            }
        }
        inner = &a.elem;
    }
    if !oversized {
        return None;
    }
    let mut init = "Default::default()".to_string();
    for _ in 0..dims {
        init = format!("std::array::from_fn(|_| {init})");
    }
    Some(init)
}
