// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The accept/refuse boundary, held against the generator instead of against a
//! hand-written rule.
//!
//! The golden tests pin rows a human chose, so a spelling nobody thought of is
//! invisible to them: nothing fails, because no row exists. This asserts the
//! boundary itself, in both directions and at every place:
//!
//! * **under-acceptance** — every spelling `.aidl` renders at a place must be
//!   accepted there, or a `.aidl` port has no equivalent trait;
//! * **over-acceptance** — every spelling accepted at a place must be one
//!   `.aidl` renders there, or a trait has no equivalent `.aidl`.
//!
//! Both sides are enumerated in code rather than listed by hand. The fixture is
//! the cross product of element type × nullability × arity × place, so a shape
//! cannot be missing from the canonical set by oversight — the one failure mode
//! that would turn this test into a source of false positives. A combination
//! `.aidl` refuses fails generation loudly and is excluded here by name, with
//! the reason; it is never dropped silently.

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
    /// A `@Backing` enum, which AIDL counts as a primitive in both rules
    /// below: the generator refuses `@nullable Mode` ("cannot get nullable
    /// annotation") and `out Mode` ("a primitive type") alike, while
    /// `@nullable Mode[]` stays legal. Both were established by letting the
    /// cell reach the generator rather than by assuming. Kept apart from
    /// `primitive` because it is a fact about what AIDL does with an enum,
    /// not a claim that the macro can tell one from a parcelable — it sees a
    /// bare path either way, which is what [`USER_PATHS`] is for.
    enum_like: bool,
}

const BASES: &[Base] = &[
    Base {
        aidl: "int",
        primitive: true,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "boolean",
        primitive: true,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "byte",
        primitive: true,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "String",
        primitive: false,
        string: true,
        enum_like: false,
    },
    Base {
        aidl: "MatrixCfg",
        primitive: false,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "MatrixMode",
        primitive: false,
        string: false,
        enum_like: true,
    },
    Base {
        aidl: "ParcelFileDescriptor",
        primitive: false,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "IMatrixCb",
        primitive: false,
        string: false,
        enum_like: false,
    },
    Base {
        aidl: "IBinder",
        primitive: false,
        string: false,
        enum_like: false,
    },
];

/// The bases a macro user spells as a bare path, which the macro therefore
/// cannot classify: an `.aidl` `enum` and a `parcelable` look identical in a
/// Rust signature, and the generator renders them differently — `in` by value
/// against by reference, and a `@nullable` array's elements bare against
/// wrapped. Both are in the fixture and both normalise to one placeholder, so a
/// spelling *either* can render counts as canonical. The macro has no way to
/// refuse one without refusing the other; this is the crate's documented blind
/// spot, stated here as a rule rather than left to drift.
/// Spelled the way `quote` prints a path, since that is the form [`norm`]
/// substitutes into.
const USER_PATHS: &[&str] = &[
    "super :: MatrixCfg :: MatrixCfg",
    "super :: MatrixMode :: MatrixMode",
];

/// What a bare user-defined path collapses to on both sides of the comparison.
const USER_PLACEHOLDER: &str = "super :: U :: U";

/// Scalar, variable-length array, fixed-size array.
const ARITIES: &[&str] = &["", "[]", "[3]"];

/// Why a cross-product cell is not in the fixture. Each is a rule AOSP's `aidl`
/// enforces too, so the cell has no `.aidl` spelling to be canonical at all —
/// which is exactly what the over-acceptance direction then demands the macro
/// refuse. Anything not listed here must generate; a new refusal in the
/// generator shows up as a generation panic, never as a quietly missing row.
fn aidl_refuses(place: &str, base: &Base, nullable: bool, arity: &str) -> bool {
    let scalar = arity.is_empty();
    // `AidlTypeSpecifier::CheckValid`: a primitive has no null form on the
    // wire, and an enum is carried as its backing scalar — the generator
    // refuses `@nullable Mode` as "Primitive type(UserDefined(..)) cannot get
    // nullable annotation". An array of either still takes `@nullable`.
    if nullable && (base.primitive || base.enum_like) && scalar {
        return true;
    }
    // `direction_at`: AIDL passes a primitive, an enum and a `String` `in`
    // only — `out Mode` is refused as "a primitive type", like `out int`.
    if scalar
        && (base.primitive || base.enum_like || base.string)
        && matches!(place, "out" | "inout")
    {
        return true;
    }
    false
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
            "p/IMatrix.aidl".to_string(),
            format!(
                "package p;\nimport p.IMatrixCb;\nimport p.MatrixCfg;\nimport p.MatrixMode;\ninterface IMatrix {{\n{methods}}}\n"
            ),
        ),
        (
            "p/MatrixFields.aidl".to_string(),
            format!(
                "package p;\nimport p.IMatrixCb;\nimport p.MatrixCfg;\nimport p.MatrixMode;\nparcelable MatrixFields {{\n{fields}}}\n"
            ),
        ),
    ]
}

