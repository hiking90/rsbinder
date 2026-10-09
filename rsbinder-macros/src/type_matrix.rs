// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Accept/refuse boundary vs the generator over type × nullability × arity × place.

use crate::aidl_shape::{self, Arity, Kind, Shape};
use crate::golden::from_aidl_files;
use crate::type_str::Place;
use std::collections::{BTreeMap, BTreeSet};
use syn::{FnArg, Item, ItemMod, ReturnType, Type};

/// An AIDL element type and the Rust base the generator spells it as.
struct Base {
    aidl: &'static str,
    primitive: bool,
    string: bool,
    /// A `@Backing` enum: AIDL refuses `@nullable Mode` and `out Mode`, the macro cannot tell.
    enum_like: bool,
    /// `IBinder` or an interface.
    binder: bool,
    pfd: bool,
}

const PLAIN: Base = Base {
    aidl: "",
    primitive: false,
    string: false,
    enum_like: false,
    binder: false,
    pfd: false,
};

const BASES: &[Base] = &[
    Base {
        aidl: "int",
        primitive: true,
        ..PLAIN
    },
    Base {
        aidl: "boolean",
        primitive: true,
        ..PLAIN
    },
    Base {
        aidl: "byte",
        primitive: true,
        ..PLAIN
    },
    Base {
        aidl: "String",
        string: true,
        ..PLAIN
    },
    Base {
        aidl: "MatrixCfg",
        ..PLAIN
    },
    Base {
        aidl: "MatrixMode",
        enum_like: true,
        ..PLAIN
    },
    Base {
        aidl: "MatrixPair<MatrixCfg>",
        ..PLAIN
    },
    Base {
        aidl: "ParcelFileDescriptor",
        pfd: true,
        ..PLAIN
    },
    Base {
        aidl: "IMatrixCb",
        binder: true,
        ..PLAIN
    },
    Base {
        aidl: "IBinder",
        binder: true,
        ..PLAIN
    },
];

/// Enum and parcelable bases, one placeholder for both: the macro cannot tell them apart.
const USER_PATHS: &[&str] = &[
    "super :: MatrixCfg :: MatrixCfg",
    "super :: MatrixMode :: MatrixMode",
];

/// What a bare user-defined path collapses to on both sides of the comparison.
const USER_PLACEHOLDER: &str = "super :: U :: U";

/// Scalar, variable-length array, fixed-size array.
const ARITIES: &[&str] = &["", "[]", "[3]"];

/// The cells AOSP's `aidl` refuses too; any other cell that fails to generate panics.
fn aidl_refuses(place: &str, base: &Base, nullable: bool, arity: &str) -> bool {
    let scalar = arity.is_empty();
    // `CheckValid`: no `@nullable` on a scalar primitive or enum; an array of either takes it.
    if nullable && (base.primitive || base.enum_like) && scalar {
        return true;
    }
    // `GetArgumentAspect`: primitive, enum, `String`, binder `in` only; a fd `in` or `inout`.
    if scalar
        && (base.primitive || base.enum_like || base.string || base.binder)
        && matches!(place, "out" | "inout")
    {
        return true;
    }
    scalar && base.pfd && place == "out"
}

