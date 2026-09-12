// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What `.aidl` would render, computed once instead of approximated per rule.
//!
//! Validation in [`crate::type_str`] grew as a set of hand-written predicates,
//! each deciding whether one spelling is wrong for one reason. The generator
//! decides the same thing in one pass, from four axes — direction, nullability,
//! arity and whether the element has a `Default` — so every predicate is a
//! partial re-derivation of a table that already exists, and they drift from it
//! and from each other one rule at a time.
//!
//! This module states the table once: a [`Shape`] is what the macro can tell
//! about a written type, and [`canonical`] renders the spelling `.aidl` gives
//! that shape at a place. A refusal then becomes a comparison — what you wrote
//! against what `.aidl` renders — and the diagnostic can name the canonical
//! spelling instead of describing a rule, which is what keeps advice from
//! recommending something another rule refuses.
//!
//! The macro cannot call the generator (its `parser` and `type_generator`
//! modules are private, and the user-defined path needs a symbol table the
//! macro has no way to build), so this is still a second copy of those rules.
//! The difference is that it is *one* copy with a single entry point, and
//! `type_matrix` holds it against the real generator for every cell of the
//! cross product rather than against a reviewer's attention.
//!
//! Ported from `TypeGenerator`: `type_decl_for_func`, `type_declaration`,
//! `list_type_decl`, `func_list_type_decl`, `make_fixed_array`,
//! `array_type_name`, `nullable_element`, `can_be_defaulted`.

use crate::type_str::Place;

/// What the macro can tell about a type from its spelling alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An AIDL scalar. An `.aidl` enum is one of these on the wire too, which
    /// is why [`Kind::User`] has to be rendered both ways.
    Primitive,
    Str,
    /// A binder, an interface handle or a fd: no `Default` for a reader to
    /// start from, so `.aidl` stores it as `Option<_>` wherever there is
    /// nothing to start from (`can_be_defaulted` false in both modes).
    NoDefault,
    /// A bare path. An `.aidl` `enum` and a `parcelable` are spelled the same
    /// way in Rust and render differently, and nothing in the signature says
    /// which it is — so both renderings are canonical here.
    User,
}

impl Kind {
    /// `TypeGenerator::is_primitive`, which counts an enum as primitive.
    fn is_primitive(self, user_is_enum: bool) -> bool {
        match self {
            Kind::Primitive => true,
            Kind::User => user_is_enum,
            _ => false,
        }
    }

    /// `TypeGenerator::is_aidl_nullable`: everything but a primitive or enum.
    fn is_aidl_nullable(self, user_is_enum: bool) -> bool {
        match self {
            Kind::Primitive => false,
            Kind::User => !user_is_enum,
            _ => true,
        }
    }

    /// `TypeGenerator::can_be_defaulted`, which agrees in both modes for every
    /// kind the macro can name: only the no-`Default` group is false.
    fn can_be_defaulted(self) -> bool {
        !matches!(self, Kind::NoDefault)
    }
}

/// Scalar, `T[]`, or `T[N]` — sizes outermost first, as `make_fixed_array`
/// folds them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Arity {
    Scalar,
    Var,
    Fixed(Vec<String>),
}

/// A written type reduced to the axes `.aidl` renders from.
#[derive(Clone)]
pub(crate) struct Shape {
    pub kind: Kind,
    /// The Rust spelling of the scalar, as the signature writes it (`i32`,
    /// `String`, `rsbinder::SIBinder`, `super::Cfg::Cfg`).
    pub base: String,
    /// The outer `@nullable`, not an element's own.
    pub nullable: bool,
    pub arity: Arity,
}

