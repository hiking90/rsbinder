// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `#[derive(ServiceSpecificError)]` — a plain Rust enum as binder
//! service-specific error codes.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident};

/// Signed reprs whose whole range fits the `i32` a `Status` carries; `i64` would truncate.
const REPRS: [&str; 3] = ["i8", "i16", "i32"];

/// Int reprs a user plausibly writes and this derive refuses, echoed in the diagnostic.
const REJECTED_REPRS: [&str; 9] = [
    "u8", "u16", "u32", "u64", "u128", "usize", "i64", "i128", "isize",
];

pub fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "a service-specific error cannot be generic — the wire carries one i32",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(ServiceSpecificError)] applies to enums — an error code is one of a \
             fixed set of values",
        ));
    };
    // Only checked: the cast below is `as i32`, which a wider repr would silently truncate.
    require_repr(input)?;

    let name = &input.ident;
    let mut variants = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "a service-specific error variant carries no data — only its code goes on \
                 the wire. Put the detail in the message, or wait for a payload envelope",
            ));
        }
        if variant.discriminant.is_none() {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "every variant needs an explicit value: it is the code a peer matches on, so \
                 it must be visible here rather than implied by declaration order",
            ));
        }
        variants.push(variant.ident.clone());
    }
    if variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "an empty error enum has no code it could ever carry",
        ));
    }

    // Cast the variant: a re-emitted discriminant (`1 << 8`) is typed apart from the repr.
    let code_arms = variants
        .iter()
        .map(|ident| quote! { #name::#ident => #name::#ident as i32, });
    let from_arms = variants.iter().map(|ident| {
        quote! { value if value == #name::#ident as i32 => ::core::option::Option::Some(#name::#ident), }
    });

    Ok(quote! {
        impl rsbinder::ServiceSpecificError for #name {
            fn code(&self) -> i32 {
                match self { #(#code_arms)* }
            }

            fn from_code(code: i32) -> ::core::option::Option<Self> {
                match code {
                    #(#from_arms)*
                    _ => ::core::option::Option::None,
                }
            }
        }
    })
}

/// The `#[repr(..)]`, which decides whether a declared code can reach the wire unchanged.
fn require_repr(input: &DeriveInput) -> syn::Result<Ident> {
    let mut found = None;
    // Kept apart from "none" so the refusal can name its reason: unsigned, wider, or `isize`.
    let mut rejected: Option<Ident> = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        // Token scan: see `binder_enum::backing_type`.
        let Ok(list) = attr.meta.require_list() else {
            continue;
        };
        for token in list.tokens.clone() {
            if let proc_macro2::TokenTree::Ident(ident) = token {
                let name = ident.to_string();
                if REPRS.contains(&name.as_str()) {
                    found = Some(ident);
                } else if REJECTED_REPRS.contains(&name.as_str()) {
                    rejected = Some(ident);
                }
            }
        }
    }
    if let Some(ident) = found {
        return Ok(ident);
    }
    let needs = "a service-specific error enum needs `#[repr(i8)]`, `#[repr(i16)]` or \
                 `#[repr(i32)]`";
    let msg = match &rejected {
        Some(repr) if repr.to_string().starts_with('u') => {
            format!("{needs}, found `#[repr({repr})]`, which is unsigned")
        }
        Some(repr) if *repr == "isize" => format!(
            "{needs}, found `#[repr(isize)]`, whose width depends on the target, so it is not \
             guaranteed to fit the `i32` a binder status carries"
        ),
        Some(repr) => format!(
            "{needs}, found `#[repr({repr})]`, which is wider than the `i32` a binder status \
             carries"
        ),
        None => format!("{needs}, found no integer repr"),
    };
    Err(syn::Error::new_spanned(&input.ident, msg))
}