/// The `.aidl` sources, with every legal cell of the cross product present.
fn fixture_files() -> Vec<(String, String)> {
    let mut methods = String::new();
    let mut fields = String::new();
    let mut n = 0usize;

    for place in ["in", "out", "inout", "return", "field"] {
        for base in BASES {
            for arity in ARITIES {
                for nullable in [false, true] {
                    if aidl_refuses(place, base, nullable, arity) {
                        continue;
                    }
                    n += 1;
                    let ty = format!(
                        "{}{}{}",
                        if nullable { "@nullable " } else { "" },
                        base.aidl,
                        arity
                    );
                    match place {
                        "return" => methods.push_str(&format!("    {ty} r_{n}();\n")),
                        "field" => fields.push_str(&format!("    {ty} f_{n};\n")),
                        "inout" => methods.push_str(&format!("    void io_{n}(inout {ty} v);\n")),
                        dir => methods.push_str(&format!("    void {dir}_{n}({dir} {ty} v);\n")),
                    }
                }
            }
        }
    }

    vec![
        (
            "p/IMatrixCb.aidl".to_string(),
            "package p;\ninterface IMatrixCb {\n    void hit();\n}\n".to_string(),
        ),
        (
            "p/MatrixCfg.aidl".to_string(),
            "package p;\nparcelable MatrixCfg {\n    int a;\n}\n".to_string(),
        ),
        (
            "p/MatrixMode.aidl".to_string(),
            "package p;\n@Backing(type=\"int\")\nenum MatrixMode {\n    FAST = 0,\n    SAFE = 1,\n}\n"
                .to_string(),
        ),
        (
            "p/MatrixPair.aidl".to_string(),
            "package p;\nparcelable MatrixPair<T> {\n    int a;\n}\n".to_string(),
        ),
        (
            "p/IMatrix.aidl".to_string(),
            format!(
                "package p;\nimport p.IMatrixCb;\nimport p.MatrixCfg;\nimport p.MatrixMode;\nimport p.MatrixPair;\ninterface IMatrix {{\n{methods}}}\n"
            ),
        ),
        (
            "p/MatrixFields.aidl".to_string(),
            format!(
                "package p;\nimport p.IMatrixCb;\nimport p.MatrixCfg;\nimport p.MatrixMode;\nimport p.MatrixPair;\nparcelable MatrixFields {{\n{fields}}}\n"
            ),
        ),
    ]
}

/// `quote`'s spacing, not `type_str`'s printers; kept, as a stripped `&mutT` re-parses wrong.
fn norm(ty: &Type) -> String {
    let mut s = norm_exact(ty);
    for path in USER_PATHS {
        s = s.replace(path, USER_PLACEHOLDER);
    }
    s
}

fn norm_str(spelling: &str) -> String {
    let ty: Type = syn::parse_str(spelling).unwrap_or_else(|e| panic!("parse `{spelling}`: {e}"));
    norm(&ty)
}

/// `BinderResult<T>`'s `T`, or `None` for the `()` a `void` renders.
fn binder_result_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(p) = ty else {
        return None;
    };
    let seg = p.path.segments.last()?;
    if seg.ident != "BinderResult" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let inner = args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })?;
    if matches!(inner, Type::Tuple(t) if t.elems.is_empty()) {
        return None;
    }
    Some(inner)
}

/// The place a fixture method name declares, by its prefix.
fn place_of(name: &str) -> Option<&'static str> {
    for (prefix, place) in [
        ("io_", "inout"),
        ("in_", "in"),
        ("out_", "out"),
        ("r_", "return"),
    ] {
        if name.starts_with(prefix) {
            return Some(place);
        }
    }
    None
}

fn place_enum(name: &str) -> Place {
    match name {
        "in" => Place::In,
        "out" => Place::Out,
        "inout" => Place::Inout,
        "return" => Place::Return,
        "field" => Place::Field,
        other => panic!("unknown place `{other}`"),
    }
}

