// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `#[derive(BinderEnum)]` — parcel a plain Rust enum as its backing scalar.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident};

/// The AIDL backing types (`byte`, `int`, `long`) and their Rust spellings.
const BACKINGS: [&str; 3] = ["i8", "i32", "i64"];

pub fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "a binder enum cannot be generic — the wire carries one scalar",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(BinderEnum)] applies to enums; use #[derive(Parcelable)] for structs",
        ));
    };

    let backing = backing_type(input)?;
    let name = &input.ident;

    let mut variants = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "a binder enum variant carries no data — only its value goes on the wire",
            ));
        }
        let Some((_, discriminant)) = &variant.discriminant else {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "every variant needs an explicit value: it is what goes on the wire, so it \
                 must be visible here rather than implied by declaration order",
            ));
        };
        // The discriminant is required so the wire value is visible at the
        // declaration, but it is never re-emitted: see `read_arms` below.
        let _ = discriminant;
        variants.push(variant.ident.clone());
    }
    if variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "an empty binder enum has no value it could ever carry",
        ));
    }

    // Cast the declared variant rather than re-emitting its discriminant
    // expression: re-emitted, the expression is typed on its own and an
    // integer literal falls back to `i32`, so `#[repr(i64)] A = 1 << 31`
    // would go on the wire as `-2147483648` while `A as i64` is
    // `2147483648`. A unit-only variant is guaranteed above, so the cast is
    // always valid.
    let read_arms = variants.iter().map(|ident| {
        quote! { v if v == #name::#ident as #backing => Ok(#name::#ident), }
    });
    // `self as #backing` would move out of `&self` unless the enum is `Copy`;
    // matching asks nothing of the user's type.
    let write_arms = variants.iter().map(|ident| {
        quote! { #name::#ident => #name::#ident as #backing, }
    });

    Ok(quote! {
        impl rsbinder::Serialize for #name {
            fn serialize(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
                parcel.write(&self.binder_value())
            }
        }

        impl rsbinder::SerializeArray for #name {
            fn serialize_array(
                slice: &[Self],
                parcel: &mut rsbinder::Parcel,
            ) -> rsbinder::Result<()> {
                let values: Vec<#backing> = slice.iter().map(#name::binder_value).collect();
                <#backing as rsbinder::SerializeArray>::serialize_array(&values, parcel)
            }
        }

        impl rsbinder::Deserialize for #name {
            fn deserialize(parcel: &mut rsbinder::Parcel) -> rsbinder::Result<Self> {
                let value: #backing = parcel.read()?;
                Self::try_from_binder_value(value)
            }
        }

        impl rsbinder::DeserializeArray for #name {
            fn deserialize_array(
                parcel: &mut rsbinder::Parcel,
            ) -> rsbinder::Result<Option<Vec<Self>>> {
                let values: Option<Vec<#backing>> =
                    <#backing as rsbinder::DeserializeArray>::deserialize_array(parcel)?;
                values
                    .map(|values| values.into_iter().map(Self::try_from_binder_value).collect())
                    .transpose()
            }
        }

        impl #name {
            /// The wire value of this variant.
            pub fn binder_value(&self) -> #backing {
                match self { #(#write_arms)* }
            }

            /// A wire value back to a variant.
            ///
            /// This enum is **closed**: a value no variant declares is
            /// `rsbinder::StatusCode::BadValue`, not a silently retained
            /// unknown. An `.aidl` enum is open — its generated newtype keeps
            /// whatever a newer peer sent — so reach for `.aidl` (or
            /// `rsbinder::declare_binder_enum!`) when the two ends can be
            /// different versions.
            pub fn try_from_binder_value(value: #backing) -> rsbinder::Result<Self> {
                match value {
                    #(#read_arms)*
                    _ => Err(rsbinder::StatusCode::BadValue),
                }
            }
        }
    })
}

