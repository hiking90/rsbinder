// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `#[derive(Parcelable)]` — the parcel codec for a plain Rust struct.

use proc_macro2::TokenStream;
use quote::quote;
use rsbinder_aidl::render::{render_parcelable, ParcelableMember, ParcelableRender};
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
    let mut top = file.items.into_iter();
    let (Some(syn::Item::Mod(module)), None) = (top.next(), top.next()) else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "generated code was not a single module (generator contract changed)",
        ));
    };
    // Only what the codec can raise: the template's blanket allow would clash with a `forbid`.
    let deprecated = input.attrs.iter().any(|a| a.path().is_ident("deprecated"))
        || matches!(&input.data, Data::Struct(s)
            if s.fields.iter().any(|f| f.attrs.iter().any(|a| a.path().is_ident("deprecated"))));
    let allow = deprecated.then(|| quote!(#[allow(deprecated)]));
    let items = module.content.map(|(_, items)| items).unwrap_or_default();

    // A derive cannot redeclare the struct or its `Default`; keep only the codec.
    let kept: Vec<_> = items.into_iter().filter(is_codec_item).collect();

    // The retained `impl_deserialize_for_parcelable!` calls `Self::default`.
    let name = &input.ident;
    Ok(quote! {
        #allow
        const _: () = {
            const _: fn() = || {
                fn __rsbinder_assert_default<T: ::core::default::Default>() {}
                __rsbinder_assert_default::<#name>();
            };
            #(#kept)*
        };
    })
}

fn is_codec_item(item: &syn::Item) -> bool {
    match item {
        syn::Item::Impl(i) => !matches!(
            i.trait_.as_ref().and_then(|(p, _)| p.segments.last()),
            Some(seg) if seg.ident == "Default"
        ),
        syn::Item::Macro(_) => true,
        _ => false,
    }
}

/// The module source before the non-codec items are dropped, split out for the golden test.
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
        let ident = crate::strip_raw(&raw).to_string();
        // rustc accepts the helper on a field too, but only the struct's is read.
        if let Some(attr) = field.attrs.iter().find(|a| a.path().is_ident("parcelable")) {
            return Err(syn::Error::new_spanned(
                attr,
                "#[parcelable(..)] belongs on the struct, not on a field",
            ));
        }
        if matches!(type_str::unwrap_group(&field.ty), syn::Type::Reference(_)) {
            return Err(syn::Error::new_spanned(
                &field.ty,
                "a parcelable field cannot be a reference — it owns what it carries",
            ));
        }
        // The signature's guards: neither shape has an `.aidl` field form.
        type_str::reject_inner_references(&field.ty)?;
        type_str::reject_nullable_primitive(&field.ty)?;
        reject_parcelable_holder(&field.ty)?;
        let decl = type_str::as_written_in(&field.ty, type_str::Ctx::Parcelable)?;
        // Read only by the `impl Default` that is dropped below.
        let mut member = ParcelableMember::new(ident, decl, "Default::default()");
        member.deprecated = crate::deprecated_of(&field.attrs)?;
        members.push(member);
    }

    // A wire name (`ParcelableHolder` compares it), so `r#` is shed as `.aidl` does.
    let descriptor = crate::attr_descriptor(&input.attrs, "parcelable")?
        .unwrap_or_else(|| crate::strip_raw(&input.ident.to_string()).to_string());

    let mut render = ParcelableRender::new(input.ident.to_string(), descriptor);
    render.members = members;
    render.deprecated = crate::deprecated_of(&input.attrs)?;

    render_parcelable(&render)
        .map(|s| s.trim().to_string())
        .map_err(|e| syn::Error::new_spanned(&input.ident, format!("codegen failed: {e}")))
}

/// Matched structurally, so a holder inside `Option<_>` is caught too.
fn reject_parcelable_holder(ty: &syn::Type) -> syn::Result<()> {
    if !type_str::mentions_parcelable_holder(ty) {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        ty,
        "a `ParcelableHolder` field needs the `.aidl` path: its stability is set before \
         the read and the derived codec would replace the whole field, so a peer's \
         `@VintfStability` holder can never be decoded",
    ))
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

    /// Owned all the way down: a nested borrow has nothing to decode into.
    #[test]
    fn rejects_a_nested_reference_field() {
        for tokens in [
            quote! { struct Bad { s: Option<&'static str> } },
            quote! { struct Bad { s: Vec<&'static str> } },
        ] {
            let input: DeriveInput = syn::parse2(tokens).unwrap();
            let err = render_source(&input).unwrap_err();
            assert!(err.to_string().contains("borrowed type nested"), "{err}");
        }
    }

    #[test]
    fn rejects_a_nullable_primitive_field() {
        for tokens in [
            quote! { struct Bad { x: Option<i32> } },
            quote! { struct Bad { x: Vec<Option<i32>> } },
        ] {
            let input: DeriveInput = syn::parse2(tokens).unwrap();
            let err = render_source(&input).unwrap_err();
            assert!(err.to_string().contains("cannot be nullable"), "{err}");
        }
    }

    /// Any allow beyond a declared `#[deprecated]` would meet a user's `#![forbid(..)]`.
    #[test]
    fn the_allow_is_emitted_only_for_a_deprecated_item() {
        let quiet: DeriveInput = syn::parse2(quote! { struct Config { name: String } }).unwrap();
        let out = expand(&quiet).expect("expand").to_string();
        assert!(!out.contains("allow"), "{out}");

        let field: DeriveInput =
            syn::parse2(quote! { struct Config { #[deprecated] name: String } }).unwrap();
        let out = expand(&field).expect("expand").to_string();
        assert!(out.contains("allow (deprecated)"), "{out}");
    }

    /// One allowed const holds every item, so a `#[deprecated]` struct's codec stays quiet.
    #[test]
    fn the_allow_covers_every_generated_item() {
        let input: DeriveInput = syn::parse2(quote! {
            #[deprecated]
            struct Old { a: i32 }
        })
        .unwrap();
        let file: syn::File = syn::parse2(expand(&input).expect("expand")).expect("parses");
        let [syn::Item::Const(c)] = file.items.as_slice() else {
            panic!("expected one const, got {} items", file.items.len());
        };
        let attrs: String = c.attrs.iter().map(|a| quote!(#a).to_string()).collect();
        assert!(attrs.contains("deprecated"), "{attrs}");
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

    /// Matched structurally, so a holder inside `Option`/`Vec` is caught too.
    #[test]
    fn rejects_a_wrapped_parcelable_holder_field() {
        for tokens in [
            quote! { struct Bad { ext: Option<rsbinder::ParcelableHolder> } },
            quote! { struct Bad { ext: Vec<ParcelableHolder> } },
        ] {
            let input: DeriveInput = syn::parse2(tokens).unwrap();
            let err = render_source(&input).unwrap_err();
            assert!(err.to_string().contains("`.aidl` path"), "{err}");
        }
    }

    /// Only the struct's `#[parcelable]` is read, so a field's would vanish.
    #[test]
    fn rejects_a_parcelable_attribute_on_a_field() {
        let input: DeriveInput = syn::parse2(quote! {
            struct Bad { #[parcelable(descriptor = "com.example.X")] a: i32 }
        })
        .unwrap();
        let err = render_source(&input).unwrap_err();
        assert!(err.to_string().contains("belongs on the struct"), "{err}");
    }

    /// The codec lands in the user's own scope, so `self::` there is the user's.
    #[test]
    fn a_self_path_field_is_accepted() {
        let s = source(quote! {
            struct Outer { cfg: self::Config }
        });
        assert!(s.contains("self.r#cfg"), "{s}");
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