/// `norm` without the placeholder, so `MatrixCfg` and `MatrixMode` stay apart.
fn norm_exact(ty: &Type) -> String {
    let mut s = quote::quote!(#ty).to_string();
    // The generator paths these (`rsbinder_aidl::render::OPTION`, …); `aidl_shape` advises bare.
    use rsbinder_aidl::render::{BOX, OPTION, STRING, VEC};
    for std in [OPTION, VEC, BOX, STRING] {
        let path: syn::Path = syn::parse_str(std).expect("a path");
        let bare = path.segments.last().expect("a segment").ident.to_string();
        s = s.replace(&quote::quote!(#path).to_string(), &bare);
    }
    s
}

/// Every spelling the generator renders, keyed by place.
fn canonical() -> BTreeMap<&'static str, BTreeSet<String>> {
    canonical_by(norm)
}

fn canonical_by(normalize: fn(&Type) -> String) -> BTreeMap<&'static str, BTreeSet<String>> {
    let owned = fixture_files();
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let mut out: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();

    let text = from_aidl_files(&files, "p/IMatrix.aidl", "IMatrix", false);
    let module: ItemMod = syn::parse_str(&text).expect("parse generated interface module");
    let items = module.content.expect("module body").1;
    let tr = items
        .iter()
        .find_map(|i| match i {
            Item::Trait(t) if t.ident == "IMatrix" => Some(t),
            _ => None,
        })
        .expect("generated trait");

    for item in &tr.items {
        let syn::TraitItem::Fn(f) = item else {
            continue;
        };
        let name = crate::strip_raw(&f.sig.ident.to_string()).to_string();
        let Some(place) = place_of(&name) else {
            continue;
        };
        for input in &f.sig.inputs {
            if let FnArg::Typed(pat_ty) = input {
                out.entry(place).or_default().insert(normalize(&pat_ty.ty));
            }
        }
        if place == "return" {
            if let ReturnType::Type(_, ty) = &f.sig.output {
                if let Some(inner) = binder_result_inner(ty) {
                    out.entry("return").or_default().insert(normalize(inner));
                }
            }
        }
    }

    let text = from_aidl_files(&files, "p/MatrixFields.aidl", "MatrixFields", false);
    let module: ItemMod = syn::parse_str(&text).expect("parse generated parcelable module");
    let items = module.content.expect("module body").1;
    let st = items
        .iter()
        .find_map(|i| match i {
            Item::Struct(s) if s.ident == "MatrixFields" => Some(s),
            _ => None,
        })
        .expect("generated struct");
    for field in &st.fields {
        out.entry("field").or_default().insert(normalize(&field.ty));
    }

    out
}

/// Candidate bases in the generator's spelling; `i8` and `u8` both, as `byte` moves.
const RUST_BASES: &[&str] = &[
    "i32",
    "bool",
    "i8",
    "u8",
    "String",
    "rsbinder::ParcelFileDescriptor",
    "rsbinder::SIBinder",
    "rsbinder::Strong<dyn super::IMatrixCb::IMatrixCb>",
    // Normalises to the placeholder, so it stands for every bare user-defined name.
    "super::MatrixCfg::MatrixCfg",
    "super::MatrixPair::MatrixPair<super::MatrixCfg::MatrixCfg>",
];

/// Every wrapping the grammar admits, as a format string over one base.
const SHAPES: &[&str] = &[
    "{b}",
    "&{b}",
    "&mut {b}",
    "Option<{b}>",
    "&Option<{b}>",
    "&mut Option<{b}>",
    "Option<&{b}>",
    "Vec<{b}>",
    "&Vec<{b}>",
    "&mut Vec<{b}>",
    "&[{b}]",
    "&mut [{b}]",
    "Option<Vec<{b}>>",
    "Option<&[{b}]>",
    "&mut Option<Vec<{b}>>",
    "Vec<Option<{b}>>",
    "&[Option<{b}>]",
    "&mut Vec<Option<{b}>>",
    "Option<&[Option<{b}>]>",
    "&mut Option<Vec<Option<{b}>>>",
    "[{b}; 3]",
    "&[{b}; 3]",
    "&mut [{b}; 3]",
    "Option<[{b}; 3]>",
    "Option<&[{b}; 3]>",
    "&mut Option<[{b}; 3]>",
    "[Option<{b}>; 3]",
    "&[Option<{b}>; 3]",
    "&mut [Option<{b}>; 3]",
    "Option<[Option<{b}>; 3]>",
    "Vec<[{b}; 3]>",
    "&[[{b}; 3]]",
    "&mut Vec<[{b}; 3]>",
    "Vec<Vec<{b}>>",
    "[Vec<{b}>; 3]",
    "Option<Vec<Option<[{b}; 3]>>>",
    "[Option<[{b}; 3]>; 2]",
    "Box<{b}>",
    "&Box<{b}>",
    "&mut Box<{b}>",
    "Vec<Box<{b}>>",
    "Option<Box<{b}>>",
    "Option<&Box<{b}>>",
    "&mut Option<Box<{b}>>",
    "Option<Option<{b}>>",
    "&mut Option<Option<{b}>>",
];

/// `String`'s borrowed spelling is `&str`, which no other base has.
const STRING_EXTRAS: &[&str] = &[
    "&str",
    "Option<&str>",
    "&[&str]",
    "Option<&[&str]>",
    "&mut str",
];

fn candidates() -> Vec<String> {
    let mut out = Vec::new();
    for base in RUST_BASES {
        for shape in SHAPES {
            out.push(shape.replace("{b}", base));
        }
    }
    out.extend(STRING_EXTRAS.iter().map(|s| s.to_string()));
    out
}

/// Places a spelling can reach, as `direction()` reads them; return and field take any.
fn places_for(ty: &Type) -> Vec<(&'static str, Place)> {
    let is_mut_ref =
        matches!(crate::type_str::unwrap_group(ty), Type::Reference(r) if r.mutability.is_some());
    let mut out = vec![("return", Place::Return), ("field", Place::Field)];
    if is_mut_ref {
        out.push(("out", Place::Out));
        out.push(("inout", Place::Inout));
    } else {
        out.push(("in", Place::In));
    }
    out
}

/// Reference-table rows under readable names; each half of [`Kind::User`] gets its own.
const DOC_ROWS: &[(&str, Kind, &str, bool)] = &[
    ("int", Kind::Primitive, "i32", false),
    ("boolean", Kind::Primitive, "bool", false),
    ("byte", Kind::Primitive, "i8", false),
    ("String", Kind::Str, "String", false),
    ("Cfg (a parcelable)", Kind::User, "Cfg", false),
    ("Mode (a @Backing enum)", Kind::User, "Mode", true),
    (
        "ParcelFileDescriptor",
        Kind::NoDefault,
        "rsbinder::ParcelFileDescriptor",
        false,
    ),
    (
        "IFoo (an interface)",
        Kind::NoDefault,
        "rsbinder::Strong<dyn IFoo>",
        false,
    ),
    ("IBinder", Kind::NoDefault, "rsbinder::SIBinder", false),
];

/// The reference table, as the crate docs include it.
fn types_md() -> String {
    let mut out = String::new();
    out.push_str(
        "# What `.aidl` renders\n\
         \n\
         The spelling the AIDL compiler produces for each kind of type, by \
         position. One scalar stands for its kind: `long`, `float`, `double` \
         and `char` follow the `int` rows as `i64`, `f32`, `f64` and `u16`. \
         `List<T>` and generic parcelables are not listed. A trait written \
         with these is a trait an `.aidl` port reproduces exactly; any other \
         spelling of these types the macro refuses, naming the cell you \
         wanted — except between the `Cfg` and `Mode` rows, which are both a \
         bare name to the macro, and a field's `Option<Box<Cfg>>`, which `.aidl` \
         writes for a `@nullable(heap=true)` parcelable field and for a \
         `@nullable` one that closes a reference cycle.\n\
         \n\
         `—` marks a combination AIDL itself rejects.\n\
         \n\
         <!-- Generated by `type_matrix::the_reference_table_matches_the_rules`.\n\
         \x20    Do not edit: run `TYPES_MD=overwrite cargo test -p rsbinder-macros`. -->\n\
         \n\
         | AIDL type | `in` | `out` | `#[inout]` | return | field |\n\
         |---|---|---|---|---|---|\n",
    );

    for &(label, kind, base, user_is_enum) in DOC_ROWS {
        let probe = Base {
            aidl: label,
            primitive: matches!(kind, Kind::Primitive),
            string: matches!(kind, Kind::Str),
            enum_like: user_is_enum,
            binder: base == "rsbinder::SIBinder" || base.starts_with("rsbinder::Strong"),
            pfd: base == "rsbinder::ParcelFileDescriptor",
        };
        for arity_aidl in ARITIES {
            let arity = match *arity_aidl {
                "" => Arity::Scalar,
                "[]" => Arity::Var,
                _ => Arity::Fixed(vec!["N".to_string()]),
            };
            // `N` on both sides; `aidl_refuses` reads only emptiness, so legality is unchanged.
            let shown = if arity_aidl.is_empty() { "" } else { "[N]" };
            let shown = if *arity_aidl == "[]" { "[]" } else { shown };
            for nullable in [false, true] {
                let shape = Shape {
                    kind,
                    base: base.to_string(),
                    nullable,
                    arity: arity.clone(),
                };
                let cells: Vec<String> = ["in", "out", "inout", "return", "field"]
                    .iter()
                    .map(|place| {
                        if aidl_refuses(place, &probe, nullable, shown) {
                            return "—".to_string();
                        }
                        match aidl_shape::render(&shape, place_enum(place), user_is_enum) {
                            Some(s) => format!("`{s}`"),
                            None => "—".to_string(),
                        }
                    })
                    .collect();
                // A row AIDL rejects everywhere teaches nothing.
                if cells.iter().all(|c| c == "—") {
                    continue;
                }
                let null = if nullable { "@nullable " } else { "" };
                out.push_str(&format!(
                    "| `{null}{}{shown}` | {} |\n",
                    strip_note(label),
                    cells.join(" | ")
                ));
            }
        }
    }
    out
}

/// `"Cfg (a parcelable)"` names the row; the cell wants just `Cfg`.
fn strip_note(label: &str) -> &str {
    label.split_once(" (").map_or(label, |(name, _)| name)
}

/// `TYPES.md` is committed, since docs.rs builds without running tests; this keeps it current.
#[test]
fn the_reference_table_matches_the_rules() {
    let expected = types_md();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("TYPES.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current == expected {
        return;
    }
    if std::env::var_os("TYPES_MD").is_some_and(|v| v == "overwrite") {
        std::fs::write(&path, &expected).expect("write TYPES.md");
        return;
    }
    panic!(
        "TYPES.md no longer matches the rules it documents.\n\
         Regenerate it with `TYPES_MD=overwrite cargo test -p rsbinder-macros`, \
         then read the diff before committing it."
    );
}

/// The generator's Rust spelling of a fixture base, and the kind the macro reads off it.
fn rust_base(aidl: &str) -> (Kind, &'static str) {
    match aidl {
        "int" => (Kind::Primitive, "i32"),
        "boolean" => (Kind::Primitive, "bool"),
        "byte" => (Kind::Primitive, "i8"),
        "String" => (Kind::Str, "String"),
        "MatrixCfg" => (Kind::User, "super::MatrixCfg::MatrixCfg"),
        "MatrixMode" => (Kind::User, "super::MatrixMode::MatrixMode"),
        "MatrixPair<MatrixCfg>" => (
            Kind::Generic,
            "super::MatrixPair::MatrixPair<super::MatrixCfg::MatrixCfg>",
        ),
        "ParcelFileDescriptor" => (Kind::NoDefault, "rsbinder::ParcelFileDescriptor"),
        "IMatrixCb" => (
            Kind::NoDefault,
            "rsbinder::Strong<dyn super::IMatrixCb::IMatrixCb>",
        ),
        "IBinder" => (Kind::NoDefault, "rsbinder::SIBinder"),
        other => panic!("no Rust base for `{other}`"),
    }
}

/// `aidl_shape`'s table must be the generator's, cell for cell, or its refusals mean nothing.
#[test]
fn the_shape_table_renders_what_the_generator_renders() {
    // Exact paths: a placeholder would let a swapped enum/parcelable render pass.
    let from_generator = canonical_by(norm_exact);
    let mut produced: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut wrong = Vec::new();

    for place in ["in", "out", "inout", "return", "field"] {
        for base in BASES {
            let (kind, rust) = rust_base(base.aidl);
            for arity_aidl in ARITIES {
                let arity = match *arity_aidl {
                    "" => Arity::Scalar,
                    "[]" => Arity::Var,
                    _ => Arity::Fixed(vec!["3".to_string()]),
                };
                for nullable in [false, true] {
                    if aidl_refuses(place, base, nullable, arity_aidl) {
                        continue;
                    }
                    let shape = Shape {
                        kind,
                        base: rust.to_string(),
                        nullable,
                        arity: arity.clone(),
                    };
                    let Some(rendered) =
                        aidl_shape::render(&shape, place_enum(place), base.enum_like)
                    else {
                        continue;
                    };
                    let normalized = norm_exact(
                        &syn::parse_str(&rendered)
                            .unwrap_or_else(|e| panic!("parse `{rendered}`: {e}")),
                    );
                    produced
                        .entry(place)
                        .or_default()
                        .insert(normalized.clone());
                    let known = from_generator
                        .get(place)
                        .is_some_and(|set| set.contains(&normalized));
                    if !known {
                        let null = if nullable { "@nullable " } else { "" };
                        wrong.push(format!(
                            "  {place:<6} {null}{}{arity_aidl} → `{rendered}`, which the generator never renders there",
                            base.aidl
                        ));
                    }
                }
            }
        }
    }

    for (place, spellings) in &from_generator {
        for spelling in spellings {
            if !produced.get(place).is_some_and(|p| p.contains(spelling)) {
                wrong.push(format!(
                    "  {place:<6} generator renders `{spelling}`, which the table never produces"
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "the shape table and the generator disagree:\n{}",
        wrong.join("\n")
    );
}

/// A canonical spelling must recover its own shape, or a correct type would be refused.
#[test]
fn every_generated_spelling_round_trips_through_its_shape() {
    let from_generator = canonical();
    let mut broken = Vec::new();

    for (place_name, spellings) in &from_generator {
        let place = place_enum(place_name);
        for spelling in spellings {
            let ty: Type = syn::parse_str(spelling)
                .unwrap_or_else(|e| panic!("generated spelling `{spelling}` does not parse: {e}"));
            let Some(shape) = aidl_shape::shape_of(&ty) else {
                broken.push(format!("  {place_name:<6} `{spelling}` has no shape"));
                continue;
            };
            let produced: Vec<String> = aidl_shape::canonical(&shape, place)
                .iter()
                .map(|s| norm_str(s))
                .collect();
            if !produced.contains(spelling) {
                broken.push(format!(
                    "  {place_name:<6} `{spelling}` normalises to a shape rendering {produced:?}"
                ));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "these spellings the generator writes do not survive the round trip:\n{}",
        broken.join("\n")
    );
}

#[test]
fn every_spelling_aidl_renders_is_accepted() {
    let canonical = canonical();
    let mut missing = Vec::new();

    for (place_name, spellings) in &canonical {
        let place = place_enum(place_name);
        for spelling in spellings {
            let ty: Type = syn::parse_str(spelling)
                .unwrap_or_else(|e| panic!("generated spelling `{spelling}` does not parse: {e}"));
            if let Err(e) = crate::type_str::check_type_at(&ty, place) {
                missing.push(format!(
                    "  {place_name:<6} {spelling}\n         refused: {e}"
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "`.aidl` renders these, so the macro must accept them — a trait moving to `.aidl` \
         would otherwise have no equivalent:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_accepted_spelling_is_one_aidl_renders() {
    let canonical = canonical();
    let mut extra = Vec::new();

    for spelling in candidates() {
        let Ok(ty) = syn::parse_str::<Type>(&spelling) else {
            continue;
        };
        for (place_name, place) in places_for(&ty) {
            if crate::type_str::check_type_at(&ty, place).is_err() {
                continue;
            }
            // `.aidl` writes it only for a cycle, and the fixture has none to render it from.
            if place_name == "field"
                && [
                    "Option<Box<super::MatrixCfg::MatrixCfg>>",
                    "Option<Box<super::MatrixPair::MatrixPair<super::MatrixCfg::MatrixCfg>>>",
                ]
                .iter()
                .any(|cycle| norm_str(&spelling) == norm_str(cycle))
            {
                continue;
            }
            let normalized = norm_str(&spelling);
            let renders = canonical
                .get(place_name)
                .is_some_and(|set| set.contains(&normalized));
            if !renders {
                // Only rustc's `Copy` assertion stops a by-value one, and not when it is `Copy`.
                let by_value = matches!(place_name, "in" | "out" | "inout")
                    && !matches!(crate::type_str::unwrap_group(&ty), Type::Reference(_));
                let note = if by_value { "  (by value)" } else { "" };
                extra.push(format!("  {place_name:<6} {spelling}{note}"));
            }
        }
    }

    assert!(
        extra.is_empty(),
        "the macro accepts these, but `.aidl` renders no such spelling at that place — the \
         trait would have no `.aidl` equivalent:\n{}",
        extra.join("\n")
    );
}

/// Path qualification is fine, so a qualified `String` or `byte[]` element is its bare spelling.
#[test]
fn a_qualified_string_or_byte_element_is_accepted() {
    for (spelling, place) in [
        ("std::string::String", Place::Return),
        ("std::string::String", Place::Field),
        ("Option<std::string::String>", Place::Field),
        ("Vec<std::string::String>", Place::Return),
        ("&[std::string::String]", Place::In),
        ("Vec<core::primitive::u8>", Place::Field),
        ("&[core::primitive::u8]", Place::In),
    ] {
        let ty: Type = syn::parse_str(spelling).unwrap();
        if let Err(e) = crate::type_str::check_type_at(&ty, place) {
            panic!("`{spelling}` refused: {e}");
        }
    }
}