/// One spelling in `quote`'s own spacing, so the two sides compare as text
/// without either going through `type_str`'s printers — those are what these
/// tests hold to account.
///
/// The spacing is kept rather than stripped: `&mut T` collapses to `&mutT`,
/// which parses as a shared reference to a path named `mutT`, so a stripped
/// spelling fed back to `syn` silently becomes a different type. Every `out`
/// and `inout` row went through that hole before the round-trip test found it.
fn norm(ty: &Type) -> String {
    let mut s = quote::quote!(#ty).to_string();
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

/// Every spelling the generator renders, keyed by place.
fn canonical() -> BTreeMap<&'static str, BTreeSet<String>> {
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
                out.entry(place).or_default().insert(norm(&pat_ty.ty));
            }
        }
        if place == "return" {
            if let ReturnType::Type(_, ty) = &f.sig.output {
                if let Some(inner) = binder_result_inner(ty) {
                    out.entry("return").or_default().insert(norm(inner));
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
        out.entry("field").or_default().insert(norm(&field.ty));
    }

    out
}

/// The Rust bases the candidates are built over, spelled as the generated code
/// spells them so the two sides compare directly. `i8`/`u8` are both here
/// because `byte` moves spelling with the place.
const RUST_BASES: &[&str] = &[
    "i32",
    "bool",
    "i8",
    "u8",
    "String",
    "rsbinder::ParcelFileDescriptor",
    "rsbinder::SIBinder",
    "rsbinder::Strong<dyn super::IMatrixCb::IMatrixCb>",
    // Normalises to the placeholder, so this one base stands for every bare
    // user-defined name — the case the macro cannot classify.
    "super::MatrixCfg::MatrixCfg",
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

/// The places a spelling can reach, mirroring `direction()`: a `&mut _`
/// argument is `out` (or `inout` with the attribute) and can be nothing else,
/// and everything else is `in`. A return and a field take any spelling.
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

/// The rows of the reference table, with names a reader recognises rather than
/// the fixture's. `canonical` treats the base as an opaque string, so swapping
/// in a readable one renders through the same rules the equivalence test holds
/// against the generator. The two halves of [`Kind::User`] get a row each:
/// "or" would leave the reader to work out which is which.
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
         Every spelling the AIDL compiler produces, by type and by position. \
         A trait written with these is a trait an `.aidl` port reproduces \
         exactly; anything else the macro refuses, naming the cell you \
         wanted.\n\
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
        };
        for arity_aidl in ARITIES {
            let arity = match *arity_aidl {
                "" => Arity::Scalar,
                "[]" => Arity::Var,
                _ => Arity::Fixed(vec!["N".to_string()]),
            };
            // A reference table documents the fixed-size case generically, so
            // both halves of the row have to say `N`: the fixture's literal
            // size would leave the AIDL column claiming `[3]` next to a Rust
            // column saying `N`. Only emptiness reaches `aidl_refuses`, so the
            // substitution cannot change which cells are legal.
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

/// The enumerated half of the crate docs drifted from the rules in four
/// separate review rounds, because nothing but attention tied them together.
/// Here the table is generated from the same rules the gate enforces, and this
/// test fails when the committed file falls behind — the file itself has to be
/// committed, since docs.rs builds documentation without running tests.
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

/// The Rust spelling the generator gives each fixture base, and what the macro
/// can tell about it from that spelling alone.
fn rust_base(aidl: &str) -> (Kind, &'static str) {
    match aidl {
        "int" => (Kind::Primitive, "i32"),
        "boolean" => (Kind::Primitive, "bool"),
        "byte" => (Kind::Primitive, "i8"),
        "String" => (Kind::Str, "String"),
        "MatrixCfg" => (Kind::User, "super::MatrixCfg::MatrixCfg"),
        "MatrixMode" => (Kind::User, "super::MatrixMode::MatrixMode"),
        "ParcelFileDescriptor" => (Kind::NoDefault, "rsbinder::ParcelFileDescriptor"),
        "IMatrixCb" => (
            Kind::NoDefault,
            "rsbinder::Strong<dyn super::IMatrixCb::IMatrixCb>",
        ),
        "IBinder" => (Kind::NoDefault, "rsbinder::SIBinder"),
        other => panic!("no Rust base for `{other}`"),
    }
}

/// `aidl_shape` states the generator's rendering table once so that a refusal
/// can be a comparison against it. That only holds if the table *is* the
/// generator's — otherwise it is a third hand-written copy, verified by nobody
/// and free to drift exactly as the scattered predicates did. So every cell of
/// the same cross product is rendered both ways and the two must agree.
#[test]
fn the_shape_table_renders_what_the_generator_renders() {
    let from_generator = canonical();
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
                    let normalized = norm_str(&rendered);
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

/// Normalize-and-compare turns a refusal into "what you wrote is not what
/// `.aidl` renders". That is only safe if the normalizer recovers the shape
/// from the canonical spelling itself — otherwise a correct type would be
/// refused for not matching its own canonical form, which is the worst
/// regression this change could cause.
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
            let normalized = norm_str(&spelling);
            let renders = canonical
                .get(place_name)
                .is_some_and(|set| set.contains(&normalized));
            if !renders {
                // A by-value argument still meets the generated `Copy`
                // assertion, so a non-`Copy` one fails in rustc rather than
                // silently; a `Copy` one diverges with nothing to catch it.
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
