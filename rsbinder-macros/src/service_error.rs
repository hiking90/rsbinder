// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `#[derive(ServiceSpecificError)]` — a plain Rust enum as binder
//! service-specific error codes.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident};

/// Reprs whose whole range fits the `i32` a `Status` carries. `i64` is
/// absent on purpose: a code that does not fit would go on the wire
/// truncated.
const REPRS: [&str; 3] = ["i8", "i16", "i32"];

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
    // Checked but not otherwise used: the cast below is `as i32`, so a
    // repr that does not fit i32 is the one thing that could silently
    // change a code between the declaration and the wire.
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

    // Cast the variant rather than re-emitting its discriminant
    // expression: re-emitted, `1 << 8` on a `#[repr(i8)]` enum would be
    // typed on its own and disagree with what the variant actually is.
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

/// The `#[repr(..)]`, which decides whether a declared code can reach the
/// wire unchanged.
fn require_repr(input: &DeriveInput) -> syn::Result<Ident> {
    let mut found = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        // Token scan, not `parse_nested_meta`, for the same reason as
        // `binder_enum`: the callback would have to consume `align(8)`'s
        // argument list, and failing to leaves syn reporting `expected ,`.
        let Ok(list) = attr.meta.require_list() else {
            continue;
        };
        for token in list.tokens.clone() {
            if let proc_macro2::TokenTree::Ident(ident) = token {
                if REPRS.contains(&ident.to_string().as_str()) {
                    found = Some(ident);
                }
            }
        }
    }
    found.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "a service-specific error enum needs `#[repr(i8)]`, `#[repr(i16)]` or \
             `#[repr(i32)]` — a binder status carries the code as an i32, so a wider repr \
             would truncate on the wire rather than fail here",
        )
    })
}