/// The shape a written type carries, recovered from any spelling rather than
/// only the canonical one — that is the whole point: a wrong spelling still has
/// to yield the shape its author meant, so [`canonical`] can name what `.aidl`
/// would have written for it.
///
/// `None` for a type with no AIDL shape at all (`()`, a trait object, a
/// tuple); those keep their own diagnostics.
pub(crate) fn shape_of(ty: &syn::Type) -> Option<Shape> {
    use crate::type_str::unwrap_group;
    use syn::Type;

    // One `&`/`&mut` is the direction's, not the shape's.
    let mut cur = unwrap_group(ty);
    if let Type::Reference(r) = cur {
        cur = unwrap_group(&r.elem);
    }

    // The outermost `Option` is the `@nullable`; an inner one belongs to an
    // element and is derived, not written.
    let mut nullable = false;
    if let Some(inner) = option_arg(cur) {
        nullable = true;
        cur = unwrap_group(inner);
        if let Type::Reference(r) = cur {
            cur = unwrap_group(&r.elem);
        }
    }

    let (arity, mut leaf) = match cur {
        Type::Slice(s) => (Arity::Var, unwrap_group(&s.elem)),
        Type::Array(_) => {
            let mut sizes = Vec::new();
            let mut elem = cur;
            while let Type::Array(a) = unwrap_group(elem) {
                let len = &a.len;
                sizes.push(quote::quote!(#len).to_string().replace(' ', ""));
                elem = &a.elem;
            }
            (Arity::Fixed(sizes), unwrap_group(elem))
        }
        _ => match vec_arg(cur) {
            Some(elem) => (Arity::Var, unwrap_group(elem)),
            None => (Arity::Scalar, cur),
        },
    };

    // An element's own `Option` is the generator's, so it is not an axis.
    if !matches!(arity, Arity::Scalar) {
        if let Some(inner) = option_arg(leaf) {
            leaf = unwrap_group(inner);
        }
    }
    if let Type::Reference(r) = leaf {
        leaf = unwrap_group(&r.elem);
    }

    let (kind, base) = leaf_kind(leaf, !matches!(arity, Arity::Scalar))?;
    Some(Shape {
        kind,
        base,
        nullable,
        arity,
    })
}

/// `Option<T>`'s `T`, matched structurally so `std::option::Option<T>` counts.
fn option_arg(ty: &syn::Type) -> Option<&syn::Type> {
    generic_arg(ty, "Option")
}

/// `Vec<T>`'s `T`.
fn vec_arg(ty: &syn::Type) -> Option<&syn::Type> {
    generic_arg(ty, "Vec")
}

fn generic_arg<'a>(ty: &'a syn::Type, name: &str) -> Option<&'a syn::Type> {
    let syn::Type::Path(p) = crate::type_str::unwrap_group(ty) else {
        return None;
    };
    let seg = p.path.segments.last()?;
    if seg.ident != name {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// What the leaf names, and the scalar spelling to render it by. `u8` is
/// `byte`'s element spelling, so inside an array it means the same scalar
/// `i8` does — otherwise the shape would render a type nobody wrote.
fn leaf_kind(leaf: &syn::Type, in_array: bool) -> Option<(Kind, String)> {
    let written = crate::type_str::as_written(leaf).ok()?;
    let name = match crate::type_str::unwrap_group(leaf) {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    };
    let plain = matches!(
        crate::type_str::unwrap_group(leaf),
        syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| matches!(s.arguments, syn::PathArguments::None))
    );
    Some(match name.as_deref() {
        Some("str" | "String") => (Kind::Str, "String".to_string()),
        Some("u8") if in_array && plain => (Kind::Primitive, "i8".to_string()),
        Some("bool" | "i8" | "i32" | "i64" | "f32" | "f64" | "u16") if plain => {
            (Kind::Primitive, written)
        }
        Some("SIBinder" | "ParcelFileDescriptor") if plain => (Kind::NoDefault, written),
        Some("Strong") => (Kind::NoDefault, written),
        Some(_) if plain => (Kind::User, written),
        _ => return None,
    })
}

/// Every spelling `.aidl` renders for this shape at this place: one, or two
/// when the base is a bare path that may be an enum or a parcelable. Empty
/// when `.aidl` refuses the combination outright (`out String`, `out int`).
pub(crate) fn canonical(shape: &Shape, place: Place) -> Vec<String> {
    let mut out = Vec::new();
    let assumptions: &[bool] = if shape.kind == Kind::User {
        &[true, false]
    } else {
        &[false]
    };
    for &user_is_enum in assumptions {
        if let Some(s) = render(shape, place, user_is_enum) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// The written type against what `.aidl` renders for its shape — the last
/// word, after the specific rules have had their say, so anything they do not
/// name is still held to the contract.
///
/// Silent on a type with no AIDL shape (`()`, a trait object, `Vec<Vec<T>>`):
/// those have their own diagnostics, and refusing them here on a shape nobody
/// computed would be worse than saying nothing.
pub(crate) fn check_canonical(ty: &syn::Type, place: Place) -> syn::Result<()> {
    let Some(shape) = shape_of(ty) else {
        return Ok(());
    };
    let rendered = canonical(&shape, place);
    if rendered.is_empty() {
        return Ok(());
    }
    let written = simplified(ty);
    if rendered
        .iter()
        .any(|c| syn::parse_str::<syn::Type>(c).is_ok_and(|t| simplified(&t) == written))
    {
        return Ok(());
    }
    // The spelling offered is the one this table computed, and `type_matrix`
    // holds that table against the generator and the generator's output
    // against the gate — so the advice is accepted by construction rather than
    // by a reviewer noticing. That is the whole reason this reads as a
    // comparison and not as another rule.
    let message = match rendered.as_slice() {
        [one] => format!(
            "`.aidl` renders this as `{one}` here, and a call site written against one does \
             not take the other; use `{one}`"
        ),
        many => format!(
            "`.aidl` renders this as `{}` here — an `.aidl` enum and a parcelable are spelled \
             the same way in Rust, so the macro cannot tell which this name is — and a call \
             site written against one does not take the other; use the one that matches",
            many.join("` or `")
        ),
    };
    Err(syn::Error::new_spanned(ty, message))
}

/// The spelling, with `Vec` and `Option` reduced to their last segment. A
/// qualified `std::vec::Vec<T>` is the same type the generator writes bare, so
/// comparing the two literally would refuse a spelling the crate supports.
fn simplified(ty: &syn::Type) -> String {
    use crate::type_str::unwrap_group;
    use syn::Type;
    match unwrap_group(ty) {
        Type::Reference(r) => {
            let inner = simplified(&r.elem);
            if r.mutability.is_some() {
                format!("&mut {inner}")
            } else {
                format!("&{inner}")
            }
        }
        Type::Slice(s) => format!("[{}]", simplified(&s.elem)),
        Type::Array(a) => {
            let len = &a.len;
            format!(
                "[{}; {}]",
                simplified(&a.elem),
                quote::quote!(#len).to_string().replace(' ', "")
            )
        }
        Type::Path(p) => {
            let Some(seg) = p.path.segments.last() else {
                return quote::quote!(#ty).to_string();
            };
            let args = match &seg.arguments {
                syn::PathArguments::AngleBracketed(a) => {
                    let inner: Vec<String> = a
                        .args
                        .iter()
                        .map(|g| match g {
                            syn::GenericArgument::Type(t) => simplified(t),
                            other => quote::quote!(#other).to_string(),
                        })
                        .collect();
                    format!("<{}>", inner.join(", "))
                }
                _ => String::new(),
            };
            if seg.ident == "Vec" || seg.ident == "Option" {
                return format!("{}{args}", seg.ident);
            }
            let head: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            format!("{}{args}", head.join("::"))
        }
        other => quote::quote!(#other).to_string(),
    }
}

/// `array_type_name`: `byte`'s spelling moves with the place — `i8` as a
/// scalar, `u8` as an array element.
fn elem_name(shape: &Shape) -> String {
    if shape.kind == Kind::Primitive && shape.base == "i8" {
        "u8".to_string()
    } else {
        shape.base.clone()
    }
}

/// `nullable_element`: a `@nullable` array wraps each element unless it is a
/// primitive, which is written bare.
fn nullable_element(shape: &Shape, user_is_enum: bool, elem: &str) -> String {
    if shape.kind.is_primitive(user_is_enum) {
        elem.to_string()
    } else {
        format!("Option<{elem}>")
    }
}

/// `[[E; 3]; 2]` from sizes `[2, 3]`, as `make_fixed_array` folds them.
fn nest(value: &str, sizes: &[String]) -> String {
    let last = sizes.last().expect("a fixed array has at least one size");
    sizes
        .iter()
        .rev()
        .skip(1)
        .fold(format!("[{value}; {last}]"), |acc, size| {
            format!("[{acc}; {size}]")
        })
}

/// The element slot of a fixed-size array (`make_fixed_array`'s `value_str`).
fn fixed_elem(shape: &Shape, place: Place, user_is_enum: bool, elem: &str) -> String {
    let wrapped = match place {
        Place::Field => {
            (shape.nullable && shape.kind.is_aidl_nullable(user_is_enum))
                || !shape.kind.can_be_defaulted()
        }
        Place::Out | Place::Inout => {
            if !shape.kind.can_be_defaulted() {
                true
            } else {
                return if shape.nullable {
                    nullable_element(shape, user_is_enum, elem)
                } else {
                    elem.to_string()
                };
            }
        }
        // `in` and a return leave a fixed array's elements alone.
        _ => false,
    };
    if wrapped {
        format!("Option<{elem}>")
    } else {
        elem.to_string()
    }
}

/// One rendering, under one assumption about what a bare path names. `None`
/// when `.aidl` refuses the combination.
///
/// Reachable on its own so the equivalence test can pin the assumption: the
/// fixture knows which of its two bare paths is the enum, and `.aidl` refuses
/// some cells for one of them only (`@nullable Mode`), which the union
/// [`canonical`] returns cannot express.
pub(crate) fn render(shape: &Shape, place: Place, user_is_enum: bool) -> Option<String> {
    let borrowed = matches!(place, Place::In | Place::Out | Place::Inout);
    match &shape.arity {
        Arity::Scalar if borrowed => scalar_arg(shape, place, user_is_enum),
        Arity::Scalar => Some(scalar_owned(shape, place)),
        Arity::Var if borrowed => Some(var_array_arg(shape, place, user_is_enum)),
        Arity::Var => Some(var_array_owned(shape, place, user_is_enum)),
        Arity::Fixed(sizes) => {
            let elem = elem_name(shape);
            let fa = nest(&fixed_elem(shape, place, user_is_enum, &elem), sizes);
            Some(if borrowed {
                fixed_array_arg(shape, place, &fa)
            } else if shape.nullable {
                format!("Option<{fa}>")
            } else {
                fa
            })
        }
    }
}

/// `type_decl_for_func`'s non-array arms.
fn scalar_arg(shape: &Shape, place: Place, user_is_enum: bool) -> Option<String> {
    let base = &shape.base;
    let out_like = matches!(place, Place::Out | Place::Inout);
    if shape.kind == Kind::Str {
        // AIDL passes a `String` `in` only.
        if out_like {
            return None;
        }
        return Some(if shape.nullable {
            "Option<&str>".to_string()
        } else {
            "&str".to_string()
        });
    }
    if out_like {
        // …and a primitive, an enum among them.
        if shape.kind.is_primitive(user_is_enum) {
            return None;
        }
        let wrapped =
            shape.nullable || (matches!(place, Place::Out) && !shape.kind.can_be_defaulted());
        return Some(if wrapped {
            format!("&mut Option<{base}>")
        } else {
            format!("&mut {base}")
        });
    }
    Some(if shape.kind.is_primitive(user_is_enum) {
        base.clone()
    } else if shape.nullable {
        format!("Option<&{base}>")
    } else {
        format!("&{base}")
    })
}

/// `type_declaration`'s non-array arm: a value with no `Default` becomes
/// `Option<_>` in a field, whatever its nullability.
fn scalar_owned(shape: &Shape, place: Place) -> String {
    let promoted =
        shape.nullable || (matches!(place, Place::Field) && !shape.kind.can_be_defaulted());
    if promoted {
        format!("Option<{}>", shape.base)
    } else {
        shape.base.clone()
    }
}

/// `func_list_type_decl`'s variable-length arms.
fn var_array_arg(shape: &Shape, place: Place, user_is_enum: bool) -> String {
    let elem = elem_name(shape);
    match place {
        Place::Out => {
            if shape.nullable {
                format!(
                    "&mut Option<Vec<{}>>",
                    nullable_element(shape, user_is_enum, &elem)
                )
            } else if shape.kind.can_be_defaulted() || shape.kind.is_primitive(user_is_enum) {
                format!("&mut Vec<{elem}>")
            } else {
                format!("&mut Vec<Option<{elem}>>")
            }
        }
        Place::Inout => {
            if shape.nullable {
                format!(
                    "&mut Option<Vec<{}>>",
                    nullable_element(shape, user_is_enum, &elem)
                )
            } else {
                format!("&mut Vec<{elem}>")
            }
        }
        _ => {
            if !shape.nullable {
                format!("&[{elem}]")
            } else if shape.kind.is_primitive(user_is_enum) {
                format!("Option<&[{elem}]>")
            } else {
                format!("Option<&[Option<{elem}>]>")
            }
        }
    }
}

/// `list_type_decl`'s `Direction::None` arm, then `type_declaration`'s wrap.
fn var_array_owned(shape: &Shape, place: Place, user_is_enum: bool) -> String {
    let elem = elem_name(shape);
    let inner = if matches!(place, Place::Field) {
        if shape.nullable && shape.kind.is_aidl_nullable(user_is_enum) {
            format!("Vec<Option<{elem}>>")
        } else {
            format!("Vec<{elem}>")
        }
    } else if !shape.nullable {
        format!("Vec<{elem}>")
    } else if shape.kind.is_primitive(user_is_enum) {
        return format!("Option<Vec<{elem}>>");
    } else {
        return format!("Option<Vec<Option<{elem}>>>");
    };
    if shape.nullable {
        format!("Option<{inner}>")
    } else {
        inner
    }
}

/// `func_list_type_decl_fixed`.
fn fixed_array_arg(shape: &Shape, place: Place, fa: &str) -> String {
    match place {
        Place::Out | Place::Inout => {
            if shape.nullable {
                format!("&mut Option<{fa}>")
            } else {
                format!("&mut {fa}")
            }
        }
        _ => {
            if shape.nullable {
                format!("Option<&{fa}>")
            } else {
                format!("&{fa}")
            }
        }
    }
}
