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
