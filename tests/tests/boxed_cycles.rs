// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A reference cycle between parcelables is cut by its one boxed field; every other field on
//! the cycle stays inline. AOSP `CheckNoRecursiveDefinition` (`parser.cpp:144-195`) accepts
//! these shapes, and the generated types must be finite, or this file does not compile.

use rsbinder::{Parcel, Parcelable};

include!(concat!(env!("OUT_DIR"), "/boxed_cycles.rs"));

use boxedcycles::BoxedCycles::{Inner::Inner, Node::Node, Outer::Outer, Tree::Tree};

fn round_trip<T>(value: &T) -> T
where
    T: Parcelable + Default,
{
    let mut parcel = Parcel::new();
    value.write_to_parcel(&mut parcel).unwrap();
    parcel.set_data_position(0);
    let mut back = T::default();
    back.read_from_parcel(&mut parcel).unwrap();
    back
}

#[test]
fn inline_field_beside_a_heap_nullable_edge_round_trips() {
    let value = Inner {
        value: 1,
        outer: Outer {
            value: 2,
            inner: Some(Box::new(Inner {
                value: 3,
                ..Default::default()
            })),
        },
    };
    let back = round_trip(&value);
    assert_eq!((back.value, back.outer.value), (1, 2));
    let inner = back.outer.inner.expect("inner");
    assert_eq!(inner.value, 3);
    assert!(inner.outer.inner.is_none());
}

#[test]
fn fixed_size_array_beside_a_boxed_back_edge_round_trips() {
    let mut value = Tree::default();
    value.nodes[1].value = 5;
    value.nodes[2].owner = Some(Box::new(Tree::default()));
    let back = round_trip(&value);
    assert_eq!(back.nodes[1].value, 5);
    assert!(back.nodes[0].owner.is_none());
    let owner = back.nodes[2].owner.as_ref().expect("owner");
    assert!(owner.nodes.iter().all(|n: &Node| n.owner.is_none()));
}
