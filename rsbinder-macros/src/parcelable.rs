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

    // A derive adds to a type, it does not replace it: the struct and the
    // `Default` impl in the rendered module are the user's to write (and
    // `#[derive(Default)]` right next to this one is the normal way). Keep
    // only the codec — which is the part that has to agree with `.aidl`
    // byte for byte, and the part nobody should hand-write.
    let kept: Vec<_> = items.into_iter().filter(is_codec_item).collect();

    Ok(quote! { #(#kept)* })
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
        let ident = field.ident.as_ref().expect("named field").to_string();
        if matches!(field.ty, syn::Type::Reference(_)) {
            return Err(syn::Error::new_spanned(
                &field.ty,
                "a parcelable field cannot be a reference — it owns what it carries",
            ));
        }
        members.push((
            ident,
            type_str::as_written(&field.ty)?,
            // Only `impl Default` reads this, and that impl is dropped: the
            // user derives or writes `Default` themselves.
            "Default::default()".to_string(),
            // `ParcelableHolder` fields and non-nullable binder fields are
            // `.aidl`-only shapes (see the crate docs) — a derived parcelable
            // spells a nullable binder field `Option<…>` and gets the plain
            // read/write path.
            false,
            false,
        ));
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
            struct Bad<'a> { s: &'a str }
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("generic") || err.to_string().contains("reference"));
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
