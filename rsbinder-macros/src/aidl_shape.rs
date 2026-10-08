// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Port of `TypeGenerator`'s spelling for a shape at a place; `type_matrix` checks the copy.

use crate::type_str::Place;

/// What the macro can tell about a type from its spelling alone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An AIDL scalar; an `.aidl` enum is one on the wire too, hence [`Kind::User`]'s two renders.
    Primitive,
    Str,
    /// A binder, interface or fd: no `Default`, so `Option<_>` wherever nothing starts it.
    NoDefault,
    /// A bare path: an enum or a parcelable, which render differently, so both are canonical.
    User,
    /// A path with type arguments: `.aidl` gives those to a parcelable or union, never an enum.
    Generic,
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

    /// `TypeGenerator::can_be_defaulted`, the same in both modes for every kind named here.
    fn can_be_defaulted(self) -> bool {
        !matches!(self, Kind::NoDefault)
    }
}

/// Scalar, `T[]`, or `T[N]` — sizes outermost first, as `make_fixed_array` folds them.
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
    /// The scalar as the signature spells it (`i32`, `String`, `super::Cfg::Cfg`).
    pub base: String,
    /// The outer `@nullable`, not an element's own.
    pub nullable: bool,
    pub arity: Arity,
}

/// The shape of any spelling, wrong ones included, so a refusal can name the right one.
pub(crate) fn shape_of(ty: &syn::Type) -> Option<Shape> {
    use crate::type_str::unwrap_group;
    use syn::Type;

    // One `&`/`&mut` is the direction's, not the shape's.
    let mut cur = unwrap_group(ty);
    if let Type::Reference(r) = cur {
        cur = unwrap_group(&r.elem);
    }

    // The outermost `Option` is the `@nullable`; an element's own is the generator's.
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

/// The leaf's kind and scalar spelling; an array's `u8` is `byte`, kept as written.
fn leaf_kind(leaf: &syn::Type, in_array: bool) -> Option<(Kind, String)> {
    // `Parcelable`: a field's `self::` is legal, so it must still reach the gate.
    let written = crate::type_str::as_written_in(leaf, crate::type_str::Ctx::Parcelable).ok()?;
    let name = match crate::type_str::unwrap_group(leaf) {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    };
    let plain = matches!(
        crate::type_str::unwrap_group(leaf),
        syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| matches!(s.arguments, syn::PathArguments::None))
    );
    let angled = matches!(
        crate::type_str::unwrap_group(leaf),
        syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| matches!(s.arguments, syn::PathArguments::AngleBracketed(_)))
    );
    Some(match name.as_deref() {
        Some("str") => (Kind::Str, "String".to_string()),
        // `written`, so a qualified spelling compares equal to itself.
        Some("String") => (Kind::Str, written),
        Some("u8") if in_array && plain => (Kind::Primitive, written),
        Some("bool" | "i8" | "i32" | "i64" | "f32" | "f64" | "u16") if plain => {
            (Kind::Primitive, written)
        }
        Some("SIBinder" | "ParcelFileDescriptor") if plain => (Kind::NoDefault, written),
        Some("Strong") => (Kind::NoDefault, written),
        Some(_) if plain => (Kind::User, written),
        Some("Vec" | "Option") => return None,
        Some(_) if angled && crate::type_str::std_box_arg(leaf).is_none() => {
            (Kind::Generic, written)
        }
        _ => return None,
    })
}

/// `.aidl`'s spellings here: two for a bare path, none where it refuses (`out int`).
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

/// The written type against `.aidl`'s rendering; silent where no shape exists (`()`, `dyn`).
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
    // The advice is this table's output, which `type_matrix` proves the gate accepts.
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

/// The spelling with `Vec`/`Option` unqualified, so `std::vec::Vec<T>` matches `Vec<T>`.
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

/// `array_type_name`: `byte` is `i8` as a scalar and `u8` as an array element.
fn elem_name(shape: &Shape) -> String {
    if shape.kind == Kind::Primitive && shape.base == "i8" {
        "u8".to_string()
    } else {
        shape.base.clone()
    }
}

/// `nullable_element`: a `@nullable` array wraps each element but a primitive.
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
        // Only `out` elements need `Default`; `@nullable` wraps in every other place.
        Place::Out if !shape.kind.can_be_defaulted() => true,
        _ => {
            return if shape.nullable {
                nullable_element(shape, user_is_enum, elem)
            } else {
                elem.to_string()
            };
        }
    };
    if wrapped {
        format!("Option<{elem}>")
    } else {
        elem.to_string()
    }
}

/// One rendering under one enum-or-not assumption (`None` if `.aidl` refuses), for the tests.
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
    // `.aidl` refuses `@nullable` on a primitive, so no by-value spelling keeps the `Option`.
    if shape.nullable && shape.kind.is_primitive(user_is_enum) {
        return None;
    }
    Some(if shape.kind.is_primitive(user_is_enum) {
        base.clone()
    } else if shape.nullable {
        format!("Option<&{base}>")
    } else {
        format!("&{base}")
    })
}

/// `type_declaration`'s non-array arm: a field with no `Default` is always `Option<_>`.
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
