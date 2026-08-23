// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `#[derive(Parcelable)]` — the parcel codec for a plain Rust struct.

use proc_macro2::TokenStream;
use quote::quote;
use rsbinder_aidl::render::{render_parcelable, ParcelableRender};
use syn::{Data, DeriveInput, Fields};

use crate::type_str;

pub fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let rendered = render_source(input)?;

    let file = syn::parse_file(&rendered).map_err(|e| {
        syn::Error::new_spanned(
            &input.ident,
            format!("generated code did not parse ({e}); generated source follows:\n{rendered}"),
        )
    })?;
    let Some(syn::Item::Mod(module)) = file.items.into_iter().next() else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "generated code was not a single module (generator contract changed)",
        ));
    };
    let items = module.content.map(|(_, items)| items).unwrap_or_default();

    // A derive adds to a type, it cannot redeclare it: keep the codec, drop
    // the struct and `Default` that the user writes.
    let kept: Vec<_> = items.into_iter().filter(is_codec_item).collect();

    // The retained `impl_deserialize_for_parcelable!` calls `Self::default`.
    let name = &input.ident;
    Ok(quote! {
        const _: fn() = || {
            fn __rsbinder_assert_default<T: ::core::default::Default>() {}
            __rsbinder_assert_default::<#name>();
        };
        #(#kept)*
    })
}

fn is_codec_item(item: &syn::Item) -> bool {
    match item {
        syn::Item::Impl(i) => !matches!(
            i.trait_.as_ref().and_then(|(_, p, _)| p.segments.last()),
            Some(seg) if seg.ident == "Default"
        ),
        syn::Item::Macro(_) => true,
        _ => false,
    }
}

/// The parcelable module source, before the non-codec items are dropped.
/// Split out so the golden test can hold it against the `.aidl` output.
pub(crate) fn render_source(input: &DeriveInput) -> syn::Result<String> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "a parcelable cannot be generic — the wire carries no type parameter",
        ));
    }
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(Parcelable)] applies to structs; use #[derive(BinderEnum)] for enums, \
             and `.aidl` for unions",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a parcelable needs named fields — field names are its wire order and its API",
        ));
    };

    let mut members = Vec::new();
    for field in &fields.named {
        // `Ident`'s `Display` keeps the `r#`, and the template adds its own.
        let raw = field.ident.as_ref().expect("named field").to_string();
        let ident = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
        if matches!(field.ty, syn::Type::Reference(_)) {
            return Err(syn::Error::new_spanned(
                &field.ty,
                "a parcelable field cannot be a reference — it owns what it carries",
            ));
        }
        let decl = type_str::as_written(&field.ty)?;
        if decl.split(['<', ':']).next_back().unwrap_or(&decl).trim() == "ParcelableHolder" {
            return Err(syn::Error::new_spanned(
                &field.ty,
                "a `ParcelableHolder` field needs the `.aidl` path: its stability is set before \
                 the read and the derived codec would replace the whole field, so a peer's \
                 `@VintfStability` holder can never be decoded",
            ));
        }
        // Read only by the `impl Default` that is dropped below.
        members.push((ident, decl, "Default::default()".to_string(), false, false));
    }

    let descriptor = crate::attr_descriptor(&input.attrs, "parcelable")?
        .unwrap_or_else(|| input.ident.to_string());

    render_parcelable(&ParcelableRender {
        crate_name: "rsbinder".to_string(),
        module: input.ident.to_string(),
        name: input.ident.to_string(),
        namespace: descriptor,
        members,
        ..Default::default()
    })
    .map(|s| s.trim().to_string())
    .map_err(|e| syn::Error::new_spanned(&input.ident, format!("codegen failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(tokens: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(tokens).expect("parse");
        render_source(&input).expect("render")
    }

    #[test]
    fn rejects_tuple_struct() {
        let input: DeriveInput = syn::parse2(quote! {
            struct Bad(i32);
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("named fields"), "{err}");
    }

    #[test]
    fn rejects_enum() {
        let input: DeriveInput = syn::parse2(quote! {
            enum Bad { A }
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("BinderEnum"), "{err}");
    }

    #[test]
    fn rejects_reference_field() {
        let input: DeriveInput = syn::parse2(quote! {
            struct Bad { s: &'static str }
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("reference"), "{err}");
    }

    #[test]
    fn rejects_parcelable_holder_field() {
        let input: DeriveInput = syn::parse2(quote! {
            struct Bad { ext: rsbinder::ParcelableHolder }
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("`.aidl` path"), "{err}");
    }

    #[test]
    fn raw_identifier_field_is_not_double_escaped() {
        let s = source(quote! {
            struct Event { r#type: i32 }
        });
        assert!(s.contains("pub r#type: i32"), "{s}");
        assert!(!s.contains("r#r#"), "{s}");
    }

    #[test]
    fn descriptor_defaults_to_the_type_name() {
        let s = source(quote! {
            struct Config { name: String }
        });
        assert!(
            s.contains(r#"fn descriptor() -> &'static str { "Config" }"#),
            "{s}"
        );
    }

    #[test]
    fn descriptor_attribute_wins() {
        let s = source(quote! {
            #[parcelable(descriptor = "com.example.Config")]
            struct Config { name: String }
        });
        assert!(
            s.contains(r#"fn descriptor() -> &'static str { "com.example.Config" }"#),
            "{s}"
        );
    }

    #[test]
    fn fields_keep_declaration_order() {
        let s = source(quote! {
            struct Config { a: i32, b: String, c: Option<Vec<u8>> }
        });
        let a = s.find("self.r#a").expect("a");
        let b = s.find("self.r#b").expect("b");
        let c = s.find("self.r#c").expect("c");
        assert!(
            a < b && b < c,
            "wire order must follow declaration order:\n{s}"
        );
    }
}