/// The `#[repr(..)]` backing type, which is what actually goes on the wire.
fn backing_type(input: &DeriveInput) -> syn::Result<Ident> {
    let mut found = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        // Token scan, not `parse_nested_meta`: the callback would have to
        // consume `align(8)`'s argument list, and failing to leaves syn
        // reporting `expected ,` instead of anything about the backing type.
        let Ok(list) = attr.meta.require_list() else {
            continue;
        };
        for token in list.tokens.clone() {
            if let proc_macro2::TokenTree::Ident(ident) = token {
                if BACKINGS.contains(&ident.to_string().as_str()) {
                    found = Some(ident);
                }
            }
        }
    }
    found.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "a binder enum needs `#[repr(i8)]`, `#[repr(i32)]` or `#[repr(i64)]` — that \
             choice is the wire format (AIDL `byte`, `int`, `long`), not an implementation \
             detail",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_of(tokens: TokenStream) -> String {
        let input: DeriveInput = syn::parse2(tokens).expect("parse");
        expand(&input).unwrap_err().to_string()
    }

    #[test]
    fn requires_a_repr() {
        assert!(err_of(quote! { enum Mode { Fast = 0 } }).contains("repr(i8)"));
    }

    #[test]
    fn rejects_an_unsupported_repr() {
        assert!(err_of(quote! {
            #[repr(u8)]
            enum Mode { Fast = 0 }
        })
        .contains("repr(i8)"));
    }

    #[test]
    fn requires_explicit_values() {
        assert!(err_of(quote! {
            #[repr(i32)]
            enum Mode { Fast }
        })
        .contains("explicit value"));
    }

    #[test]
    fn rejects_data_carrying_variants() {
        assert!(err_of(quote! {
            #[repr(i32)]
            enum Mode { Fast(i32) = 0 }
        })
        .contains("carries no data"));
    }

    #[test]
    fn rejects_structs() {
        assert!(err_of(quote! { struct Mode { a: i32 } }).contains("Parcelable"));
    }

    #[test]
    fn emits_the_four_codec_impls() {
        let input: DeriveInput = syn::parse2(quote! {
            #[repr(i32)]
            enum Mode { Fast = 0, Safe = 1 }
        })
        .unwrap();
        let out = expand(&input).unwrap().to_string();
        for expected in [
            "impl rsbinder :: Serialize for Mode",
            "impl rsbinder :: SerializeArray for Mode",
            "impl rsbinder :: Deserialize for Mode",
            "impl rsbinder :: DeserializeArray for Mode",
        ] {
            assert!(out.contains(expected), "missing {expected} in:\n{out}");
        }
    }

    #[test]
    fn accepts_a_repr_with_extra_modifiers() {
        let input: DeriveInput = syn::parse2(quote! {
            #[repr(i32, align(8))]
            enum Mode { Fast = 0 }
        })
        .unwrap();
        assert!(expand(&input).is_ok());
    }

    #[test]
    fn does_not_require_copy() {
        // `self as #backing` would move out of `&self`; the emitted code must
        // match on the variant instead.
        let input: DeriveInput = syn::parse2(quote! {
            #[repr(i32)]
            enum Mode { Fast = 0, Safe = 1 }
        })
        .unwrap();
        let out = expand(&input).unwrap().to_string();
        assert!(!out.contains("* self"), "{out}");
        assert!(out.contains("match self"), "{out}");
    }

    /// The wire value must come from the declared variant, not from a
    /// re-emitted discriminant expression: re-emitted, an integer literal is
    /// typed on its own and falls back to `i32`, so `#[repr(i64)] A = 1 << 31`
    /// would go out as `-2147483648`.
    #[test]
    fn casts_the_variant_not_the_discriminant_expression() {
        let input: DeriveInput = syn::parse2(quote! {
            #[repr(i64)]
            enum Wide { A = 1 << 31 }
        })
        .unwrap();
        let out = expand(&input).unwrap().to_string();
        assert!(out.contains("Wide :: A as i64"), "{out}");
        assert!(!out.contains("1 << 31"), "{out}");
    }
}
